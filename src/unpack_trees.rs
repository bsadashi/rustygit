//! `unpack-trees` engine — safely materialize a target tree into the working
//! directory and the index.
//!
//! This module is the core of `git checkout`, `git switch`, `git restore` and
//! `git reset --hard`. It owns the conflict-detection logic that refuses to
//! clobber local modifications or untracked files unless `force=true`.
//!
//! Algorithm sketch (matching `unpack-trees.c`'s 1-way merge):
//!
//! 1. Flatten the target tree to a `BTreeMap<path, (mode, oid)>`.
//! 2. Read the current index → `BTreeMap` of stage-0 entries.
//! 3. For each path in `target ∪ index`:
//!    - **Both, same (mode, oid)**: skip; already correct.
//!    - **Target only** (creating): if an untracked file exists at that path
//!      in the workdir → `UntrackedClobber`; else `to_create`.
//!    - **Index only** (deleting): if `keep_extra`, leave alone. Else if
//!      workdir differs from index → `LocalModifications`; else `to_delete`.
//!    - **Both, different**: if workdir differs from the *index* (dirty) →
//!      `LocalModifications`; else `to_update`.
//!
//! The CLI commands ride on top of this module — they own the ref/HEAD work
//! and call `checkout_tree` for the data plane.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::hash::{HashError, ObjectId};
use crate::index::{Index, IndexEntry, IndexError};
use crate::object::{ObjectKind, RawObject};
use crate::odb::OdbError;
use crate::refs::RefError;
use crate::repo::Repository;
use crate::tree::{FileMode, Tree, TreeError};

// ---------------------------------------------------------------------------
// Public API (Track B's call surface — do not change without coordination).
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, Default)]
pub struct UnpackOpts {
    /// Overwrite dirty files in the workdir (the equivalent of `git checkout -f`).
    /// Default: false → fail with `LocalModifications` for any tracked file
    /// whose workdir content differs from its current index oid.
    pub force: bool,
    /// Treat the operation as `--keep` (don't delete files that are absent in
    /// the target). Defaults to false: paths in the source-but-not-target are
    /// deleted from the workdir + index.
    pub keep_extra: bool,
    /// Whether to update the workdir at all. `reset --soft` and `reset --mixed`
    /// pass `false`; `checkout` and `reset --hard` pass `true`.
    pub update_workdir: bool,
    /// Whether to update the index. `reset --soft` passes `false`; everything
    /// else passes `true`.
    pub update_index: bool,
}

#[derive(Debug, Clone)]
pub struct UnpackPlan {
    /// Paths that will be added to the workdir (created from the target).
    pub to_create: Vec<Vec<u8>>,
    /// Paths whose content will change.
    pub to_update: Vec<Vec<u8>>,
    /// Paths that will be removed from the workdir + index.
    pub to_delete: Vec<Vec<u8>>,
    /// Paths that prevent the operation. With `force=false` these are blockers;
    /// with `force=true` we'd overwrite/remove them.
    pub conflicts: Vec<UnpackConflict>,
}

