//! Text diff engine — Myers SES + unified-diff emitter.
//!
//! This module is the M5 port of git's `xdiff/` directory. It owns:
//! 1. The Myers `O(ND)` shortest-edit-script algorithm.
//! 2. Hunk grouping with N lines of context (default 3).
//! 3. The `@@ -<old-start>,<old-len> +<new-start>,<new-len> @@` hunk header,
//!    optionally with a "function context" trailer (mirroring `xemit.c`'s
//!    `def_ff` heuristic so we stay byte-compat with `git diff`).
//! 4. The `+`/`-`/space line prefixes and git's `\ No newline at end of file`
//!    annotation for records lacking a trailing newline.
//!
//! Header lines (`diff --git`, `--- a/`, `+++ b/`) are NOT this module's job —
//! the caller (`diff` plumbing in M14+) emits those.
//!
//! Bytes-only by design: diff inputs are arbitrary bytes (binary detection
//! happens elsewhere). All operations work on `&[u8]` slices.

use std::io::Write;

use thiserror::Error;

/// Options controlling unified diff emission.
#[derive(Debug, Clone)]
pub struct UnifiedDiffOpts {
    /// Lines of context before/after each change. Default 3.
    pub context: usize,
    /// Diff algorithm. M5 only ships Myers.
    pub algorithm: Algorithm,
}

/// Diff algorithm selector. The histogram/patience variants are deferred to
/// M16+; for now we only have Myers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Algorithm {
    Myers,
}

impl Default for UnifiedDiffOpts {
    fn default() -> Self {
        Self {
            context: 3,
            algorithm: Algorithm::Myers,
        }
    }
}

#[derive(Error, Debug)]
pub enum XdiffError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// Compute a unified diff between `a` and `b`, treated as line-oriented byte
/// streams. Writes the body — `@@ ... @@` header(s) plus +/-/space lines — to
/// `out`. Does NOT emit `--- a/` / `+++ b/` headers; that's the caller's job.
///
/// If `a == b` byte-for-byte, returns `Ok(())` without writing anything.
///
/// "No newline at end of file" annotations are emitted exactly as git does:
/// when a record lacks a trailing `\n`, the annotation line follows that record.
pub fn unified_diff<W: Write>(
    a: &[u8],
    b: &[u8],
    opts: &UnifiedDiffOpts,
    out: &mut W,
) -> Result<(), XdiffError> {
    if a == b {
        return Ok(());
    }
    let a_lines = split_lines(a);
    let b_lines = split_lines(b);
    let edits = myers_diff(&a_lines, &b_lines);
    let hunks = group_hunks(&edits, a_lines.len(), b_lines.len(), opts.context);
    for hunk in &hunks {
        emit_hunk(&a_lines, &b_lines, hunk, out)?;
    }
    Ok(())
}

/// Split `input` into lines, preserving the line terminator (`\n`) when
/// present. The last line may have no terminator, in which case the slice
/// still contains it (no `\n` byte). An empty input yields `vec![]` —
/// callers who care about "is the last line newline-terminated?" can ask
/// `lines.last().is_some_and(|l| l.last() == Some(&b'\n'))`.
///
/// Exposed because callers (e.g. `diff --raw`) sometimes want byte-level
/// access to the same line splitting we use internally.
pub fn split_lines(input: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut i = 0usize;
    while i < input.len() {
        if input[i] == b'\n' {
            out.push(&input[start..=i]);
            start = i + 1;
        }
        i += 1;
    }
    if start < input.len() {
        out.push(&input[start..]);
    }
    out
}

// ----------------------------------------------------------------------------
// Myers diff (O(ND))
// ----------------------------------------------------------------------------

/// One element of a shortest edit script. We carry indices into the original
/// `a`/`b` line vectors so the formatter can grab the line bytes directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Edit {
    /// `a[ai] == b[bi]` — passes through unchanged.
    Equal { ai: usize, bi: usize },
    /// Line removed from `a`.
    Delete { ai: usize },
    /// Line inserted from `b`.
    Insert { bi: usize },
}

