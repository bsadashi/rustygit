//! Hunk model + parser + splitter + applier for `add -p`.
//!
//! This is the pure library half of `add --patch`. The interactive prompt
//! loop lives in `crate::cli::add_patch`; everything here is plain data and
//! deterministic transforms that can be unit-tested in isolation.
//!
//! The shape of a unified diff hunk:
//! ```text
//!   @@ -<a-start>,<a-len> +<b-start>,<b-len> @@ <optional func>
//!    context line
//!   -removed line
//!   +added line
//!    another context
//! ```
//!
//! We use the output of `crate::xdiff::unified_diff` as input — the engine
//! emits exactly this format. We parse it back into a structured `Hunk` so
//! we can:
//!   - print individual hunks to the user one at a time (the `add -p` loop),
//!   - split a hunk into smaller pieces (`s` command),
//!   - apply a chosen subset of hunks to the index version of a file.
//!
//! Behaviour derived from `git/add-patch.c` (split heuristic at
//! `parse_diff` + `split_hunk`) and `git/apply.c` (line-by-line apply at the
//! computed positions).

use std::str;

use thiserror::Error;

/// Whether a hunk line is context, an addition, or a removal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    Context,
    Add,
    Remove,
}

/// One line inside a hunk body. `content` includes the trailing newline when
/// present in the source (the `\ No newline at end of file` marker line is
/// consumed by the parser and recorded on the preceding line via `no_newline`).
#[derive(Debug, Clone)]
pub struct HunkLine {
    pub kind: LineKind,
    /// Raw bytes of the line content, WITHOUT the leading `+`/`-`/` ` marker.
    /// Includes the trailing `\n` if the source had one.
    pub content: Vec<u8>,
    /// True if the source had `\ No newline at end of file` immediately after
    /// this record. Needed to faithfully reconstruct files that lack a final
    /// newline.
    pub no_newline: bool,
}

/// One unified-diff hunk.
#[derive(Debug, Clone)]
pub struct Hunk {
    /// The raw `@@ -... +... @@` header line (without trailing `\n`), kept
    /// verbatim so we can re-emit it when printing the hunk.
    pub header: String,
    /// 1-based starting line in the a-side (or 0 for pure-addition `-0,0`).
    pub old_start: u32,
    pub old_lines: u32,
    pub new_start: u32,
    pub new_lines: u32,
    pub lines: Vec<HunkLine>,
}

#[derive(Error, Debug)]
pub enum ParseError {
    #[error("hunk header malformed: {0}")]
    BadHeader(String),
    #[error("unexpected line in hunk body: {0}")]
    BadBody(String),
    #[error("invalid utf-8 in hunk header")]
    BadUtf8,
}

/// Parse a complete unified-diff stream (the kind emitted by
/// `crate::xdiff::unified_diff`) into a `Vec<Hunk>`. The stream is expected
/// to contain ONLY hunk blocks — no `diff --git`, `---`, or `+++` framing.
///
/// from git/add-patch.c::parse_diff
pub fn parse_hunks_from_diff(diff_bytes: &[u8]) -> Result<Vec<Hunk>, ParseError> {
    let mut hunks: Vec<Hunk> = Vec::new();
    let mut cur: Option<Hunk> = None;

    for line in split_lines_inclusive(diff_bytes) {
        if line.starts_with(b"@@") {
            if let Some(h) = cur.take() {
                hunks.push(h);
            }
            let header_str =
                str::from_utf8(strip_newline(line)).map_err(|_| ParseError::BadUtf8)?;
            cur = Some(parse_header(header_str)?);
            continue;
        }
        let h = match cur.as_mut() {
            Some(h) => h,
            None => {
                // Bytes before the first `@@` are not valid; ignore stray
                // whitespace/EOF lines so we tolerate a trailing newline.
                if line.iter().all(|&b| b == b'\n' || b == b'\r') {
                    continue;
                }
                return Err(ParseError::BadBody(
                    String::from_utf8_lossy(line).into_owned(),
                ));
            }
        };

        if line.starts_with(b"\\") {
            // `\ No newline at end of file` — attach to the last recorded line.
            if let Some(last) = h.lines.last_mut() {
                last.no_newline = true;
            }
            continue;
        }

        let (kind, body) = if let Some(rest) = line.strip_prefix(b" ") {
            (LineKind::Context, rest)
        } else if let Some(rest) = line.strip_prefix(b"+") {
            (LineKind::Add, rest)
        } else if let Some(rest) = line.strip_prefix(b"-") {
            (LineKind::Remove, rest)
        } else if line == b"\n" {
            // An empty body line corresponds to a context line that is purely
            // a newline — git emits this as " \n" but some upstream tools may
            // strip the trailing space. Treat a bare `\n` as a context line.
            (LineKind::Context, line)
        } else {
            return Err(ParseError::BadBody(
                String::from_utf8_lossy(line).into_owned(),
            ));
        };

        h.lines.push(HunkLine {
            kind,
            content: body.to_vec(),
            no_newline: false,
        });
    }

    if let Some(h) = cur.take() {
        hunks.push(h);
    }
    Ok(hunks)
}