#[derive(Debug, Clone)]
pub struct UnpackConflict {
    pub path: Vec<u8>,
    pub reason: ConflictReason,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictReason {
    /// Tracked file has local modifications relative to the current index.
    LocalModifications,
    /// Untracked file would be overwritten by the target.
    UntrackedClobber,
    /// Path is currently a directory in the workdir but the target wants it as
    /// a file (or vice versa).
    TypeMismatch,
}

#[derive(Error, Debug)]
pub enum UnpackError {
    #[error("would overwrite local modifications or untracked files; use --force to override")]
    Conflicts(Vec<UnpackConflict>),
    #[error("io error on {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    /// A workdir operation that this platform cannot represent natively.
    ///
    /// Today this fires only on `not(unix)` when checking out a symlink-mode
    /// entry while `core.symlinks` is unset (or `true`). The runtime refuses
    /// rather than silently writing the link target as a regular file, which
    /// would change the blob oid on the next `add`. Set `core.symlinks =
    /// false` to opt into the fallback (matches upstream git semantics).
    #[error("platform does not support {feature} for path '{}'", path.display())]
    PlatformUnsupported {
        path: PathBuf,
        feature: &'static str,
    },
    /// An index path is not valid UTF-8 on a platform that cannot represent
    /// arbitrary bytes in a `Path` (i.e. Windows). Returned instead of
    /// silently U+FFFD-replacing the path, which would lose information and
    /// risk writing to the wrong file. The Unix code path uses
    /// `OsStr::from_bytes` and never hits this.
    #[error("non-UTF-8 index path during {op}: {}", crate::cli::win_paths::format_path_bytes(.bytes))]
    PathEncodingError { bytes: Vec<u8>, op: String },
    #[error(transparent)]
    Odb(#[from] OdbError),
    #[error(transparent)]
    Index(#[from] IndexError),
    #[error(transparent)]
    Tree(#[from] TreeError),
    #[error(transparent)]
    Hash(#[from] HashError),
    #[error(transparent)]
    Ref(#[from] RefError),
}

// ---------------------------------------------------------------------------
// Public functions
// ---------------------------------------------------------------------------

/// Compute what would happen if we checked out `target_tree`. Does NOT mutate
/// anything. Used by `checkout`/`switch` to surface conflicts up-front.
pub fn plan_checkout(
    repo: &Repository,
    target_tree: ObjectId,
    opts: &UnpackOpts,
) -> Result<UnpackPlan, UnpackError> {
    plan_inner(repo, target_tree, opts, None)
}

/// Apply the target tree: write the files (if `opts.update_workdir`), update
/// the index (if `opts.update_index`). Returns Ok if the apply succeeded, or
/// `UnpackError::Conflicts` if `opts.force` is false and conflicts were found.
///
/// Atomicity:
/// * The index is written via [`Index::write`] (lockfile).
/// * Workdir mutations are transactional via [`StagedCheckout`]: every new
///   file is written to a shadow dir under `.git/checkout.tmp.<pid>.<ts>/`,
///   then on commit existing files are renamed aside before new content is
///   atomically renamed in. Any failure rolls back via reverse renames so the
///   workdir is restored to its pre-call state. See [`StagedCheckout::commit`]
///   for the per-phase failure semantics.
pub fn checkout_tree(
    repo: &Repository,
    target_tree: ObjectId,
    opts: &UnpackOpts,
) -> Result<UnpackPlan, UnpackError> {
    apply_inner(repo, target_tree, opts, None)
}

/// `reset --soft <commit>`: only updates HEAD/branch (which is the caller's
/// job — they own the ref). This module exposes a no-op helper for symmetry,
/// purely so the CLI's call-site stays uniform.
pub fn reset_soft(_repo: &Repository, _target: ObjectId) -> Result<(), UnpackError> {
    Ok(())
}

/// `reset --mixed <commit>`: replace the index with `target_tree`'s contents,
/// leave the workdir alone.
pub fn reset_mixed(repo: &Repository, target_tree: ObjectId) -> Result<(), UnpackError> {
    let opts = UnpackOpts {
        force: true, // workdir doesn't get touched, so dirty-file check is moot
        keep_extra: false,
        update_workdir: false,
        update_index: true,
    };
    checkout_tree(repo, target_tree, &opts)?;
    Ok(())
}

/// `restore --staged <pathspec>`: restore index entries for the given paths
/// from `source_tree` (typically HEAD's tree). `restore --worktree <pathspec>`
/// restores workdir files from the index. The simplest model: callers compute
/// the path filter and call `checkout_tree_for_paths`.
pub fn checkout_tree_for_paths(
    repo: &Repository,
    target_tree: ObjectId,
    paths: &[Vec<u8>],
    opts: &UnpackOpts,
) -> Result<UnpackPlan, UnpackError> {
    let filter: Option<&[Vec<u8>]> = Some(paths);
    apply_inner(repo, target_tree, opts, filter)
}

// ---------------------------------------------------------------------------
// Implementation
// ---------------------------------------------------------------------------

/// Compute the plan only; never mutates.
fn plan_inner(
    repo: &Repository,
    target_tree: ObjectId,
    opts: &UnpackOpts,
    path_filter: Option<&[Vec<u8>]>,
) -> Result<UnpackPlan, UnpackError> {
    let target = flatten_tree(repo, &target_tree)?;
    let index = Index::read(repo)?;
    let indexed = index_to_map(&index);

    let mut to_create = Vec::new();
    let mut to_update = Vec::new();
    let mut to_delete = Vec::new();
    let mut conflicts = Vec::new();

    // Walk the union of paths.
    let mut paths: BTreeMap<&[u8], ()> = BTreeMap::new();
    for k in target.keys() {
        paths.insert(k.as_slice(), ());
    }
    for k in indexed.keys() {
        paths.insert(k.as_slice(), ());
    }

    for path in paths.keys() {
        if let Some(filter) = path_filter {
            if !path_in_filter(path, filter) {
                continue;
            }
        }

        let target_pair = target.get(*path);
        let index_pair = indexed.get(*path);

        match (target_pair, index_pair) {
            (None, None) => {
                // Can't happen given the union iteration.
            }
            (Some((tmode, toid)), Some(ent)) => {
                // Both sides have it. Same (mode, oid) → skip. Otherwise we'd
                // be updating the workdir; check if it's dirty against the
                // index first.
                let idx_mode = FileMode::from_index_mode(ent.mode).ok();
                let same = idx_mode == Some(*tmode) && ent.oid == *toid;
                if same {
                    continue;
                }
                // Always queue the update; if the workdir is dirty also flag a
                // conflict so the caller can refuse with `force=false`. With
                // `force=true` the update still happens.
                to_update.push(path.to_vec());
                if opts.update_workdir {
                    match workdir_state(repo, path, ent)? {
                        WorkdirState::Missing | WorkdirState::CleanMatch => {}
                        WorkdirState::Dirty => {
                            conflicts.push(UnpackConflict {
                                path: path.to_vec(),
                                reason: ConflictReason::LocalModifications,
                            });
                        }
                        WorkdirState::WrongType => {
                            conflicts.push(UnpackConflict {
                                path: path.to_vec(),
                                reason: ConflictReason::TypeMismatch,
                            });
                        }
                    }
                }
            }
            (Some(_), None) => {
                // Creating.
                to_create.push(path.to_vec());
                if opts.update_workdir {
                    match untracked_at(repo, path)? {
                        UntrackedAt::None => {}
                        UntrackedAt::File => {
                            conflicts.push(UnpackConflict {
                                path: path.to_vec(),
                                reason: ConflictReason::UntrackedClobber,
                            });
                        }
                        UntrackedAt::Directory => {
                            conflicts.push(UnpackConflict {
                                path: path.to_vec(),
                                reason: ConflictReason::TypeMismatch,
                            });
                        }
                    }
                }
            }
            (None, Some(ent)) => {
                // Deleting from the index/workdir.
                if opts.keep_extra {
                    continue;
                }
                to_delete.push(path.to_vec());
                if opts.update_workdir {
                    match workdir_state(repo, path, ent)? {
                        WorkdirState::Missing | WorkdirState::CleanMatch => {}
                        WorkdirState::Dirty => {
                            conflicts.push(UnpackConflict {
                                path: path.to_vec(),
                                reason: ConflictReason::LocalModifications,
                            });
                        }
                        WorkdirState::WrongType => {
                            conflicts.push(UnpackConflict {
                                path: path.to_vec(),
                                reason: ConflictReason::TypeMismatch,
                            });
                        }
                    }
                }
            }
        }
    }

    Ok(UnpackPlan {
        to_create,
        to_update,
        to_delete,
        conflicts,
    })
}

/// Compute the plan and apply it.
fn apply_inner(
    repo: &Repository,
    target_tree: ObjectId,
    opts: &UnpackOpts,
    path_filter: Option<&[Vec<u8>]>,
) -> Result<UnpackPlan, UnpackError> {
    let plan = plan_inner(repo, target_tree, opts, path_filter)?;

    if !plan.conflicts.is_empty() && !opts.force {
        return Err(UnpackError::Conflicts(plan.conflicts));
    }

    // Re-flatten the tree for the mutation phase. We could thread it through
    // the planner but plans are typically small relative to the tree work, so
    // a single re-read is cheap.
    let target = flatten_tree(repo, &target_tree)?;

    if opts.update_workdir {
        crate::trace!(
            "checkout",
            "writing {} entries ({} create + {} update, {} delete)",
            plan.to_create.len() + plan.to_update.len(),
            plan.to_create.len(),
            plan.to_update.len(),
            plan.to_delete.len()
        );
        let mut staged = StagedCheckout::new(repo.gitdir())?;
        // Resolve config-driven write policies once per apply. `core.symlinks`
        // governs whether non-Unix targets refuse symlink-mode entries or fall
        // back to writing the target as a regular file; `core.autocrlf=true`
        // converts LF→CRLF for text blobs on checkout. On Unix the symlinks
        // flag is read but unused — gate the binding to avoid an unused-var
        // warning.
        #[cfg(not(unix))]
        let symlinks_enabled = repo.core_symlinks();
        let autocrlf = repo.core_autocrlf();
        // Stage phase: write every new blob into the shadow dir. Failures
        // here roll back via Drop with the workdir untouched.
        for path in plan.to_create.iter().chain(plan.to_update.iter()) {
            let (mode, oid) = target
                .get(path)
                .copied()
                .expect("plan derived from same tree");
            let raw = repo.odb().read(&oid)?;
            if raw.kind != ObjectKind::Blob {
                if mode == FileMode::Gitlink {
                    // Submodule — we leave them out of the workdir mutation.
                    continue;
                }
                return Err(UnpackError::Io {
                    path: repo
                        .workdir()
                        .join(bytes_to_relpath_checked(path, "checkout")?),
                    source: io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("expected blob, got {:?}", raw.kind),
                    ),
                });
            }
            // Refuse symlink writes on platforms without native symlink
            // support, unless the user explicitly opted into the
            // "store-target-as-file" fallback via `core.symlinks = false`.
            // See NON_GOALS A10 — silent corruption is what we're avoiding.
            #[cfg(not(unix))]
            if mode == FileMode::Symlink && symlinks_enabled {
                return Err(UnpackError::PlatformUnsupported {
                    path: repo
                        .workdir()
                        .join(bytes_to_relpath_checked(path, "checkout")?),
                    feature: "symlink",
                });
            }
            let target_abs = repo
                .workdir()
                .join(bytes_to_relpath_checked(path, "checkout")?);
            // Apply LF→CRLF if `core.autocrlf=true` and this looks like text.
            // Symlink contents are link targets and must never be transformed.
            let payload: std::borrow::Cow<'_, [u8]> = if mode != FileMode::Symlink
                && autocrlf.map(|m| m.converts_on_checkout()).unwrap_or(false)
                && crate::config::is_text_blob(&raw.data)
            {
                crate::config::convert_lf_to_crlf(&raw.data)
            } else {
                std::borrow::Cow::Borrowed(raw.data.as_slice())
            };
            let is_update = plan.to_update.iter().any(|p| p == path);
            if is_update {
                staged.stage_update(target_abs, mode, &payload)?;
            } else {
                staged.stage_create(target_abs, mode, &payload)?;
            }
        }
        // Deletes don't need new content — record them for the commit phase.
        for path in &plan.to_delete {
            staged.stage_delete(
                repo.workdir()
                    .join(bytes_to_relpath_checked(path, "checkout")?),
            );
        }
        // Commit: rename originals aside, then rename new content into place.
        // On failure, restore via reverse renames.
        staged.commit()?;
    }

    if opts.update_index {
        // Rebuild the index from scratch from the target tree, but for paths
        // that aren't in the filter (when one is supplied) keep the existing
        // index entry. This matches `git restore --staged`'s behavior.
        let old_index = Index::read(repo)?;
        let old_map = index_to_map(&old_index);
        // Preserve the on-disk index version so we don't silently downgrade
        // a v3 (or v4) index to v2. `Index::write` will bump to 3 if any of
        // the new entries require extended flags.
        let mut new_index = Index::empty(old_index.version);

        // Paths from the target tree become new entries.
        for (path, (mode, oid)) in target.iter() {
            if let Some(filter) = path_filter {
                if !path_in_filter(path, filter) {
                    // Outside filter: preserve any existing index entry.
                    if let Some(existing) = old_map.get(path) {
                        new_index.entries.push((*existing).clone());
                    }
                    continue;
                }
            }
            // Stat the workfile to fill the cache fields, if it exists. If
            // not, zeros are fine (matches `git read-tree` after a checkout
            // without update-workdir).
            let stat = stat_for_path(repo, path);
            let entry = build_index_entry(path, *mode, *oid, stat);
            new_index.entries.push(entry);
        }

        // For paths in the old index that aren't in the target *and* are
        // outside the filter, keep them. Inside the filter, drop them
        // (matches `git restore --staged` semantics: target wins for filtered
        // paths). When `keep_extra` is set, also preserve filtered paths
        // missing from the target.
        for (path, ent) in old_map.iter() {
            if target.contains_key(path.as_slice()) {
                continue; // already replaced above
            }
            let inside_filter = path_filter.map(|f| path_in_filter(path, f)).unwrap_or(true);
            if !inside_filter || opts.keep_extra {
                new_index.entries.push((*ent).clone());
            }
        }

        new_index.sort();
        new_index.write(repo)?;
    }

    Ok(plan)
}

// ---------------------------------------------------------------------------
// Tree flattening (specialized — we want (mode, oid) pairs, not DiffEntry).
// ---------------------------------------------------------------------------

fn flatten_tree(
    repo: &Repository,
    tree_oid: &ObjectId,
) -> Result<BTreeMap<Vec<u8>, (FileMode, ObjectId)>, UnpackError> {
    let mut out = BTreeMap::new();
    let mut prefix = Vec::new();
    flatten_tree_inner(repo, tree_oid, &mut prefix, &mut out)?;
    Ok(out)
}

fn flatten_tree_inner(
    repo: &Repository,
    tree_oid: &ObjectId,
    prefix: &mut Vec<u8>,
    out: &mut BTreeMap<Vec<u8>, (FileMode, ObjectId)>,
) -> Result<(), UnpackError> {
    let raw = repo.odb().read(tree_oid)?;
    if raw.kind != ObjectKind::Tree {
        return Ok(());
    }
    let tree = Tree::parse(&raw.data, repo.hash_kind())?;
    for entry in &tree.entries {
        let saved = prefix.len();
        if !prefix.is_empty() {
            prefix.push(b'/');
        }
        prefix.extend_from_slice(&entry.name);
        if entry.mode.is_tree() {
            flatten_tree_inner(repo, &entry.oid, prefix, out)?;
        } else {
            out.insert(prefix.clone(), (entry.mode, entry.oid));
        }
        prefix.truncate(saved);
    }
    Ok(())
}

fn index_to_map(index: &Index) -> BTreeMap<Vec<u8>, &IndexEntry> {
    let mut out = BTreeMap::new();
    for ent in &index.entries {
        if ent.stage != 0 {
            continue;
        }
        out.insert(ent.path.clone(), ent);
    }
    out
}

// ---------------------------------------------------------------------------
// Workdir probing
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
enum WorkdirState {
    /// File doesn't exist in the workdir.
    Missing,
    /// File exists and matches the index entry's content.
    CleanMatch,
    /// File exists and differs from the index entry.
    Dirty,
    /// File exists but is the wrong type (e.g. directory where a file is
    /// expected, or vice versa).
    WrongType,
}

#[derive(Debug, Clone, Copy)]
enum UntrackedAt {
    /// Nothing at that path.
    None,
    /// A file/symlink exists.
    File,
    /// A directory exists.
    Directory,
}

fn workdir_state(
    repo: &Repository,
    path: &[u8],
    ent: &IndexEntry,
) -> Result<WorkdirState, UnpackError> {
    let abs = repo.workdir().join(bytes_to_relpath(path));
    let metadata = match fs::symlink_metadata(&abs) {
        Ok(m) => m,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(WorkdirState::Missing),
        Err(source) => return Err(UnpackError::Io { path: abs, source }),
    };
    let ft = metadata.file_type();

    // Type check first.
    let stored_mode = FileMode::from_index_mode(ent.mode).ok();
    if ft.is_dir() {
        return Ok(WorkdirState::WrongType);
    }
    let on_disk_blob = ft.is_file();
    let on_disk_symlink = ft.is_symlink();
    let stored_is_blob = matches!(
        stored_mode,
        Some(FileMode::Regular) | Some(FileMode::Executable)
    );
    let stored_is_symlink = matches!(stored_mode, Some(FileMode::Symlink));
    if (on_disk_blob && stored_is_symlink) || (on_disk_symlink && stored_is_blob) {
        // Type swap (blob ↔ symlink).
        return Ok(WorkdirState::WrongType);
    }

    // Stat fast-path: matching size + mtime ⇒ assume content unchanged.
    if stat_matches_index(&metadata, ent) {
        // Also consider mode bits for blob-vs-blob. If the exec bit toggled,
        // that's a (small) change worth flagging as Dirty so we don't silently
        // overwrite an intentional chmod.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let on_disk_exec = metadata.permissions().mode() & 0o111 != 0;
            let idx_exec = matches!(stored_mode, Some(FileMode::Executable));
            if on_disk_blob && on_disk_exec != idx_exec {
                return Ok(WorkdirState::Dirty);
            }
        }
        return Ok(WorkdirState::CleanMatch);
    }

    // Stat says something *might* have changed. Re-hash and compare.
    let payload = if on_disk_symlink {
        match fs::read_link(&abs) {
            Ok(t) => t.as_os_str().to_string_lossy().into_owned().into_bytes(),
            Err(source) => return Err(UnpackError::Io { path: abs, source }),
        }
    } else {
        match fs::read(&abs) {
            Ok(b) => b,
            Err(source) => return Err(UnpackError::Io { path: abs, source }),
        }
    };
    let blob = RawObject::new(ObjectKind::Blob, payload);
    let oid = blob.oid(repo.hash_kind());
    if oid == ent.oid {
        Ok(WorkdirState::CleanMatch)
    } else {
        Ok(WorkdirState::Dirty)
    }
}

fn untracked_at(repo: &Repository, path: &[u8]) -> Result<UntrackedAt, UnpackError> {
    let abs = repo.workdir().join(bytes_to_relpath(path));
    match fs::symlink_metadata(&abs) {
        Ok(m) => {
            let ft = m.file_type();
            if ft.is_dir() {
                Ok(UntrackedAt::Directory)
            } else {
                Ok(UntrackedAt::File)
            }
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(UntrackedAt::None),
        Err(source) => Err(UnpackError::Io { path: abs, source }),
    }
}

#[cfg(unix)]
fn stat_matches_index(meta: &fs::Metadata, ent: &IndexEntry) -> bool {
    use std::os::unix::fs::MetadataExt;
    let size_ok = (meta.size().min(u32::MAX as u64) as u32) == ent.size;
    let mtime_ok =
        (meta.mtime() as u32) == ent.mtime_s && (meta.mtime_nsec() as u32) == ent.mtime_n;
    size_ok && mtime_ok
}

#[cfg(not(unix))]
fn stat_matches_index(_meta: &fs::Metadata, _ent: &IndexEntry) -> bool {
    false
}

// ---------------------------------------------------------------------------
// Workdir mutations — transactional via StagedCheckout
// ---------------------------------------------------------------------------

/// Per-checkout shadow directory + queued operations.
///
/// Created under `<gitdir>/checkout.tmp.<pid>.<nanos>/`. Each create/update
/// blob is first written to a numbered shadow file. The `commit` method then
/// (a) renames the EXISTING content of every Update/Delete target into a
/// shadow_orig slot, (b) renames the shadow_new files into the targets. Any
/// failure during commit reverses the renames so the workdir is restored to
/// its pre-call state.
///
/// Drop removes the shadow dir best-effort.
struct StagedCheckout {
    shadow_dir: PathBuf,
    next_id: u64,
    ops: Vec<StageOp>,
}

enum StageOp {
    Create {
        target: PathBuf,
        shadow_new: PathBuf,
    },
    Update {
        target: PathBuf,
        shadow_new: PathBuf,
        shadow_orig: PathBuf,
    },
    Delete {
        target: PathBuf,
        shadow_orig: PathBuf,
    },
}

impl StagedCheckout {
    fn new(gitdir: &Path) -> Result<Self, UnpackError> {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let name = format!("checkout.tmp.{}.{nanos}", std::process::id());
        let shadow_dir = gitdir.join(name);
        fs::create_dir_all(&shadow_dir).map_err(|source| UnpackError::Io {
            path: shadow_dir.clone(),
            source,
        })?;
        Ok(Self {
            shadow_dir,
            next_id: 0,
            ops: Vec::new(),
        })
    }

    fn alloc_shadow(&mut self) -> PathBuf {
        let id = self.next_id;
        self.next_id += 1;
        self.shadow_dir.join(format!("{id:x}"))
    }

    fn stage_create(
        &mut self,
        target: PathBuf,
        mode: FileMode,
        content: &[u8],
    ) -> Result<(), UnpackError> {
        let shadow_new = self.alloc_shadow();
        write_blob_at(&shadow_new, mode, content)?;
        self.ops.push(StageOp::Create { target, shadow_new });
        Ok(())
    }

    fn stage_update(
        &mut self,
        target: PathBuf,
        mode: FileMode,
        content: &[u8],
    ) -> Result<(), UnpackError> {
        let shadow_new = self.alloc_shadow();
        let shadow_orig = self.alloc_shadow();
        write_blob_at(&shadow_new, mode, content)?;
        self.ops.push(StageOp::Update {
            target,
            shadow_new,
            shadow_orig,
        });
        Ok(())
    }

    fn stage_delete(&mut self, target: PathBuf) {
        let shadow_orig = self.alloc_shadow();
        self.ops.push(StageOp::Delete {
            target,
            shadow_orig,
        });
    }

    /// Two-phase commit. Phase A renames each existing Update/Delete target
    /// aside; Phase B renames each new blob into its Create/Update target.
    /// Any failure reverses every completed rename in this call so the
    /// workdir is restored to its pre-commit state.
    ///
    /// If rollback ITSELF cannot fully restore (e.g. a rename-back fails due
    /// to permissions or EXDEV), the shadow dir is renamed to a sibling
    /// `<gitdir>/checkout.recover.<pid>.<nanos>/` and the path is logged to
    /// stderr — Drop is then prevented from deleting it. The user can move
    /// the originals back by hand.
    fn commit(mut self) -> Result<(), UnpackError> {
        // Take ownership of ops up front so the iteration loops can borrow
        // them while `handle_rollback` borrows the rest of `self` mutably.
        let ops = std::mem::take(&mut self.ops);

        // Phase A: move originals aside. Skip paths that don't currently
        // exist — that's normal (Update of a tracked-but-missing file, or
        // Delete of an already-gone file).
        let mut moved: Vec<(PathBuf, PathBuf)> = Vec::new();
        for op in &ops {
            let (target, shadow_orig) = match op {
                StageOp::Update {
                    target,
                    shadow_orig,
                    ..
                } => (target, shadow_orig),
                StageOp::Delete {
                    target,
                    shadow_orig,
                } => (target, shadow_orig),
                StageOp::Create { .. } => continue,
            };
            match fs::symlink_metadata(target) {
                Ok(_) => {
                    // Whether the target is a regular file, symlink, or a
                    // directory (force-mode TypeMismatch), `rename` moves the
                    // entry aside intact so rollback can restore it.
                    if let Err(source) = fs::rename(target, shadow_orig) {
                        self.handle_rollback(&moved, &[]);
                        return Err(UnpackError::Io {
                            path: target.clone(),
                            source,
                        });
                    }
                    moved.push((target.clone(), shadow_orig.clone()));
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    // Nothing to move aside; commit phase will just place
                    // (Create/Update) or skip (Delete).
                }
                Err(source) => {
                    self.handle_rollback(&moved, &[]);
                    return Err(UnpackError::Io {
                        path: target.clone(),
                        source,
                    });
                }
            }
        }

        // Phase B: rename new content into place.
        let mut placed: Vec<PathBuf> = Vec::new();
        for op in &ops {
            let (target, shadow_new) = match op {
                StageOp::Create { target, shadow_new } => (target, shadow_new),
                StageOp::Update {
                    target, shadow_new, ..
                } => (target, shadow_new),
                StageOp::Delete { .. } => continue,
            };
            if let Some(parent) = target.parent() {
                if let Err(source) = fs::create_dir_all(parent) {
                    self.handle_rollback(&moved, &placed);
                    return Err(UnpackError::Io {
                        path: parent.to_path_buf(),
                        source,
                    });
                }
            }
            if let Err(source) = fs::rename(shadow_new, target) {
                self.handle_rollback(&moved, &placed);
                return Err(UnpackError::Io {
                    path: target.clone(),
                    source,
                });
            }
            placed.push(target.clone());
        }

        // Success — Drop will sweep the shadow dir, which now holds only the
        // originals we displaced (safe to discard).
        Ok(())
    }

    /// Rollback wrapper: runs the actual rename-back, and if anything in the
    /// rollback itself fails (so a real original may still be inside the
    /// shadow dir), renames the shadow dir to a sibling recovery path,
    /// blanks `self.shadow_dir` so Drop won't touch it, and writes the
    /// recovery path to stderr.
    fn handle_rollback(&mut self, moved: &[(PathBuf, PathBuf)], placed: &[PathBuf]) {
        let fully_restored = rollback(moved, placed);
        if fully_restored {
            return;
        }
        // Move the shadow dir aside so Drop's `remove_dir_all` cannot wipe
        // a stranded original. Best-effort; if even the rename-aside fails,
        // we leave the shadow dir in place and still skip Drop's cleanup.
        let parent = self.shadow_dir.parent().map(|p| p.to_path_buf());
        let recovery = parent.map(|p| {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            p.join(format!("checkout.recover.{}.{nanos}", std::process::id()))
        });
        let recovered_to = match &recovery {
            Some(dest) => match fs::rename(&self.shadow_dir, dest) {
                Ok(()) => Some(dest.clone()),
                Err(_) => None,
            },
            None => None,
        };
        let report_path = recovered_to.unwrap_or_else(|| self.shadow_dir.clone());
        eprintln!(
            "rustygit: checkout: rollback could not fully restore the workdir; \
             original content preserved in {}",
            report_path.display()
        );
        // Blank the field so Drop is a no-op (an empty PathBuf turns
        // `remove_dir_all` into an immediate Err that we swallow).
        self.shadow_dir = PathBuf::new();
    }
}

impl Drop for StagedCheckout {
    fn drop(&mut self) {
        // Empty path = `handle_rollback` blanked us because rollback couldn't
        // fully restore; the shadow dir was renamed aside for recovery and
        // is no longer ours to delete.
        if self.shadow_dir.as_os_str().is_empty() {
            return;
        }
        let _ = fs::remove_dir_all(&self.shadow_dir);
    }
}

/// Restore aside-moved originals and undo placed new files. Returns true if
/// every step succeeded (workdir definitively restored to its pre-commit
/// state); false if any step failed, in which case the caller must NOT let
/// the shadow dir be wiped — a stranded original is still inside it.
fn rollback(moved: &[(PathBuf, PathBuf)], placed: &[PathBuf]) -> bool {
    let mut ok = true;
    for p in placed.iter().rev() {
        if fs::remove_file(p).is_err() {
            ok = false;
        }
    }
    for (target, shadow_orig) in moved.iter().rev() {
        // Skip if the shadow_orig isn't actually there (e.g. Phase A's
        // rename never landed it, though `moved` only records successful
        // renames so this is defensive).
        if !shadow_orig.exists() {
            continue;
        }
        if fs::rename(shadow_orig, target).is_err() {
            ok = false;
        }
    }
    ok
}

/// Write `content` into `dst` with the file mode that `mode` implies. For
/// symlinks, `content` is the link target. Used by [`StagedCheckout`] to
/// build shadow files; the destination is always inside the shadow dir, never
/// the workdir.
fn write_blob_at(dst: &Path, mode: FileMode, content: &[u8]) -> Result<(), UnpackError> {
    match mode {
        FileMode::Symlink => write_symlink(dst, content),
        FileMode::Regular | FileMode::Executable => {
            if let Err(source) = fs::write(dst, content) {
                return Err(UnpackError::Io {
                    path: dst.to_path_buf(),
                    source,
                });
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let perm = if mode == FileMode::Executable {
                    0o755
                } else {
                    0o644
                };
                if let Err(source) = fs::set_permissions(dst, fs::Permissions::from_mode(perm)) {
                    return Err(UnpackError::Io {
                        path: dst.to_path_buf(),
                        source,
                    });
                }
            }
            Ok(())
        }
        FileMode::Tree | FileMode::Gitlink => Ok(()),
    }
}

#[cfg(unix)]
fn write_symlink(abs: &Path, target: &[u8]) -> Result<(), UnpackError> {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    // Shadow paths are unique per call so there is nothing to remove first;
    // we still defensively unlink to keep the helper reusable.
    let _ = fs::remove_file(abs);
    let target_os = OsStr::from_bytes(target);
    if let Err(source) = std::os::unix::fs::symlink(target_os, abs) {
        return Err(UnpackError::Io {
            path: abs.to_path_buf(),
            source,
        });
    }
    Ok(())
}

#[cfg(not(unix))]
fn write_symlink(abs: &Path, target: &[u8]) -> Result<(), UnpackError> {
    // Best-effort: write the target path as a regular file's content. git does
    // similar on platforms without symlink support unless `core.symlinks` is
    // configured.
    if let Err(source) = fs::write(abs, target) {
        return Err(UnpackError::Io {
            path: abs.to_path_buf(),
            source,
        });
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Index-entry construction
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Clone, Copy)]
struct StatBits {
    ctime_s: u32,
    ctime_n: u32,
    mtime_s: u32,
    mtime_n: u32,
    dev: u32,
    ino: u32,
    uid: u32,
    gid: u32,
    size: u32,
}

fn stat_for_path(repo: &Repository, path: &[u8]) -> StatBits {
    let abs = repo.workdir().join(bytes_to_relpath(path));
    let Ok(meta) = fs::symlink_metadata(&abs) else {
        return StatBits::default();
    };
    stat_bits_from_meta(&meta)
}

#[cfg(unix)]
fn stat_bits_from_meta(meta: &fs::Metadata) -> StatBits {
    use std::os::unix::fs::MetadataExt;
    StatBits {
        ctime_s: meta.ctime() as u32,
        ctime_n: meta.ctime_nsec() as u32,
        mtime_s: meta.mtime() as u32,
        mtime_n: meta.mtime_nsec() as u32,
        dev: meta.dev() as u32,
        ino: meta.ino() as u32,
        uid: meta.uid(),
        gid: meta.gid(),
        size: meta.size().min(u32::MAX as u64) as u32,
    }
}

#[cfg(not(unix))]
fn stat_bits_from_meta(_meta: &fs::Metadata) -> StatBits {
    StatBits::default()
}

fn build_index_entry(path: &[u8], mode: FileMode, oid: ObjectId, stat: StatBits) -> IndexEntry {
    let path_vec = path.to_vec();
    IndexEntry {
        ctime_s: stat.ctime_s,
        ctime_n: stat.ctime_n,
        mtime_s: stat.mtime_s,
        mtime_n: stat.mtime_n,
        dev: stat.dev,
        ino: stat.ino,
        mode: mode.to_index_mode(),
        uid: stat.uid,
        gid: stat.gid,
        size: stat.size,
        oid,
        flags: encode_flags(path_vec.len()),
        path: path_vec,
        stage: 0,
        assume_valid: false,
        extended: false,
        extended_flags: 0,
    }
}

fn encode_flags(name_len: usize) -> u16 {
    (name_len.min(0x0FFF) as u16) & 0x0FFF
}

// ---------------------------------------------------------------------------
// Path utilities
// ---------------------------------------------------------------------------

fn bytes_to_relpath(b: &[u8]) -> PathBuf {
    #[cfg(unix)]
    {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;
        PathBuf::from(OsStr::from_bytes(b))
    }
    #[cfg(not(unix))]
    {
        // Lossy fallback retained for the small set of callsites (status,
        // workdir scans) that consume the path purely for display. Hot paths
        // that mutate the workdir should use `bytes_to_relpath_checked` so
        // non-UTF-8 names refuse instead of corrupting silently.
        PathBuf::from(String::from_utf8_lossy(b).into_owned())
    }
}

/// Variant of [`bytes_to_relpath`] that refuses non-UTF-8 names on platforms
/// without `OsStr::from_bytes`. Returns
/// [`UnpackError::PathEncodingError`] with the offending bytes so the
/// caller can surface a precise error instead of writing to a
/// U+FFFD-substituted path. On Unix this is identical to `bytes_to_relpath`
/// — `OsStr::from_bytes` handles arbitrary bytes natively.
fn bytes_to_relpath_checked(b: &[u8], op: &str) -> Result<PathBuf, UnpackError> {
    #[cfg(unix)]
    {
        let _ = op;
        Ok(bytes_to_relpath(b))
    }
    #[cfg(not(unix))]
    {
        match std::str::from_utf8(b) {
            Ok(s) => Ok(PathBuf::from(s)),
            Err(_) => Err(UnpackError::PathEncodingError {
                bytes: b.to_vec(),
                op: op.to_string(),
            }),
        }
    }
}

/// Literal path matching: the path is in the filter if it is byte-equal to a
/// filter entry, or if it lives inside a filter directory (entry + `/`). This
/// is M6's simplification — no pathspec magic, no globs.
fn path_in_filter(path: &[u8], filter: &[Vec<u8>]) -> bool {
    for f in filter {
        if path == f.as_slice() {
            return true;
        }
        if path.len() > f.len() && path.starts_with(f) && path[f.len()] == b'/' {
            return true;
        }
    }
    false
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::{ObjectKind, RawObject};
    use crate::tree::{FileMode, Tree, TreeEntry};
    use std::path::Path;
    use tempfile::TempDir;

    /// Lay out a minimal `.git` directory by hand. We avoid the system `git`
    /// binary so the test is hermetic.
    fn init_repo() -> TempDir {
        let tmp = tempfile::tempdir().unwrap();
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
        ] {
            std::fs::create_dir_all(gitdir.join(sub)).unwrap();
        }
        std::fs::write(gitdir.join("HEAD"), b"ref: refs/heads/main\n").unwrap();
        std::fs::write(
            gitdir.join("config"),
            b"[core]\n\trepositoryformatversion = 0\n\tfilemode = true\n\tbare = false\n",
        )
        .unwrap();
        tmp
    }

    fn open_repo(tmp: &TempDir) -> Repository {
        Repository::discover(tmp.path()).unwrap()
    }

    /// Write a blob and return its OID.
    fn write_blob(repo: &Repository, content: &[u8]) -> ObjectId {
        let blob = RawObject::new(ObjectKind::Blob, content.to_vec());
        repo.odb().write(&blob).unwrap()
    }

    /// Build a tree object from `(name, mode, content)` triples and return its
    /// OID. All entries land at the root level.
    fn write_flat_tree(repo: &Repository, files: &[(&[u8], FileMode, &[u8])]) -> ObjectId {
        let mut entries = Vec::new();
        for (name, mode, content) in files {
            let oid = write_blob(repo, content);
            entries.push(TreeEntry {
                mode: *mode,
                name: name.to_vec(),
                oid,
            });
        }
        let tree = Tree::new(entries);
        repo.odb().write(&tree.to_object()).unwrap()
    }

    /// Stage a single file into the index. Caller must have written the blob
    /// already (we re-hash for safety).
    fn stage_file(repo: &Repository, rel: &[u8], content: &[u8], mode: FileMode) {
        let abs = repo.workdir().join(bytes_to_relpath(rel));
        if let Some(p) = abs.parent() {
            std::fs::create_dir_all(p).unwrap();
        }
        // Ensure the file exists on disk to match a real `add`.
        if mode == FileMode::Symlink {
            #[cfg(unix)]
            {
                let _ = std::fs::remove_file(&abs);
                use std::ffi::OsStr;
                use std::os::unix::ffi::OsStrExt;
                std::os::unix::fs::symlink(OsStr::from_bytes(content), &abs).unwrap();
            }
            #[cfg(not(unix))]
            {
                std::fs::write(&abs, content).unwrap();
            }
        } else {
            std::fs::write(&abs, content).unwrap();
            #[cfg(unix)]
            if mode == FileMode::Executable {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&abs, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
        }
        let blob = RawObject::new(ObjectKind::Blob, content.to_vec());
        let oid = repo.odb().write(&blob).unwrap();
        let meta = std::fs::symlink_metadata(&abs).unwrap();
        let stat = stat_bits_from_meta(&meta);
        let entry = build_index_entry(rel, mode, oid, stat);
        let mut idx = Index::read(repo).unwrap();
        idx.upsert(entry);
        idx.write(repo).unwrap();
    }

    fn read_workfile(repo: &Repository, rel: &[u8]) -> Vec<u8> {
        let abs = repo.workdir().join(bytes_to_relpath(rel));
        std::fs::read(&abs).unwrap()
    }

    fn workfile_exists(repo: &Repository, rel: &[u8]) -> bool {
        let abs = repo.workdir().join(bytes_to_relpath(rel));
        std::fs::symlink_metadata(&abs).is_ok()
    }

    fn opts_for_checkout() -> UnpackOpts {
        UnpackOpts {
            force: false,
            keep_extra: false,
            update_workdir: true,
            update_index: true,
        }
    }

    // --- 1. Clean checkout ---------------------------------------------------

    #[test]
    fn clean_checkout_replaces_workdir_and_index() {
        let tmp = init_repo();
        let repo = open_repo(&tmp);

        // Stage one file via "add" so the index reflects the workdir.
        stage_file(&repo, b"keep.txt", b"old\n", FileMode::Regular);
        stage_file(&repo, b"gone.txt", b"bye\n", FileMode::Regular);

        // Build a target tree: keep.txt's content changes, gone.txt vanishes,
        // new.txt appears.
        let target = write_flat_tree(
            &repo,
            &[
                (b"keep.txt", FileMode::Regular, b"new\n"),
                (b"new.txt", FileMode::Regular, b"hello\n"),
            ],
        );

        let plan = checkout_tree(&repo, target, &opts_for_checkout()).unwrap();
        assert!(
            plan.conflicts.is_empty(),
            "unexpected conflicts: {:?}",
            plan.conflicts
        );
        assert_eq!(plan.to_create, vec![b"new.txt".to_vec()]);
        assert_eq!(plan.to_update, vec![b"keep.txt".to_vec()]);
        assert_eq!(plan.to_delete, vec![b"gone.txt".to_vec()]);

        // Workdir reflects the target.
        assert_eq!(read_workfile(&repo, b"keep.txt"), b"new\n");
        assert_eq!(read_workfile(&repo, b"new.txt"), b"hello\n");
        assert!(!workfile_exists(&repo, b"gone.txt"));

        // Index reflects the target too.
        let idx = Index::read(&repo).unwrap();
        let paths: Vec<_> = idx.entries.iter().map(|e| e.path.clone()).collect();
        let mut expected = vec![b"keep.txt".to_vec(), b"new.txt".to_vec()];
        expected.sort();
        let mut got = paths.clone();
        got.sort();
        assert_eq!(got, expected);
    }

    // --- 2. Refuses dirty file -----------------------------------------------

    #[test]
    fn refuses_dirty_tracked_file() {
        let tmp = init_repo();
        let repo = open_repo(&tmp);
        stage_file(&repo, b"a.txt", b"original\n", FileMode::Regular);

        // Modify on disk so it diverges from the index.
        std::fs::write(repo.workdir().join("a.txt"), b"DIRTY\n").unwrap();

        let target = write_flat_tree(&repo, &[(b"a.txt", FileMode::Regular, b"target\n")]);

        let err = checkout_tree(&repo, target, &opts_for_checkout()).unwrap_err();
        match err {
            UnpackError::Conflicts(cs) => {
                assert_eq!(cs.len(), 1);
                assert_eq!(cs[0].path, b"a.txt".to_vec());
                assert_eq!(cs[0].reason, ConflictReason::LocalModifications);
            }
            other => panic!("expected Conflicts, got {other:?}"),
        }

        // Workdir untouched.
        assert_eq!(read_workfile(&repo, b"a.txt"), b"DIRTY\n");
    }

    // --- 3. Force overrides dirty file ---------------------------------------

    #[test]
    fn force_overrides_dirty_tracked_file() {
        let tmp = init_repo();
        let repo = open_repo(&tmp);
        stage_file(&repo, b"a.txt", b"original\n", FileMode::Regular);
        std::fs::write(repo.workdir().join("a.txt"), b"DIRTY\n").unwrap();

        let target = write_flat_tree(&repo, &[(b"a.txt", FileMode::Regular, b"target\n")]);

        let mut opts = opts_for_checkout();
        opts.force = true;
        let _plan = checkout_tree(&repo, target, &opts).unwrap();
        assert_eq!(read_workfile(&repo, b"a.txt"), b"target\n");
    }

    // --- 4. Refuses untracked clobber ----------------------------------------

    #[test]
    fn refuses_untracked_clobber() {
        let tmp = init_repo();
        let repo = open_repo(&tmp);

        // Empty index. An untracked foo.txt sits in the workdir.
        std::fs::write(repo.workdir().join("foo.txt"), b"untracked\n").unwrap();

        let target = write_flat_tree(&repo, &[(b"foo.txt", FileMode::Regular, b"target\n")]);

        let err = checkout_tree(&repo, target, &opts_for_checkout()).unwrap_err();
        match err {
            UnpackError::Conflicts(cs) => {
                assert_eq!(cs.len(), 1);
                assert_eq!(cs[0].path, b"foo.txt".to_vec());
                assert_eq!(cs[0].reason, ConflictReason::UntrackedClobber);
            }
            other => panic!("expected Conflicts, got {other:?}"),
        }
        // File untouched.
        assert_eq!(read_workfile(&repo, b"foo.txt"), b"untracked\n");

        // With force, it gets replaced.
        let mut opts = opts_for_checkout();
        opts.force = true;
        let _ = checkout_tree(&repo, target, &opts).unwrap();
        assert_eq!(read_workfile(&repo, b"foo.txt"), b"target\n");
    }

    // --- 5. Deletes vanished files -------------------------------------------

    #[test]
    fn deletes_files_absent_in_target() {
        let tmp = init_repo();
        let repo = open_repo(&tmp);
        stage_file(&repo, b"survivor.txt", b"yes\n", FileMode::Regular);
        stage_file(&repo, b"victim.txt", b"die\n", FileMode::Regular);

        let target = write_flat_tree(&repo, &[(b"survivor.txt", FileMode::Regular, b"yes\n")]);

        let plan = checkout_tree(&repo, target, &opts_for_checkout()).unwrap();
        assert!(plan.conflicts.is_empty());
        assert_eq!(plan.to_delete, vec![b"victim.txt".to_vec()]);
        assert!(!workfile_exists(&repo, b"victim.txt"));

        let idx = Index::read(&repo).unwrap();
        let paths: Vec<_> = idx.entries.iter().map(|e| e.path.clone()).collect();
        assert_eq!(paths, vec![b"survivor.txt".to_vec()]);
    }

    // --- 6. Mode change (Unix) ----------------------------------------------

    #[test]
    #[cfg(unix)]
    fn mode_change_executable_bit_toggles() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = init_repo();
        let repo = open_repo(&tmp);

        // Probe whether the filesystem honors the exec bit. macOS tmp paths
        // generally do; some CI setups don't.
        let probe = tmp.path().join(".probe");
        std::fs::write(&probe, b"x").unwrap();
        std::fs::set_permissions(&probe, std::fs::Permissions::from_mode(0o755)).unwrap();
        let exec_after = std::fs::metadata(&probe).unwrap().permissions().mode() & 0o111;
        let _ = std::fs::remove_file(&probe);
        if exec_after == 0 {
            eprintln!("skipping: filesystem doesn't honor exec bit");
            return;
        }

        // Stage a regular file.
        stage_file(&repo, b"script.sh", b"echo hi\n", FileMode::Regular);

        // Target makes it executable. Same content!
        let target = write_flat_tree(&repo, &[(b"script.sh", FileMode::Executable, b"echo hi\n")]);

        let plan = checkout_tree(&repo, target, &opts_for_checkout()).unwrap();
        assert!(plan.conflicts.is_empty());
        assert_eq!(plan.to_update, vec![b"script.sh".to_vec()]);

        let perms = std::fs::metadata(repo.workdir().join("script.sh"))
            .unwrap()
            .permissions()
            .mode()
            & 0o111;
        assert!(perms != 0, "exec bit should be set after checkout");
    }

    // --- 7. Plan-only does not mutate ----------------------------------------

    #[test]
    fn plan_only_does_not_mutate() {
        let tmp = init_repo();
        let repo = open_repo(&tmp);
        stage_file(&repo, b"a.txt", b"v1\n", FileMode::Regular);

        let target = write_flat_tree(&repo, &[(b"a.txt", FileMode::Regular, b"v2\n")]);

        let plan = plan_checkout(&repo, target, &opts_for_checkout()).unwrap();
        // Plan reports the would-be update.
        assert_eq!(plan.to_update, vec![b"a.txt".to_vec()]);
        // But the workdir is unchanged.
        assert_eq!(read_workfile(&repo, b"a.txt"), b"v1\n");
        // And the index still shows the old oid.
        let idx = Index::read(&repo).unwrap();
        let ent = idx.entries.iter().find(|e| e.path == b"a.txt").unwrap();
        let blob = RawObject::new(ObjectKind::Blob, b"v1\n".to_vec());
        assert_eq!(ent.oid, blob.oid(repo.hash_kind()));
    }

    // --- 8. reset_mixed: index updates, workdir untouched --------------------

    #[test]
    fn reset_mixed_updates_index_only() {
        let tmp = init_repo();
        let repo = open_repo(&tmp);
        stage_file(&repo, b"a.txt", b"alpha\n", FileMode::Regular);

        // Workdir has divergent (uncommitted) content.
        std::fs::write(repo.workdir().join("a.txt"), b"workdir-only\n").unwrap();

        let target = write_flat_tree(&repo, &[(b"a.txt", FileMode::Regular, b"target\n")]);

        reset_mixed(&repo, target).unwrap();

        // Workdir untouched: still "workdir-only".
        assert_eq!(read_workfile(&repo, b"a.txt"), b"workdir-only\n");

        // Index reflects the target.
        let idx = Index::read(&repo).unwrap();
        let ent = idx.entries.iter().find(|e| e.path == b"a.txt").unwrap();
        let target_blob = RawObject::new(ObjectKind::Blob, b"target\n".to_vec());
        assert_eq!(ent.oid, target_blob.oid(repo.hash_kind()));
    }

    // --- Bonus: keep_extra preserves files -----------------------------------

    #[test]
    fn keep_extra_preserves_index_only_paths() {
        let tmp = init_repo();
        let repo = open_repo(&tmp);
        stage_file(&repo, b"keep.txt", b"k\n", FileMode::Regular);
        stage_file(&repo, b"survives.txt", b"s\n", FileMode::Regular);

        // Target only has keep.txt.
        let target = write_flat_tree(&repo, &[(b"keep.txt", FileMode::Regular, b"k\n")]);

        let mut opts = opts_for_checkout();
        opts.keep_extra = true;
        let plan = checkout_tree(&repo, target, &opts).unwrap();
        assert!(plan.to_delete.is_empty());

        // survives.txt still on disk and in index.
        assert!(workfile_exists(&repo, b"survives.txt"));
        let idx = Index::read(&repo).unwrap();
        assert!(idx.entries.iter().any(|e| e.path == b"survives.txt"));
    }

    // --- Bonus: reset_soft is a no-op at this layer --------------------------

    #[test]
    fn reset_soft_is_noop() {
        let tmp = init_repo();
        let repo = open_repo(&tmp);
        stage_file(&repo, b"a.txt", b"alpha\n", FileMode::Regular);

        let target = write_flat_tree(&repo, &[(b"a.txt", FileMode::Regular, b"changed\n")]);
        // No-op.
        reset_soft(&repo, target).unwrap();

        // Workdir + index unchanged.
        assert_eq!(read_workfile(&repo, b"a.txt"), b"alpha\n");
        let idx = Index::read(&repo).unwrap();
        let ent = idx.entries.iter().find(|e| e.path == b"a.txt").unwrap();
        let alpha = RawObject::new(ObjectKind::Blob, b"alpha\n".to_vec());
        assert_eq!(ent.oid, alpha.oid(repo.hash_kind()));
    }

    // --- Bonus: nested directories work too ----------------------------------

    #[test]
    fn nested_paths_round_trip() {
        let tmp = init_repo();
        let repo = open_repo(&tmp);

        // Build a tree with a nested subtree.
        let inner_blob_a = write_blob(&repo, b"hello\n");
        let inner_tree = {
            let entries = vec![TreeEntry {
                mode: FileMode::Regular,
                name: b"inner.rs".to_vec(),
                oid: inner_blob_a,
            }];
            let t = Tree::new(entries);
            repo.odb().write(&t.to_object()).unwrap()
        };
        let root_blob = write_blob(&repo, b"top\n");
        let root_tree_oid = {
            let entries = vec![
                TreeEntry {
                    mode: FileMode::Regular,
                    name: b"top.txt".to_vec(),
                    oid: root_blob,
                },
                TreeEntry {
                    mode: FileMode::Tree,
                    name: b"src".to_vec(),
                    oid: inner_tree,
                },
            ];
            let t = Tree::new(entries);
            repo.odb().write(&t.to_object()).unwrap()
        };

        let plan = checkout_tree(&repo, root_tree_oid, &opts_for_checkout()).unwrap();
        assert!(plan.conflicts.is_empty());
        // Both leaves should appear in the create set.
        let mut creates = plan.to_create.clone();
        creates.sort();
        assert_eq!(creates, vec![b"src/inner.rs".to_vec(), b"top.txt".to_vec()]);

        // Workdir contains them.
        assert_eq!(read_workfile(&repo, b"top.txt"), b"top\n");
        assert_eq!(read_workfile(&repo, b"src/inner.rs"), b"hello\n");
    }

    // --- Bonus: path filter limits scope -------------------------------------

    #[test]
    fn checkout_tree_for_paths_filters_by_path() {
        let tmp = init_repo();
        let repo = open_repo(&tmp);

        stage_file(&repo, b"a.txt", b"a-orig\n", FileMode::Regular);
        stage_file(&repo, b"b.txt", b"b-orig\n", FileMode::Regular);

        // Target has new content for both, but we only restore a.txt.
        let target = write_flat_tree(
            &repo,
            &[
                (b"a.txt", FileMode::Regular, b"a-new\n"),
                (b"b.txt", FileMode::Regular, b"b-new\n"),
            ],
        );

        let filter = vec![b"a.txt".to_vec()];
        let plan = checkout_tree_for_paths(&repo, target, &filter, &opts_for_checkout()).unwrap();
        assert!(plan.conflicts.is_empty());
        assert_eq!(plan.to_update, vec![b"a.txt".to_vec()]);

        // a.txt was updated; b.txt was not.
        assert_eq!(read_workfile(&repo, b"a.txt"), b"a-new\n");
        assert_eq!(read_workfile(&repo, b"b.txt"), b"b-orig\n");

        // Index also updated only for a.txt.
        let idx = Index::read(&repo).unwrap();
        let a = idx.entries.iter().find(|e| e.path == b"a.txt").unwrap();
        let b = idx.entries.iter().find(|e| e.path == b"b.txt").unwrap();
        let a_target = RawObject::new(ObjectKind::Blob, b"a-new\n".to_vec()).oid(repo.hash_kind());
        let b_orig = RawObject::new(ObjectKind::Blob, b"b-orig\n".to_vec()).oid(repo.hash_kind());
        assert_eq!(a.oid, a_target);
        assert_eq!(b.oid, b_orig);
    }

    #[test]
    fn path_filter_matches_directory_prefix() {
        let filter = vec![b"src".to_vec()];
        assert!(path_in_filter(b"src", &filter));
        assert!(path_in_filter(b"src/lib.rs", &filter));
        assert!(path_in_filter(b"src/sub/mod.rs", &filter));
        assert!(!path_in_filter(b"srcular.txt", &filter));
        assert!(!path_in_filter(b"other", &filter));
    }

    // --- Transactional rollback ---------------------------------------------

    /// If the staged-checkout commit phase fails partway through (here we
    /// simulate it by pre-creating a directory at one of the target paths so
    /// the rename collides), the workdir must be restored to its pre-call
    /// state. None of the other planned files should have been written.
    #[test]
    #[cfg(unix)]
    fn commit_failure_rolls_back_workdir() {
        let tmp = init_repo();
        let repo = open_repo(&tmp);
        // Existing tracked file we'll be UPDATING.
        stage_file(&repo, b"keep.txt", b"original\n", FileMode::Regular);

        // Pre-place a directory at the target path of one of the new files.
        // Because the workdir is "clean" (no untracked file at that path —
        // an untracked DIR isn't a UntrackedClobber, only a TypeMismatch
        // would block, and only when --force is off). We force=true so the
        // planner accepts; the rename in commit phase will still fail
        // because rename(file → existing-non-empty-dir) is EISDIR / ENOTDIR.
        std::fs::create_dir_all(repo.workdir().join("blocked.txt/inner")).unwrap();
        std::fs::write(repo.workdir().join("blocked.txt/inner/file"), b"trapped\n").unwrap();

        let target = write_flat_tree(
            &repo,
            &[
                (b"keep.txt", FileMode::Regular, b"new\n"),
                (b"blocked.txt", FileMode::Regular, b"would not land\n"),
            ],
        );

        let mut opts = opts_for_checkout();
        opts.force = true; // skip the planner's TypeMismatch refusal
        let err = checkout_tree(&repo, target, &opts).unwrap_err();
        assert!(matches!(err, UnpackError::Io { .. }), "got {err:?}");

        // keep.txt's content should match its PRE-call value, not the new
        // content from the target tree.
        assert_eq!(
            read_workfile(&repo, b"keep.txt"),
            b"original\n",
            "keep.txt must be rolled back to its pre-call content"
        );

        // blocked.txt should still be the directory we pre-placed.
        let meta = std::fs::symlink_metadata(repo.workdir().join("blocked.txt")).unwrap();
        assert!(meta.file_type().is_dir(), "blocked.txt directory restored");
        assert_eq!(
            std::fs::read(repo.workdir().join("blocked.txt/inner/file")).unwrap(),
            b"trapped\n",
            "directory contents preserved through rollback"
        );
    }

    /// Drop of `StagedCheckout` must remove the shadow dir even if no
    /// commit was attempted (e.g. a stage error returned early).
    #[test]
    fn shadow_dir_is_cleaned_up_on_drop() {
        let tmp = init_repo();
        let repo = open_repo(&tmp);
        let gitdir = repo.gitdir().to_path_buf();
        {
            let mut staged = StagedCheckout::new(&gitdir).unwrap();
            staged
                .stage_create(repo.workdir().join("a.txt"), FileMode::Regular, b"x\n")
                .unwrap();
            // Drop without commit.
        }
        // No checkout.tmp.* dirs should remain.
        let leftovers: Vec<_> = std::fs::read_dir(&gitdir)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with("checkout.tmp."))
            .collect();
        assert!(
            leftovers.is_empty(),
            "leaked shadow dirs: {:?}",
            leftovers.iter().map(|e| e.file_name()).collect::<Vec<_>>()
        );
    }

    /// After a successful checkout, no `checkout.tmp.*` dirs should remain
    /// either — the `StagedCheckout` Drop sweeps the originals it displaced.
    #[test]
    fn shadow_dir_is_cleaned_up_after_successful_checkout() {
        let tmp = init_repo();
        let repo = open_repo(&tmp);
        stage_file(&repo, b"a.txt", b"v1\n", FileMode::Regular);
        let target = write_flat_tree(&repo, &[(b"a.txt", FileMode::Regular, b"v2\n")]);
        let _ = checkout_tree(&repo, target, &opts_for_checkout()).unwrap();

        let leftovers: Vec<_> = std::fs::read_dir(repo.gitdir())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with("checkout.tmp."))
            .collect();
        assert!(
            leftovers.is_empty(),
            "shadow dir leaked after success: {:?}",
            leftovers.iter().map(|e| e.file_name()).collect::<Vec<_>>()
        );
    }

    // -------------------------------------------------------------------
    // Quiet warnings: ensure path types we reference are imported.
    // (Path / TempDir are used above.)
    // -------------------------------------------------------------------
    #[allow(dead_code)]
    fn _silence_unused(_: &Path) {}
}