/// Myers' "An O(ND) Difference Algorithm and Its Variations" (1986).
///
/// The greedy variant: build the V[k] trace through D-paths, then walk back
/// from `(N, M)` to the origin to recover the edit script. We use the basic
/// O(ND) memory variant (storing all V snapshots); for the typical-text-file
/// inputs we'll see in practice (few thousand lines), the memory hit is
/// negligible. The divide-and-conquer linear-space refinement is a
/// straightforward refactor when we need it; deferred to M16+.
fn myers_diff<T: AsRef<[u8]>>(a: &[T], b: &[T]) -> Vec<Edit> {
    let n = a.len() as isize;
    let m = b.len() as isize;
    let max = n + m;

    if n == 0 && m == 0 {
        return Vec::new();
    }

    // V is indexed by k in [-max, max], shifted by `max` to fit in a Vec.
    let v_len = (2 * max + 1) as usize;
    let offset = max;
    let mut v = vec![0isize; v_len];
    let mut trace: Vec<Vec<isize>> = Vec::new();

    let mut found_d: Option<usize> = None;
    'outer: for d in 0..=max {
        // Snapshot V before mutating it for this D, so we can backtrack.
        trace.push(v.clone());
        let mut k = -d;
        while k <= d {
            // Pick the better of the two predecessors: down (insert from b)
            // or right (delete from a).
            let down =
                k == -d || (k != d && v[(k - 1 + offset) as usize] < v[(k + 1 + offset) as usize]);
            let mut x = if down {
                v[(k + 1 + offset) as usize]
            } else {
                v[(k - 1 + offset) as usize] + 1
            };
            let mut y = x - k;

            // Slide along the diagonal as long as a[x] == b[y].
            while x < n && y < m && eq(&a[x as usize], &b[y as usize]) {
                x += 1;
                y += 1;
            }

            v[(k + offset) as usize] = x;
            if x >= n && y >= m {
                found_d = Some(d as usize);
                break 'outer;
            }
            k += 2;
        }
    }

    let d = found_d.expect("Myers always terminates within N+M steps");
    backtrack(&trace, n, m, d)
}

#[inline]
fn eq<T: AsRef<[u8]>, U: AsRef<[u8]>>(a: &T, b: &U) -> bool {
    a.as_ref() == b.as_ref()
}

/// Walk the trace backwards from `(n, m)` to the origin to recover the SES.
/// At each D-step we look up the V snapshot and decide whether to go down
/// (insert) or right (delete), then slide back along any diagonal.
fn backtrack(trace: &[Vec<isize>], n: isize, m: isize, d_final: usize) -> Vec<Edit> {
    let max = n + m;
    let offset = max;
    let mut x = n;
    let mut y = m;
    // We build `edits` reversed and flip at the end.
    let mut edits: Vec<Edit> = Vec::new();

    for d in (0..=d_final).rev() {
        let v = &trace[d];
        let k = x - y;

        let down = k == -(d as isize)
            || (k != d as isize && v[(k - 1 + offset) as usize] < v[(k + 1 + offset) as usize]);
        let prev_k = if down { k + 1 } else { k - 1 };
        let prev_x = v[(prev_k + offset) as usize];
        let prev_y = prev_x - prev_k;

        // Slide back along the diagonal (these are "Equal" edits).
        while x > prev_x && y > prev_y {
            edits.push(Edit::Equal {
                ai: (x - 1) as usize,
                bi: (y - 1) as usize,
            });
            x -= 1;
            y -= 1;
        }

        if d > 0 {
            // The single non-diagonal step at this D level: either an insert
            // (we came from down) or a delete (we came from right).
            if down {
                edits.push(Edit::Insert {
                    bi: prev_y as usize,
                });
            } else {
                edits.push(Edit::Delete {
                    ai: prev_x as usize,
                });
            }
        }
        x = prev_x;
        y = prev_y;
    }

    edits.reverse();
    edits
}

// ----------------------------------------------------------------------------
// Hunk grouping
// ----------------------------------------------------------------------------

/// A run of consecutive edits to be emitted as one `@@` block, with up to
/// `context` lines of surrounding equal content on each side.
///
/// `a_start` / `a_len` are 0-based offsets and lengths into the `a_lines`
/// vector; same for `b_start` / `b_len`.
#[derive(Debug, Clone)]
struct Hunk {
    a_start: usize,
    a_len: usize,
    b_start: usize,
    b_len: usize,
    /// The slice of the SES this hunk represents, with leading/trailing
    /// context Equals included.
    edits: Vec<Edit>,
}

