//! Format a single `DiffPair` as `git diff` does.
//!
//! Output shape (matches git's defaults — no color, 3 lines context, no rename
//! detection):
//!
//! ```text
//! diff --git a/<path> b/<path>
//! [old mode <oct>\nnew mode <oct>\n]
//! index <a-short>..<b-short> [<mode>]
//! --- a/<path>
//! +++ b/<path>
//! <hunks from xdiff::unified_diff>
//! ```
//!
//! Edge cases:
//!  - **Added**: `new file mode <m>` line; a-side is `0000000` and `/dev/null`.
//!  - **Deleted**: `deleted file mode <m>` line; b-side is `0000000` and `/dev/null`.
//!  - **Mode-only change** (oids equal): emit `old mode` / `new mode` and stop —
//!    no `index` line, no hunks.
//!  - **Binary files**: instead of hunks, emit
//!    `Binary files a/<path> and b/<path> differ\n`.
//!
//! Body hunks are produced by `crate::xdiff::unified_diff`, which Track A
//! provides. Until that lands, this module won't compile — that's the
//! intentional integration seam.

use std::io::{self, Write};

use crate::diff::{DiffEntry, DiffPair, DiffStatus};
use crate::object::ObjectKind;
use crate::repo::Repository;
use crate::tree::FileMode;
use crate::xdiff::{unified_diff, UnifiedDiffOpts};

const SHORT_OID_LEN: usize = 7;
/// Size of the prefix we scan for binary detection. Must be small enough to be
/// cheap, large enough to catch most binaries (matches git's `BUFSIZ`-ish heuristic).
const BINARY_DETECT_PROBE: usize = 8000;

/// Format one `DiffPair`, writing all of its lines (header + body) to `out`.
pub fn format_pair<W: Write>(repo: &Repository, pair: &DiffPair, out: &mut W) -> io::Result<()> {
    match pair.status {
        DiffStatus::Added => format_added(repo, pair, out),
        DiffStatus::Deleted => format_deleted(repo, pair, out),
        DiffStatus::ModeChanged => format_mode_only(pair, out),
        DiffStatus::Modified | DiffStatus::TypeChanged => format_modified(repo, pair, out),
    }
}

fn header_line<W: Write>(out: &mut W, a: &[u8], b: &[u8]) -> io::Result<()> {
    out.write_all(b"diff --git a/")?;
    out.write_all(a)?;
    out.write_all(b" b/")?;
    out.write_all(b)?;
    out.write_all(b"\n")
}

fn format_added<W: Write>(repo: &Repository, pair: &DiffPair, out: &mut W) -> io::Result<()> {
    let b = pair
        .b
        .as_ref()
        .expect("Added pair must have a b-side entry");
    header_line(out, &b.path, &b.path)?;
    writeln!(out, "new file mode {}", b.mode.as_octal())?;
    writeln!(
        out,
        "index {}..{}",
        zero_short(),
        b.oid.short_hex(SHORT_OID_LEN)
    )?;

    let b_data = read_blob_payload(repo, b)?;
    if is_binary(&b_data) {
        // Per git: "Binary files /dev/null and b/<path> differ".
        out.write_all(b"Binary files /dev/null and b/")?;
        out.write_all(&b.path)?;
        out.write_all(b" differ\n")?;
        return Ok(());
    }
    out.write_all(b"--- /dev/null\n")?;
    out.write_all(b"+++ b/")?;
    out.write_all(&b.path)?;
    out.write_all(b"\n")?;
    write_hunks(&[], &b_data, out)
}

fn format_deleted<W: Write>(repo: &Repository, pair: &DiffPair, out: &mut W) -> io::Result<()> {
    let a = pair
        .a
        .as_ref()
        .expect("Deleted pair must have an a-side entry");
    header_line(out, &a.path, &a.path)?;
    writeln!(out, "deleted file mode {}", a.mode.as_octal())?;
    writeln!(
        out,
        "index {}..{}",
        a.oid.short_hex(SHORT_OID_LEN),
        zero_short()
    )?;

    let a_data = read_blob_payload(repo, a)?;
    if is_binary(&a_data) {
        out.write_all(b"Binary files a/")?;
        out.write_all(&a.path)?;
        out.write_all(b" and /dev/null differ\n")?;
        return Ok(());
    }
    out.write_all(b"--- a/")?;
    out.write_all(&a.path)?;
    out.write_all(b"\n")?;
    out.write_all(b"+++ /dev/null\n")?;
    write_hunks(&a_data, &[], out)
}

