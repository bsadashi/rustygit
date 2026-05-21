//! Three-way status: HEAD tree vs. index vs. working tree.
//!
//! The output is byte-compatible with `git status --porcelain` (porcelain
//! v1, the stable machine-readable format):
//!
//! ```text
//! XY <path>
//! ```
//!
//! `X` ("index column") is what changed between HEAD and the index. `Y`
//! ("worktree column") is what changed between the index and the working
//! tree. `??` is untracked, `!!` is ignored. Renames and copies aren't
//! detected in M4 (we never emit `R` or `C`).
//!
//! This module is intentionally a pure function over a `&Repository` plus
//! some filesystem reads — no mutation, no shared state, no caches that
//! outlive the call. That keeps `cargo test` deterministic.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::commit::{Commit, CommitError};
use crate::hash::{HashError, HashKind, ObjectId};
use crate::index::{Index, IndexEntry, IndexError};
use crate::object::{ObjectKind, RawObject};
use crate::odb::OdbError;
use crate::refs::{FullName, RefError, RefTarget, Reference};
use crate::repo::Repository;
use crate::tree::{FileMode, Tree, TreeError};

/// What changed between HEAD's tree and the index for a given path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StageState {
    Unchanged,
    Added,
    Modified,
    Deleted,
    TypeChanged,
    /// Renamed from another path (deferred until rename detection lands).
    Renamed,
    /// Copied from another path (deferred until rename detection lands).
    Copied,
}

/// What changed between the index and the working tree for a given path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorktreeState {
    Unchanged,
    Modified,
    Deleted,
    TypeChanged,
    Untracked,
    Ignored,
    Conflicted,
}

/// One row of the porcelain output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusEntry {
    pub path: Vec<u8>,
    pub index_state: StageState,
    pub worktree_state: WorktreeState,
    /// Source path for renames/copies. Always `None` in M4.
    pub orig_path: Option<Vec<u8>>,
}

/// The full status of the working tree.
#[derive(Debug, Clone)]
pub struct StatusReport {
    /// Entries sorted by path bytes ascending.
    pub entries: Vec<StatusEntry>,
    /// Current branch (when HEAD is symbolic). `None` if detached.
    pub branch: Option<FullName>,
    /// HEAD's commit OID. `None` on a fresh repo before the first commit.
    pub head_commit: Option<ObjectId>,
}

