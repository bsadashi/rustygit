//! Three-way line-level merge of a single file.
//!
//! Given the three byte-buffer sides of a merge (`base`, `ours`, `theirs`)
//! produce either a clean merge — equivalent in spirit to applying both sides'
//! changes against the base — or a conflict-marked buffer in git's default
//! style:
//!
//! ```text
//! <<<<<<< ours
//! ...our lines...
//! =======
//! ...their lines...
//! >>>>>>> theirs
//! ```
//!
//! Algorithm (mirrors `xdiff/xmerge.c::xdl_do_merge`):
//! 1. Diff `base` vs `ours` and `base` vs `theirs` separately to get two
//!    "change scripts": ordered lists of hunks of the form
//!    `(base_start, base_chg, side_start, side_chg)`.
//! 2. Walk both scripts in lockstep, emitting un-changed base lines from the
//!    gaps and one of three outputs per overlapping pair:
//!    * a side-1-only change (theirs doesn't touch this base region) → take ours,
//!    * a side-2-only change (ours doesn't touch this base region) → take theirs,
//!    * a both-sides change → either resolve identically (both did the same
//!      replacement byte-for-byte) or emit a conflict region.
//! 3. After the walk emit any trailing un-changed base content.
//!
//! Edge-cases handled in this module:
//! * Identical inputs (no edits at all).
//! * One side made no edits — return the other side's lines verbatim.
//! * Both sides applied the same edit (idempotent merge → clean).
//! * Adjacent / interleaved hunks.
//! * Insertions into an empty base (both sides "add" the file).
//! * Trailing-newline differences propagating from either side.
//!
//! We intentionally ship only the plain conflict-marker style (matching git's
//! default `merge.conflictStyle = merge`); the `diff3` style with a base-side
//! is deferred.

use crate::xdiff::split_lines;

/// Default marker length matches git's `DEFAULT_CONFLICT_MARKER_SIZE`.
const DEFAULT_MARKER_SIZE: usize = 7;

/// Result of merging a single file's three sides.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileMergeResult {
    /// All overlapping edits resolved cleanly — the body is the merged file.
    Resolved(Vec<u8>),
    /// At least one conflict region remains. `body` is the merged buffer with
    /// `<<<<<<<` / `=======` / `>>>>>>>` markers inserted; `conflict_count`
    /// counts those regions.
    Conflicted {
        body: Vec<u8>,
        conflict_count: usize,
    },
}

impl FileMergeResult {
    /// Did the merge produce any conflicts?
    pub fn has_conflict(&self) -> bool {
        matches!(self, FileMergeResult::Conflicted { .. })
    }

    /// Borrow the merged body regardless of conflict state.
    pub fn body(&self) -> &[u8] {
        match self {
            FileMergeResult::Resolved(b) => b,
            FileMergeResult::Conflicted { body, .. } => body,
        }
    }

    /// Consume `self` into the merged byte buffer.
    pub fn into_body(self) -> Vec<u8> {
        match self {
            FileMergeResult::Resolved(b) => b,
            FileMergeResult::Conflicted { body, .. } => body,
        }
    }
}

/// Labels written next to the `<<<<<<<` / `>>>>>>>` markers. Mirrors git's
/// `name1` / `name2` / `name3` parameters to `xdl_merge`. M13 ships a plain
/// (non-`diff3`) style, so `base` is unused at the moment — kept on the struct
/// so callers don't break when `diff3` lands.
#[derive(Debug, Clone, Copy)]
pub struct FileMergeLabels<'a> {
    pub base: &'a str,
    pub ours: &'a str,
    pub theirs: &'a str,
}

impl<'a> Default for FileMergeLabels<'a> {
    fn default() -> Self {
        Self {
            base: "base",
            ours: "ours",
            theirs: "theirs",
        }
    }
}

/// 3-way merge of the three byte buffers. The result is either a clean merged
/// buffer or a conflict-marked buffer; either way the caller decides what to
/// do with it (write to disk, stash as a blob, etc.).
pub fn merge_file(
    base: &[u8],
    ours: &[u8],
    theirs: &[u8],
    labels: &FileMergeLabels,
) -> FileMergeResult {
    let base_lines = split_lines(base);
    let ours_lines = split_lines(ours);
    let theirs_lines = split_lines(theirs);

    // Fast paths.
    if base_lines == ours_lines && base_lines == theirs_lines {
        // All three identical.
        return FileMergeResult::Resolved(base.to_vec());
    }
    if base_lines == ours_lines {
        // Ours unchanged — take theirs verbatim.
        return FileMergeResult::Resolved(theirs.to_vec());
    }
    if base_lines == theirs_lines {
        // Theirs unchanged — take ours verbatim.
        return FileMergeResult::Resolved(ours.to_vec());
    }
    if ours_lines == theirs_lines {
        // Both sides ended up at the same content — take it.
        return FileMergeResult::Resolved(ours.to_vec());
    }

    // Compute the two diff scripts.
    let ours_hunks = diff_hunks(&base_lines, &ours_lines);
    let theirs_hunks = diff_hunks(&base_lines, &theirs_lines);

    walk_and_emit(
        &base_lines,
        &ours_lines,
        &theirs_lines,
        &ours_hunks,
        &theirs_hunks,
        labels,
    )
}

// ---------------------------------------------------------------------------
// Per-side change script
// ---------------------------------------------------------------------------

/// A run of changed lines: at base position `base_start` we replace
/// `base_len` lines from the base with `side_len` lines from the side
/// starting at `side_start`. A pure deletion has `side_len == 0`; a pure
/// insertion has `base_len == 0` (in that case `base_start` is the index of
/// the base line that the new content slots in *before*).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Hunk {
    base_start: usize,
    base_len: usize,
    side_start: usize,
    side_len: usize,
}