/// Walk the SES, accumulate non-Equal edits into hunks, and merge adjacent
/// hunks whose intervening Equals are short enough to absorb. Once finalized,
/// each hunk is padded with up to `context` Equals on each side.
fn group_hunks(edits: &[Edit], a_total: usize, b_total: usize, context: usize) -> Vec<Hunk> {
    let mut hunks: Vec<Hunk> = Vec::new();
    if edits.is_empty() {
        return hunks;
    }

    // First pass: find runs of non-Equal edits with their indices in `edits`.
    let mut runs: Vec<(usize, usize)> = Vec::new(); // (start_in_edits, end_in_edits inclusive)
    let mut i = 0;
    while i < edits.len() {
        if !matches!(edits[i], Edit::Equal { .. }) {
            let start = i;
            while i < edits.len() && !matches!(edits[i], Edit::Equal { .. }) {
                i += 1;
            }
            runs.push((start, i - 1));
        } else {
            i += 1;
        }
    }

    if runs.is_empty() {
        return hunks;
    }

    // Second pass: merge runs whose gap (number of Equals between them) is
    // <= 2 * context. Otherwise the context windows would not overlap and
    // each run becomes its own hunk.
    let mut merged: Vec<(usize, usize)> = Vec::new();
    for &(s, e) in &runs {
        if let Some(last) = merged.last_mut() {
            // Number of Equal edits strictly between `last.1` and `s`.
            let gap = s.saturating_sub(last.1 + 1);
            if gap <= 2 * context {
                last.1 = e;
                continue;
            }
        }
        merged.push((s, e));
    }

    // Third pass: turn each merged run into a Hunk with up to `context`
    // surrounding Equals.
    for &(s, e) in &merged {
        let pre_start = s.saturating_sub(context);
        let post_end = (e + context + 1).min(edits.len()); // exclusive
        let slice = edits[pre_start..post_end].to_vec();

        // Determine a_start, b_start by looking at the first edit.
        let (a_start, b_start) = match slice.first().expect("non-empty hunk") {
            Edit::Equal { ai, bi } => (*ai, *bi),
            Edit::Delete { ai } => {
                // The position in b is the next b-index in the SES. We can
                // recover it by counting Equals/Inserts in the SES prefix
                // that precedes this run.
                let bi = count_b_before(edits, pre_start);
                (*ai, bi)
            }
            Edit::Insert { bi } => {
                let ai = count_a_before(edits, pre_start);
                (ai, *bi)
            }
        };

        let mut a_len = 0;
        let mut b_len = 0;
        for ed in &slice {
            match ed {
                Edit::Equal { .. } => {
                    a_len += 1;
                    b_len += 1;
                }
                Edit::Delete { .. } => a_len += 1,
                Edit::Insert { .. } => b_len += 1,
            }
        }

        // Sanity: don't overflow file length.
        debug_assert!(a_start + a_len <= a_total);
        debug_assert!(b_start + b_len <= b_total);
        let _ = (a_total, b_total);

        hunks.push(Hunk {
            a_start,
            a_len,
            b_start,
            b_len,
            edits: slice,
        });
    }

    hunks
}

/// Count the number of `a` lines that have been consumed by edits before
/// position `pos` in the SES.
fn count_a_before(edits: &[Edit], pos: usize) -> usize {
    edits[..pos]
        .iter()
        .filter(|e| matches!(e, Edit::Equal { .. } | Edit::Delete { .. }))
        .count()
}

/// Count the number of `b` lines that have been consumed by edits before
/// position `pos` in the SES.
fn count_b_before(edits: &[Edit], pos: usize) -> usize {
    edits[..pos]
        .iter()
        .filter(|e| matches!(e, Edit::Equal { .. } | Edit::Insert { .. }))
        .count()
}

// ----------------------------------------------------------------------------
// Hunk emission
// ----------------------------------------------------------------------------