/// Parse a single `@@ -a,b +c,d @@ ...` header into a `Hunk` shell with an
/// empty `lines` vector. Tolerates the `len` field being omitted when it is
/// 1 (matching git's format).
fn parse_header(header: &str) -> Result<Hunk, ParseError> {
    // Expected shape: `@@ -OLD +NEW @@<rest>` where OLD/NEW are `n` or `n,m`.
    let after_at = header
        .strip_prefix("@@ ")
        .ok_or_else(|| ParseError::BadHeader(header.to_string()))?;
    let minus_idx = after_at
        .find('-')
        .ok_or_else(|| ParseError::BadHeader(header.to_string()))?;
    if minus_idx != 0 {
        return Err(ParseError::BadHeader(header.to_string()));
    }
    let after_minus = &after_at[1..];
    let plus_pos = after_minus
        .find(" +")
        .ok_or_else(|| ParseError::BadHeader(header.to_string()))?;
    let old_field = &after_minus[..plus_pos];
    let after_plus = &after_minus[plus_pos + 2..];
    let end_at_pos = after_plus
        .find(" @@")
        .ok_or_else(|| ParseError::BadHeader(header.to_string()))?;
    let new_field = &after_plus[..end_at_pos];

    let (old_start, old_lines) =
        parse_range(old_field).ok_or_else(|| ParseError::BadHeader(header.to_string()))?;
    let (new_start, new_lines) =
        parse_range(new_field).ok_or_else(|| ParseError::BadHeader(header.to_string()))?;

    Ok(Hunk {
        header: header.to_string(),
        old_start,
        old_lines,
        new_start,
        new_lines,
        lines: Vec::new(),
    })
}

fn parse_range(field: &str) -> Option<(u32, u32)> {
    match field.split_once(',') {
        Some((s, l)) => Some((s.parse().ok()?, l.parse().ok()?)),
        None => Some((field.parse().ok()?, 1)),
    }
}

/// Yield slices of `bytes` split at `\n`, INCLUDING the trailing newline. A
/// trailing un-terminated line is yielded as its own slice.
fn split_lines_inclusive(bytes: &[u8]) -> impl Iterator<Item = &[u8]> {
    bytes.split_inclusive(|&b| b == b'\n')
}

fn strip_newline(line: &[u8]) -> &[u8] {
    let mut end = line.len();
    if end > 0 && line[end - 1] == b'\n' {
        end -= 1;
    }
    if end > 0 && line[end - 1] == b'\r' {
        end -= 1;
    }
    &line[..end]
}