#[derive(Error, Debug)]
pub enum StatusError {
    #[error("io error on {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error(transparent)]
    Ref(#[from] RefError),
    #[error(transparent)]
    Odb(#[from] OdbError),
    #[error(transparent)]
    Tree(#[from] TreeError),
    #[error(transparent)]
    Commit(#[from] CommitError),
    #[error(transparent)]
    Index(#[from] IndexError),
    #[error(transparent)]
    Hash(#[from] HashError),
    #[error("HEAD points at {0} but it is not a commit")]
    HeadNotCommit(ObjectId),
    /// An index path's bytes are not valid UTF-8 on a non-Unix host. See
    /// [`crate::unpack_trees::UnpackError::PathEncodingError`]; same posture.
    #[error("non-UTF-8 index path during {op}: {}", crate::cli::win_paths::format_path_bytes(.bytes))]
    PathEncodingError { bytes: Vec<u8>, op: String },
}

/// Compute the working-tree status.
pub fn status(repo: &Repository) -> Result<StatusReport, StatusError> {
    // 1. Resolve HEAD.
    let (branch, head_commit) = resolve_head(repo)?;

    // 2. Flatten HEAD tree.
    let head_entries = match head_commit {
        Some(oid) => load_head_tree(repo, oid)?,
        None => BTreeMap::new(),
    };

    // 3. Read the index.
    let index = Index::read(repo)?;

    // 4. Group entries by path: stage 0 vs. conflicted (1/2/3).
    let mut indexed: BTreeMap<Vec<u8>, &IndexEntry> = BTreeMap::new();
    let mut conflicted: BTreeMap<Vec<u8>, ()> = BTreeMap::new();
    for ent in &index.entries {
        if ent.stage == 0 {
            indexed.insert(ent.path.clone(), ent);
        } else {
            conflicted.insert(ent.path.clone(), ());
        }
    }

    let mut entries: Vec<StatusEntry> = Vec::new();

    // 5. Diff HEAD tree against the index, then index against the worktree.
    //    Iterate the union of HEAD-tree paths and index paths.
    let union_paths: BTreeMap<Vec<u8>, ()> = head_entries
        .keys()
        .chain(indexed.keys())
        .map(|p| (p.clone(), ()))
        .collect();

    for path in union_paths.keys() {
        let head_pair = head_entries.get(path);
        let idx_entry = indexed.get(path);

        // Conflicted entries override everything else.
        if conflicted.contains_key(path) {
            entries.push(StatusEntry {
                path: path.clone(),
                index_state: StageState::Unchanged,
                worktree_state: WorktreeState::Conflicted,
                orig_path: None,
            });
            continue;
        }

        let index_state = compute_index_state(head_pair, idx_entry);
        let worktree_state = match idx_entry {
            Some(ent) => compute_worktree_state(repo, ent)?,
            None => WorktreeState::Unchanged,
        };

        // Skip rows where both sides are unchanged.
        if index_state == StageState::Unchanged && worktree_state == WorktreeState::Unchanged {
            continue;
        }

        entries.push(StatusEntry {
            path: path.clone(),
            index_state,
            worktree_state,
            orig_path: None,
        });
    }

    // 6. Walk the workdir for files not in the index. Emit them as untracked,
    //    consulting the gitignore stack so files matched by `.gitignore` /
    //    `.git/info/exclude` are filtered out (matching `git status --porcelain`'s
    //    default behavior, which omits ignored files entirely).
    let ignore = build_ignore_stack(repo);
    walk_untracked(repo, &indexed, &ignore, &mut entries)?;

    // Sort by path bytes ascending — porcelain v1's documented order.
    entries.sort_by(|a, b| a.path.cmp(&b.path));

    Ok(StatusReport {
        entries,
        branch,
        head_commit,
    })
}

/// Resolve HEAD into `(branch, commit)`.
///
/// - Detached HEAD → `(None, Some(commit_oid))`.
/// - Symbolic HEAD pointing at a branch with a commit → `(Some(branch), Some(commit))`.
/// - Symbolic HEAD pointing at a branch with no commit yet (unborn) → `(Some(branch), None)`.
/// - HEAD missing entirely → `(None, None)`.
fn resolve_head(repo: &Repository) -> Result<(Option<FullName>, Option<ObjectId>), StatusError> {
    let head_name = FullName::new("HEAD").map_err(RefError::from)?;
    let head_ref: Option<Reference> = repo.refs().read(&head_name)?;
    let Some(head_ref) = head_ref else {
        return Ok((None, None));
    };
    match head_ref.target {
        RefTarget::Direct(oid) => Ok((None, Some(oid))),
        RefTarget::Symbolic(branch) => {
            let resolved = repo.refs().read(&branch)?;
            match resolved {
                Some(Reference {
                    target: RefTarget::Direct(oid),
                    ..
                }) => Ok((Some(branch), Some(oid))),
                Some(Reference {
                    target: RefTarget::Symbolic(_),
                    ..
                }) => {
                    // Chained symbolic: walk it. Reuse RefTarget::resolve for safety.
                    match RefTarget::resolve(repo.refs(), &branch)? {
                        Some((_, oid)) => Ok((Some(branch), Some(oid))),
                        None => Ok((Some(branch), None)),
                    }
                }
                None => Ok((Some(branch), None)),
            }
        }
    }
}

/// Read HEAD's commit, then walk its tree into a flat map.
fn load_head_tree(
    repo: &Repository,
    commit_oid: ObjectId,
) -> Result<BTreeMap<Vec<u8>, (FileMode, ObjectId)>, StatusError> {
    let obj = repo.odb().read(&commit_oid)?;
    if obj.kind != ObjectKind::Commit {
        return Err(StatusError::HeadNotCommit(commit_oid));
    }
    let commit = Commit::parse(&obj.data, repo.hash_kind())?;
    let mut out = BTreeMap::new();
    flatten_tree(repo, &commit.tree, &mut Vec::new(), &mut out)?;
    Ok(out)
}

/// Recursively walk a tree, prepending `prefix` (no trailing slash on entry).
fn flatten_tree(
    repo: &Repository,
    tree_oid: &ObjectId,
    prefix: &mut Vec<u8>,
    out: &mut BTreeMap<Vec<u8>, (FileMode, ObjectId)>,
) -> Result<(), StatusError> {
    let raw = repo.odb().read(tree_oid)?;
    if raw.kind != ObjectKind::Tree {
        // Defensive — caller should have checked.
        return Ok(());
    }
    let tree = Tree::parse(&raw.data, repo.hash_kind())?;
    for entry in &tree.entries {
        let saved_len = prefix.len();
        if !prefix.is_empty() {
            prefix.push(b'/');
        }
        prefix.extend_from_slice(&entry.name);
        if entry.mode.is_tree() {
            flatten_tree(repo, &entry.oid, prefix, out)?;
        } else {
            out.insert(prefix.clone(), (entry.mode, entry.oid));
        }
        prefix.truncate(saved_len);
    }
    Ok(())
}

/// Compute the X column based on HEAD-tree vs. index entries.
fn compute_index_state(
    head_pair: Option<&(FileMode, ObjectId)>,
    idx_entry: Option<&&IndexEntry>,
) -> StageState {
    match (head_pair, idx_entry) {
        (None, None) => StageState::Unchanged, // can't happen given the union iteration
        (None, Some(_)) => StageState::Added,
        (Some(_), None) => StageState::Deleted,
        (Some((head_mode, head_oid)), Some(ent)) => {
            // First compare oids — that's the cheapest signal.
            let oid_eq = ent.oid == *head_oid;
            // Then mode. The index stores a 32-bit POSIX mode; HEAD stores a
            // FileMode. Compare via FileMode::from_index_mode so e.g. 0o100644
            // and 0o100755 differ as Regular vs. Executable.
            let idx_mode = FileMode::from_index_mode(ent.mode).ok();
            let mode_eq = idx_mode.map(|m| m == *head_mode).unwrap_or(false);
            if oid_eq && mode_eq {
                StageState::Unchanged
            } else if !oid_eq && mode_eq {
                StageState::Modified
            } else {
                // Mode differs.
                let head_is_blob = matches!(head_mode, FileMode::Regular | FileMode::Executable);
                let head_is_link = matches!(head_mode, FileMode::Symlink);
                let idx_is_blob = matches!(
                    idx_mode,
                    Some(FileMode::Regular) | Some(FileMode::Executable)
                );
                let idx_is_link = matches!(idx_mode, Some(FileMode::Symlink));
                let blob_link_swap = (head_is_blob && idx_is_link) || (head_is_link && idx_is_blob);
                if blob_link_swap {
                    StageState::TypeChanged
                } else {
                    // Within "blob" (regular vs executable) git reports M, not T.
                    StageState::Modified
                }
            }
        }
    }
}

/// Compute the Y column for a path that's in the index.
fn compute_worktree_state(
    repo: &Repository,
    ent: &IndexEntry,
) -> Result<WorktreeState, StatusError> {
    let abs = repo
        .workdir()
        .join(bytes_to_relpath_checked(&ent.path, "status")?);
    let metadata = match fs::symlink_metadata(&abs) {
        Ok(m) => m,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(WorktreeState::Deleted),
        Err(e) => {
            return Err(StatusError::Io {
                path: abs,
                source: e,
            });
        }
    };

    let ft = metadata.file_type();
    let stored_mode = FileMode::from_index_mode(ent.mode).ok();

    // Type-changed checks — does the on-disk type match what the index says
    // it should be?
    let on_disk_kind = if ft.is_symlink() {
        FileMode::Symlink
    } else if ft.is_file() {
        // Pick Regular vs Executable on Unix; on non-unix we always say Regular.
        derive_blob_mode(&metadata)
    } else {
        // Directory or other — definitely a type change.
        return Ok(WorktreeState::TypeChanged);
    };

    if let Some(stored) = stored_mode {
        // Symlink ↔ blob is a type change. Regular ↔ Executable is a Modified
        // (mode change) — we report it as Modified so the caller emits ` M`.
        let stored_is_blob = matches!(stored, FileMode::Regular | FileMode::Executable);
        let on_disk_is_blob = matches!(on_disk_kind, FileMode::Regular | FileMode::Executable);
        if stored_is_blob != on_disk_is_blob {
            return Ok(WorktreeState::TypeChanged);
        }
        if stored != on_disk_kind {
            // Same family (both blob), different exec bit.
            return Ok(WorktreeState::Modified);
        }
    }

    // Stat-based fast path: if the stat matches the stored stat, treat the
    // file as Unchanged. This is the same idea as git's `ce_match_stat`.
    if stat_matches_index(&metadata, ent) {
        return Ok(WorktreeState::Unchanged);
    }

    // Stat says something might have changed. Re-hash the content; if the
    // hash matches the index oid, the file is actually clean (an editor saved
    // unchanged bytes, etc.).
    let oid = blob_oid_for_workfile(&abs, &ft, repo.hash_kind())
        .map_err(|source| StatusError::Io { path: abs, source })?;
    if oid == ent.oid {
        Ok(WorktreeState::Unchanged)
    } else {
        Ok(WorktreeState::Modified)
    }
}

/// Heuristic: if every cheap stat field matches what we recorded, declare the
/// file unchanged. Mirrors git's behavior except we don't have the racy-clean
/// trick yet — that requires the index timestamp and a second pass.
fn stat_matches_index(meta: &fs::Metadata, ent: &IndexEntry) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        // Size is the most discriminating cheap signal. Then mtime. We
        // intentionally skip ctime/inode/dev so the function still returns
        // true after a `git stash` round-trip on the same file.
        let size_ok = (meta.size().min(u32::MAX as u64) as u32) == ent.size;
        let mtime_ok =
            (meta.mtime() as u32) == ent.mtime_s && (meta.mtime_nsec() as u32) == ent.mtime_n;
        size_ok && mtime_ok
    }
    #[cfg(not(unix))]
    {
        let _ = (meta, ent);
        false
    }
}

#[cfg(unix)]
fn derive_blob_mode(meta: &fs::Metadata) -> FileMode {
    use std::os::unix::fs::PermissionsExt;
    if meta.permissions().mode() & 0o111 != 0 {
        FileMode::Executable
    } else {
        FileMode::Regular
    }
}

#[cfg(not(unix))]
fn derive_blob_mode(_meta: &fs::Metadata) -> FileMode {
    FileMode::Regular
}

/// Hash the on-disk content as git would, returning the blob oid.
fn blob_oid_for_workfile(
    abs: &Path,
    ft: &fs::FileType,
    hash_kind: HashKind,
) -> Result<ObjectId, io::Error> {
    let payload = if ft.is_symlink() {
        fs::read_link(abs)?
            .as_os_str()
            .to_string_lossy()
            .into_owned()
            .into_bytes()
    } else {
        fs::read(abs)?
    };
    let blob = RawObject::new(ObjectKind::Blob, payload);
    Ok(blob.oid(hash_kind))
}

/// Build the seed IgnoreStack with the bottom layers that apply repo-wide:
/// `.git/info/exclude` and the root `.gitignore`. Per-directory `.gitignore`
/// files are pushed and popped as the walker descends/ascends.
fn build_ignore_stack(repo: &Repository) -> crate::ignore::IgnoreStack {
    let mut stack = crate::ignore::IgnoreStack::empty();
    // .git/info/exclude is the lowest-priority layer.
    let info_exclude = repo.gitdir().join("info").join("exclude");
    if let Ok(bytes) = std::fs::read(&info_exclude) {
        stack.push_file(&bytes, b"");
    }
    let root_ignore = repo.workdir().join(".gitignore");
    if let Ok(bytes) = std::fs::read(&root_ignore) {
        stack.push_file(&bytes, b"");
    }
    stack
}

fn walk_untracked(
    repo: &Repository,
    indexed: &BTreeMap<Vec<u8>, &IndexEntry>,
    ignore: &crate::ignore::IgnoreStack,
    out: &mut Vec<StatusEntry>,
) -> Result<(), StatusError> {
    // We mutate a clone of the seed stack as we recurse. Each directory may
    // push its own `.gitignore` layer; on the way back up, we pop the layers
    // we pushed (tracking how many we added).
    let mut stack = ignore.clone();
    let workdir = repo.workdir().to_path_buf();
    walk_dir(repo, &workdir, b"", indexed, &mut stack, out)
}

fn walk_dir(
    repo: &Repository,
    dir: &std::path::Path,
    rel: &[u8],
    indexed: &BTreeMap<Vec<u8>, &IndexEntry>,
    ignore: &mut crate::ignore::IgnoreStack,
    out: &mut Vec<StatusEntry>,
) -> Result<(), StatusError> {
    // Per-directory `.gitignore` — push BEFORE we iterate so files in this
    // dir see the layer.  Skip the root's `.gitignore` (already pushed by
    // `build_ignore_stack`) by checking whether `rel` is empty.
    let pushed_here = if !rel.is_empty() {
        let nested = dir.join(".gitignore");
        match std::fs::read(&nested) {
            Ok(bytes) => {
                ignore.push_file(&bytes, rel);
                1
            }
            Err(_) => 0,
        }
    } else {
        0
    };

    let result = walk_dir_inner(repo, dir, rel, indexed, ignore, out);

    // Pop our layers no matter what the inner call returned.
    for _ in 0..pushed_here {
        ignore.pop_layer();
    }
    result
}

fn walk_dir_inner(
    repo: &Repository,
    dir: &std::path::Path,
    rel: &[u8],
    indexed: &BTreeMap<Vec<u8>, &IndexEntry>,
    ignore: &mut crate::ignore::IgnoreStack,
    out: &mut Vec<StatusEntry>,
) -> Result<(), StatusError> {
    let entries = match fs::read_dir(dir) {
        Ok(it) => it,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(source) => {
            return Err(StatusError::Io {
                path: dir.to_path_buf(),
                source,
            });
        }
    };
    for ent in entries {
        let ent = ent.map_err(|source| StatusError::Io {
            path: dir.to_path_buf(),
            source,
        })?;
        let name = ent.file_name();
        if name == ".git" {
            continue;
        }
        let ft = match ent.file_type() {
            Ok(t) => t,
            Err(source) => {
                return Err(StatusError::Io {
                    path: ent.path(),
                    source,
                });
            }
        };
        let mut child_rel = rel.to_vec();
        if !child_rel.is_empty() {
            child_rel.push(b'/');
        }
        child_rel.extend_from_slice(&os_str_bytes(&name));

        if ft.is_dir() {
            if ignore.is_ignored(&child_rel, true) {
                continue;
            }
            walk_dir(repo, &ent.path(), &child_rel, indexed, ignore, out)?;
            continue;
        }
        if indexed.contains_key(&child_rel) {
            continue;
        }
        if ignore.is_ignored(&child_rel, false) {
            continue;
        }
        out.push(StatusEntry {
            path: child_rel,
            index_state: StageState::Unchanged,
            worktree_state: WorktreeState::Untracked,
            orig_path: None,
        });
    }
    Ok(())
}

#[cfg(unix)]
fn os_str_bytes(s: &std::ffi::OsStr) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    s.as_bytes().to_vec()
}

#[cfg(not(unix))]
fn os_str_bytes(s: &std::ffi::OsStr) -> Vec<u8> {
    s.to_string_lossy().into_owned().into_bytes()
}

fn bytes_to_relpath(b: &[u8]) -> PathBuf {
    #[cfg(unix)]
    {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;
        PathBuf::from(OsStr::from_bytes(b))
    }
    #[cfg(not(unix))]
    {
        PathBuf::from(String::from_utf8_lossy(b).into_owned())
    }
}

/// Strict variant that refuses non-UTF-8 names on non-Unix platforms.
/// Identical to `bytes_to_relpath` on Unix. See
/// [`StatusError::PathEncodingError`] for the failure mode.
fn bytes_to_relpath_checked(b: &[u8], op: &str) -> Result<PathBuf, StatusError> {
    #[cfg(unix)]
    {
        let _ = op;
        Ok(bytes_to_relpath(b))
    }
    #[cfg(not(unix))]
    {
        match std::str::from_utf8(b) {
            Ok(s) => Ok(PathBuf::from(s)),
            Err(_) => Err(StatusError::PathEncodingError {
                bytes: b.to_vec(),
                op: op.to_string(),
            }),
        }
    }
}

// -----------------------------------------------------------------------
// Porcelain v1 formatter
// -----------------------------------------------------------------------

/// Render [`StatusReport`] as porcelain v1 lines.
pub struct PorcelainV1<'a> {
    pub report: &'a StatusReport,
}

impl<'a> PorcelainV1<'a> {
    pub fn new(report: &'a StatusReport) -> Self {
        Self { report }
    }

    /// Write all lines into `buf`.
    pub fn write<W: io::Write>(&self, buf: &mut W) -> io::Result<()> {
        for entry in &self.report.entries {
            write_entry(buf, entry)?;
        }
        Ok(())
    }

    /// Convenience: serialize to a `Vec<u8>`.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut v = Vec::new();
        self.write(&mut v).expect("Vec write never fails");
        v
    }
}

fn write_entry<W: io::Write>(buf: &mut W, entry: &StatusEntry) -> io::Result<()> {
    let (x, y) = match (entry.worktree_state, entry.index_state) {
        (WorktreeState::Untracked, _) => (b'?', b'?'),
        (WorktreeState::Ignored, _) => (b'!', b'!'),
        (WorktreeState::Conflicted, _) => (b'U', b'U'),
        (y, x) => (state_to_x(x), worktree_to_y(y)),
    };
    buf.write_all(&[x, y, b' '])?;
    write_porcelain_path(buf, &entry.path)?;
    if let Some(orig) = &entry.orig_path {
        // Renames/copies use "<dst>\0<src>" in -z mode but " <dst> -> <src>"
        // in regular mode. M4 never produces these, but support the
        // formatting for completeness.
        buf.write_all(b" -> ")?;
        write_porcelain_path(buf, orig)?;
    }
    buf.write_all(b"\n")?;
    Ok(())
}

fn state_to_x(s: StageState) -> u8 {
    match s {
        StageState::Unchanged => b' ',
        StageState::Added => b'A',
        StageState::Modified => b'M',
        StageState::Deleted => b'D',
        StageState::TypeChanged => b'T',
        StageState::Renamed => b'R',
        StageState::Copied => b'C',
    }
}

fn worktree_to_y(s: WorktreeState) -> u8 {
    match s {
        WorktreeState::Unchanged => b' ',
        WorktreeState::Modified => b'M',
        WorktreeState::Deleted => b'D',
        WorktreeState::TypeChanged => b'T',
        WorktreeState::Untracked => b'?',
        WorktreeState::Ignored => b'!',
        WorktreeState::Conflicted => b'U',
    }
}

/// Apply git's default path quoting (matching `core.quotePath = true`).
///
/// Paths with whitespace, backslashes, double quotes, control bytes, or
/// high-bit bytes are wrapped in `"…"` and the body is C-style escaped.
/// Everything else is written verbatim.
fn write_porcelain_path<W: io::Write>(buf: &mut W, path: &[u8]) -> io::Result<()> {
    if needs_quote(path) {
        write_c_quoted(buf, path)
    } else {
        buf.write_all(path)
    }
}

fn needs_quote(path: &[u8]) -> bool {
    path.iter().any(|&b| {
        matches!(
            b,
            b'"' | b'\\'
                | 0..=0x1f
                | 0x7f
                | 0x80..=0xff
        )
    })
}

fn write_c_quoted<W: io::Write>(buf: &mut W, path: &[u8]) -> io::Result<()> {
    buf.write_all(b"\"")?;
    for &b in path {
        match b {
            b'"' => buf.write_all(b"\\\"")?,
            b'\\' => buf.write_all(b"\\\\")?,
            0x07 => buf.write_all(b"\\a")?,
            0x08 => buf.write_all(b"\\b")?,
            0x09 => buf.write_all(b"\\t")?,
            0x0a => buf.write_all(b"\\n")?,
            0x0b => buf.write_all(b"\\v")?,
            0x0c => buf.write_all(b"\\f")?,
            0x0d => buf.write_all(b"\\r")?,
            // Other control chars and high-bit bytes get \xxx octal.
            0..=0x1f | 0x7f | 0x80..=0xff => {
                let s = format!("\\{:03o}", b);
                buf.write_all(s.as_bytes())?;
            }
            _ => buf.write_all(&[b])?,
        }
    }
    buf.write_all(b"\"")?;
    Ok(())
}

// -----------------------------------------------------------------------
// Human formatter ("git status" with no flag — `git status --long`)
// -----------------------------------------------------------------------

/// Render [`StatusReport`] in the verbose human-readable form that `git status`
/// emits by default. The shape:
///
/// ```text
/// On branch <name>
/// <upstream-tracking-line-if-any>
///
/// Changes to be committed:
///   (use "git restore --staged <file>..." to unstage)
///         modified:   <path>
///
/// Changes not staged for commit:
///   (use "git add <file>..." to update what will be committed)
///   (use "git restore <file>..." to discard changes in working directory)
///         modified:   <path>
///
/// Untracked files:
///   (use "git add <file>..." to include in what will be committed)
///         <path>
///
/// no changes added to commit (use "git add" and/or "git commit -a")
/// ```
///
/// The empty-tree footer ("nothing to commit, working tree clean") and the
/// "nothing added to commit but untracked files present" variant are handled
/// by [`Human::write`].
///
/// Upstream tracking: we stub the "Your branch is up to date with…" line if
/// `refs/remotes/origin/<branch>` resolves to the same oid as HEAD. Real
/// ahead/behind counts land with networking in a later milestone.
pub struct Human<'a> {
    report: &'a StatusReport,
    upstream_line: Option<String>,
}