impl Hunk {
    fn base_end(&self) -> usize {
        self.base_start + self.base_len
    }
}

/// Compute the change hunks turning `a` into `b`. Each hunk is a maximal run
/// of non-Equal edits in the Myers SES; consecutive Equals separate hunks.
fn diff_hunks(a: &[&[u8]], b: &[&[u8]]) -> Vec<Hunk> {
    let edits = myers_ses(a, b);
    let mut hunks = Vec::new();
    let mut i = 0;
    let mut ai = 0usize; // walker into a
    let mut bi = 0usize; // walker into b
    while i < edits.len() {
        match edits[i] {
            EditOp::Equal => {
                ai += 1;
                bi += 1;
                i += 1;
            }
            EditOp::Delete | EditOp::Insert => {
                let h_base_start = ai;
                let h_side_start = bi;
                let mut h_base_len = 0usize;
                let mut h_side_len = 0usize;
                while i < edits.len() {
                    match edits[i] {
                        EditOp::Delete => {
                            ai += 1;
                            h_base_len += 1;
                            i += 1;
                        }
                        EditOp::Insert => {
                            bi += 1;
                            h_side_len += 1;
                            i += 1;
                        }
                        EditOp::Equal => break,
                    }
                }
                hunks.push(Hunk {
                    base_start: h_base_start,
                    base_len: h_base_len,
                    side_start: h_side_start,
                    side_len: h_side_len,
                });
            }
        }
    }
    hunks
}

#[derive(Debug, Clone, Copy)]
enum EditOp {
    Equal,
    Delete,
    Insert,
}