/// Emit one hunk to `out`. Mirrors git's `xdl_emit_diff` for a single hunk.
fn emit_hunk<W: Write>(
    a_lines: &[&[u8]],
    b_lines: &[&[u8]],
    hunk: &Hunk,
    out: &mut W,
) -> Result<(), XdiffError> {
    // Header. Mirror git's `xdl_format_hunk_hdr`:
    //   "@@ -" + (len ? start+1 : start) + (len != 1 ? ",len" : "")
    //   " +" + ...
    //   " @@" + (func ? " " + func : "") + "\n"
    out.write_all(b"@@ -")?;
    write_pos(out, hunk.a_start, hunk.a_len)?;
    out.write_all(b" +")?;
    write_pos(out, hunk.b_start, hunk.b_len)?;
    out.write_all(b" @@")?;
    if let Some(func) = func_line_for_hunk(a_lines, hunk.a_start) {
        out.write_all(b" ")?;
        out.write_all(func)?;
    }
    out.write_all(b"\n")?;

    // Body: walk the edits and emit each line with its prefix.
    for edit in &hunk.edits {
        match edit {
            Edit::Equal { ai, .. } => emit_record(out, b" ", a_lines[*ai])?,
            Edit::Delete { ai } => emit_record(out, b"-", a_lines[*ai])?,
            Edit::Insert { bi } => emit_record(out, b"+", b_lines[*bi])?,
        }
    }

    Ok(())
}

/// Write `start+1` (1-based) when `len != 0`, or `start` when `len == 0`,
/// followed by `,len` if `len != 1`.
fn write_pos<W: Write>(out: &mut W, start: usize, len: usize) -> Result<(), XdiffError> {
    let printed_start = if len == 0 { start } else { start + 1 };
    write_usize(out, printed_start)?;
    if len != 1 {
        out.write_all(b",")?;
        write_usize(out, len)?;
    }
    Ok(())
}

fn write_usize<W: Write>(out: &mut W, n: usize) -> Result<(), XdiffError> {
    let mut buf = [0u8; 20];
    let mut len = 0;
    let mut x = n;
    if x == 0 {
        out.write_all(b"0")?;
        return Ok(());
    }
    while x > 0 {
        buf[len] = b'0' + (x % 10) as u8;
        x /= 10;
        len += 1;
    }
    buf[..len].reverse();
    out.write_all(&buf[..len])?;
    Ok(())
}

/// Emit a single record (line) with a one-byte prefix (`-`, `+`, or ` `).
/// Mirrors git's `xdl_emit_diffrec`: if the line lacks a trailing `\n`, append
/// `\n\\ No newline at end of file\n` after it.
fn emit_record<W: Write>(out: &mut W, prefix: &[u8], line: &[u8]) -> Result<(), XdiffError> {
    out.write_all(prefix)?;
    out.write_all(line)?;
    if !line.is_empty() && line[line.len() - 1] != b'\n' {
        out.write_all(b"\n\\ No newline at end of file\n")?;
    }
    Ok(())
}

