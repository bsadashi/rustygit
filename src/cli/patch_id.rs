//! `rustygit patch-id` — compute a stable identity for a unified-diff
//! patch. Identity ignores file paths, line numbers, and whitespace —
//! so cherry-picking the same change to a different branch keeps the
//! same patch-id.
//!
//! Algorithm (matches `git patch-id`):
//!   1. Read unified diff from stdin (typically `git diff-tree -p`
//!      output for one or more commits).
//!   2. For each commit chunk (delimited by `From <oid> ` or `commit <oid>`
//!      lines), accumulate the diff body with:
//!       * `@@ … @@` lines stripped
//!       * leading `+`/`-`/' ' kept; everything else stripped
//!       * all whitespace removed within the kept content (default,
//!         non-stable mode); with `--stable`, hash per-file instead.
//!   3. Print `<patch-sha1> <commit-sha1>` per commit.

use std::io::{self, Read};

use clap::Args;

use crate::hash::{hash_all, HashKind, ObjectId};

#[derive(Debug, Args)]
pub struct PatchIdArgs {
    /// Use the order-stable hashing variant (per-file rolling).
    #[arg(long = "stable")]
    pub stable: bool,
    /// Use the order-unstable variant (default).
    #[arg(long = "unstable")]
    pub unstable: bool,
}

pub fn run(args: PatchIdArgs) -> io::Result<i32> {
    let mut input = Vec::new();
    io::stdin().read_to_end(&mut input)?;

    if args.stable && args.unstable {
        eprintln!("rustygit: patch-id: --stable and --unstable are mutually exclusive");
        return Ok(129);
    }

    // Default behavior: --unstable. (Matches `git patch-id` default in
    // recent versions.)
    let stable = args.stable;

    for (patch_oid, commit_oid) in compute_patch_ids(&input, stable) {
        let commit_str = commit_oid
            .map(|o| o.to_string())
            .unwrap_or_else(|| "0000000000000000000000000000000000000000".to_string());
        println!("{patch_oid} {commit_str}");
    }
    Ok(0)
}

/// Compute the patch-id for one or more diff chunks. Returns
/// `(patch_id, commit_id_if_known)` per chunk.
pub fn compute_patch_ids(input: &[u8], stable: bool) -> Vec<(ObjectId, Option<ObjectId>)> {
    let mut out = Vec::new();
    let mut current_commit: Option<ObjectId> = None;
    let mut buf: Vec<u8> = Vec::new();

    let flush = |buf: &mut Vec<u8>,
                 current_commit: &mut Option<ObjectId>,
                 out: &mut Vec<(ObjectId, Option<ObjectId>)>| {
        if !buf.is_empty() {
            let id = hash_all(HashKind::Sha1, buf);
            out.push((id, *current_commit));
            buf.clear();
        }
        *current_commit = None;
    };

    for line in input.split(|&b| b == b'\n') {
        // Strip optional trailing \r so CRLF inputs work.
        let line: &[u8] = if line.ends_with(b"\r") {
            &line[..line.len() - 1]
        } else {
            line
        };

        // A new commit boundary flushes any in-flight chunk.
        let commit_oid = parse_commit_header(line);
        if commit_oid.is_some() {
            flush(&mut buf, &mut current_commit, &mut out);
            current_commit = commit_oid;
            continue;
        }

        if line.starts_with(b"diff --git ") {
            // In stable mode, the per-file marker contributes to the hash.
            if stable {
                buf.extend_from_slice(line);
                buf.push(b'\n');
            }
            continue;
        }
        // Skip diff metadata that doesn't affect identity.
        if line.starts_with(b"index ")
            || line.starts_with(b"--- ")
            || line.starts_with(b"+++ ")
            || line.starts_with(b"@@")
            || line.starts_with(b"old mode ")
            || line.starts_with(b"new mode ")
            || line.starts_with(b"deleted file mode ")
            || line.starts_with(b"new file mode ")
            || line.starts_with(b"similarity index ")
            || line.starts_with(b"rename from ")
            || line.starts_with(b"rename to ")
        {
            continue;
        }

        // Keep only +/-/' ' prefixed body lines. Strip whitespace within.
        if line.starts_with(b"+") || line.starts_with(b"-") || line.starts_with(b" ") {
            let kept: Vec<u8> = line
                .iter()
                .copied()
                .filter(|b| !b.is_ascii_whitespace())
                .collect();
            buf.extend_from_slice(&kept);
        }
    }

    flush(&mut buf, &mut current_commit, &mut out);
    out
}

/// Recognize commit-boundary lines emitted by `git format-patch` or
/// `git diff-tree -p`.
fn parse_commit_header(line: &[u8]) -> Option<ObjectId> {
    // Variant 1: `From <40-hex> Mon Sep ...` (format-patch).
    if let Some(rest) = line.strip_prefix(b"From ") {
        if rest.len() >= 40 && rest[40..].first() == Some(&b' ') {
            if let Ok(hex) = std::str::from_utf8(&rest[..40]) {
                if let Ok(oid) = ObjectId::parse_hex(HashKind::Sha1, hex) {
                    return Some(oid);
                }
            }
        }
    }
    // Variant 2: `commit <40-hex>` (git log -p).
    if let Some(rest) = line.strip_prefix(b"commit ") {
        let hex_part = rest.split(|&b| b == b' ').next().unwrap_or(rest);
        if hex_part.len() == 40 {
            if let Ok(hex) = std::str::from_utf8(hex_part) {
                if let Ok(oid) = ObjectId::parse_hex(HashKind::Sha1, hex) {
                    return Some(oid);
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Different leading whitespace + line numbers must NOT change patch-id.
    #[test]
    fn whitespace_and_hunk_lines_dont_affect_id() {
        let a = b"diff --git a/x b/x
index 1234567..89abcde 100644
--- a/x
+++ b/x
@@ -1,3 +1,3 @@
 a
-b
+B
 c
";
        let b = b"diff --git a/x b/x
index 0000000..1111111 100644
--- a/x
+++ b/x
@@ -100,3 +100,3 @@
   a
- b
+ B
   c
";
        let ida = compute_patch_ids(a, false)[0].0;
        let idb = compute_patch_ids(b, false)[0].0;
        assert_eq!(ida, idb);
    }

    /// Different content DOES change patch-id.
    #[test]
    fn content_difference_changes_id() {
        let a = b"--- a/x
+++ b/x
@@ -1 +1 @@
-old
+new
";
        let b = b"--- a/x
+++ b/x
@@ -1 +1 @@
-old
+different
";
        let ida = compute_patch_ids(a, false)[0].0;
        let idb = compute_patch_ids(b, false)[0].0;
        assert_ne!(ida, idb);
    }

    #[test]
    fn picks_up_commit_oid_when_present() {
        let input = b"commit abcdefabcdefabcdefabcdefabcdefabcdefabcd
Author: x
Date: ...

--- a/x
+++ b/x
@@ -1 +1 @@
-a
+b
";
        let out = compute_patch_ids(input, false);
        assert_eq!(out.len(), 1);
        assert_eq!(
            out[0].1.unwrap().to_string(),
            "abcdefabcdefabcdefabcdefabcdefabcdefabcd"
        );
    }
}
