//! `blame` — for every line of a file, find the commit that introduced it.
//!
//! Algorithm sketch (mirrors `blame.c`, minus the bells and whistles):
//!
//! 1. Read `path` from the tree at `start_commit`. Split into lines. Each line
//!    starts life as "still suspect" — we don't know which commit introduced
//!    it yet. Track its 1-based final line number and the line's bytes.
//!
//! 2. Walk commits backwards via first-parent. At each commit C with parent P:
//!    - If `path@C` and `path@P` have the same oid → nothing changed in this
//!      commit for this file; just bridge through to P.
//!    - If `path@P` doesn't exist (or doesn't exist after a follow-renames
//!      lookup): every still-suspect line gets attributed to C. Walk stops
//!      for those lines.
//!    - Otherwise: run xdiff Myers between `path@P` and `path@C`. Use the
//!      edit script to map "currently-suspect line at position X in C's
//!      version" back to "line at position Y in P's version". Lines that
//!      match are still suspect (now with their P-side line number); lines
//!      that are new in C are attributed to C and frozen.
//!
//! 3. When the walk hits a root commit (no parent) any still-suspect lines
//!    are attributed to that root.
//!
//! Rename-following (`-C` / `--follow`): if `path@P` doesn't exist, we don't
//! immediately freeze the lines onto C. Instead we look at C's full
//! tree-vs-tree diff and ask `crate::diff::rename` whether any deleted entry
//! in C's diff (i.e. a path that exists in P but not in C) is similar enough
//! to our current file to be a rename. If so, we keep following but under
//! the parent's path. We record the parent path in the per-line
//! `origin_path` so the output can show "this line was originally at <old
//! path>".

use std::collections::HashMap;
use std::path::Path;

use thiserror::Error;

use crate::commit::{Commit, CommitError};
use crate::diff::rename::{detect_renames, RenameOpts};
use crate::diff::{flatten_tree, peel_to_tree, DiffEntry, DiffError};
use crate::hash::{HashError, ObjectId};
use crate::object::{ObjectKind, RawObject};
use crate::odb::OdbError;
use crate::repo::Repository;
use crate::tree::{FileMode, TreeError};

/// One annotated line in the final output.
#[derive(Debug, Clone)]
pub struct BlameLine {
    /// Commit that last touched this line.
    pub commit: ObjectId,
    /// Author name from that commit.
    pub author: String,
    /// Author email.
    pub author_email: String,
    /// Author time (Unix seconds).
    pub author_time: i64,
    /// Author timezone offset (minutes east of UTC).
    pub author_tz_offset: i32,
    /// The line content (without trailing newline).
    pub content: Vec<u8>,
    /// 1-based line number in the final file (the one we blamed).
    pub final_lineno: u32,
    /// 1-based line number in the commit's version (1 for newly-introduced
    /// content that wasn't part of the parent at all).
    pub origin_lineno: u32,
    /// Path at the time the line was introduced (may differ from the input
    /// `path` if the file was renamed and `follow_renames` is on).
    pub origin_path: Vec<u8>,
}

/// Options for `blame`.
#[derive(Debug, Clone, Default)]
pub struct BlameOpts {
    /// If true, follow the file across renames detected by `crate::diff::rename`.
    pub follow_renames: bool,
    /// Inclusive (start, end) 1-based line range, restricting which lines we
    /// return. None means the whole file. The walk still considers all lines;
    /// we just filter at the end.
    pub line_range: Option<(u32, u32)>,
}