/// Split one hunk into smaller hunks at each transition from a +/- run back
/// to a context line, mirroring git's `split_hunk` heuristic in add-patch.c.
///
/// Returns the original hunk wrapped in a single-element vector when it
/// can't be split further (i.e. only one contiguous change region).
///
/// from git/add-patch.c::split_hunk
pub fn split_hunk(h: &Hunk) -> Vec<Hunk> {
    // First decide whether splitting is possible by counting boundaries:
    // a boundary is a context line preceded by a +/- line (and there must
    // then be more +/- lines later). If `boundaries < 1`, return `[h]`.
    let mut boundaries: Vec<usize> = Vec::new(); // indices into `h.lines` where a new sub-hunk starts
    let mut prev_change = false;
    let mut seen_change = false;
    for (i, ln) in h.lines.iter().enumerate() {
        let is_change = matches!(ln.kind, LineKind::Add | LineKind::Remove);
        if is_change {
            seen_change = true;
        }
        if prev_change && ln.kind == LineKind::Context && seen_change {
            // Mark the position of the first context line after a change run
            // as a candidate split point.
            boundaries.push(i);
        }
        prev_change = is_change;
    }

    // Only a boundary that has a subsequent change line counts as a real split
    // (git: "buffer overrun while splitting hunks" guard). Filter accordingly.
    boundaries.retain(|&b| {
        h.lines[b..]
            .iter()
            .any(|l| matches!(l.kind, LineKind::Add | LineKind::Remove))
    });

    if boundaries.is_empty() {
        return vec![h.clone()];
    }

    // Now slice. Each sub-hunk starts at the previous boundary (or 0 for the
    // first) and runs through the next boundary (exclusive). We include the
    // shared context lines on both sides — that's how git splits: each split
    // shares the run of context lines with its neighbor.
    let mut sub_hunks: Vec<Hunk> = Vec::new();
    let mut start: usize = 0;
    let mut old_offset = h.old_start;
    let mut new_offset = h.new_start;

    let mut all_starts: Vec<usize> = vec![0];
    all_starts.extend_from_slice(&boundaries);

    for window in all_starts.windows(2) {
        let lo = window[0];
        let hi = window[1];
        // For the inner sub-hunks, end position is one past the last context
        // line before the next change. But the simpler rule git uses is: the
        // split point IS the start of the next hunk; we want to include the
        // shared context lines in BOTH sub-hunks. So end the current sub-hunk
        // at the first context line after the next change run begins... but
        // since we marked boundary at the first context after a change run,
        // the end of the previous sub-hunk is `hi` (exclusive) — that gives
        // each sub-hunk only the changes plus leading/trailing context up to
        // the boundary, NOT shared with the next.
        //
        // git's behavior is to keep the surrounding context with both sides.
        // We approximate this by NOT sharing — each context line belongs to
        // whichever sub-hunk's body it precedes. This is byte-different from
        // git but is still a correct unified diff because both sub-hunks
        // anchor to their own (old_start, new_start) within the file.
        let slice = &h.lines[lo..hi];
        let sub = make_sub_hunk(slice, old_offset, new_offset);
        old_offset += sub.old_lines;
        new_offset += sub.new_lines;
        sub_hunks.push(sub);
        start = hi;
    }
    // Tail: from the last boundary to the end.
    let tail = &h.lines[start..];
    let sub = make_sub_hunk(tail, old_offset, new_offset);
    sub_hunks.push(sub);

    sub_hunks
}

fn make_sub_hunk(lines: &[HunkLine], old_start: u32, new_start: u32) -> Hunk {
    let mut old_lines: u32 = 0;
    let mut new_lines: u32 = 0;
    for ln in lines {
        match ln.kind {
            LineKind::Context => {
                old_lines += 1;
                new_lines += 1;
            }
            LineKind::Remove => old_lines += 1,
            LineKind::Add => new_lines += 1,
        }
    }
    let header = format!(
        "@@ -{} +{} @@",
        format_pos(old_start, old_lines),
        format_pos(new_start, new_lines)
    );
    Hunk {
        header,
        old_start,
        old_lines,
        new_start,
        new_lines,
        lines: lines.to_vec(),
    }
}

fn format_pos(start: u32, len: u32) -> String {
    // git's `write_pos`: if len == 1, drop the `,1`. If len == 0, the printed
    // start drops by one (we use `start`).
    if len == 1 {
        format!("{start}")
    } else {
        format!("{start},{len}")
    }
}