impl<'a> Human<'a> {
    pub fn new(report: &'a StatusReport) -> Self {
        Self {
            report,
            upstream_line: None,
        }
    }

    /// Plug in the upstream-tracking line (e.g. `Your branch is up to date with 'origin/main'.`).
    /// Pass `None` to suppress it.
    pub fn with_upstream(mut self, line: Option<String>) -> Self {
        self.upstream_line = line;
        self
    }

    /// Convenience: build the upstream line from the repository if possible.
    /// Emits `Your branch is up to date with 'origin/<branch>'.` when the
    /// matching `refs/remotes/origin/<branch>` exists with the same oid as HEAD;
    /// otherwise no line. We don't yet compute ahead/behind counts.
    pub fn with_upstream_from(self, repo: &Repository) -> Self {
        let line = compute_upstream_line(repo, self.report);
        self.with_upstream(line)
    }

    pub fn write<W: io::Write>(&self, buf: &mut W) -> io::Result<()> {
        // 1. Header: branch / detached / unborn. Returns whether the header
        //    already ended with a blank line — when it did, we must NOT emit
        //    another blank before the first section.
        let header_trailing_blank =
            write_human_header(buf, self.report, self.upstream_line.as_deref())?;

        // 2. Bucket entries into sections.
        let mut staged: Vec<&StatusEntry> = Vec::new();
        let mut unstaged: Vec<&StatusEntry> = Vec::new();
        let mut untracked: Vec<&StatusEntry> = Vec::new();
        let mut conflicted: Vec<&StatusEntry> = Vec::new();
        for entry in &self.report.entries {
            if entry.worktree_state == WorktreeState::Conflicted {
                conflicted.push(entry);
                continue;
            }
            if entry.worktree_state == WorktreeState::Untracked {
                untracked.push(entry);
                continue;
            }
            if entry.worktree_state == WorktreeState::Ignored {
                continue; // human form omits ignored by default
            }
            if entry.index_state != StageState::Unchanged {
                staged.push(entry);
            }
            if entry.worktree_state != WorktreeState::Unchanged {
                unstaged.push(entry);
            }
        }

        let unborn = self.report.head_commit.is_none() && self.report.branch.is_some();

        // 3. Sections in git's documented order.
        //    The blank-line policy: git emits a blank line BEFORE each
        //    section, EXCEPT skip the leading blank before the FIRST section
        //    when the header didn't already trail with one.
        let mut first_section = true;
        let leading_blank = |buf: &mut W, first: &mut bool| -> io::Result<()> {
            if *first {
                if header_trailing_blank {
                    writeln!(buf)?;
                }
                *first = false;
            } else {
                writeln!(buf)?;
            }
            Ok(())
        };

        if !staged.is_empty() {
            leading_blank(buf, &mut first_section)?;
            writeln!(buf, "Changes to be committed:")?;
            if unborn {
                writeln!(buf, "  (use \"git rm --cached <file>...\" to unstage)")?;
            } else {
                writeln!(buf, "  (use \"git restore --staged <file>...\" to unstage)")?;
            }
            for entry in &staged {
                write_human_indexed_entry(buf, entry)?;
            }
        }

        if !conflicted.is_empty() {
            leading_blank(buf, &mut first_section)?;
            writeln!(buf, "Unmerged paths:")?;
            writeln!(buf, "  (use \"git add <file>...\" to mark resolution)")?;
            for entry in &conflicted {
                buf.write_all(b"\tboth modified:   ")?;
                write_porcelain_path(buf, &entry.path)?;
                buf.write_all(b"\n")?;
            }
        }

        if !unstaged.is_empty() {
            leading_blank(buf, &mut first_section)?;
            writeln!(buf, "Changes not staged for commit:")?;
            writeln!(
                buf,
                "  (use \"git add <file>...\" to update what will be committed)"
            )?;
            writeln!(
                buf,
                "  (use \"git restore <file>...\" to discard changes in working directory)"
            )?;
            for entry in &unstaged {
                write_human_unstaged_entry(buf, entry)?;
            }
        }

        if !untracked.is_empty() {
            leading_blank(buf, &mut first_section)?;
            writeln!(buf, "Untracked files:")?;
            writeln!(
                buf,
                "  (use \"git add <file>...\" to include in what will be committed)"
            )?;
            for entry in &untracked {
                buf.write_all(b"\t")?;
                write_porcelain_path(buf, &entry.path)?;
                buf.write_all(b"\n")?;
            }
        }

        // 4. Footer.
        let any_staged = !staged.is_empty();
        let any_unstaged = !unstaged.is_empty() || !conflicted.is_empty();
        let any_untracked = !untracked.is_empty();
        write_human_footer(
            buf,
            unborn,
            any_staged,
            any_unstaged,
            any_untracked,
            header_trailing_blank,
        )?;
        Ok(())
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut v = Vec::new();
        self.write(&mut v).expect("Vec write never fails");
        v
    }
}