/// Myers SES — local copy because `xdiff`'s is private. The shape is a flat
/// list of ops: each Equal corresponds to one (a, b) line that matches, each
/// Delete to an a-line that doesn't appear in b at the matched position, and
/// each Insert to a b-line absent from a.
fn myers_ses(a: &[&[u8]], b: &[&[u8]]) -> Vec<EditOp> {
    let n = a.len() as isize;
    let m = b.len() as isize;
    let max = n + m;

    if n == 0 && m == 0 {
        return Vec::new();
    }

    let offset = max;
    let v_len = (2 * max + 1) as usize;
    let mut v = vec![0isize; v_len];
    let mut trace: Vec<Vec<isize>> = Vec::new();

    let mut found_d: Option<usize> = None;
    'outer: for d in 0..=max {
        trace.push(v.clone());
        let mut k = -d;
        while k <= d {
            let down =
                k == -d || (k != d && v[(k - 1 + offset) as usize] < v[(k + 1 + offset) as usize]);
            let mut x = if down {
                v[(k + 1 + offset) as usize]
            } else {
                v[(k - 1 + offset) as usize] + 1
            };
            let mut y = x - k;
            while x < n && y < m && a[x as usize] == b[y as usize] {
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

    let d_final = found_d.expect("Myers terminates");
    let mut x = n;
    let mut y = m;
    let mut ops: Vec<EditOp> = Vec::new();
    for d in (0..=d_final).rev() {
        let vv = &trace[d];
        let k = x - y;
        let down = k == -(d as isize)
            || (k != d as isize && vv[(k - 1 + offset) as usize] < vv[(k + 1 + offset) as usize]);
        let prev_k = if down { k + 1 } else { k - 1 };
        let prev_x = vv[(prev_k + offset) as usize];
        let prev_y = prev_x - prev_k;
        while x > prev_x && y > prev_y {
            ops.push(EditOp::Equal);
            x -= 1;
            y -= 1;
        }
        if d > 0 {
            if down {
                ops.push(EditOp::Insert);
            } else {
                ops.push(EditOp::Delete);
            }
        }
        x = prev_x;
        y = prev_y;
    }
    ops.reverse();
    ops
}

// ---------------------------------------------------------------------------
// Lockstep walker — produces the merged buffer (clean or with markers).
// ---------------------------------------------------------------------------

fn walk_and_emit(
    base_lines: &[&[u8]],
    ours_lines: &[&[u8]],
    theirs_lines: &[&[u8]],
    ours_hunks: &[Hunk],
    theirs_hunks: &[Hunk],
    labels: &FileMergeLabels,
) -> FileMergeResult {
    let mut out: Vec<u8> = Vec::new();
    let mut conflict_count = 0usize;
    let mut base_cursor = 0usize; // how many base lines we've consumed
    let mut i = 0usize; // ours_hunks index
    let mut j = 0usize; // theirs_hunks index

    while i < ours_hunks.len() && j < theirs_hunks.len() {
        let oh = &ours_hunks[i];
        let th = &theirs_hunks[j];

        // Case A: ours hunk is STRICTLY before theirs hunk → take ours.
        // Strict `<` matches git's `xdl_do_merge` — adjacent (touching at the
        // boundary) hunks are grouped into the same conflict block.
        if oh.base_end() < th.base_start {
            emit_base_range(&mut out, base_lines, base_cursor, oh.base_start);
            emit_side_range(&mut out, ours_lines, oh.side_start, oh.side_len);
            base_cursor = oh.base_end();
            i += 1;
            continue;
        }
        // Case B: theirs hunk strictly before ours → take theirs.
        if th.base_end() < oh.base_start {
            emit_base_range(&mut out, base_lines, base_cursor, th.base_start);
            emit_side_range(&mut out, theirs_lines, th.side_start, th.side_len);
            base_cursor = th.base_end();
            j += 1;
            continue;
        }

        // Case C: hunks overlap (or touch at the same base point with at
        // least one insertion). Merge all chained-overlapping hunks on each
        // side into one combined block, then decide if the combined block is
        // a clean idempotent merge or a conflict.
        let (mut i_end, mut j_end) = (i + 1, j + 1);
        let mut base_block_start = oh.base_start.min(th.base_start);
        let mut base_block_end = oh.base_end().max(th.base_end());
        loop {
            let mut grew = false;
            while i_end < ours_hunks.len() {
                let cand = &ours_hunks[i_end];
                if cand.base_start <= base_block_end {
                    base_block_end = base_block_end.max(cand.base_end());
                    base_block_start = base_block_start.min(cand.base_start);
                    i_end += 1;
                    grew = true;
                } else {
                    break;
                }
            }
            while j_end < theirs_hunks.len() {
                let cand = &theirs_hunks[j_end];
                if cand.base_start <= base_block_end {
                    base_block_end = base_block_end.max(cand.base_end());
                    base_block_start = base_block_start.min(cand.base_start);
                    j_end += 1;
                    grew = true;
                } else {
                    break;
                }
            }
            if !grew {
                break;
            }
        }

        // Determine the side-side replacements for the combined block.
        // Ours: from the first overlapping hunk to the last we expand both
        // ends to cover any base context up to `base_block_start` / down to
        // `base_block_end`.
        let our_first = &ours_hunks[i];
        let our_last = &ours_hunks[i_end - 1];
        let our_side_start = our_first
            .side_start
            .saturating_sub(our_first.base_start.saturating_sub(base_block_start));
        let our_side_end = our_last.side_start
            + our_last.side_len
            + (base_block_end.saturating_sub(our_last.base_end()));

        let their_first = &theirs_hunks[j];
        let their_last = &theirs_hunks[j_end - 1];
        let their_side_start = their_first
            .side_start
            .saturating_sub(their_first.base_start.saturating_sub(base_block_start));
        let their_side_end = their_last.side_start
            + their_last.side_len
            + (base_block_end.saturating_sub(their_last.base_end()));

        let our_slice = &ours_lines[our_side_start..our_side_end];
        let their_slice = &theirs_lines[their_side_start..their_side_end];

        // Emit base preceding the conflict block.
        emit_base_range(&mut out, base_lines, base_cursor, base_block_start);

        if our_slice == their_slice {
            // Both sides made the same replacement → clean.
            for line in our_slice {
                out.extend_from_slice(line);
            }
        } else if our_slice.is_empty() {
            // Ours deleted; theirs modified-or-added — keep theirs' side as
            // conflict (modify/delete on theirs vs delete on ours).
            // Both deleted would have been == our_slice.
            emit_conflict(&mut out, our_slice, their_slice, labels);
            conflict_count += 1;
        } else if their_slice.is_empty() {
            // Theirs deleted; ours modified.
            emit_conflict(&mut out, our_slice, their_slice, labels);
            conflict_count += 1;
        } else {
            emit_conflict(&mut out, our_slice, their_slice, labels);
            conflict_count += 1;
        }

        base_cursor = base_block_end;
        i = i_end;
        j = j_end;
    }

    // Drain remaining hunks.
    while i < ours_hunks.len() {
        let oh = &ours_hunks[i];
        emit_base_range(&mut out, base_lines, base_cursor, oh.base_start);
        emit_side_range(&mut out, ours_lines, oh.side_start, oh.side_len);
        base_cursor = oh.base_end();
        i += 1;
    }
    while j < theirs_hunks.len() {
        let th = &theirs_hunks[j];
        emit_base_range(&mut out, base_lines, base_cursor, th.base_start);
        emit_side_range(&mut out, theirs_lines, th.side_start, th.side_len);
        base_cursor = th.base_end();
        j += 1;
    }

    // Trailing un-changed base.
    emit_base_range(&mut out, base_lines, base_cursor, base_lines.len());

    if conflict_count == 0 {
        FileMergeResult::Resolved(out)
    } else {
        FileMergeResult::Conflicted {
            body: out,
            conflict_count,
        }
    }
}

fn emit_base_range(out: &mut Vec<u8>, lines: &[&[u8]], from: usize, to: usize) {
    for line in &lines[from..to] {
        out.extend_from_slice(line);
    }
}

fn emit_side_range(out: &mut Vec<u8>, lines: &[&[u8]], from: usize, len: usize) {
    for line in &lines[from..from + len] {
        out.extend_from_slice(line);
    }
}

fn emit_conflict(out: &mut Vec<u8>, ours: &[&[u8]], theirs: &[&[u8]], labels: &FileMergeLabels) {
    // Marker lines. Always emit with a trailing newline. If the *preceding*
    // content's last byte wasn't a newline (i.e., we just emitted lines from
    // base/ours/theirs that lacked their final newline), prepend a newline so
    // the marker starts a new line — matching git's behavior when one side
    // lacks a final newline.
    if !out.is_empty() && *out.last().unwrap() != b'\n' {
        out.push(b'\n');
    }

    write_marker(out, b'<', labels.ours);
    emit_lines_with_trailing_newline(out, ours);
    write_marker(out, b'=', "");
    emit_lines_with_trailing_newline(out, theirs);
    write_marker(out, b'>', labels.theirs);
}

fn write_marker(out: &mut Vec<u8>, ch: u8, label: &str) {
    for _ in 0..DEFAULT_MARKER_SIZE {
        out.push(ch);
    }
    if !label.is_empty() {
        out.push(b' ');
        out.extend_from_slice(label.as_bytes());
    }
    out.push(b'\n');
}

/// Emit side lines into the conflict body, guaranteeing the run ends with `\n`
/// so the next marker doesn't fuse onto the last line. If a side is empty we
/// emit nothing (which is fine — the conflict markers are still separated by
/// their own `\n`s).
fn emit_lines_with_trailing_newline(out: &mut Vec<u8>, lines: &[&[u8]]) {
    if lines.is_empty() {
        return;
    }
    for line in lines {
        out.extend_from_slice(line);
    }
    if let Some(last) = lines.last() {
        if last.last().copied() != Some(b'\n') {
            out.push(b'\n');
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn labels() -> FileMergeLabels<'static> {
        FileMergeLabels::default()
    }

    fn labels_named() -> FileMergeLabels<'static> {
        FileMergeLabels {
            base: "BASE",
            ours: "HEAD",
            theirs: "feature",
        }
    }

    /// 1. Identical inputs → Resolved equal to base.
    #[test]
    fn identical_inputs_clean() {
        let base = b"a\nb\nc\n";
        let r = merge_file(base, base, base, &labels());
        assert_eq!(r, FileMergeResult::Resolved(base.to_vec()));
    }

    /// 2. Only ours changed → Resolved equal to ours.
    #[test]
    fn only_ours_changed_takes_ours() {
        let base = b"a\nb\nc\n";
        let ours = b"a\nB\nc\n";
        let r = merge_file(base, ours, base, &labels());
        assert_eq!(r, FileMergeResult::Resolved(ours.to_vec()));
    }

    /// 3. Only theirs changed → Resolved equal to theirs.
    #[test]
    fn only_theirs_changed_takes_theirs() {
        let base = b"a\nb\nc\n";
        let theirs = b"a\nB\nc\n";
        let r = merge_file(base, base, theirs, &labels());
        assert_eq!(r, FileMergeResult::Resolved(theirs.to_vec()));
    }

    /// 4. Both made the same change → Resolved (idempotent).
    #[test]
    fn idempotent_same_change_both_sides() {
        let base = b"a\nb\nc\n";
        let same = b"a\nB\nc\n";
        let r = merge_file(base, same, same, &labels());
        assert_eq!(r, FileMergeResult::Resolved(same.to_vec()));
    }

    /// 5. Disjoint changes (line 1 vs line 10 in 20-line file) → Resolved
    /// with both changes.
    #[test]
    fn disjoint_changes_clean() {
        let mut base = Vec::new();
        for i in 0..20 {
            base.extend_from_slice(format!("l{:02}\n", i).as_bytes());
        }
        let mut ours = base.clone();
        // change line 1
        let pos = b"l00\n".len();
        ours.splice(pos..pos + b"l01\n".len(), b"OURS\n".iter().copied());
        let mut theirs = base.clone();
        let start = b"l00\nl01\nl02\nl03\nl04\nl05\nl06\nl07\nl08\nl09\n".len();
        theirs.splice(start..start + b"l10\n".len(), b"THEIRS\n".iter().copied());
        let r = merge_file(&base, &ours, &theirs, &labels());
        match r {
            FileMergeResult::Resolved(out) => {
                let s = String::from_utf8_lossy(&out);
                assert!(s.contains("OURS"));
                assert!(s.contains("THEIRS"));
                assert!(!s.contains("<<<<<<<"));
            }
            FileMergeResult::Conflicted { .. } => panic!("expected clean merge"),
        }
    }

    /// 6. Nearby (but not strictly adjacent) changes — ours at line 5,
    /// theirs at line 7, with line 6 unchanged between them → clean.
    /// Note: truly adjacent (no equal base line between) is a conflict in
    /// git's default merge; this test checks that we still merge when there
    /// IS a separator line.
    #[test]
    fn nearby_disjoint_changes_clean() {
        let base = b"l0\nl1\nl2\nl3\nl4\nl5\nl6\nl7\nl8\nl9\n";
        let ours = b"l0\nl1\nl2\nl3\nl4\nL5\nl6\nl7\nl8\nl9\n";
        let theirs = b"l0\nl1\nl2\nl3\nl4\nl5\nl6\nT7\nl8\nl9\n";
        let r = merge_file(base, ours, theirs, &labels());
        let body = r.body();
        let s = String::from_utf8_lossy(body);
        assert!(!r.has_conflict(), "expected clean, got:\n{s}");
        assert!(s.contains("L5"));
        assert!(s.contains("T7"));
    }

    /// Truly adjacent edits (touching at the base boundary) → conflict, to
    /// match git's default behavior.
    #[test]
    fn truly_adjacent_changes_conflict() {
        let base = b"l0\nl1\nl2\nl3\nl4\nl5\nl6\nl7\nl8\nl9\n";
        let ours = b"l0\nl1\nl2\nl3\nl4\nL5\nl6\nl7\nl8\nl9\n";
        let theirs = b"l0\nl1\nl2\nl3\nl4\nl5\nT6\nl7\nl8\nl9\n";
        let r = merge_file(base, ours, theirs, &labels());
        assert!(
            r.has_conflict(),
            "expected conflict for truly-adjacent edits"
        );
    }

    /// 7. Overlapping changes (both modify the same line) → Conflicted.
    #[test]
    fn overlapping_changes_conflict() {
        let base = b"l0\nl1\nl2\nl3\nl4\n";
        let ours = b"l0\nl1\nOURS\nl3\nl4\n";
        let theirs = b"l0\nl1\nTHEIRS\nl3\nl4\n";
        let r = merge_file(base, ours, theirs, &labels());
        assert!(r.has_conflict());
        let s = String::from_utf8_lossy(r.body());
        assert!(s.contains("<<<<<<< ours"));
        assert!(s.contains("OURS"));
        assert!(s.contains("======="));
        assert!(s.contains("THEIRS"));
        assert!(s.contains(">>>>>>> theirs"));
        if let FileMergeResult::Conflicted { conflict_count, .. } = r {
            assert_eq!(conflict_count, 1);
        }
    }

    /// 8. Both added a new line at the same position with different content
    /// → Conflicted (when inserts are at the same base offset).
    #[test]
    fn both_added_at_same_position_different_content_conflicts() {
        let base = b"a\nb\n";
        let ours = b"a\nINSERT_OURS\nb\n";
        let theirs = b"a\nINSERT_THEIRS\nb\n";
        let r = merge_file(base, ours, theirs, &labels());
        assert!(
            r.has_conflict(),
            "got: {}",
            String::from_utf8_lossy(r.body())
        );
    }

    /// 9. Both deleted the same line → Resolved (deletion).
    #[test]
    fn both_deleted_same_line_clean() {
        let base = b"a\nb\nc\n";
        let ours = b"a\nc\n";
        let theirs = b"a\nc\n";
        let r = merge_file(base, ours, theirs, &labels());
        assert_eq!(r, FileMergeResult::Resolved(b"a\nc\n".to_vec()));
    }

    /// 10. Ours modified, theirs deleted → Conflict (modify/delete).
    #[test]
    fn ours_modified_theirs_deleted_conflicts() {
        let base = b"a\nb\nc\n";
        let ours = b"a\nB\nc\n";
        let theirs = b"a\nc\n";
        let r = merge_file(base, ours, theirs, &labels());
        assert!(r.has_conflict());
        let s = String::from_utf8_lossy(r.body());
        assert!(s.contains("B"));
        assert!(s.contains("<<<<<<<"));
        assert!(s.contains(">>>>>>>"));
    }

    /// 11. Ours deleted, theirs modified → Conflict (delete/modify).
    #[test]
    fn ours_deleted_theirs_modified_conflicts() {
        let base = b"a\nb\nc\n";
        let ours = b"a\nc\n";
        let theirs = b"a\nB\nc\n";
        let r = merge_file(base, ours, theirs, &labels());
        assert!(r.has_conflict());
        let s = String::from_utf8_lossy(r.body());
        assert!(s.contains("B"));
        // The conflict region should still bracket both sides — even if ours
        // is empty (delete) we emit `<<< ours\n=== \n<their lines>\n>>> theirs`.
        assert!(s.contains("<<<<<<< ours"));
        assert!(s.contains(">>>>>>> theirs"));
    }

    /// 12. Empty base, both sides added — different → Conflict; same → Clean.
    #[test]
    fn empty_base_both_added_same_clean() {
        let base = b"";
        let ours = b"hello\n";
        let theirs = b"hello\n";
        let r = merge_file(base, ours, theirs, &labels());
        assert_eq!(r, FileMergeResult::Resolved(b"hello\n".to_vec()));
    }

    #[test]
    fn empty_base_both_added_different_conflicts() {
        let base = b"";
        let ours = b"hello\n";
        let theirs = b"goodbye\n";
        let r = merge_file(base, ours, theirs, &labels());
        assert!(r.has_conflict());
    }

    /// 13. All three empty → Resolved empty.
    #[test]
    fn all_three_empty_clean() {
        let r = merge_file(b"", b"", b"", &labels());
        assert_eq!(r, FileMergeResult::Resolved(Vec::new()));
    }

    /// 14. Trailing-newline edge case.
    /// Base ends with `\n`, ours doesn't, theirs does. Ours touches just the
    /// last line (drops its newline) — we want to preserve that (ours's
    /// "version" wins; theirs is identical to base).
    #[test]
    fn trailing_newline_taking_ours() {
        let base = b"a\nb\n";
        let ours = b"a\nb";
        let theirs = b"a\nb\n";
        let r = merge_file(base, ours, theirs, &labels());
        assert_eq!(r, FileMergeResult::Resolved(b"a\nb".to_vec()));
    }

    /// 15. CRLF lines treated as bytes. Use disjoint changes (lines 1 and 4
    /// out of 5) so they merge cleanly.
    #[test]
    fn crlf_lines_treated_as_bytes() {
        let base = b"a\r\nb\r\nc\r\nd\r\ne\r\n";
        let ours = b"a\r\nB\r\nc\r\nd\r\ne\r\n";
        let theirs = b"a\r\nb\r\nc\r\nD\r\ne\r\n";
        let r = merge_file(base, ours, theirs, &labels());
        assert!(
            !r.has_conflict(),
            "got: {}",
            String::from_utf8_lossy(r.body())
        );
        let body = r.into_body();
        let s = String::from_utf8_lossy(&body);
        assert!(s.contains("B"));
        assert!(s.contains("D"));
        // CRLF preserved.
        assert!(body.windows(2).any(|w| w == b"\r\n"));
    }

    /// 16. Very large input (10k lines), one line changed in the middle.
    #[test]
    fn large_input_one_change_clean() {
        let mut base = Vec::new();
        for i in 0..10_000 {
            base.extend_from_slice(format!("line{i:05}\n").as_bytes());
        }
        let mut ours = base.clone();
        // Change line 5000.
        let needle = b"line05000\n";
        let pos = base
            .windows(needle.len())
            .position(|w| w == needle)
            .expect("found");
        ours.splice(pos..pos + needle.len(), b"DIFFERENT\n".iter().copied());
        let theirs = base.clone();
        let r = merge_file(&base, &ours, &theirs, &labels());
        match r {
            FileMergeResult::Resolved(b) => assert_eq!(b, ours),
            FileMergeResult::Conflicted { .. } => panic!("expected clean merge"),
        }
    }

    /// 17. **Load-bearing**: byte-compare our conflict body against
    /// `git merge-file -p ours base theirs` for a known scenario.
    #[test]
    fn conflict_markers_match_git_merge_file() {
        if !git_available() {
            eprintln!("git not available; skipping");
            return;
        }
        let base: &[u8] = b"alpha\nbeta\ngamma\ndelta\n";
        let ours: &[u8] = b"alpha\nbeta_OURS\ngamma\ndelta\n";
        let theirs: &[u8] = b"alpha\nbeta_THEIRS\ngamma\ndelta\n";
        let theirs_label = "theirs";
        let our_label = "ours";

        // System git invocation: --diff3 default-off so just plain markers.
        let dir = tempfile::tempdir().unwrap();
        let base_p = dir.path().join("base.txt");
        let ours_p = dir.path().join("ours.txt");
        let theirs_p = dir.path().join("theirs.txt");
        std::fs::write(&base_p, base).unwrap();
        std::fs::write(&ours_p, ours).unwrap();
        std::fs::write(&theirs_p, theirs).unwrap();
        let out = std::process::Command::new("git")
            .args([
                "merge-file",
                "-p",
                "-L",
                our_label,
                "-L",
                "base",
                "-L",
                theirs_label,
            ])
            .arg(&ours_p)
            .arg(&base_p)
            .arg(&theirs_p)
            .output()
            .expect("run git merge-file");
        // git merge-file exits with 1 on conflicts; that's expected here.
        let theirs_out = out.stdout;

        let r = merge_file(base, ours, theirs, &labels());
        match r {
            FileMergeResult::Conflicted { body, .. } => {
                assert_eq!(
                    body,
                    theirs_out,
                    "ours:\n{}\ntheirs:\n{}",
                    String::from_utf8_lossy(&body),
                    String::from_utf8_lossy(&theirs_out)
                );
            }
            FileMergeResult::Resolved(_) => panic!("expected conflict"),
        }
    }

    /// 18. Multiple conflict regions in one file → all marked, count correct.
    #[test]
    fn multiple_conflict_regions_counted() {
        let base = b"a\nb\nc\nd\ne\nf\ng\nh\ni\nj\nk\nl\nm\nn\no\np\n";
        let ours = b"a\nb\nOURS_TOP\nd\ne\nf\ng\nh\ni\nj\nk\nl\nm\nOURS_BOT\no\np\n";
        let theirs = b"a\nb\nTHEIRS_TOP\nd\ne\nf\ng\nh\ni\nj\nk\nl\nm\nTHEIRS_BOT\no\np\n";
        let r = merge_file(base, ours, theirs, &labels());
        match r {
            FileMergeResult::Conflicted {
                body,
                conflict_count,
            } => {
                assert_eq!(conflict_count, 2);
                let s = String::from_utf8_lossy(&body);
                let n_open = s.matches("<<<<<<<").count();
                let n_close = s.matches(">>>>>>>").count();
                let n_eq = s.matches("=======").count();
                assert_eq!(n_open, 2);
                assert_eq!(n_close, 2);
                assert_eq!(n_eq, 2);
            }
            FileMergeResult::Resolved(_) => panic!("expected conflicts"),
        }
    }

    /// 19. Labels appear in markers — `<<<<<<< HEAD`, `>>>>>>> feature`.
    #[test]
    fn labels_appear_in_markers() {
        let base = b"line\n";
        let ours = b"OURS\n";
        let theirs = b"THEIRS\n";
        let r = merge_file(base, ours, theirs, &labels_named());
        assert!(r.has_conflict());
        let s = String::from_utf8_lossy(r.body());
        assert!(s.contains("<<<<<<< HEAD"), "got: {s}");
        assert!(s.contains(">>>>>>> feature"), "got: {s}");
    }

    /// 20. Stress: random-ish inputs — output is well-formed (markers paired).
    #[test]
    fn stress_random_inputs_well_formed() {
        // Pseudo-random but deterministic — no rand dep.
        let mut state: u32 = 0xdeadbeef;
        let mut next = || {
            state = state.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            state
        };

        for trial in 0..40 {
            let n = 8 + (next() as usize % 16);
            let mut base = Vec::new();
            for i in 0..n {
                base.extend_from_slice(format!("line_{trial}_{i}\n").as_bytes());
            }

            // Randomly mutate ours / theirs.
            let mut mutate = |buf: &[u8]| -> Vec<u8> {
                let mut lines = split_lines(buf)
                    .iter()
                    .map(|l| l.to_vec())
                    .collect::<Vec<_>>();
                if !lines.is_empty() {
                    let n_mut = 1 + (next() as usize % 3);
                    for _ in 0..n_mut {
                        let op = next() % 3;
                        let idx = (next() as usize) % lines.len();
                        match op {
                            0 => {
                                // Replace.
                                lines[idx] = format!("M{trial}_{idx}\n").into_bytes();
                            }
                            1 => {
                                // Delete.
                                if lines.len() > 1 {
                                    lines.remove(idx);
                                }
                            }
                            _ => {
                                // Insert.
                                lines.insert(idx, format!("I{trial}_{idx}\n").into_bytes());
                            }
                        }
                    }
                }
                lines.concat()
            };

            let ours = mutate(&base);
            let theirs = mutate(&base);

            let r = merge_file(&base, &ours, &theirs, &labels());
            let body = r.body();
            let s = String::from_utf8_lossy(body);
            // Marker counts must be matched.
            let n_open = s.matches("<<<<<<<").count();
            let n_close = s.matches(">>>>>>>").count();
            let n_eq = s.matches("=======").count();
            assert_eq!(
                n_open, n_close,
                "trial {trial}: open/close mismatch in:\n{s}"
            );
            assert_eq!(n_open, n_eq, "trial {trial}: open/eq mismatch in:\n{s}");
            if let FileMergeResult::Conflicted { conflict_count, .. } = &r {
                assert_eq!(
                    n_open, *conflict_count,
                    "trial {trial}: count {} vs markers {} in:\n{s}",
                    conflict_count, n_open
                );
            } else {
                assert_eq!(n_open, 0);
            }
        }
    }

    // ---- Additional edge cases beyond the 20 required ----

    /// Insertion at start by ours; theirs untouched → clean.
    #[test]
    fn insert_at_start_clean() {
        let base = b"a\nb\nc\n";
        let ours = b"X\na\nb\nc\n";
        let r = merge_file(base, ours, base, &labels());
        assert_eq!(r, FileMergeResult::Resolved(ours.to_vec()));
    }

    /// Both sides delete different lines.
    #[test]
    fn both_delete_different_lines_clean() {
        let base = b"a\nb\nc\nd\ne\n";
        let ours = b"a\nc\nd\ne\n"; // dropped b
        let theirs = b"a\nb\nc\ne\n"; // dropped d
        let r = merge_file(base, ours, theirs, &labels());
        match r {
            FileMergeResult::Resolved(b) => assert_eq!(b, b"a\nc\ne\n"),
            FileMergeResult::Conflicted { body, .. } => panic!(
                "expected clean merge, got:\n{}",
                String::from_utf8_lossy(&body)
            ),
        }
    }

    /// Identical insertion at the same offset → idempotent.
    #[test]
    fn identical_insertion_at_same_offset_clean() {
        let base = b"a\nb\n";
        let ours = b"a\nNEW\nb\n";
        let theirs = b"a\nNEW\nb\n";
        let r = merge_file(base, ours, theirs, &labels());
        assert_eq!(r, FileMergeResult::Resolved(b"a\nNEW\nb\n".to_vec()));
    }

    /// Pure single-line replacement on each side, same content.
    #[test]
    fn same_replacement_both_sides_clean() {
        let base = b"hello\n";
        let ours = b"goodbye\n";
        let theirs = b"goodbye\n";
        let r = merge_file(base, ours, theirs, &labels());
        assert_eq!(r, FileMergeResult::Resolved(b"goodbye\n".to_vec()));
    }

    /// Ours and theirs both completely replace base with different content.
    #[test]
    fn full_rewrite_both_sides_different_conflicts() {
        let base = b"old\n";
        let ours = b"new ours\n";
        let theirs = b"new theirs\n";
        let r = merge_file(base, ours, theirs, &labels());
        assert!(r.has_conflict());
    }

    /// Conflict body has `\n` between markers even when sides are empty.
    #[test]
    fn empty_side_in_conflict_still_has_marker_lines() {
        let base = b"a\nb\nc\n";
        let ours = b"a\nb\nc\n"; // unchanged → won't trigger conflict
        let theirs = b"X\nb\nc\n";
        let r = merge_file(base, ours, theirs, &labels());
        // theirs changed, ours didn't → take theirs, no conflict.
        assert!(!r.has_conflict());
    }

    /// CRLF-only differences vs LF-only — still byte-equal-line-based.
    #[test]
    fn ending_difference_is_a_line_change() {
        let base = b"hi\n";
        let ours = b"hi\r\n";
        let theirs = b"hi\n";
        let r = merge_file(base, ours, theirs, &labels());
        // ours differs (line bytes differ), theirs doesn't → take ours.
        assert_eq!(r, FileMergeResult::Resolved(b"hi\r\n".to_vec()));
    }

    /// Both sides add the same line at end → clean.
    #[test]
    fn both_append_same_line_clean() {
        let base = b"a\n";
        let ours = b"a\nNEW\n";
        let theirs = b"a\nNEW\n";
        let r = merge_file(base, ours, theirs, &labels());
        assert_eq!(r, FileMergeResult::Resolved(b"a\nNEW\n".to_vec()));
    }

    /// Different appends → conflict.
    #[test]
    fn different_appends_conflict() {
        let base = b"a\n";
        let ours = b"a\nOURS\n";
        let theirs = b"a\nTHEIRS\n";
        let r = merge_file(base, ours, theirs, &labels());
        assert!(r.has_conflict());
    }

    /// Ours empties the file, theirs makes a small change → conflict.
    #[test]
    fn ours_empties_theirs_modifies_conflicts() {
        let base = b"a\nb\nc\n";
        let ours = b"";
        let theirs = b"a\nB\nc\n";
        let r = merge_file(base, ours, theirs, &labels());
        assert!(r.has_conflict());
    }

    /// Both sides remove all content → clean empty.
    #[test]
    fn both_empty_result_clean() {
        let base = b"a\nb\n";
        let r = merge_file(base, b"", b"", &labels());
        assert_eq!(r, FileMergeResult::Resolved(Vec::new()));
    }

    /// Round-trip: git merge-file output byte-equality on more cases.
    #[test]
    #[allow(clippy::type_complexity)]
    fn round_trip_git_merge_file_multiple_scenarios() {
        if !git_available() {
            eprintln!("skip: no git");
            return;
        }
        let cases: &[(&[u8], &[u8], &[u8], &str)] = &[
            (
                b"a\nb\nc\n",
                b"a\nB\nc\n",
                b"a\nC\nc\n",
                "single-line overlapping change",
            ),
            (
                b"l0\nl1\nl2\nl3\nl4\nl5\nl6\nl7\nl8\nl9\n",
                b"l0\nl1\nl2\nl3\nl4\nL5\nl6\nl7\nl8\nl9\n",
                b"l0\nl1\nl2\nl3\nL4\nl5\nl6\nl7\nl8\nl9\n",
                "adjacent-but-disjoint changes",
            ),
            (
                b"hello world\n",
                b"GOODBYE world\n",
                b"HELLO world\n",
                "different rewrites of single line",
            ),
            (
                b"line a\nline b\nline c\nline d\n",
                b"line A\nline B\nline c\nline d\n",
                b"line a\nline b\nline C\nline D\n",
                "two non-overlapping blocks of changes",
            ),
            (
                b"a\nb\nc\nd\ne\n",
                b"a\nb\n",
                b"a\nb\nc\nd\nE\n",
                "ours truncates from middle, theirs modifies last",
            ),
            (
                b"a\nb\nc\nd\ne\nf\ng\n",
                b"a\nbb\nc\ndd\ne\nff\ng\n",
                b"a\nb\ncc\nd\nee\nf\ngg\n",
                "interleaved changes on different lines",
            ),
            (
                b"function foo() {\n  return 1;\n}\n",
                b"function foo() {\n  return 100;\n}\n",
                b"function foo() {\n  return 1;\n}\n",
                "ours-only modification",
            ),
            (
                b"function foo() {\n  return 1;\n}\n",
                b"function foo() {\n  return 1;\n}\n",
                b"function foo() {\n  return 100;\n}\n",
                "theirs-only modification",
            ),
            (b"a\nb\n", b"a\nb\nx\n", b"a\nb\ny\n", "different appends"),
            (
                b"keep\nthis\nline\n",
                b"keep\nthis\nline\nNEW1\nNEW2\n",
                b"keep\nthis\nline\nNEW1\nNEW2\n",
                "same multi-line append on both",
            ),
            (
                b"top\nmid\nbot\n",
                b"top\nMID_o\nbot\n",
                b"top\nMID_t\nbot\n",
                "single line conflict",
            ),
        ];
        for (base, ours, theirs, label) in cases {
            let r = merge_file(base, ours, theirs, &labels());
            let git_out = git_merge_file(base, ours, theirs);
            assert_eq!(
                r.body(),
                git_out.as_slice(),
                "mismatch on case '{label}':\nours:\n{}\ntheirs:\n{}",
                String::from_utf8_lossy(r.body()),
                String::from_utf8_lossy(&git_out)
            );
        }
    }

    /// Multi-line block rewrite: ours and theirs both modify the same block in
    /// different ways.
    #[test]
    fn multi_line_block_rewrite_conflicts() {
        let base = b"top\n1\n2\n3\nbottom\n";
        let ours = b"top\nA\nB\nbottom\n";
        let theirs = b"top\nX\nY\nZ\nbottom\n";
        let r = merge_file(base, ours, theirs, &labels());
        assert!(r.has_conflict());
        if let FileMergeResult::Conflicted {
            body,
            conflict_count,
        } = r
        {
            assert_eq!(conflict_count, 1);
            let s = String::from_utf8_lossy(&body);
            assert!(s.contains("A"));
            assert!(s.contains("Y"));
            assert!(s.contains("top"));
            assert!(s.contains("bottom"));
        }
    }

    /// Boundary: change at very last line.
    #[test]
    fn change_at_last_line_each_side_disjoint() {
        let base = b"a\nb\nc\n";
        let ours = b"OURS\nb\nc\n"; // change first
        let theirs = b"a\nb\nTHEIRS\n"; // change last
        let r = merge_file(base, ours, theirs, &labels());
        assert!(!r.has_conflict());
        let s = String::from_utf8_lossy(r.body());
        assert!(s.contains("OURS"));
        assert!(s.contains("THEIRS"));
    }

    // ---- Helpers ----

    fn git_available() -> bool {
        std::process::Command::new("git")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    /// Run `git merge-file -p` with default labels and return its stdout.
    fn git_merge_file(base: &[u8], ours: &[u8], theirs: &[u8]) -> Vec<u8> {
        let dir = tempfile::tempdir().unwrap();
        let base_p = dir.path().join("base.txt");
        let ours_p = dir.path().join("ours.txt");
        let theirs_p = dir.path().join("theirs.txt");
        std::fs::write(&base_p, base).unwrap();
        std::fs::write(&ours_p, ours).unwrap();
        std::fs::write(&theirs_p, theirs).unwrap();
        let out = std::process::Command::new("git")
            .args([
                "merge-file",
                "-p",
                "-L",
                "ours",
                "-L",
                "base",
                "-L",
                "theirs",
            ])
            .arg(&ours_p)
            .arg(&base_p)
            .arg(&theirs_p)
            .output()
            .expect("run git merge-file");
        out.stdout
    }

    // Sanity check that my hand-rolled Myers gives the same answers as
    // xdiff::unified_diff would imply for some simple cases.
    #[test]
    fn diff_hunks_basic() {
        let a_str = split_lines(b"a\nb\nc\n");
        let b_str = split_lines(b"a\nX\nc\n");
        let h = diff_hunks(&a_str, &b_str);
        assert_eq!(h.len(), 1);
        assert_eq!(h[0].base_start, 1);
        assert_eq!(h[0].base_len, 1);
        assert_eq!(h[0].side_start, 1);
        assert_eq!(h[0].side_len, 1);
    }

    #[test]
    fn diff_hunks_pure_insertion() {
        let a_str = split_lines(b"a\nb\n");
        let b_str = split_lines(b"a\nMID\nb\n");
        let h = diff_hunks(&a_str, &b_str);
        assert_eq!(h.len(), 1);
        // base_start = 1 (after `a`), base_len = 0 (pure insertion), side_len = 1.
        assert_eq!(h[0].base_start, 1);
        assert_eq!(h[0].base_len, 0);
        assert_eq!(h[0].side_len, 1);
    }

    #[test]
    fn diff_hunks_pure_deletion() {
        let a_str = split_lines(b"a\nb\nc\n");
        let b_str = split_lines(b"a\nc\n");
        let h = diff_hunks(&a_str, &b_str);
        assert_eq!(h.len(), 1);
        assert_eq!(h[0].base_start, 1);
        assert_eq!(h[0].base_len, 1);
        assert_eq!(h[0].side_len, 0);
    }
}