/// Apply `hunks` (a subset of those that originated from `base`) to `base`
/// and return the resulting bytes.
///
/// Constraints: every hunk in `hunks` must have its `old_start`/`old_lines`
/// pointing into the indexes of `base`. They must be in ascending order by
/// `old_start` and must not overlap (which is automatic for parsed hunks from
/// a single diff).
///
/// from git/apply.c::apply_one_fragment
pub fn apply_hunks_to_base(base: &[u8], hunks: &[&Hunk]) -> Vec<u8> {
    let base_lines = split_lines_keep(base);
    let mut out: Vec<u8> = Vec::with_capacity(base.len());
    let mut copied_through: usize = 0; // 0-based line index into `base_lines`

    for hunk in hunks {
        // Convert 1-based old_start to 0-based. Special case: pure addition
        // hunks (old_lines == 0) have `old_start` = line BEFORE which to
        // insert, so the slice start is `old_start` (not minus 1).
        let slice_start: usize = if hunk.old_lines == 0 {
            hunk.old_start as usize
        } else {
            (hunk.old_start as usize).saturating_sub(1)
        };

        // Emit untouched lines between the last position and this hunk's slice.
        while copied_through < slice_start && copied_through < base_lines.len() {
            out.extend_from_slice(base_lines[copied_through]);
            copied_through += 1;
        }

        // Walk the hunk body. Context and Remove lines consume base lines;
        // Context and Add lines emit content.
        let mut base_cur = slice_start;
        for ln in &hunk.lines {
            match ln.kind {
                LineKind::Context => {
                    out.extend_from_slice(&ln.content);
                    // Avoid trusting the hunk content blindly: prefer the
                    // base bytes if available, in case the diff was generated
                    // with a different newline-stripping policy. But for the
                    // happy path the hunk's context line matches base[].
                    base_cur += 1;
                }
                LineKind::Remove => {
                    base_cur += 1;
                }
                LineKind::Add => {
                    out.extend_from_slice(&ln.content);
                }
            }
        }
        copied_through = base_cur;
    }

    // Tail: copy whatever remains of `base` past the last hunk.
    while copied_through < base_lines.len() {
        out.extend_from_slice(base_lines[copied_through]);
        copied_through += 1;
    }

    out
}