/// Write the status header. Returns `true` if the header itself contains
/// multiple lines (an unborn-branch "No commits yet" or an upstream-tracking
/// line) — meaning the caller must NOT emit an extra blank line before the
/// first section.
fn write_human_header<W: io::Write>(
    buf: &mut W,
    report: &StatusReport,
    upstream_line: Option<&str>,
) -> io::Result<bool> {
    let mut multiline = false;
    match (&report.branch, &report.head_commit) {
        (Some(branch), _) => {
            // Strip the standard prefix so we print "main" not "refs/heads/main".
            let short = branch
                .as_str()
                .strip_prefix("refs/heads/")
                .unwrap_or(branch.as_str());
            writeln!(buf, "On branch {short}")?;
            if report.head_commit.is_none() {
                writeln!(buf)?;
                writeln!(buf, "No commits yet")?;
                multiline = true;
            } else if let Some(line) = upstream_line {
                writeln!(buf, "{line}")?;
                multiline = true;
            }
        }
        (None, Some(oid)) => {
            // Detached HEAD. git's wording: `HEAD detached at <short-oid>`.
            writeln!(buf, "HEAD detached at {}", oid.short_hex(7))?;
        }
        (None, None) => {
            // HEAD missing entirely — exceedingly rare. Match git's
            // "No commits yet" for a freshly-stamped gitdir.
            writeln!(buf, "On branch main")?;
            writeln!(buf)?;
            writeln!(buf, "No commits yet")?;
            multiline = true;
        }
    }
    Ok(multiline)
}