fn format_mode_only<W: Write>(pair: &DiffPair, out: &mut W) -> io::Result<()> {
    let a = pair.a.as_ref().expect("ModeChanged needs a-side");
    let b = pair.b.as_ref().expect("ModeChanged needs b-side");
    header_line(out, &a.path, &b.path)?;
    writeln!(out, "old mode {}", a.mode.as_octal())?;
    writeln!(out, "new mode {}", b.mode.as_octal())?;
    Ok(())
}

fn format_modified<W: Write>(repo: &Repository, pair: &DiffPair, out: &mut W) -> io::Result<()> {
    let a = pair.a.as_ref().expect("Modified needs a-side");
    let b = pair.b.as_ref().expect("Modified needs b-side");
    header_line(out, &a.path, &b.path)?;

    let mode_changed = a.mode != b.mode;
    if mode_changed {
        writeln!(out, "old mode {}", a.mode.as_octal())?;
        writeln!(out, "new mode {}", b.mode.as_octal())?;
    }

    // The trailing mode on the index line is omitted when the mode changes —
    // git only emits it when both sides share a mode.
    if mode_changed {
        writeln!(
            out,
            "index {}..{}",
            a.oid.short_hex(SHORT_OID_LEN),
            b.oid.short_hex(SHORT_OID_LEN)
        )?;
    } else {
        writeln!(
            out,
            "index {}..{} {}",
            a.oid.short_hex(SHORT_OID_LEN),
            b.oid.short_hex(SHORT_OID_LEN),
            a.mode.as_octal()
        )?;
    }

    let a_data = read_blob_payload(repo, a)?;
    let b_data = read_blob_payload(repo, b)?;

    if is_binary(&a_data) || is_binary(&b_data) {
        out.write_all(b"Binary files a/")?;
        out.write_all(&a.path)?;
        out.write_all(b" and b/")?;
        out.write_all(&b.path)?;
        out.write_all(b" differ\n")?;
        return Ok(());
    }

    out.write_all(b"--- a/")?;
    out.write_all(&a.path)?;
    out.write_all(b"\n")?;
    out.write_all(b"+++ b/")?;
    out.write_all(&b.path)?;
    out.write_all(b"\n")?;
    write_hunks(&a_data, &b_data, out)
}

/// Read the blob payload for `entry.oid` from the object database. For
/// gitlinks we synthesize a one-line "Subproject commit <oid>\n" payload
/// (matching `git diff`'s rendering of submodule pointers).
fn read_blob_payload(repo: &Repository, entry: &DiffEntry) -> io::Result<Vec<u8>> {
    if matches!(entry.mode, FileMode::Gitlink) {
        return Ok(format!("Subproject commit {}\n", entry.oid).into_bytes());
    }
    let obj = repo
        .odb()
        .read(&entry.oid)
        .map_err(|e| io::Error::other(format!("{e}")))?;
    if obj.kind != ObjectKind::Blob {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("expected blob, got {} for {}", obj.kind, entry.oid),
        ));
    }
    Ok(obj.data)
}

/// `0000000` (7 chars) — the all-zero short oid git uses for the "missing" side
/// of an Added or Deleted line.
fn zero_short() -> String {
    "0".repeat(SHORT_OID_LEN)
}

/// True if the buffer contains a NUL byte in its first 8000 bytes (matches
/// git's `buffer_is_binary` heuristic).
fn is_binary(data: &[u8]) -> bool {
    let probe_len = data.len().min(BINARY_DETECT_PROBE);
    data[..probe_len].contains(&0)
}

/// Delegate to xdiff for hunk generation. We use git's defaults: 3 lines of
/// context, Myers algorithm.
fn write_hunks<W: Write>(a: &[u8], b: &[u8], out: &mut W) -> io::Result<()> {
    let opts = UnifiedDiffOpts::default();
    unified_diff(a, b, &opts, out).map_err(|e| io::Error::other(format!("{e}")))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{is_binary, BINARY_DETECT_PROBE};

    #[test]
    fn binary_detection_finds_nul() {
        assert!(is_binary(b"abc\0def"));
    }

    #[test]
    fn pure_text_is_not_binary() {
        assert!(!is_binary(b"hello\nworld\n"));
    }

    #[test]
    fn empty_buffer_is_not_binary() {
        assert!(!is_binary(b""));
    }

    #[test]
    fn nul_after_probe_window_is_not_binary() {
        let mut data = vec![b'a'; BINARY_DETECT_PROBE];
        data.push(0);
        assert!(!is_binary(&data));
    }
}