#[derive(Error, Debug)]
pub enum BlameError {
    #[error(transparent)]
    Odb(#[from] OdbError),
    #[error(transparent)]
    Commit(#[from] CommitError),
    #[error(transparent)]
    Tree(#[from] TreeError),
    #[error(transparent)]
    Hash(#[from] HashError),
    #[error(transparent)]
    Diff(#[from] DiffError),
    #[error(transparent)]
    Rename(#[from] crate::diff::rename::RenameError),
    #[error("path {0} not found at start commit")]
    PathNotFound(String),
}

/// Compute blame annotations for `path` at `start_commit`.
pub fn blame(
    repo: &Repository,
    path: &[u8],
    start_commit: ObjectId,
    opts: &BlameOpts,
) -> Result<Vec<BlameLine>, BlameError> {
    // Resolve the file at the start commit.
    let start_tree = peel_to_tree(repo, start_commit)?;
    let (start_oid, _start_mode) = match find_path_in_tree(repo, &start_tree, path)? {
        Some(e) => e,
        None => {
            return Err(BlameError::PathNotFound(
                String::from_utf8_lossy(path).into_owned(),
            ));
        }
    };
    let start_bytes = read_blob(repo, &start_oid)?;
    let lines: Vec<Vec<u8>> = split_lines_owned(&start_bytes);
    let n_lines = lines.len();

    // For each line: where it currently sits in the commit-under-walk's
    // version (1-based), or `None` once attributed.
    //
    // `cur_path` is the path we're currently chasing — initially the input,
    // possibly different after a rename hop.
    let mut suspects: Vec<Option<u32>> = (0..n_lines).map(|i| Some(i as u32 + 1)).collect();
    let mut origin_path: Vec<Vec<u8>> = (0..n_lines).map(|_| path.to_vec()).collect();
    let mut origin_lineno: Vec<u32> = vec![0; n_lines];
    let mut origin_commit: Vec<Option<ObjectId>> = vec![None; n_lines];

    // The path we're currently following. Starts as the input.
    let mut cur_path: Vec<u8> = path.to_vec();
    // Lines whose current-commit position is what `suspects` tracks. As we
    // walk to the next commit, we update positions.
    let mut cur_commit = start_commit;
    // Cached blob bytes for the current commit's version of cur_path.
    let mut cur_bytes = start_bytes.clone();

    // Walk.
    loop {
        let obj = repo.odb().read(&cur_commit)?;
        if obj.kind != ObjectKind::Commit {
            // Defensive: shouldn't happen if the caller passed a commit oid.
            // Attribute everything still suspect to this oid.
            attribute_remaining(
                &mut suspects,
                &mut origin_lineno,
                &mut origin_commit,
                cur_commit,
            );
            break;
        }
        let commit = Commit::parse(&obj.data, repo.hash_kind())?;

        // Pick the first parent. Multi-parent (merge) commits: M16 follows
        // first-parent only.
        let parent_oid = commit.parents.first().copied();
        let Some(parent_oid) = parent_oid else {
            // Root commit. Every still-suspect line is from here.
            attribute_remaining(
                &mut suspects,
                &mut origin_lineno,
                &mut origin_commit,
                cur_commit,
            );
            break;
        };

        // What does the parent's tree have at cur_path?
        let parent_obj = repo.odb().read(&parent_oid)?;
        let parent_commit = Commit::parse(&parent_obj.data, repo.hash_kind())?;
        let parent_tree = parent_commit.tree;
        let parent_entry = find_path_in_tree(repo, &parent_tree, &cur_path)?;

        match parent_entry {
            Some((p_oid, _)) if p_oid == oid_for_blob(&cur_bytes, repo) => {
                // Same content. The file wasn't touched in this commit.
                // Bridge straight through, preserving line positions.
                cur_commit = parent_oid;
                // `cur_bytes` stays the same (oid matches).
                continue;
            }
            Some((p_oid, _)) => {
                // Modified. Diff and remap line positions.
                let parent_bytes = read_blob(repo, &p_oid)?;
                let mapping = compute_line_mapping(&parent_bytes, &cur_bytes);
                // For each currently-suspect line at cur position `i+1`, look
                // up where it lives in the parent. If it's still there
                // (mapping[i] = Some(p_idx)), update the position. If not
                // (mapping[i] = None), the line was added by `cur_commit` —
                // attribute and freeze.
                for line_idx in 0..n_lines {
                    let Some(cur_lineno_1based) = suspects[line_idx] else {
                        continue;
                    };
                    let cur_idx_0 = (cur_lineno_1based - 1) as usize;
                    match mapping.get(cur_idx_0).copied().flatten() {
                        Some(parent_idx_0) => {
                            // Still in parent at parent_idx_0. Stay suspect,
                            // update position.
                            suspects[line_idx] = Some(parent_idx_0 + 1);
                        }
                        None => {
                            // Newly introduced by cur_commit (at cur position).
                            origin_lineno[line_idx] = cur_lineno_1based;
                            origin_commit[line_idx] = Some(cur_commit);
                            origin_path[line_idx] = cur_path.clone();
                            suspects[line_idx] = None;
                        }
                    }
                }
                cur_commit = parent_oid;
                cur_bytes = parent_bytes;
                continue;
            }
            None => {
                // cur_path doesn't exist in the parent. Either this commit
                // introduced the file, or it renamed it from something.
                let renamed_from = if opts.follow_renames {
                    try_find_rename_source(repo, &cur_path, &cur_bytes, &parent_tree, &commit.tree)?
                } else {
                    None
                };
                match renamed_from {
                    Some((from_path, from_oid)) => {
                        // Treat as a modification under the parent's name.
                        let parent_bytes = read_blob(repo, &from_oid)?;
                        let mapping = compute_line_mapping(&parent_bytes, &cur_bytes);
                        for line_idx in 0..n_lines {
                            let Some(cur_lineno_1based) = suspects[line_idx] else {
                                continue;
                            };
                            let cur_idx_0 = (cur_lineno_1based - 1) as usize;
                            match mapping.get(cur_idx_0).copied().flatten() {
                                Some(parent_idx_0) => {
                                    suspects[line_idx] = Some(parent_idx_0 + 1);
                                }
                                None => {
                                    origin_lineno[line_idx] = cur_lineno_1based;
                                    origin_commit[line_idx] = Some(cur_commit);
                                    // For renamed-then-edited lines, the path
                                    // at the introducing commit is the *new*
                                    // name (cur_path), not the parent's name.
                                    origin_path[line_idx] = cur_path.clone();
                                    suspects[line_idx] = None;
                                }
                            }
                        }
                        cur_path = from_path;
                        cur_commit = parent_oid;
                        cur_bytes = parent_bytes;
                        continue;
                    }
                    None => {
                        // File is new in cur_commit. Every still-suspect line
                        // is attributed here.
                        attribute_remaining(
                            &mut suspects,
                            &mut origin_lineno,
                            &mut origin_commit,
                            cur_commit,
                        );
                        break;
                    }
                }
            }
        }
    }

    // Build the BlameLine vec. Resolve each commit oid to its author info.
    let mut author_cache: HashMap<ObjectId, AuthorInfo> = HashMap::new();
    let mut out: Vec<BlameLine> = Vec::with_capacity(n_lines);
    for i in 0..n_lines {
        // Anything still in `suspects` means the walk ended without ever
        // attributing it (e.g. for a single-commit repo with no parents
        // we already attributed at root). Belt-and-suspenders: fall back to
        // start_commit.
        let oid = origin_commit[i].unwrap_or(start_commit);
        let author = author_cache
            .entry(oid)
            .or_insert_with(|| AuthorInfo::load(repo, &oid).unwrap_or_default())
            .clone();
        let raw = &lines[i];
        let trimmed = strip_trailing_newline(raw);
        let lineno_origin = if origin_lineno[i] > 0 {
            origin_lineno[i]
        } else {
            i as u32 + 1
        };
        out.push(BlameLine {
            commit: oid,
            author: author.name,
            author_email: author.email,
            author_time: author.time_seconds,
            author_tz_offset: author.tz_offset,
            content: trimmed.to_vec(),
            final_lineno: i as u32 + 1,
            origin_lineno: lineno_origin,
            origin_path: origin_path[i].clone(),
        });
    }

    if let Some((lo, hi)) = opts.line_range {
        let lo = lo.max(1) as usize;
        let hi = hi as usize;
        let from = lo.saturating_sub(1).min(out.len());
        let to = hi.min(out.len());
        if from >= to {
            return Ok(Vec::new());
        }
        return Ok(out[from..to].to_vec());
    }

    Ok(out)
}

#[derive(Clone, Default)]
struct AuthorInfo {
    name: String,
    email: String,
    time_seconds: i64,
    tz_offset: i32,
}

impl AuthorInfo {
    fn load(repo: &Repository, oid: &ObjectId) -> Option<Self> {
        let obj = repo.odb().read(oid).ok()?;
        if obj.kind != ObjectKind::Commit {
            return None;
        }
        let c = Commit::parse(&obj.data, repo.hash_kind()).ok()?;
        Some(Self {
            name: c.author.name,
            email: c.author.email,
            time_seconds: c.author.when.seconds,
            tz_offset: c.author.when.offset_minutes,
        })
    }
}

fn attribute_remaining(
    suspects: &mut [Option<u32>],
    origin_lineno: &mut [u32],
    origin_commit: &mut [Option<ObjectId>],
    to: ObjectId,
) {
    for i in 0..suspects.len() {
        if let Some(lineno) = suspects[i] {
            origin_lineno[i] = lineno;
            origin_commit[i] = Some(to);
            suspects[i] = None;
        }
    }
}

/// Lookup a slash-separated path in a tree, returning `(blob_oid, mode)` for
/// the leaf if found. Returns `None` if any intermediate segment is missing
/// or non-tree-like.
fn find_path_in_tree(
    repo: &Repository,
    tree_oid: &ObjectId,
    path: &[u8],
) -> Result<Option<(ObjectId, FileMode)>, BlameError> {
    let mut cur_tree = *tree_oid;
    let segments: Vec<&[u8]> = path
        .split(|&b| b == b'/')
        .filter(|s| !s.is_empty())
        .collect();
    if segments.is_empty() {
        return Ok(None);
    }
    for (i, seg) in segments.iter().enumerate() {
        let raw = repo.odb().read(&cur_tree)?;
        if raw.kind != ObjectKind::Tree {
            return Ok(None);
        }
        let tree = crate::tree::Tree::parse(&raw.data, repo.hash_kind())?;
        let entry = tree.entries.iter().find(|e| e.name == *seg);
        let Some(entry) = entry else {
            return Ok(None);
        };
        let last = i + 1 == segments.len();
        if last {
            return Ok(Some((entry.oid, entry.mode)));
        }
        if !entry.mode.is_tree() {
            return Ok(None);
        }
        cur_tree = entry.oid;
    }
    Ok(None)
}

/// Hash `data` as a Blob would be hashed. The Repository carries the algorithm
/// to use. This is a convenience so we can verify blob identity without doing
/// an odb write.
fn oid_for_blob(data: &[u8], repo: &Repository) -> ObjectId {
    let obj = RawObject::new(ObjectKind::Blob, data.to_vec());
    obj.oid(repo.hash_kind())
}

fn read_blob(repo: &Repository, oid: &ObjectId) -> Result<Vec<u8>, BlameError> {
    let raw = repo.odb().read(oid)?;
    if raw.kind != ObjectKind::Blob {
        // Defensive: not a blob. Treat as empty content so we don't crash.
        return Ok(Vec::new());
    }
    Ok(raw.data)
}

/// Split data into 1-line chunks, owning the bytes. Each line includes its
/// trailing `\n` if present; the last line may not have one.
fn split_lines_owned(data: &[u8]) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let mut start = 0;
    for (i, &b) in data.iter().enumerate() {
        if b == b'\n' {
            out.push(data[start..=i].to_vec());
            start = i + 1;
        }
    }
    if start < data.len() {
        out.push(data[start..].to_vec());
    }
    out
}

fn strip_trailing_newline(line: &[u8]) -> &[u8] {
    if line.last() == Some(&b'\n') {
        let mut end = line.len() - 1;
        if end > 0 && line[end - 1] == b'\r' {
            end -= 1;
        }
        &line[..end]
    } else {
        line
    }
}

// ---------------------------------------------------------------------------
// Line correspondence via Myers
// ---------------------------------------------------------------------------

/// For each line in `b` (in order), say which line of `a` it came from
/// (0-based), or `None` if it's new content. This is enough for blame:
/// "still-suspect lines" are exactly the lines mapped through.
///
/// We implement a minimal Myers backtrack inline rather than depend on the
/// xdiff module — we want the SES, not the unified-diff text — and to avoid
/// promoting xdiff's internals.
fn compute_line_mapping(a: &[u8], b: &[u8]) -> Vec<Option<u32>> {
    let a_lines = split_lines_borrowed(a);
    let b_lines = split_lines_borrowed(b);
    let edits = myers_ses(&a_lines, &b_lines);
    let mut out: Vec<Option<u32>> = vec![None; b_lines.len()];
    for ed in edits {
        match ed {
            SesEdit::Equal { ai, bi } => out[bi] = Some(ai as u32),
            SesEdit::Delete { .. } | SesEdit::Insert { .. } => {}
        }
    }
    out
}

fn split_lines_borrowed(data: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut start = 0usize;
    for (i, &b) in data.iter().enumerate() {
        if b == b'\n' {
            out.push(&data[start..=i]);
            start = i + 1;
        }
    }
    if start < data.len() {
        out.push(&data[start..]);
    }
    out
}

#[derive(Debug, Clone, Copy)]
enum SesEdit {
    Equal {
        ai: usize,
        bi: usize,
    },
    #[allow(dead_code)]
    Delete {
        ai: usize,
    },
    #[allow(dead_code)]
    Insert {
        bi: usize,
    },
}

/// Local Myers SES — mirrors `xdiff::myers_diff` but produces our `SesEdit`
/// enum directly so we don't have to depend on xdiff's private types.
fn myers_ses<T: AsRef<[u8]>>(a: &[T], b: &[T]) -> Vec<SesEdit> {
    let n = a.len() as isize;
    let m = b.len() as isize;
    let max = n + m;

    if n == 0 && m == 0 {
        return Vec::new();
    }
    if n == 0 {
        return (0..m as usize).map(|i| SesEdit::Insert { bi: i }).collect();
    }
    if m == 0 {
        return (0..n as usize).map(|i| SesEdit::Delete { ai: i }).collect();
    }

    let v_len = (2 * max + 1) as usize;
    let offset = max;
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
            while x < n && y < m && a[x as usize].as_ref() == b[y as usize].as_ref() {
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
    let mut edits: Vec<SesEdit> = Vec::new();
    for d in (0..=d_final).rev() {
        let v = &trace[d];
        let k = x - y;
        let down = k == -(d as isize)
            || (k != d as isize && v[(k - 1 + offset) as usize] < v[(k + 1 + offset) as usize]);
        let prev_k = if down { k + 1 } else { k - 1 };
        let prev_x = v[(prev_k + offset) as usize];
        let prev_y = prev_x - prev_k;
        while x > prev_x && y > prev_y {
            edits.push(SesEdit::Equal {
                ai: (x - 1) as usize,
                bi: (y - 1) as usize,
            });
            x -= 1;
            y -= 1;
        }
        if d > 0 {
            if down {
                edits.push(SesEdit::Insert {
                    bi: prev_y as usize,
                });
            } else {
                edits.push(SesEdit::Delete {
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

// ---------------------------------------------------------------------------
// Rename-following helper
// ---------------------------------------------------------------------------

/// If `cur_path` exists in `commit_tree` but not in `parent_tree`, ask the
/// rename detector whether any path that was in `parent_tree` but not in
/// `commit_tree` is similar enough to be the source. Returns `(from_path,
/// from_oid)` of the chosen source, or `None` if no rename.
fn try_find_rename_source(
    repo: &Repository,
    cur_path: &[u8],
    cur_bytes: &[u8],
    parent_tree: &ObjectId,
    commit_tree: &ObjectId,
) -> Result<Option<(Vec<u8>, ObjectId)>, BlameError> {
    // Flatten both trees. We need the "added" side (paths in commit_tree not
    // in parent_tree) and the "deleted" side (paths in parent_tree not in
    // commit_tree).
    let parent_entries = flatten_tree(repo, parent_tree)?;
    let commit_entries = flatten_tree(repo, commit_tree)?;
    let parent_map: HashMap<&Vec<u8>, &DiffEntry> =
        parent_entries.iter().map(|e| (&e.path, e)).collect();
    let commit_map: HashMap<&Vec<u8>, &DiffEntry> =
        commit_entries.iter().map(|e| (&e.path, e)).collect();

    // Only consider cur_path on the "added" side — that's the slot the rename
    // detector would try to match.
    let cur_entry = match commit_map.get(&cur_path.to_vec()) {
        Some(e) => *e,
        None => return Ok(None),
    };
    let added = vec![(cur_path.to_vec(), cur_entry.mode, cur_entry.oid)];
    let mut deleted: Vec<(Vec<u8>, FileMode, ObjectId)> = Vec::new();
    for pe in &parent_entries {
        if !commit_map.contains_key(&pe.path) {
            deleted.push((pe.path.clone(), pe.mode, pe.oid));
        }
    }
    // Silence the unused `parent_map`. (Keeping it allocated above means we
    // could later e.g. look up byte content cheaply.)
    let _ = parent_map;
    let _ = cur_bytes;

    let renames = detect_renames(repo, &added, &deleted, &RenameOpts::default())?;
    // We added exactly one entry; pull the matching rename.
    if let Some(r) = renames.into_iter().find(|r| r.to == cur_path) {
        Ok(Some((r.from, r.from_oid)))
    } else {
        Ok(None)
    }
}

// ---------------------------------------------------------------------------
// Public helper used by the CLI
// ---------------------------------------------------------------------------

/// Format a single `BlameLine` in the same shape `git blame` uses by default.
///
/// `<short8> (<author> <YYYY-MM-DD HH:MM:SS> <±HHMM> <lineno>) <content>\n`
///
/// `lineno` is left-padded to a width that fits the largest line number in
/// the run; the caller passes that as `lineno_width`.
pub fn format_line(line: &BlameLine, lineno_width: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(80 + line.content.len());
    out.extend_from_slice(line.commit.short_hex(8).as_bytes());
    out.extend_from_slice(b" (");
    out.extend_from_slice(line.author.as_bytes());
    out.push(b' ');
    let date = format_blame_date(line.author_time, line.author_tz_offset);
    out.extend_from_slice(date.as_bytes());
    out.push(b' ');
    let lineno_str = format!("{:>width$}", line.final_lineno, width = lineno_width);
    out.extend_from_slice(lineno_str.as_bytes());
    out.extend_from_slice(b") ");
    out.extend_from_slice(&line.content);
    out.push(b'\n');
    out
}

/// `YYYY-MM-DD HH:MM:SS ±HHMM`. Same shell-out trick `log` uses.
fn format_blame_date(seconds: i64, offset_minutes: i32) -> String {
    use std::process::Command;
    let tz = offset_tz_env(offset_minutes);
    let raw = Command::new("date")
        .args(["-r", &seconds.to_string(), "+%Y-%m-%d %H:%M:%S"])
        .env("TZ", tz)
        .output();
    if let Ok(o) = raw {
        if o.status.success() {
            if let Ok(s) = std::str::from_utf8(&o.stdout) {
                let sign = if offset_minutes < 0 { '-' } else { '+' };
                let abs = offset_minutes.unsigned_abs();
                return format!("{} {sign}{:02}{:02}", s.trim(), abs / 60, abs % 60);
            }
        }
    }
    // Fallback: raw unix seconds.
    let sign = if offset_minutes < 0 { '-' } else { '+' };
    let abs = offset_minutes.unsigned_abs();
    format!("{} {sign}{:02}{:02}", seconds, abs / 60, abs % 60)
}

fn offset_tz_env(offset_min: i32) -> String {
    let inv = -offset_min;
    let sign = if inv < 0 { '-' } else { '+' };
    let abs = inv.unsigned_abs();
    format!("UTC{sign}{}:{:02}", abs / 60, abs % 60)
}

// Suppress unused-import warning when `Path` is referenced only via docs.
#[allow(dead_code)]
fn _doctype(_p: &Path) {}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::process::Command;

    fn has_git() -> bool {
        Command::new("git")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    fn run_git(cwd: &std::path::Path, args: &[&str]) {
        let out = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .env("GIT_AUTHOR_NAME", "T")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "T")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .env("GIT_AUTHOR_DATE", "1700000000 +0000")
            .env("GIT_COMMITTER_DATE", "1700000000 +0000")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn head_oid(repo: &Repository) -> ObjectId {
        let head_name = crate::refs::FullName::new("HEAD").unwrap();
        let (_, oid) = crate::refs::RefTarget::resolve(repo.refs(), &head_name)
            .unwrap()
            .expect("HEAD resolved");
        oid
    }

    #[test]
    fn blame_single_commit_repo_all_lines_to_that_commit() {
        if !has_git() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let path: PathBuf = dir.path().to_path_buf();
        run_git(&path, &["init", "-q", "-b", "main"]);
        run_git(&path, &["config", "user.email", "t@t"]);
        run_git(&path, &["config", "user.name", "T"]);
        std::fs::write(path.join("f.txt"), b"a\nb\nc\n").unwrap();
        run_git(&path, &["add", "f.txt"]);
        run_git(&path, &["commit", "-qm", "c1"]);

        let repo = Repository::open(path.join(".git")).unwrap();
        let head = head_oid(&repo);
        let lines = blame(&repo, b"f.txt", head, &BlameOpts::default()).unwrap();
        assert_eq!(lines.len(), 3);
        for ln in &lines {
            assert_eq!(
                ln.commit, head,
                "line {} not blamed to head",
                ln.final_lineno
            );
        }
        assert_eq!(lines[0].content, b"a");
        assert_eq!(lines[0].final_lineno, 1);
        assert_eq!(lines[2].content, b"c");
        assert_eq!(lines[2].final_lineno, 3);
    }

    #[test]
    fn blame_after_modifying_one_line_attributes_split() {
        if !has_git() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let path: PathBuf = dir.path().to_path_buf();
        run_git(&path, &["init", "-q", "-b", "main"]);
        run_git(&path, &["config", "user.email", "t@t"]);
        run_git(&path, &["config", "user.name", "T"]);

        std::fs::write(path.join("f.txt"), b"a\nb\nc\n").unwrap();
        run_git(&path, &["add", "f.txt"]);
        run_git(&path, &["commit", "-qm", "c1"]);
        let repo = Repository::open(path.join(".git")).unwrap();
        let c1 = head_oid(&repo);

        std::fs::write(path.join("f.txt"), b"a\nBBB\nc\n").unwrap();
        run_git(&path, &["add", "f.txt"]);
        run_git(&path, &["commit", "-qm", "c2"]);
        let repo = Repository::open(path.join(".git")).unwrap();
        let c2 = head_oid(&repo);

        let lines = blame(&repo, b"f.txt", c2, &BlameOpts::default()).unwrap();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].commit, c1, "line 'a' should be from c1");
        assert_eq!(lines[1].commit, c2, "line 'BBB' should be from c2");
        assert_eq!(lines[2].commit, c1, "line 'c' should be from c1");
    }

    #[test]
    fn blame_skips_unrelated_commits() {
        if !has_git() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let path: PathBuf = dir.path().to_path_buf();
        run_git(&path, &["init", "-q", "-b", "main"]);
        run_git(&path, &["config", "user.email", "t@t"]);
        run_git(&path, &["config", "user.name", "T"]);

        std::fs::write(path.join("a.txt"), b"hello\nworld\n").unwrap();
        run_git(&path, &["add", "a.txt"]);
        run_git(&path, &["commit", "-qm", "c1"]);
        let repo = Repository::open(path.join(".git")).unwrap();
        let c1 = head_oid(&repo);

        std::fs::write(path.join("b.txt"), b"unrelated\n").unwrap();
        run_git(&path, &["add", "b.txt"]);
        run_git(&path, &["commit", "-qm", "c2-unrelated"]);
        let repo = Repository::open(path.join(".git")).unwrap();
        let c3 = head_oid(&repo);
        let _ = c3; // we'll start blame at HEAD

        let lines = blame(&repo, b"a.txt", c3, &BlameOpts::default()).unwrap();
        assert_eq!(lines.len(), 2);
        for ln in &lines {
            assert_eq!(ln.commit, c1, "all of a.txt should blame to c1");
        }
    }

    #[test]
    fn blame_line_range_filters_output() {
        if !has_git() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let path: PathBuf = dir.path().to_path_buf();
        run_git(&path, &["init", "-q", "-b", "main"]);
        run_git(&path, &["config", "user.email", "t@t"]);
        run_git(&path, &["config", "user.name", "T"]);

        std::fs::write(path.join("f.txt"), b"l1\nl2\nl3\nl4\nl5\n").unwrap();
        run_git(&path, &["add", "f.txt"]);
        run_git(&path, &["commit", "-qm", "c1"]);
        let repo = Repository::open(path.join(".git")).unwrap();
        let head = head_oid(&repo);

        let lines = blame(
            &repo,
            b"f.txt",
            head,
            &BlameOpts {
                follow_renames: false,
                line_range: Some((2, 4)),
            },
        )
        .unwrap();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].final_lineno, 2);
        assert_eq!(lines[1].final_lineno, 3);
        assert_eq!(lines[2].final_lineno, 4);
        assert_eq!(lines[0].content, b"l2");
    }

    #[test]
    fn blame_across_rename_with_follow() {
        if !has_git() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let path: PathBuf = dir.path().to_path_buf();
        run_git(&path, &["init", "-q", "-b", "main"]);
        run_git(&path, &["config", "user.email", "t@t"]);
        run_git(&path, &["config", "user.name", "T"]);

        std::fs::write(
            path.join("a.txt"),
            b"L1\nL2\nL3\nL4\nL5\nL6\nL7\nL8\nL9\nL10\n",
        )
        .unwrap();
        run_git(&path, &["add", "a.txt"]);
        run_git(&path, &["commit", "-qm", "c1"]);
        let repo = Repository::open(path.join(".git")).unwrap();
        let c1 = head_oid(&repo);

        // Rename to b.txt and modify one line.
        std::fs::rename(path.join("a.txt"), path.join("b.txt")).unwrap();
        std::fs::write(
            path.join("b.txt"),
            b"L1\nL2\nL3\nL4\nMODIFIED\nL6\nL7\nL8\nL9\nL10\n",
        )
        .unwrap();
        run_git(&path, &["add", "-A"]);
        run_git(&path, &["commit", "-qm", "c2"]);
        let repo = Repository::open(path.join(".git")).unwrap();
        let c2 = head_oid(&repo);

        let lines = blame(
            &repo,
            b"b.txt",
            c2,
            &BlameOpts {
                follow_renames: true,
                line_range: None,
            },
        )
        .unwrap();
        assert_eq!(lines.len(), 10);
        // The unmodified lines should track back to c1.
        for ln in &lines {
            if ln.content == b"MODIFIED" {
                assert_eq!(ln.commit, c2);
            } else {
                assert_eq!(
                    ln.commit,
                    c1,
                    "line {:?} should blame to c1 but blamed to {}",
                    String::from_utf8_lossy(&ln.content),
                    ln.commit
                );
            }
        }
    }

    #[test]
    fn blame_without_follow_attributes_renamed_file_to_rename_commit() {
        if !has_git() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let path: PathBuf = dir.path().to_path_buf();
        run_git(&path, &["init", "-q", "-b", "main"]);
        run_git(&path, &["config", "user.email", "t@t"]);
        run_git(&path, &["config", "user.name", "T"]);

        std::fs::write(path.join("a.txt"), b"X\nY\nZ\n").unwrap();
        run_git(&path, &["add", "a.txt"]);
        run_git(&path, &["commit", "-qm", "c1"]);

        std::fs::rename(path.join("a.txt"), path.join("b.txt")).unwrap();
        run_git(&path, &["add", "-A"]);
        run_git(&path, &["commit", "-qm", "c2-rename"]);
        let repo = Repository::open(path.join(".git")).unwrap();
        let c2 = head_oid(&repo);

        // Without follow_renames, b.txt looks like a brand-new file at c2.
        let lines = blame(&repo, b"b.txt", c2, &BlameOpts::default()).unwrap();
        assert_eq!(lines.len(), 3);
        for ln in &lines {
            assert_eq!(ln.commit, c2, "no follow → all lines attributed to c2");
        }
    }

    #[test]
    fn cross_check_with_git_blame_porcelain() {
        if !has_git() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let path: PathBuf = dir.path().to_path_buf();
        run_git(&path, &["init", "-q", "-b", "main"]);
        run_git(&path, &["config", "user.email", "t@t"]);
        run_git(&path, &["config", "user.name", "T"]);

        std::fs::write(path.join("f.txt"), b"alpha\nbravo\ncharlie\n").unwrap();
        run_git(&path, &["add", "f.txt"]);
        run_git(&path, &["commit", "-qm", "c1"]);

        std::fs::write(path.join("f.txt"), b"alpha\nBRAVO2\ncharlie\n").unwrap();
        run_git(&path, &["add", "f.txt"]);
        run_git(&path, &["commit", "-qm", "c2"]);

        std::fs::write(path.join("f.txt"), b"alpha\nBRAVO2\nCHARLIE3\n").unwrap();
        run_git(&path, &["add", "f.txt"]);
        run_git(&path, &["commit", "-qm", "c3"]);

        // Parse git's porcelain output: every line group starts with
        // "<oid> <orig> <final> [<n>]\n".
        let porcelain = Command::new("git")
            .args(["blame", "--porcelain", "f.txt"])
            .current_dir(&path)
            .output()
            .unwrap();
        assert!(porcelain.status.success());
        let lines: Vec<&[u8]> = porcelain.stdout.split(|&b| b == b'\n').collect();
        // Find the per-line attributions: lines that start with 40 hex chars
        // followed by space.
        let mut git_attribs: Vec<String> = Vec::new();
        for ln in &lines {
            if ln.len() < 41 {
                continue;
            }
            // Must look like "<40-hex> <orig> <final>".
            let head = &ln[..40];
            if head.iter().all(|b| b.is_ascii_hexdigit()) && ln[40] == b' ' {
                git_attribs.push(String::from_utf8_lossy(head).to_string());
            }
        }
        assert_eq!(
            git_attribs.len(),
            3,
            "expected 3 attributions, got {git_attribs:?}"
        );

        let repo = Repository::open(path.join(".git")).unwrap();
        let head = head_oid(&repo);
        let ours = blame(&repo, b"f.txt", head, &BlameOpts::default()).unwrap();
        assert_eq!(ours.len(), 3);
        for i in 0..3 {
            assert_eq!(
                ours[i].commit.to_string(),
                git_attribs[i],
                "line {} mismatch: ours={} git={}",
                i + 1,
                ours[i].commit,
                git_attribs[i]
            );
        }
    }

    #[test]
    fn blame_format_line_basic_shape() {
        // Build a BlameLine by hand and make sure the formatter doesn't panic
        // and produces the expected shape.
        let oid = ObjectId::parse_hex(
            crate::hash::HashKind::Sha1,
            "03e73e25abcdef1234567890123456789012abcd",
        )
        .unwrap();
        let line = BlameLine {
            commit: oid,
            author: "Daisy".to_string(),
            author_email: "d@x".to_string(),
            author_time: 1700000000,
            author_tz_offset: -240,
            content: b"fn main() {".to_vec(),
            final_lineno: 1,
            origin_lineno: 1,
            origin_path: b"f.rs".to_vec(),
        };
        let out = format_line(&line, 3);
        let s = String::from_utf8_lossy(&out);
        assert!(s.starts_with("03e73e25 (Daisy "));
        assert!(s.contains("fn main() {"));
        assert!(s.ends_with("\n"));
    }
}