/// Render an entry on the index side ("Changes to be committed").
fn write_human_indexed_entry<W: io::Write>(buf: &mut W, entry: &StatusEntry) -> io::Result<()> {
    let label = stage_state_label(entry.index_state);
    buf.write_all(b"\t")?;
    write_human_label(buf, label)?;
    write_porcelain_path(buf, &entry.path)?;
    buf.write_all(b"\n")?;
    Ok(())
}

/// Render an entry on the worktree side ("Changes not staged for commit").
fn write_human_unstaged_entry<W: io::Write>(buf: &mut W, entry: &StatusEntry) -> io::Result<()> {
    let label = worktree_state_label(entry.worktree_state);
    buf.write_all(b"\t")?;
    write_human_label(buf, label)?;
    write_porcelain_path(buf, &entry.path)?;
    buf.write_all(b"\n")?;
    Ok(())
}

/// Pad the human label out to 12 chars (matches git's `wt-status.c`).
fn write_human_label<W: io::Write>(buf: &mut W, label: &str) -> io::Result<()> {
    // git uses `%-12s` for the label (e.g. "modified:   " = 12 chars).
    write!(buf, "{label:<12}")
}

fn stage_state_label(s: StageState) -> &'static str {
    match s {
        StageState::Added => "new file:",
        StageState::Modified => "modified:",
        StageState::Deleted => "deleted:",
        StageState::TypeChanged => "typechange:",
        StageState::Renamed => "renamed:",
        StageState::Copied => "copied:",
        // Unchanged shouldn't reach the labeled writer, but keep a safe fallback.
        StageState::Unchanged => "",
    }
}