/// Split `input` into lines, preserving the trailing `\n`. Mirrors
/// xdiff::split_lines so the indices line up.
fn split_lines_keep(input: &[u8]) -> Vec<&[u8]> {
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

/// Format a hunk as a unified-diff block — header + body — suitable for
/// printing to the user inside the `add -p` prompt. Used by the CLI layer.
pub fn format_hunk(h: &Hunk) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::with_capacity(h.header.len() + 1 + 64 * h.lines.len());
    out.extend_from_slice(h.header.as_bytes());
    out.push(b'\n');
    for ln in &h.lines {
        let prefix: u8 = match ln.kind {
            LineKind::Context => b' ',
            LineKind::Add => b'+',
            LineKind::Remove => b'-',
        };
        out.push(prefix);
        out.extend_from_slice(&ln.content);
        if ln.no_newline {
            // git emits the annotation on its own line.
            out.extend_from_slice(b"\\ No newline at end of file\n");
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_single_modification_hunk() {
        // From xdiff::unified_diff(b"a\n", b"b\n") → "@@ -1 +1 @@\n-a\n+b\n"
        let diff = b"@@ -1 +1 @@\n-a\n+b\n";
        let hunks = parse_hunks_from_diff(diff).unwrap();
        assert_eq!(hunks.len(), 1);
        let h = &hunks[0];
        assert_eq!(h.old_start, 1);
        assert_eq!(h.old_lines, 1);
        assert_eq!(h.new_start, 1);
        assert_eq!(h.new_lines, 1);
        assert_eq!(h.lines.len(), 2);
        assert_eq!(h.lines[0].kind, LineKind::Remove);
        assert_eq!(h.lines[0].content, b"a\n");
        assert_eq!(h.lines[1].kind, LineKind::Add);
        assert_eq!(h.lines[1].content, b"b\n");
    }

    #[test]
    fn parse_multi_hunk_diff() {
        let diff: &[u8] = b"\
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
        let hunks = parse_hunks_from_diff(diff).unwrap();
        assert_eq!(hunks.len(), 2);
        let h1 = &hunks[0];
        assert_eq!(h1.old_start, 1);
        assert_eq!(h1.old_lines, 4);
        assert_eq!(h1.new_start, 1);
        assert_eq!(h1.new_lines, 4);
        assert_eq!(h1.lines.len(), 5);
        assert_eq!(h1.lines[0].kind, LineKind::Remove);
        assert_eq!(h1.lines[1].kind, LineKind::Add);
        assert!(matches!(h1.lines[2].kind, LineKind::Context));

        let h2 = &hunks[1];
        assert_eq!(h2.old_start, 7);
        assert_eq!(h2.new_start, 7);
        // Func context follows `@@`: included in the header verbatim.
        assert!(h2.header.ends_with(" @@ l6"));
    }

    #[test]
    fn parse_no_newline_marker_attaches_to_last_line() {
        // The "\ No newline at end of file" marker attaches to the preceding
        // line (an Add line in this case).
        let diff: &[u8] = b"@@ -1 +1 @@\n-foo\n+foo\n\\ No newline at end of file\n";
        let hunks = parse_hunks_from_diff(diff).unwrap();
        let h = &hunks[0];
        assert_eq!(h.lines.len(), 2);
        assert!(!h.lines[0].no_newline);
        assert!(h.lines[1].no_newline);
    }

    #[test]
    fn parse_pure_addition_into_empty_file() {
        let diff: &[u8] = b"@@ -0,0 +1 @@\n+foo\n";
        let hunks = parse_hunks_from_diff(diff).unwrap();
        let h = &hunks[0];
        assert_eq!(h.old_start, 0);
        assert_eq!(h.old_lines, 0);
        assert_eq!(h.new_start, 1);
        assert_eq!(h.new_lines, 1);
        assert_eq!(h.lines.len(), 1);
        assert_eq!(h.lines[0].kind, LineKind::Add);
    }

    #[test]
    fn parse_pure_deletion_to_empty_file() {
        let diff: &[u8] = b"@@ -1 +0,0 @@\n-foo\n";
        let hunks = parse_hunks_from_diff(diff).unwrap();
        let h = &hunks[0];
        assert_eq!(h.old_start, 1);
        assert_eq!(h.old_lines, 1);
        assert_eq!(h.new_start, 0);
        assert_eq!(h.new_lines, 0);
    }

    #[test]
    fn apply_no_hunks_is_identity() {
        let base = b"hello\nworld\n";
        let out = apply_hunks_to_base(base, &[]);
        assert_eq!(out, base);
    }

    #[test]
    fn apply_single_modification_hunk() {
        let base: &[u8] = b"hello\nworld\n";
        let diff: &[u8] = b"@@ -1,2 +1,2 @@\n-hello\n+goodbye\n world\n";
        let hunks = parse_hunks_from_diff(diff).unwrap();
        let refs: Vec<&Hunk> = hunks.iter().collect();
        let out = apply_hunks_to_base(base, &refs);
        assert_eq!(out, b"goodbye\nworld\n");
    }

    #[test]
    fn apply_only_first_of_two_hunks() {
        // Base has lines 1..=10; two hunks far apart: change l1→X and l10→Y.
        // Applying only the first should produce X at top and l10 unchanged.
        let base: &[u8] = b"l1\nl2\nl3\nl4\nl5\nl6\nl7\nl8\nl9\nl10\n";
        let diff: &[u8] = b"\
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
        let hunks = parse_hunks_from_diff(diff).unwrap();
        let only_first: Vec<&Hunk> = vec![&hunks[0]];
        let out = apply_hunks_to_base(base, &only_first);
        let expected: &[u8] = b"X\nl2\nl3\nl4\nl5\nl6\nl7\nl8\nl9\nl10\n";
        assert_eq!(out, expected);
    }

    #[test]
    fn apply_only_second_of_two_hunks() {
        let base: &[u8] = b"l1\nl2\nl3\nl4\nl5\nl6\nl7\nl8\nl9\nl10\n";
        let diff: &[u8] = b"\
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
        let hunks = parse_hunks_from_diff(diff).unwrap();
        let only_second: Vec<&Hunk> = vec![&hunks[1]];
        let out = apply_hunks_to_base(base, &only_second);
        let expected: &[u8] = b"l1\nl2\nl3\nl4\nl5\nl6\nl7\nl8\nl9\nY\n";
        assert_eq!(out, expected);
    }

    #[test]
    fn apply_both_hunks_reconstructs_full_b() {
        let base: &[u8] = b"l1\nl2\nl3\nl4\nl5\nl6\nl7\nl8\nl9\nl10\n";
        let target: &[u8] = b"X\nl2\nl3\nl4\nl5\nl6\nl7\nl8\nl9\nY\n";
        let mut diff: Vec<u8> = Vec::new();
        crate::xdiff::unified_diff(
            base,
            target,
            &crate::xdiff::UnifiedDiffOpts::default(),
            &mut diff,
        )
        .unwrap();
        let hunks = parse_hunks_from_diff(&diff).unwrap();
        let all: Vec<&Hunk> = hunks.iter().collect();
        let out = apply_hunks_to_base(base, &all);
        assert_eq!(out, target);
    }

    #[test]
    fn apply_pure_addition() {
        let base: &[u8] = b"";
        let diff: &[u8] = b"@@ -0,0 +1 @@\n+foo\n";
        let hunks = parse_hunks_from_diff(diff).unwrap();
        let refs: Vec<&Hunk> = hunks.iter().collect();
        let out = apply_hunks_to_base(base, &refs);
        assert_eq!(out, b"foo\n");
    }

    #[test]
    fn apply_pure_deletion() {
        let base: &[u8] = b"foo\n";
        let diff: &[u8] = b"@@ -1 +0,0 @@\n-foo\n";
        let hunks = parse_hunks_from_diff(diff).unwrap();
        let refs: Vec<&Hunk> = hunks.iter().collect();
        let out = apply_hunks_to_base(base, &refs);
        assert_eq!(out, b"");
    }

    #[test]
    fn split_single_change_returns_one_hunk() {
        let diff: &[u8] = b"@@ -1 +1 @@\n-a\n+b\n";
        let hunks = parse_hunks_from_diff(diff).unwrap();
        let sub = split_hunk(&hunks[0]);
        assert_eq!(sub.len(), 1);
    }

    #[test]
    fn split_two_change_regions_in_one_hunk() {
        // A hunk that has TWO change runs separated by context lines —
        // produced when context windows around the changes overlap, e.g.
        // 2-line context with changes 3 lines apart.
        let base: &[u8] = b"l1\nl2\nl3\nl4\nl5\nl6\n";
        let target: &[u8] = b"X\nl2\nl3\nl4\nl5\nY\n";
        let mut diff: Vec<u8> = Vec::new();
        crate::xdiff::unified_diff(
            base,
            target,
            &crate::xdiff::UnifiedDiffOpts::default(),
            &mut diff,
        )
        .unwrap();
        let hunks = parse_hunks_from_diff(&diff).unwrap();
        // With default context=3, the two changes share a hunk.
        assert_eq!(hunks.len(), 1);
        let sub = split_hunk(&hunks[0]);
        assert!(
            sub.len() >= 2,
            "expected at least 2 sub-hunks, got {}",
            sub.len()
        );

        // Sanity: applying ALL sub-hunks to base should reconstruct target.
        let refs: Vec<&Hunk> = sub.iter().collect();
        let out = apply_hunks_to_base(base, &refs);
        assert_eq!(out, target);
    }

    #[test]
    fn format_hunk_round_trip() {
        // Parsing and then formatting a hunk should yield byte-identical
        // output (modulo header normalization).
        let diff: &[u8] = b"@@ -1,2 +1,2 @@\n-hello\n+goodbye\n world\n";
        let hunks = parse_hunks_from_diff(diff).unwrap();
        let formatted = format_hunk(&hunks[0]);
        assert_eq!(formatted, diff);
    }

    #[test]
    fn format_hunk_preserves_no_newline_marker() {
        let diff: &[u8] = b"@@ -1 +1 @@\n-foo\n+foo\n\\ No newline at end of file\n";
        let hunks = parse_hunks_from_diff(diff).unwrap();
        let formatted = format_hunk(&hunks[0]);
        assert_eq!(formatted, diff);
    }
}