/// Find the function-context line for a hunk that starts at `a_start` (0-based).
/// We walk backwards from `a_start - 1` looking for the first record that
/// matches `def_ff` (line starts with alphabetic / `_` / `$`). Mirrors git's
/// `def_ff` + `get_func_line` for the no-userdiff-driver case.
fn func_line_for_hunk<'a>(a_lines: &'a [&'a [u8]], a_start: usize) -> Option<&'a [u8]> {
    if a_start == 0 {
        return None;
    }
    let mut i = a_start as isize - 1;
    while i >= 0 {
        if let Some(trimmed) = def_ff(a_lines[i as usize]) {
            return Some(trimmed);
        }
        i -= 1;
    }
    None
}

/// Default git function-line matcher: a record whose first byte is an ASCII
/// alphabetic character, `_`, or `$`. Returns `Some(trimmed)` where `trimmed`
/// is the line with trailing whitespace (incl. `\n`) stripped, capped at 80
/// bytes (matching git's stack buf size). Returns `None` when the line doesn't
/// look like an identifier head.
fn def_ff(line: &[u8]) -> Option<&[u8]> {
    if line.is_empty() {
        return None;
    }
    let first = line[0];
    let is_id_head = first.is_ascii_alphabetic() || first == b'_' || first == b'$';
    if !is_id_head {
        return None;
    }
    // Cap at 80 bytes (git's func_line.buf size).
    let mut len = line.len().min(80);
    while len > 0 && (line[len - 1] as char).is_ascii_whitespace() {
        len -= 1;
    }
    Some(&line[..len])
}

// ----------------------------------------------------------------------------
// Tests
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn run(a: &[u8], b: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        unified_diff(a, b, &UnifiedDiffOpts::default(), &mut out).unwrap();
        out
    }

    fn run_ctx(a: &[u8], b: &[u8], context: usize) -> Vec<u8> {
        let mut out = Vec::new();
        unified_diff(
            a,
            b,
            &UnifiedDiffOpts {
                context,
                algorithm: Algorithm::Myers,
            },
            &mut out,
        )
        .unwrap();
        out
    }

    // ---------- split_lines ----------

    #[test]
    fn split_lines_basic() {
        let empty: Vec<&[u8]> = Vec::new();
        assert_eq!(split_lines(b""), empty);
        assert_eq!(split_lines(b"\n"), vec![&b"\n"[..]]);
        assert_eq!(split_lines(b"foo\n"), vec![&b"foo\n"[..]]);
        assert_eq!(split_lines(b"foo"), vec![&b"foo"[..]]);
        assert_eq!(split_lines(b"a\nb\n"), vec![&b"a\n"[..], &b"b\n"[..]]);
        assert_eq!(split_lines(b"a\nb"), vec![&b"a\n"[..], &b"b"[..]]);
    }

    #[test]
    fn split_lines_empty_lines_preserved() {
        assert_eq!(
            split_lines(b"a\n\nb\n"),
            vec![&b"a\n"[..], &b"\n"[..], &b"b\n"[..]]
        );
    }

    // ---------- identical inputs ----------

    #[test]
    fn identical_inputs_no_output() {
        assert_eq!(run(b"foo\nbar\n", b"foo\nbar\n"), b"");
    }

    #[test]
    fn two_empty_inputs_no_output() {
        assert_eq!(run(b"", b""), b"");
    }

    // ---------- single-line cases ----------

    #[test]
    fn single_line_modification() {
        // Spec: b"a\n" vs b"b\n" → @@ -1 +1 @@\n-a\n+b\n
        assert_eq!(run(b"a\n", b"b\n"), b"@@ -1 +1 @@\n-a\n+b\n");
    }

    #[test]
    fn single_byte_newline() {
        // Edge case: input is one byte `\n`. No diff if same.
        assert_eq!(run(b"\n", b"\n"), b"");
    }

    #[test]
    fn newline_vs_empty() {
        // empty file vs single empty line. git: @@ -0,0 +1 @@\n+\n
        assert_eq!(run(b"", b"\n"), b"@@ -0,0 +1 @@\n+\n");
    }

    // ---------- insertion / deletion ----------

    #[test]
    fn insertion_at_start() {
        // Spec: b"a\nb\n" vs b"x\na\nb\n" → @@ -1,2 +1,3 @@\n+x\n a\n b\n
        assert_eq!(
            run(b"a\nb\n", b"x\na\nb\n"),
            &b"@@ -1,2 +1,3 @@\n+x\n a\n b\n"[..]
        );
    }

    #[test]
    fn deletion_at_end() {
        // Spec: b"a\nb\nc\n" vs b"a\nb\n" → @@ -1,3 +1,2 @@\n a\n b\n-c\n
        assert_eq!(
            run(b"a\nb\nc\n", b"a\nb\n"),
            &b"@@ -1,3 +1,2 @@\n a\n b\n-c\n"[..]
        );
    }

    #[test]
    fn pure_addition_into_empty() {
        // git emits @@ -0,0 +1 @@ for pure addition into empty file.
        assert_eq!(run(b"", b"foo\n"), &b"@@ -0,0 +1 @@\n+foo\n"[..]);
    }

    #[test]
    fn pure_deletion_to_empty() {
        // git emits @@ -1 +0,0 @@ for pure deletion to empty file.
        assert_eq!(run(b"foo\n", b""), &b"@@ -1 +0,0 @@\n-foo\n"[..]);
    }

    // ---------- no-newline annotation ----------

    #[test]
    fn no_newline_annotation_on_left() {
        // Spec: b"foo" vs b"foo\n" should produce a diff with the
        // annotation on the `-` line (since that side lacks the newline).
        let out = run(b"foo", b"foo\n");
        let expected: &[u8] = b"@@ -1 +1 @@\n-foo\n\\ No newline at end of file\n+foo\n";
        assert_eq!(out, expected);
    }

    #[test]
    fn no_newline_annotation_on_right() {
        let out = run(b"foo\n", b"foo");
        let expected: &[u8] = b"@@ -1 +1 @@\n-foo\n+foo\n\\ No newline at end of file\n";
        assert_eq!(out, expected);
    }

    #[test]
    fn no_newline_annotation_both() {
        let out = run(b"foo", b"bar");
        let expected: &[u8] =
            b"@@ -1 +1 @@\n-foo\n\\ No newline at end of file\n+bar\n\\ No newline at end of file\n";
        assert_eq!(out, expected);
    }

    #[test]
    fn no_newlines_at_all_input() {
        // Single non-terminated line on both sides, both differ.
        let out = run(b"abc", b"def");
        let expected: &[u8] =
            b"@@ -1 +1 @@\n-abc\n\\ No newline at end of file\n+def\n\\ No newline at end of file\n";
        assert_eq!(out, expected);
    }

    // ---------- multiple hunks ----------

    #[test]
    fn multiple_hunks() {
        let a: &[u8] = b"l1\nl2\nl3\nl4\nl5\nl6\nl7\nl8\nl9\nl10\n";
        let b: &[u8] = b"X\nl2\nl3\nl4\nl5\nl6\nl7\nl8\nl9\nY\n";
        let out = run(a, b);
        // Expected, from git diff (default 3 lines context):
        //   @@ -1,4 +1,4 @@
        //   -l1
        //   +X
        //    l2
        //    l3
        //    l4
        //   @@ -7,4 +7,4 @@ l6
        //    l7
        //    l8
        //    l9
        //   -l10
        //   +Y
        let expected: &[u8] = b"\
@@ -1,4 +1,4 @@
-l1
+X
 l2
 l3
 l4
@@ -7,4 +7,4 @@ l6
 l7
 l8
 l9
-l10
+Y
";
        assert_eq!(out, expected);
    }

    #[test]
    fn long_file_single_change_one_hunk() {
        // 1000-line file with one line in the middle changed should produce a
        // single hunk: 3 context + 1 del + 1 ins + 3 context = 8 body lines
        // plus the 1 header line = 9 lines.
        let mut a = Vec::new();
        let mut b = Vec::new();
        for i in 0..1000 {
            let line = format!("line{:04}\n", i);
            a.extend_from_slice(line.as_bytes());
            if i == 500 {
                b.extend_from_slice(b"different\n");
            } else {
                b.extend_from_slice(line.as_bytes());
            }
        }
        let out = run(&a, &b);
        // Should be exactly one @@ block.
        let n_hunks = out
            .split(|&c| c == b'\n')
            .filter(|l| l.starts_with(b"@@"))
            .count();
        assert_eq!(n_hunks, 1, "expected exactly 1 hunk, got {n_hunks}");

        let body_lines: Vec<&[u8]> = out
            .split(|&c| c == b'\n')
            .filter(|s| !s.is_empty())
            .collect();
        assert_eq!(body_lines.len(), 9);
        assert_eq!(body_lines[0], b"@@ -498,7 +498,7 @@ line0496");
        assert_eq!(body_lines[1], b" line0497");
        assert_eq!(body_lines[2], b" line0498");
        assert_eq!(body_lines[3], b" line0499");
        assert_eq!(body_lines[4], b"-line0500");
        assert_eq!(body_lines[5], b"+different");
        assert_eq!(body_lines[6], b" line0501");
        assert_eq!(body_lines[7], b" line0502");
        assert_eq!(body_lines[8], b" line0503");
    }

    // ---------- function context ----------

    #[test]
    fn func_context_line_emitted() {
        // Two hunks far apart in a file where every line starts with a letter:
        // the second hunk's header should include the prior func line.
        let a: &[u8] =
            b"aaa\nbbb\nccc\nddd\neee\nfff\nggg\nhhh\niii\njjj\nkkk\nlll\nmmm\nnnn\nooo\nppp\n";
        let b: &[u8] =
            b"aaa\nbbb\nccc\nDDD\neee\nfff\nggg\nhhh\niii\njjj\nkkk\nlll\nmmm\nNNN\nooo\nppp\n";
        let out = run(a, b);
        let header_lines: Vec<&[u8]> = out
            .split(|&c| c == b'\n')
            .filter(|l| l.starts_with(b"@@"))
            .collect();
        assert_eq!(header_lines.len(), 2);
        assert_eq!(header_lines[0], b"@@ -1,7 +1,7 @@");
        assert_eq!(header_lines[1], b"@@ -11,6 +11,6 @@ jjj");
    }

    #[test]
    fn func_context_skipped_for_non_id_lines() {
        // Lines starting with whitespace shouldn't qualify as func lines.
        let a: &[u8] =
            b"   aaa\n   bbb\n   ccc\n   ddd\n   eee\n   fff\n   ggg\n   hhh\n   iii\n   jjj\n   kkk\n   lll\n   mmm\n   nnn\n   ooo\n   ppp\n";
        let b: &[u8] =
            b"   aaa\n   bbb\n   ccc\n   DDD\n   eee\n   fff\n   ggg\n   hhh\n   iii\n   jjj\n   kkk\n   lll\n   mmm\n   NNN\n   ooo\n   ppp\n";
        let out = run(a, b);
        let header_lines: Vec<&[u8]> = out
            .split(|&c| c == b'\n')
            .filter(|l| l.starts_with(b"@@"))
            .collect();
        assert_eq!(header_lines.len(), 2);
        assert_eq!(header_lines[0], b"@@ -1,7 +1,7 @@");
        assert_eq!(header_lines[1], b"@@ -11,6 +11,6 @@");
    }

    // ---------- def_ff helper ----------

    #[test]
    fn def_ff_matches_identifier_starts() {
        assert_eq!(def_ff(b"hello\n"), Some(&b"hello"[..]));
        assert_eq!(def_ff(b"_foo\n"), Some(&b"_foo"[..]));
        assert_eq!(def_ff(b"$bar\n"), Some(&b"$bar"[..]));
        assert_eq!(def_ff(b"   indent\n"), None);
        assert_eq!(def_ff(b"123digit\n"), None);
        assert_eq!(def_ff(b""), None);
        assert_eq!(def_ff(b"\n"), None);
        // Trailing whitespace stripped.
        assert_eq!(def_ff(b"foo \t\n"), Some(&b"foo"[..]));
    }

    // ---------- context size ----------

    #[test]
    fn context_zero_just_changes() {
        let a: &[u8] = b"a\nb\nc\nd\ne\n";
        let b: &[u8] = b"a\nb\nX\nd\ne\n";
        let out = run_ctx(a, b, 0);
        // Note: with context=0 the func-context heuristic still walks back
        // and finds `b` (line 2) as the function header.
        let expected: &[u8] = b"@@ -3 +3 @@ b\n-c\n+X\n";
        assert_eq!(out, expected);
    }

    #[test]
    fn context_one() {
        let a: &[u8] = b"a\nb\nc\nd\ne\n";
        let b: &[u8] = b"a\nb\nX\nd\ne\n";
        let out = run_ctx(a, b, 1);
        let expected: &[u8] = b"@@ -2,3 +2,3 @@ a\n b\n-c\n+X\n d\n";
        assert_eq!(out, expected);
    }

    // ---------- round-trip vs system git ----------

    /// Strip git's leading framing (`diff --git`, `index`, `---`, `+++` lines)
    /// from a raw `git diff --no-index` output, leaving just the hunks.
    /// Used by the round-trip test to compare apples to apples.
    fn strip_git_headers(out: &[u8]) -> Vec<u8> {
        let mut result = Vec::new();
        let mut after_headers = false;
        for line in out.split_inclusive(|&c| c == b'\n') {
            if after_headers {
                result.extend_from_slice(line);
                continue;
            }
            if line.starts_with(b"diff --git ")
                || line.starts_with(b"index ")
                || line.starts_with(b"--- ")
                || line.starts_with(b"+++ ")
                || line.starts_with(b"new file mode ")
                || line.starts_with(b"deleted file mode ")
                || line.starts_with(b"old mode ")
                || line.starts_with(b"new mode ")
                || line.starts_with(b"similarity index ")
                || line.starts_with(b"rename from ")
                || line.starts_with(b"rename to ")
                || line.starts_with(b"Binary files ")
            {
                continue;
            }
            // First non-header line — usually `@@`. Switch into pass-through mode.
            after_headers = true;
            result.extend_from_slice(line);
        }
        result
    }

    /// Run system git on two byte buffers and return its diff output, or
    /// `None` if git isn't available (so the test can skip cleanly).
    fn run_system_git(a: &[u8], b: &[u8]) -> Option<Vec<u8>> {
        use std::process::Command;

        // Probe `git --version` first.
        let probe = Command::new("git").arg("--version").output().ok()?;
        if !probe.status.success() {
            return None;
        }

        let dir = tempfile::tempdir().ok()?;
        let pa = dir.path().join("a");
        let pb = dir.path().join("b");
        std::fs::write(&pa, a).ok()?;
        std::fs::write(&pb, b).ok()?;
        let output = Command::new("git")
            .arg("diff")
            .arg("--no-index")
            .arg("--no-color")
            .arg("--")
            .arg(&pa)
            .arg(&pb)
            .output()
            .ok()?;
        // git diff --no-index returns 1 when files differ; that's expected.
        let _ = output.status;
        Some(output.stdout)
    }

    /// Compare our output with system git's body bytes for a list of
    /// (a, b, label) cases. Skips cleanly if git is unavailable.
    fn assert_matches_git(cases: &[(&[u8], &[u8], &str)]) {
        if run_system_git(b"", b"").is_none() {
            eprintln!("git not available; skipping round-trip test");
            return;
        }
        for (a, b, label) in cases {
            let mut ours = Vec::new();
            unified_diff(a, b, &UnifiedDiffOpts::default(), &mut ours).unwrap();
            let theirs_full = match run_system_git(a, b) {
                Some(o) => o,
                None => return,
            };
            let theirs = strip_git_headers(&theirs_full);
            assert_eq!(
                ours,
                theirs,
                "mismatch for case `{label}`\nours:\n{}\ntheirs:\n{}",
                String::from_utf8_lossy(&ours),
                String::from_utf8_lossy(&theirs)
            );
        }
    }

    #[test]
    fn round_trip_against_system_git() {
        let big_a: Vec<u8> = (0..50)
            .map(|i| format!("line{i:03}\n"))
            .collect::<String>()
            .into_bytes();
        let big_b: Vec<u8> = (0..50)
            .map(|i| {
                if i == 5 {
                    "X\n".to_string()
                } else if i == 30 {
                    "Y\n".to_string()
                } else {
                    format!("line{i:03}\n")
                }
            })
            .collect::<String>()
            .into_bytes();

        let cases: &[(&[u8], &[u8], &str)] = &[
            (b"a\n", b"b\n", "single-line modification"),
            (b"a\nb\n", b"x\na\nb\n", "insertion at start"),
            (b"a\nb\nc\n", b"a\nb\n", "deletion at end"),
            (b"foo", b"foo\n", "no-newline left only"),
            (b"foo\n", b"foo", "no-newline right only"),
            (b"foo", b"bar", "no-newline both, modified"),
            (b"abc", b"def", "single-line no-newline both"),
            (b"", b"foo\n", "addition into empty"),
            (b"foo\n", b"", "deletion to empty"),
            (b"", b"\n", "empty vs newline"),
            (
                b"l1\nl2\nl3\nl4\nl5\nl6\nl7\nl8\nl9\nl10\n",
                b"X\nl2\nl3\nl4\nl5\nl6\nl7\nl8\nl9\nY\n",
                "two distant hunks",
            ),
            (
                b"aaa\nbbb\nccc\nddd\neee\nfff\nggg\nhhh\niii\njjj\nkkk\nlll\nmmm\nnnn\nooo\nppp\n",
                b"aaa\nbbb\nccc\nDDD\neee\nfff\nggg\nhhh\niii\njjj\nkkk\nlll\nmmm\nNNN\nooo\nppp\n",
                "two hunks with func context",
            ),
            (&big_a, &big_b, "50-line file two distant changes"),
        ];
        assert_matches_git(cases);
    }

    #[test]
    fn round_trip_complex_replacement() {
        // Replace a block of lines in the middle.
        let a: &[u8] = b"\
header
1
2
3
4
5
6
7
8
trailer
";
        let b: &[u8] = b"\
header
A
B
C
trailer
";
        let cases: &[(&[u8], &[u8], &str)] = &[(a, b, "block replacement")];
        assert_matches_git(cases);
    }
}