fn worktree_state_label(s: WorktreeState) -> &'static str {
    match s {
        WorktreeState::Modified => "modified:",
        WorktreeState::Deleted => "deleted:",
        WorktreeState::TypeChanged => "typechange:",
        // The other variants don't get rendered in this section.
        _ => "",
    }
}

fn write_human_footer<W: io::Write>(
    buf: &mut W,
    unborn: bool,
    any_staged: bool,
    any_unstaged: bool,
    any_untracked: bool,
    header_trailing_blank: bool,
) -> io::Result<()> {
    // For the "all clean" cases there are no sections; emit a blank line
    // before the footer ONLY if the header was multiline (unborn or upstream
    // tracking). Otherwise the footer follows the header directly.
    let leading = if header_trailing_blank { "\n" } else { "" };

    if unborn && !any_staged && !any_unstaged && !any_untracked {
        writeln!(
            buf,
            "{leading}nothing to commit (create/copy files and use \"git add\" to track)"
        )?;
        return Ok(());
    }
    if !any_staged && !any_unstaged && !any_untracked {
        writeln!(buf, "{leading}nothing to commit, working tree clean")?;
        return Ok(());
    }
    // Once any section has been emitted, the leading blank line BEFORE the
    // footer is always present (separating the last section from the footer).
    if !any_staged && !any_unstaged && any_untracked {
        writeln!(
            buf,
            "\nnothing added to commit but untracked files present (use \"git add\" to track)"
        )?;
        return Ok(());
    }
    if !any_staged && any_unstaged {
        writeln!(
            buf,
            "\nno changes added to commit (use \"git add\" and/or \"git commit -a\")"
        )?;
        return Ok(());
    }
    // any_staged is true: git emits no closing footer.
    Ok(())
}

/// Compute the "Your branch is up to date with 'origin/<branch>'." stub line.
/// Returns None when there's no matching remote-tracking ref or we can't read
/// it; ahead/behind divergence reporting is deferred until networking lands.
fn compute_upstream_line(repo: &Repository, report: &StatusReport) -> Option<String> {
    let branch = report.branch.as_ref()?;
    let short = branch.as_str().strip_prefix("refs/heads/")?;
    let head_oid = report.head_commit.as_ref()?;

    let remote_name = format!("refs/remotes/origin/{short}");
    let remote_full = FullName::new(remote_name).ok()?;
    let remote_ref = repo.refs().read(&remote_full).ok()??;
    let remote_oid = match remote_ref.target {
        RefTarget::Direct(o) => o,
        // Resolve a symbolic remote-tracking ref one level if we hit one.
        RefTarget::Symbolic(name) => match repo.refs().read(&name).ok()?? {
            Reference {
                target: RefTarget::Direct(o),
                ..
            } => o,
            _ => return None,
        },
    };
    if &remote_oid == head_oid {
        Some(format!("Your branch is up to date with 'origin/{short}'."))
    } else {
        // Divergence detected but ahead/behind not yet wired — match git's
        // wording when it can't compute counts.
        Some(format!(
            "Your branch and 'origin/{short}' have diverged,\nand have different commits each, respectively."
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::commit_tree::create_commit;
    use crate::cli::write_tree::build_tree_from_index;
    use crate::index::Index;
    use crate::object::{ObjectKind, RawObject};
    use crate::refs::{ExpectedOldValue, NewValue, RefTarget, ReflogMessage};
    use std::process::Command;
    use tempfile::tempdir;

    /// Initialize a repo via system `git init`. We avoid `crate::cli::init`
    /// because the tests run in parallel and that command does a filesystem
    /// probe (writes a `.rustygit-probe` file) that races. `git init` alone
    /// is enough — we don't need byte-equal layout for status tests.
    fn init_rg_repo() -> tempfile::TempDir {
        let tmp = tempdir().unwrap();
        // Lay out .git ourselves to avoid any cwd or shell dependency.
        let gitdir = tmp.path().join(".git");
        for sub in [
            "",
            "objects",
            "objects/info",
            "objects/pack",
            "refs",
            "refs/heads",
            "refs/tags",
            "info",
            "hooks",
        ] {
            std::fs::create_dir_all(gitdir.join(sub)).unwrap();
        }
        std::fs::write(gitdir.join("HEAD"), b"ref: refs/heads/main\n").unwrap();
        std::fs::write(
            gitdir.join("config"),
            b"[core]\n\trepositoryformatversion = 0\n\tfilemode = false\n\tbare = false\n\tlogallrefupdates = true\n",
        )
        .unwrap();
        tmp
    }

    /// Stage one file (or symlink). Cwd-free version of `cli::add::stage_one`.
    fn rg_add_one(repo: &Repository, rel_path: &str) {
        let abs = repo.workdir().join(rel_path);
        let metadata = std::fs::symlink_metadata(&abs).unwrap();
        let mode = derive_blob_mode(&metadata);
        let mode = if metadata.file_type().is_symlink() {
            FileMode::Symlink
        } else {
            mode
        };
        let payload = if metadata.file_type().is_symlink() {
            std::fs::read_link(&abs)
                .unwrap()
                .as_os_str()
                .to_string_lossy()
                .into_owned()
                .into_bytes()
        } else {
            std::fs::read(&abs).unwrap()
        };
        let blob = RawObject::new(ObjectKind::Blob, payload);
        let oid = repo.odb().write(&blob).unwrap();

        let stat = stat_for_test(&metadata);
        let path_bytes = rel_path.as_bytes().to_vec();
        let entry = IndexEntry {
            ctime_s: stat.0,
            ctime_n: stat.1,
            mtime_s: stat.2,
            mtime_n: stat.3,
            dev: stat.4,
            ino: stat.5,
            mode: mode.to_index_mode(),
            uid: stat.6,
            gid: stat.7,
            size: stat.8,
            oid,
            flags: encode_flags(path_bytes.len()),
            path: path_bytes,
            stage: 0,
            assume_valid: false,
            extended: false,
            extended_flags: 0,
        };
        let mut idx = Index::read(repo).unwrap();
        idx.upsert(entry);
        idx.write(repo).unwrap();
    }

    fn encode_flags(name_len: usize) -> u16 {
        (name_len.min(0x0FFF) as u16) & 0x0FFF
    }

    #[cfg(unix)]
    fn stat_for_test(meta: &std::fs::Metadata) -> (u32, u32, u32, u32, u32, u32, u32, u32, u32) {
        use std::os::unix::fs::MetadataExt;
        (
            meta.ctime() as u32,
            meta.ctime_nsec() as u32,
            meta.mtime() as u32,
            meta.mtime_nsec() as u32,
            meta.dev() as u32,
            meta.ino() as u32,
            meta.uid(),
            meta.gid(),
            meta.size().min(u32::MAX as u64) as u32,
        )
    }

    #[cfg(not(unix))]
    fn stat_for_test(_meta: &std::fs::Metadata) -> (u32, u32, u32, u32, u32, u32, u32, u32, u32) {
        (0, 0, 0, 0, 0, 0, 0, 0, 0)
    }

    /// Cwd-free version of `cli::commit::run`. Builds a tree from the index
    /// and creates a commit pointing at it, updating `refs/heads/main`.
    fn rg_commit(repo: &Repository, msg: &str) {
        // Pin the identity env vars (process-wide; tests don't fight over
        // these because we always set them to the same thing).
        std::env::set_var("GIT_AUTHOR_NAME", "Tester");
        std::env::set_var("GIT_AUTHOR_EMAIL", "t@e.x");
        std::env::set_var("GIT_AUTHOR_DATE", "1700000000 +0000");
        std::env::set_var("GIT_COMMITTER_NAME", "Tester");
        std::env::set_var("GIT_COMMITTER_EMAIL", "t@e.x");
        std::env::set_var("GIT_COMMITTER_DATE", "1700000000 +0000");

        let tree_oid = build_tree_from_index(repo).unwrap();

        let branch = FullName::new("refs/heads/main").unwrap();
        let parent_oid = repo
            .refs()
            .read(&branch)
            .unwrap()
            .and_then(|r| match r.target {
                RefTarget::Direct(o) => Some(o),
                RefTarget::Symbolic(_) => None,
            });

        let parents: Vec<String> = parent_oid.iter().map(|o| o.to_string()).collect();
        let parent_refs: Vec<&str> = parents.iter().map(String::as_str).collect();
        let commit_oid = create_commit(repo, &tree_oid.to_string(), &parent_refs, msg).unwrap();

        let expected = match parent_oid {
            Some(o) => ExpectedOldValue::Direct(o),
            None => ExpectedOldValue::Missing,
        };
        let mut tx = repo.refs().transaction();
        tx.update(
            &branch,
            expected,
            NewValue::Direct(commit_oid),
            ReflogMessage::from(format!("commit: {msg}")),
        )
        .unwrap();
        tx.commit().unwrap();
    }

    fn open_repo(dir: &Path) -> Repository {
        Repository::discover(dir).unwrap()
    }

    #[test]
    fn fresh_repo_no_commits_no_files() {
        let tmp = init_rg_repo();
        let repo = open_repo(tmp.path());
        let report = status(&repo).unwrap();
        assert!(report.entries.is_empty(), "{:?}", report.entries);
        assert!(report.head_commit.is_none());
        assert_eq!(
            report.branch.as_ref().map(|b| b.as_str()),
            Some("refs/heads/main")
        );
    }

    #[test]
    fn untracked_file_appears_as_question_marks() {
        let tmp = init_rg_repo();
        std::fs::write(tmp.path().join("a.txt"), b"hello").unwrap();
        let repo = open_repo(tmp.path());
        let report = status(&repo).unwrap();

        assert_eq!(report.entries.len(), 1);
        let e = &report.entries[0];
        assert_eq!(e.path, b"a.txt".to_vec());
        assert_eq!(e.worktree_state, WorktreeState::Untracked);
        assert_eq!(e.index_state, StageState::Unchanged);

        let bytes = PorcelainV1::new(&report).to_bytes();
        assert_eq!(bytes, b"?? a.txt\n");
    }

    #[test]
    fn staged_file_is_added() {
        let tmp = init_rg_repo();
        std::fs::write(tmp.path().join("a.txt"), b"alpha\n").unwrap();
        let repo = open_repo(tmp.path());
        rg_add_one(&repo, "a.txt");
        let report = status(&repo).unwrap();

        assert_eq!(report.entries.len(), 1);
        let e = &report.entries[0];
        assert_eq!(e.path, b"a.txt".to_vec());
        assert_eq!(e.index_state, StageState::Added);
        assert_eq!(e.worktree_state, WorktreeState::Unchanged);

        let bytes = PorcelainV1::new(&report).to_bytes();
        assert_eq!(bytes, b"A  a.txt\n");
    }

    #[test]
    fn staged_then_modified_emits_double_m() {
        let tmp = init_rg_repo();
        std::fs::write(tmp.path().join("a.txt"), b"alpha\n").unwrap();
        let repo = open_repo(tmp.path());
        rg_add_one(&repo, "a.txt");
        // Commit the original so HEAD has it; otherwise both columns can't be M.
        rg_commit(&repo, "first");

        // Edit, stage, then edit again to get MM (different from HEAD AND from index).
        std::fs::write(tmp.path().join("a.txt"), b"alpha2\n").unwrap();
        rg_add_one(&repo, "a.txt");
        std::fs::write(tmp.path().join("a.txt"), b"alpha3\n").unwrap();

        let report = status(&repo).unwrap();

        assert_eq!(report.entries.len(), 1);
        let e = &report.entries[0];
        assert_eq!(e.path, b"a.txt".to_vec());
        assert_eq!(e.index_state, StageState::Modified);
        assert_eq!(e.worktree_state, WorktreeState::Modified);

        let bytes = PorcelainV1::new(&report).to_bytes();
        assert_eq!(bytes, b"MM a.txt\n");
    }

    #[test]
    fn deleted_file_after_commit() {
        let tmp = init_rg_repo();
        std::fs::write(tmp.path().join("a.txt"), b"alpha\n").unwrap();
        let repo = open_repo(tmp.path());
        rg_add_one(&repo, "a.txt");
        rg_commit(&repo, "with a.txt");

        std::fs::remove_file(tmp.path().join("a.txt")).unwrap();
        let report = status(&repo).unwrap();

        // a.txt is in HEAD-tree and index, missing in workdir → " D".
        assert_eq!(report.entries.len(), 1);
        let e = &report.entries[0];
        assert_eq!(e.path, b"a.txt".to_vec());
        assert_eq!(e.index_state, StageState::Unchanged);
        assert_eq!(e.worktree_state, WorktreeState::Deleted);

        let bytes = PorcelainV1::new(&report).to_bytes();
        assert_eq!(bytes, b" D a.txt\n");
    }

    #[test]
    fn unmodified_file_has_no_entry() {
        let tmp = init_rg_repo();
        std::fs::write(tmp.path().join("a.txt"), b"alpha\n").unwrap();
        let repo = open_repo(tmp.path());
        rg_add_one(&repo, "a.txt");
        rg_commit(&repo, "with a.txt");

        let report = status(&repo).unwrap();
        assert!(
            report.entries.is_empty(),
            "expected clean tree, got {:?}",
            report.entries
        );
    }

    #[test]
    #[cfg(unix)]
    fn mode_change_executable_appears_modified() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = init_rg_repo();
        std::fs::write(tmp.path().join("script"), b"#!/bin/sh\necho hi\n").unwrap();
        let repo = open_repo(tmp.path());
        rg_add_one(&repo, "script");
        rg_commit(&repo, "first");

        // Skip if filesystem doesn't honor the exec bit.
        let probe = tmp.path().join(".probe");
        std::fs::write(&probe, b"x").unwrap();
        std::fs::set_permissions(&probe, std::fs::Permissions::from_mode(0o755)).unwrap();
        let after = std::fs::metadata(&probe).unwrap().permissions().mode() & 0o111;
        let _ = std::fs::remove_file(&probe);
        if after == 0 {
            eprintln!("skipping: filesystem doesn't honor exec bit");
            return;
        }

        // Flip exec bit on the tracked file.
        let p = tmp.path().join("script");
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();

        let report = status(&repo).unwrap();
        assert_eq!(report.entries.len(), 1);
        let e = &report.entries[0];
        assert_eq!(e.path, b"script".to_vec());
        assert_eq!(e.index_state, StageState::Unchanged);
        assert_eq!(e.worktree_state, WorktreeState::Modified);

        let bytes = PorcelainV1::new(&report).to_bytes();
        assert_eq!(bytes, b" M script\n");
    }

    #[test]
    fn quoting_does_not_fire_for_simple_names() {
        let mut buf = Vec::new();
        write_porcelain_path(&mut buf, b"src/lib.rs").unwrap();
        assert_eq!(buf, b"src/lib.rs");
    }

    #[test]
    fn quoting_fires_for_spaces_and_high_bits() {
        let mut buf = Vec::new();
        write_porcelain_path(&mut buf, b"hello world.txt").unwrap();
        // Spaces alone don't fire quoting in git (test that, since this matches
        // git's behavior). Adjust if/when we discover otherwise.
        // git's quote.c only quotes when the byte is in the unsafe set; a
        // bare space is fine. So this should NOT be quoted.
        assert_eq!(buf, b"hello world.txt");

        let mut buf2 = Vec::new();
        write_porcelain_path(&mut buf2, b"a\tb").unwrap();
        // Tab needs quoting.
        assert_eq!(buf2, b"\"a\\tb\"");

        let mut buf3 = Vec::new();
        write_porcelain_path(&mut buf3, &[b'x', 0xc3, 0xa9, b'.', b't']).unwrap();
        // High-bit (UTF-8 'é' here) → octal escape per byte. git's default.
        assert_eq!(buf3, b"\"x\\303\\251.t\"");
    }

    #[test]
    fn backslash_and_quote_in_path() {
        let mut buf = Vec::new();
        write_porcelain_path(&mut buf, b"a\\b\"c").unwrap();
        assert_eq!(buf, b"\"a\\\\b\\\"c\"");
    }

    /// Optional sanity: if system `git` is on PATH, run `rustygit add` then
    /// have `git status --porcelain` agree with our output.
    #[test]
    fn matches_git_status_porcelain_for_simple_cases() {
        if !has_system_git() {
            eprintln!("skipping: git not on PATH");
            return;
        }

        let tmp = tempdir().unwrap();
        // Initialize via system git so the index/refs format is canonical.
        run_git(tmp.path(), &["init", "-q", "."]);
        run_git(tmp.path(), &["config", "user.name", "T"]);
        run_git(tmp.path(), &["config", "user.email", "t@e"]);

        // Untracked.
        std::fs::write(tmp.path().join("u.txt"), b"u\n").unwrap();
        // Staged add.
        std::fs::write(tmp.path().join("s.txt"), b"s\n").unwrap();
        run_git(tmp.path(), &["add", "s.txt"]);

        // Run git status to capture canonical output.
        let g = run_git(tmp.path(), &["status", "--porcelain"]);
        let g_out = g.stdout;

        let repo = open_repo(tmp.path());
        let report = status(&repo).unwrap();
        let ours = PorcelainV1::new(&report).to_bytes();

        assert_eq!(
            ours,
            g_out,
            "porcelain mismatch\nours: {:?}\ntheirs: {:?}",
            String::from_utf8_lossy(&ours),
            String::from_utf8_lossy(&g_out)
        );
    }

    fn has_system_git() -> bool {
        Command::new("git")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    fn run_git(dir: &Path, args: &[&str]) -> std::process::Output {
        let out = Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .expect("run git");
        if !out.status.success() {
            panic!(
                "git {:?} failed: stdout={:?} stderr={:?}",
                args,
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
        }
        out
    }
}
