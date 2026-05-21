//! `rustygit stash` — save, list, apply, and drop work-in-progress changes.
//!
//! Storage shape (matches upstream git):
//!
//! * `refs/stash` is a single direct ref. Each stash is a commit at the
//!   tip of `refs/stash`'s reflog. Walking `.git/logs/refs/stash` newest-
//!   first gives the `stash@{0}`, `stash@{1}`, … sequence.
//!
//! * A stash commit `S` has:
//!     - `tree = w-tree`   — the workdir snapshot at stash time.
//!     - `parent[0] = HEAD` at stash time.
//!     - `parent[1] = i-commit`, a one-off commit whose tree is `i-tree`
//!       (the staged state at stash time). Its parent is HEAD. Message:
//!       `index on <branch>: <head-sub>`.
//!     - `parent[2]` (only when `-u`/`--include-untracked` is set, NOT
//!       implemented today) — a one-off commit holding untracked files.
//!
//! * The stash commit message is the canonical
//!   `WIP on <branch>: <head-short-oid> <head-subject>` (no `-m`) or
//!   `On <branch>: <user message>` (with `-m`).
//!
//! Subcommands shipped:
//!   * `stash` / `stash push [-m <msg>]` — save the current state.
//!   * `stash list` — newest-first listing.
//!   * `stash show [stash@{N}]` — diff stash@{N} vs its first parent (HEAD-at-stash-time).
//!   * `stash apply [stash@{N}]` — re-apply, don't drop.
//!   * `stash pop [stash@{N}]` — apply + drop.
//!   * `stash drop [stash@{N}]` — remove a specific entry.
//!   * `stash clear` — remove all entries.
//!
//! NOT implemented (deferred):
//!   * `-u`/`--include-untracked`
//!   * `--keep-index`, `--patch`
//!   * `stash branch <name>` (create branch from stash)

use std::io::{self, Write};

use clap::{Args, Subcommand};

use crate::commit::Commit;
use crate::config::Config;
use crate::hash::ObjectId;
use crate::identity::{Signature, Time};
use crate::index::Index;
use crate::refs::{ExpectedOldValue, FullName, NewValue, RefTarget, ReflogMessage};
use crate::repo::Repository;
use crate::unpack_trees::{checkout_tree, UnpackOpts};

#[derive(Debug, Args)]
pub struct StashArgs {
    #[command(subcommand)]
    pub sub: Option<StashSub>,
}

#[derive(Debug, Subcommand)]
pub enum StashSub {
    /// Save the current state to a new stash (default action).
    Push(PushArgs),
    /// List recorded stashes, newest first.
    List,
    /// Show the diff between a stash and its parent commit.
    Show(IndexArg),
    /// Re-apply a stash without removing it.
    Apply(IndexArg),
    /// Apply a stash, then drop it.
    Pop(IndexArg),
    /// Remove a single stash entry by index.
    Drop(IndexArg),
    /// Remove all stash entries.
    Clear,
}

#[derive(Debug, Args)]
pub struct PushArgs {
    /// Custom message. Without it, the message is
    /// `WIP on <branch>: <head-short-oid> <head-subject>`.
    #[arg(short = 'm', long = "message")]
    pub message: Option<String>,
    /// Include untracked files in the stash (matches `git stash -u`).
    /// Adds a third parent commit whose tree holds the untracked files,
    /// and removes them from the workdir as part of the stash push.
    #[arg(short = 'u', long = "include-untracked")]
    pub include_untracked: bool,
}

#[derive(Debug, Args)]
pub struct IndexArg {
    /// `stash@{N}` or just `N`. Defaults to `stash@{0}`.
    #[arg(value_name = "STASH")]
    pub stash: Option<String>,
}

pub fn run(args: StashArgs) -> io::Result<i32> {
    let repo = Repository::discover_from_cwd().map_err(io_err)?;
    match args.sub.unwrap_or(StashSub::Push(PushArgs {
        message: None,
        include_untracked: false,
    })) {
        StashSub::Push(p) => push(&repo, p.message.as_deref(), p.include_untracked),
        StashSub::List => list(&repo),
        StashSub::Show(i) => show(&repo, i.stash.as_deref()),
        StashSub::Apply(i) => apply(&repo, i.stash.as_deref(), false),
        StashSub::Pop(i) => apply(&repo, i.stash.as_deref(), true),
        StashSub::Drop(i) => drop_entry(&repo, i.stash.as_deref()),
        StashSub::Clear => clear(&repo),
    }
}

// ---------------------------------------------------------------------------
// push
// ---------------------------------------------------------------------------

fn push(repo: &Repository, user_msg: Option<&str>, include_untracked: bool) -> io::Result<i32> {
    let (branch, head_oid) = read_head(repo)?;
    let branch_short = branch
        .as_str()
        .strip_prefix("refs/heads/")
        .unwrap_or(branch.as_str())
        .to_string();

    // 1. i-tree: write-tree on the current index.
    let i_tree = match crate::cli::write_tree::build_tree_from_index(repo) {
        Ok(o) => o,
        Err(crate::cli::write_tree::WriteTreeError::EmptyIndex) => {
            eprintln!("rustygit: stash: no changes to stash (empty index)");
            return Ok(0);
        }
        Err(e) => return Err(io_err(e)),
    };

    // 2. Build a workdir-snapshot index without touching the user's index.
    //    Strategy: read current index, replay every tracked entry from the
    //    workdir's current bytes, keep paths that no longer exist (matches
    //    `git stash`'s tracked-only behavior — deleted files stay tracked
    //    as in the index for now).
    let mut workdir_index = Index::read(repo).map_err(io_err)?;
    refresh_index_from_workdir(repo, &mut workdir_index)?;

    let w_tree = match crate::cli::write_tree::build_tree_from_index_ref(repo, &workdir_index) {
        Ok(o) => o,
        Err(crate::cli::write_tree::WriteTreeError::EmptyIndex) => {
            eprintln!("rustygit: stash: no changes to stash");
            return Ok(0);
        }
        Err(e) => return Err(io_err(e)),
    };

    // 3. Read HEAD's tree (we need it later for both the early-exit
    //    check and the workdir reset).
    let head_tree = read_commit_tree(repo, head_oid)?;

    // 4. Build identities + head subject.
    let config = Config::from_repo_dir(repo.gitdir()).map_err(io_err)?;
    let now = Time::now_local();
    let author = Signature::author_from_env_or_config(&config, now).map_err(io_err)?;
    let committer = Signature::committer_from_env_or_config(&config, now).map_err(io_err)?;
    let head_subject = head_commit_subject(repo, head_oid)?;
    let head_short = head_oid.short_hex(7);

    // 5. Create the index commit. Message: `index on <branch>: <head-sub>`.
    let i_commit_msg = format!("index on {branch_short}: {head_short} {head_subject}\n");
    let i_commit = Commit {
        tree: i_tree,
        parents: vec![head_oid],
        author: author.clone(),
        committer: committer.clone(),
        message: i_commit_msg.into_bytes(),
        encoding: None,
        gpgsig: None,
    };
    let i_commit_oid = repo.odb().write(&i_commit.to_object()).map_err(io_err)?;

    // 5b. If -u was passed, build an untracked-files commit and capture
    //     the list of files we stashed so we can delete them after the
    //     stash commit is written.
    let mut untracked_paths: Vec<Vec<u8>> = Vec::new();
    let u_commit_oid: Option<ObjectId> = if include_untracked {
        let entries = collect_untracked(repo).map_err(io_err)?;
        if entries.is_empty() {
            None
        } else {
            untracked_paths = entries.iter().map(|(p, _, _)| p.clone()).collect();
            let u_tree = build_tree_from_untracked_entries(repo, &entries).map_err(io_err)?;
            let u_commit_msg =
                format!("untracked files on {branch_short}: {head_short} {head_subject}\n");
            let u_commit = Commit {
                tree: u_tree,
                parents: Vec::new(),
                author: author.clone(),
                committer: committer.clone(),
                message: u_commit_msg.into_bytes(),
                encoding: None,
                gpgsig: None,
            };
            Some(repo.odb().write(&u_commit.to_object()).map_err(io_err)?)
        }
    } else {
        None
    };

    // If push --include-untracked was given but there are no tracked
    // changes AND no untracked files, exit 0 with no-op.
    if i_tree == head_tree && w_tree == head_tree && u_commit_oid.is_none() {
        eprintln!("rustygit: stash: no local changes to save");
        return Ok(0);
    }

    // 6. Create the stash commit. Parents are HEAD, i-commit, and
    //    (when present) u-commit — matches upstream git's 3-parent
    //    `git stash -u` shape.
    let stash_msg = match user_msg {
        Some(m) => format!("On {branch_short}: {m}\n"),
        None => format!("WIP on {branch_short}: {head_short} {head_subject}\n"),
    };
    let mut parents = vec![head_oid, i_commit_oid];
    if let Some(u) = u_commit_oid {
        parents.push(u);
    }
    let stash_commit = Commit {
        tree: w_tree,
        parents,
        author,
        committer,
        message: stash_msg.clone().into_bytes(),
        encoding: None,
        gpgsig: None,
    };
    let stash_oid = repo
        .odb()
        .write(&stash_commit.to_object())
        .map_err(io_err)?;

    // 7. Update refs/stash atomically with reflog. The reflog message is
    //    exactly the stash commit's subject line (matches git).
    let stash_ref = FullName::new("refs/stash").map_err(io_err)?;
    let mut tx = repo.refs().transaction();
    tx.update(
        &stash_ref,
        ExpectedOldValue::Any,
        NewValue::Direct(stash_oid),
        ReflogMessage::from(stash_msg.trim_end().to_string()),
    )
    .map_err(io_err)?;
    tx.commit().map_err(io_err)?;

    // 8. Reset workdir + index to HEAD's tree (the standard "stash and clean" flow).
    let head_tree = read_commit_tree(repo, head_oid)?;
    let unpack_opts = UnpackOpts {
        force: true,
        keep_extra: false,
        update_workdir: true,
        update_index: true,
    };
    checkout_tree(repo, head_tree, &unpack_opts).map_err(io_err)?;

    // 8b. checkout_tree skips workdir paths whose index oid already matches
    //     the target tree's oid — but the workdir bytes may still be dirty
    //     against the cached blob (that's the whole reason we're stashing!).
    //     Force-rewrite each tracked file from its (now-restored) index
    //     entry's blob so the workdir is byte-equal to HEAD's tree.
    materialize_index_to_workdir(repo)?;

    // 8c. With `-u`, also remove the untracked files we stashed.
    for p in &untracked_paths {
        let s = std::str::from_utf8(p).map_err(|_| io::Error::other("non-utf8 untracked path"))?;
        let abs = repo.workdir().join(s);
        match std::fs::remove_file(&abs) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(io::Error::other(format!(
                    "stash: failed to remove untracked file {s}: {e}"
                )))
            }
        }
    }

    println!(
        "Saved working directory and index state {}",
        stash_msg.trim_end()
    );
    Ok(0)
}

/// Recursively walk a tree object, writing every blob entry to the
/// workdir at its full path. Used by `stash apply/pop` to materialize
/// the untracked-files commit's tree (parent[2]) back to disk WITHOUT
/// touching the index. Refuses to overwrite an existing file (matches
/// `git stash pop -u` semantics: it refuses if the untracked file came
/// back and now collides with something the user created since).
fn restore_untracked_from_tree(repo: &Repository, tree_oid: ObjectId) -> io::Result<()> {
    walk_tree_to_workdir(repo, tree_oid, Vec::new())
}

fn walk_tree_to_workdir(repo: &Repository, tree_oid: ObjectId, prefix: Vec<u8>) -> io::Result<()> {
    let raw = repo.odb().read(&tree_oid).map_err(io_err)?;
    let tree = crate::tree::Tree::parse(&raw.data, repo.hash_kind()).map_err(io_err)?;
    for entry in &tree.entries {
        let mut full = prefix.clone();
        if !full.is_empty() {
            full.push(b'/');
        }
        full.extend_from_slice(&entry.name);

        match entry.mode {
            crate::tree::FileMode::Tree => {
                walk_tree_to_workdir(repo, entry.oid, full)?;
            }
            crate::tree::FileMode::Regular
            | crate::tree::FileMode::Executable
            | crate::tree::FileMode::Symlink => {
                let path_str = match std::str::from_utf8(&full) {
                    Ok(s) => s,
                    Err(_) => continue,
                };
                let abs = repo.workdir().join(path_str);
                if abs.exists() {
                    return Err(io::Error::other(format!(
                        "could not restore untracked file '{path_str}': already exists"
                    )));
                }
                if let Some(parent) = abs.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                let blob = repo.odb().read(&entry.oid).map_err(io_err)?;
                if entry.mode == crate::tree::FileMode::Symlink {
                    #[cfg(unix)]
                    {
                        let target = std::str::from_utf8(&blob.data)
                            .map_err(|_| io::Error::other("non-utf8 symlink target"))?;
                        std::os::unix::fs::symlink(target, &abs)?;
                    }
                    #[cfg(not(unix))]
                    {
                        std::fs::write(&abs, &blob.data)?;
                    }
                } else {
                    std::fs::write(&abs, &blob.data)?;
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        let bits = if entry.mode == crate::tree::FileMode::Executable {
                            0o755
                        } else {
                            0o644
                        };
                        std::fs::set_permissions(&abs, std::fs::Permissions::from_mode(bits))?;
                    }
                }
            }
            _ => {
                // Gitlink / unknown — skip; untracked stash entries
                // shouldn't contain these.
            }
        }
    }
    Ok(())
}

/// Collect every untracked, non-gitignored file in the workdir.
/// Returns `(rel_path, FileMode, blob_oid)` tuples; the blobs have
/// already been written to the odb so the caller can build a tree.
///
/// Reuses the status walker so the gitignore semantics match
/// `rustygit status`'s "?" classification exactly.
fn collect_untracked(
    repo: &Repository,
) -> io::Result<Vec<(Vec<u8>, crate::tree::FileMode, ObjectId)>> {
    let report = crate::worktree::status::status(repo).map_err(io_err)?;
    let mut out = Vec::new();
    for entry in &report.entries {
        if entry.worktree_state != crate::worktree::status::WorktreeState::Untracked {
            continue;
        }
        let path_str = match std::str::from_utf8(&entry.path) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let abs = repo.workdir().join(path_str);
        let meta = match std::fs::symlink_metadata(&abs) {
            Ok(m) => m,
            Err(_) => continue,
        };
        let mode = if meta.file_type().is_symlink() {
            crate::tree::FileMode::Symlink
        } else {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if meta.permissions().mode() & 0o111 != 0 {
                    crate::tree::FileMode::Executable
                } else {
                    crate::tree::FileMode::Regular
                }
            }
            #[cfg(not(unix))]
            {
                crate::tree::FileMode::Regular
            }
        };
        let payload = if meta.file_type().is_symlink() {
            std::fs::read_link(&abs)?
                .as_os_str()
                .to_string_lossy()
                .into_owned()
                .into_bytes()
        } else if meta.is_file() {
            std::fs::read(&abs)?
        } else {
            // Directories show up in status as untracked too (collapsed);
            // skip — git stash -u doesn't recurse into them through the
            // collapsed form, and our `status` already iterates files.
            continue;
        };
        let oid = repo
            .odb()
            .write(&crate::object::RawObject::new(
                crate::object::ObjectKind::Blob,
                payload,
            ))
            .map_err(io_err)?;
        out.push((entry.path.clone(), mode, oid));
    }
    Ok(out)
}

/// Build a tree object whose entries are exactly the given (path, mode, oid)
/// triples. Internally reuses the index → tree path: stage every entry into
/// an in-memory `Index`, then call [`crate::cli::write_tree::build_tree_from_index_ref`].
fn build_tree_from_untracked_entries(
    repo: &Repository,
    entries: &[(Vec<u8>, crate::tree::FileMode, ObjectId)],
) -> io::Result<ObjectId> {
    let mut idx = Index::empty(2);
    for (path, mode, oid) in entries {
        idx.upsert(crate::index::IndexEntry {
            ctime_s: 0,
            ctime_n: 0,
            mtime_s: 0,
            mtime_n: 0,
            dev: 0,
            ino: 0,
            mode: mode.to_index_mode(),
            uid: 0,
            gid: 0,
            size: 0,
            oid: *oid,
            flags: path.len().min(0xFFF) as u16,
            path: path.clone(),
            stage: 0,
            assume_valid: false,
            extended: false,
            extended_flags: 0,
        });
    }
    idx.sort();
    crate::cli::write_tree::build_tree_from_index_ref(repo, &idx).map_err(io_err)
}

/// Walk the current index and write each entry's cached blob to the
/// workdir at its path. Used after stash push to guarantee the workdir
/// is byte-equal to the (just-reset) index. Re-stats every written file
/// and updates the index entry's cached stat fields so `git status` /
/// `git stash apply` don't see false-positive "dirty workdir" signals.
fn materialize_index_to_workdir(repo: &Repository) -> io::Result<()> {
    let mut idx = Index::read(repo).map_err(io_err)?;
    for entry in idx.entries.iter_mut() {
        let path = std::str::from_utf8(&entry.path)
            .map_err(|_| io::Error::other("non-utf8 path in index"))?;
        let abs = repo.workdir().join(path);
        let raw = repo.odb().read(&entry.oid).map_err(io_err)?;
        if let Some(parent) = abs.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mode = crate::tree::FileMode::from_index_mode(entry.mode)
            .map_err(|e| io::Error::other(format!("bad mode {:o}: {e}", entry.mode)))?;
        if mode == crate::tree::FileMode::Symlink {
            let _ = std::fs::remove_file(&abs);
            #[cfg(unix)]
            {
                let target = std::str::from_utf8(&raw.data)
                    .map_err(|_| io::Error::other("non-utf8 symlink target"))?;
                std::os::unix::fs::symlink(target, &abs)?;
            }
            #[cfg(not(unix))]
            {
                std::fs::write(&abs, &raw.data)?;
            }
        } else {
            std::fs::write(&abs, &raw.data)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let perm_bits = if mode == crate::tree::FileMode::Executable {
                    0o755
                } else {
                    0o644
                };
                std::fs::set_permissions(&abs, std::fs::Permissions::from_mode(perm_bits))?;
            }
        }

        // Refresh the entry's cached stat from the just-written file so
        // subsequent `git status` calls treat the workdir as clean.
        if let Ok(meta) = std::fs::symlink_metadata(&abs) {
            update_entry_stat(entry, &meta);
        }
    }
    idx.write(repo).map_err(io_err)?;
    Ok(())
}

fn update_entry_stat(entry: &mut crate::index::IndexEntry, meta: &std::fs::Metadata) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        entry.ctime_s = meta.ctime() as u32;
        entry.ctime_n = meta.ctime_nsec() as u32;
        entry.mtime_s = meta.mtime() as u32;
        entry.mtime_n = meta.mtime_nsec() as u32;
        entry.dev = meta.dev() as u32;
        entry.ino = meta.ino() as u32;
        entry.uid = meta.uid();
        entry.gid = meta.gid();
        entry.size = meta.size().min(u32::MAX as u64) as u32;
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        let _ = entry;
    }
}

// ---------------------------------------------------------------------------
// list
// ---------------------------------------------------------------------------

fn list(repo: &Repository) -> io::Result<i32> {
    let entries = read_stash_reflog(repo)?;
    let stdout = io::stdout();
    let mut out = stdout.lock();
    for (i, entry) in entries.iter().rev().enumerate() {
        writeln!(out, "stash@{{{i}}}: {}", entry.message)?;
    }
    Ok(0)
}

// ---------------------------------------------------------------------------
// show
// ---------------------------------------------------------------------------

fn show(repo: &Repository, sel: Option<&str>) -> io::Result<i32> {
    let (oid, _idx) = resolve_stash(repo, sel)?;
    let commit = read_commit(repo, oid)?;
    let base = commit
        .parents
        .first()
        .copied()
        .ok_or_else(|| io::Error::other("stash commit has no parent"))?;

    // Use existing diff-tree machinery: base = HEAD-at-stash-time,
    // b-side = stash's workdir-snapshot tree.
    let stdout = io::stdout();
    let mut out = stdout.lock();
    crate::diff::diff_two_trees(repo, base, commit.tree, &mut out).map_err(io_err)?;
    Ok(0)
}

// ---------------------------------------------------------------------------
// apply / pop
// ---------------------------------------------------------------------------

fn apply(repo: &Repository, sel: Option<&str>, drop_after: bool) -> io::Result<i32> {
    let (stash_oid, n) = resolve_stash(repo, sel)?;
    let stash_commit = read_commit(repo, stash_oid)?;
    let w_tree = stash_commit.tree;
    let i_commit_oid = *stash_commit.parents.get(1).ok_or_else(|| {
        io::Error::other("stash commit missing index-commit parent; not a valid stash entry")
    })?;
    let i_tree = read_commit_tree(repo, i_commit_oid)?;
    // Optional 3rd parent: the untracked-files commit (from `git stash -u`).
    let u_tree: Option<ObjectId> = match stash_commit.parents.get(2).copied() {
        Some(u_commit) => Some(read_commit_tree(repo, u_commit)?),
        None => None,
    };

    // Restore the workdir to w_tree (force=true matches `git stash apply`'s
    // default: it overwrites tracked files).
    let opts = UnpackOpts {
        force: true,
        keep_extra: true,
        update_workdir: true,
        update_index: false,
    };
    checkout_tree(repo, w_tree, &opts).map_err(io_err)?;

    // Now overwrite the index to i_tree so the staged state is restored.
    let opts_idx = UnpackOpts {
        force: true,
        keep_extra: true,
        update_workdir: false,
        update_index: true,
    };
    checkout_tree(repo, i_tree, &opts_idx).map_err(io_err)?;

    // If the stash had a 3rd parent, restore each untracked file from
    // that tree to the workdir WITHOUT touching the index (matches
    // `git stash pop`'s `-u` restoration: they go back as untracked).
    if let Some(u) = u_tree {
        restore_untracked_from_tree(repo, u)?;
    }

    if drop_after {
        drop_at_index(repo, n)?;
        println!("Dropped stash@{{{n}}}.");
    } else {
        let entries = read_stash_reflog(repo)?;
        if let Some(entry) = entries.iter().rev().nth(n) {
            println!("Applied stash@{{{n}}}: {}", entry.message);
        } else {
            println!("Applied stash@{{{n}}}.");
        }
    }
    Ok(0)
}

// ---------------------------------------------------------------------------
// drop
// ---------------------------------------------------------------------------

fn drop_entry(repo: &Repository, sel: Option<&str>) -> io::Result<i32> {
    let (_oid, n) = resolve_stash(repo, sel)?;
    drop_at_index(repo, n)?;
    println!("Dropped stash@{{{n}}}.");
    Ok(0)
}

/// Remove the reflog entry at index `n` (counted newest-first), then
/// update `refs/stash` to the new tip (or delete the ref if no entries
/// remain).
fn drop_at_index(repo: &Repository, n: usize) -> io::Result<()> {
    let log_path = repo.gitdir().join("logs/refs/stash");
    let bytes = std::fs::read(&log_path).map_err(io_err)?;
    let mut lines: Vec<Vec<u8>> = bytes
        .split(|&b| b == b'\n')
        .filter(|l| !l.is_empty())
        .map(|l| l.to_vec())
        .collect();
    if n >= lines.len() {
        return Err(io::Error::other(format!(
            "stash@{{{n}}} is not a valid reference (only {} stash entries)",
            lines.len()
        )));
    }
    // n=0 is the newest = last in file order.
    let last_idx = lines.len() - 1 - n;
    lines.remove(last_idx);

    let stash_ref = FullName::new("refs/stash").map_err(io_err)?;
    if lines.is_empty() {
        // Remove the ref and the reflog file entirely.
        let mut tx = repo.refs().transaction();
        tx.delete(&stash_ref, ExpectedOldValue::Any)
            .map_err(io_err)?;
        tx.commit().map_err(io_err)?;
        // Also drop the reflog file so list() doesn't see ghost entries.
        let _ = std::fs::remove_file(&log_path);
        return Ok(());
    }

    // Rewrite the reflog file in-place.
    let mut out = Vec::with_capacity(bytes.len());
    for l in &lines {
        out.extend_from_slice(l);
        out.push(b'\n');
    }
    std::fs::write(&log_path, &out).map_err(io_err)?;

    // Re-point refs/stash at the newest remaining entry's "new" oid. We
    // write the loose ref file directly to avoid appending a new reflog
    // entry — `tx.update` would record a "drop stash@{N}" line that we
    // don't want (matches git: `git stash drop` doesn't add a reflog
    // entry, it just rewrites both files).
    let new_tip = parse_new_oid_from_reflog_line(lines.last().unwrap())?;
    let ref_path = repo.gitdir().join(stash_ref.as_str());
    if let Some(parent) = ref_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&ref_path, format!("{new_tip}\n")).map_err(io_err)?;
    Ok(())
}

fn parse_new_oid_from_reflog_line(line: &[u8]) -> io::Result<ObjectId> {
    // `<old> <new> <ident...>\t<msg>`
    let head = match line.iter().position(|&b| b == b'\t') {
        Some(t) => &line[..t],
        None => line,
    };
    let mut toks = head.split(|&b| b == b' ');
    let _old = toks.next();
    let new = toks
        .next()
        .ok_or_else(|| io::Error::other("malformed reflog line"))?;
    let hex = std::str::from_utf8(new).map_err(|_| io::Error::other("non-utf8 oid"))?;
    ObjectId::parse_hex(crate::hash::HashKind::Sha1, hex.trim()).map_err(io_err)
}

// ---------------------------------------------------------------------------
// clear
// ---------------------------------------------------------------------------

fn clear(repo: &Repository) -> io::Result<i32> {
    let stash_ref = FullName::new("refs/stash").map_err(io_err)?;
    if repo.refs().read(&stash_ref).map_err(io_err)?.is_none() {
        return Ok(0);
    }
    let mut tx = repo.refs().transaction();
    tx.delete(&stash_ref, ExpectedOldValue::Any)
        .map_err(io_err)?;
    tx.commit().map_err(io_err)?;
    let _ = std::fs::remove_file(repo.gitdir().join("logs/refs/stash"));
    Ok(0)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn read_head(repo: &Repository) -> io::Result<(FullName, ObjectId)> {
    let head_name = FullName::new("HEAD").map_err(io_err)?;
    let head_ref = repo
        .refs()
        .read(&head_name)
        .map_err(io_err)?
        .ok_or_else(|| io::Error::other("HEAD missing"))?;
    let branch = match head_ref.target {
        RefTarget::Symbolic(b) => b,
        RefTarget::Direct(_) => {
            return Err(io::Error::other("detached HEAD is not supported"));
        }
    };
    let oid = match repo.refs().read(&branch).map_err(io_err)? {
        Some(r) => match r.target {
            RefTarget::Direct(o) => o,
            RefTarget::Symbolic(_) => {
                return Err(io::Error::other(format!("{branch} resolves to symbolic")));
            }
        },
        None => {
            return Err(io::Error::other(format!("{branch} does not exist")));
        }
    };
    Ok((branch, oid))
}

fn read_commit(repo: &Repository, oid: ObjectId) -> io::Result<Commit> {
    let raw = repo.odb().read(&oid).map_err(io_err)?;
    Commit::parse(&raw.data, repo.hash_kind()).map_err(io_err)
}

fn read_commit_tree(repo: &Repository, oid: ObjectId) -> io::Result<ObjectId> {
    Ok(read_commit(repo, oid)?.tree)
}

fn head_commit_subject(repo: &Repository, oid: ObjectId) -> io::Result<String> {
    let commit = read_commit(repo, oid)?;
    let nl = commit
        .message
        .iter()
        .position(|&b| b == b'\n')
        .unwrap_or(commit.message.len());
    Ok(String::from_utf8_lossy(&commit.message[..nl]).into_owned())
}

/// Walk tracked entries in the workdir and re-hash any that changed,
/// upserting them into `idx`. Entries whose workdir file is missing are
/// LEFT in the index (their oid still points at the cached blob —
/// matches `git stash`'s tracked-only mode where deletes stay staged).
fn refresh_index_from_workdir(repo: &Repository, idx: &mut Index) -> io::Result<()> {
    let workdir = repo.workdir().to_path_buf();
    let paths: Vec<Vec<u8>> = idx.entries.iter().map(|e| e.path.clone()).collect();
    for path in paths {
        let path_str =
            std::str::from_utf8(&path).map_err(|_| io::Error::other("non-utf8 path in index"))?;
        let abs = workdir.join(path_str);
        let meta = match std::fs::symlink_metadata(&abs) {
            Ok(m) => m,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e),
        };
        let mode = if meta.file_type().is_symlink() {
            crate::tree::FileMode::Symlink
        } else {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if meta.permissions().mode() & 0o111 != 0 {
                    crate::tree::FileMode::Executable
                } else {
                    crate::tree::FileMode::Regular
                }
            }
            #[cfg(not(unix))]
            {
                crate::tree::FileMode::Regular
            }
        };
        let payload = if meta.file_type().is_symlink() {
            std::fs::read_link(&abs)?
                .as_os_str()
                .to_string_lossy()
                .into_owned()
                .into_bytes()
        } else {
            std::fs::read(&abs)?
        };
        let oid = repo
            .odb()
            .write(&crate::object::RawObject::new(
                crate::object::ObjectKind::Blob,
                payload,
            ))
            .map_err(io_err)?;
        // Find the entry and update only mode + oid; preserve stat bits.
        if let Some(entry) = idx.entries.iter_mut().find(|e| e.path == path) {
            entry.mode = mode.to_index_mode();
            entry.oid = oid;
        }
    }
    idx.sort();
    Ok(())
}

#[derive(Debug)]
struct ReflogLine {
    new_oid: ObjectId,
    message: String,
}

fn read_stash_reflog(repo: &Repository) -> io::Result<Vec<ReflogLine>> {
    let path = repo.gitdir().join("logs/refs/stash");
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut out = Vec::new();
    for line in bytes.split(|&b| b == b'\n') {
        if line.is_empty() {
            continue;
        }
        let (head, msg) = match line.iter().position(|&b| b == b'\t') {
            Some(t) => (&line[..t], &line[t + 1..]),
            None => (line, &b""[..]),
        };
        let mut toks = head.split(|&b| b == b' ');
        let _old = toks.next();
        let new = match toks.next() {
            Some(t) => t,
            None => continue,
        };
        let hex = match std::str::from_utf8(new) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let new_oid = match ObjectId::parse_hex(crate::hash::HashKind::Sha1, hex.trim()) {
            Ok(o) => o,
            Err(_) => continue,
        };
        out.push(ReflogLine {
            new_oid,
            message: String::from_utf8_lossy(msg).into_owned(),
        });
    }
    Ok(out)
}

/// Parse a `stash@{N}` or bare `N` selector. Returns the (oid, n).
fn resolve_stash(repo: &Repository, sel: Option<&str>) -> io::Result<(ObjectId, usize)> {
    let n = parse_stash_index(sel)?;
    let entries = read_stash_reflog(repo)?;
    if entries.is_empty() {
        return Err(io::Error::other("no stash entries"));
    }
    let total = entries.len();
    if n >= total {
        return Err(io::Error::other(format!(
            "stash@{{{n}}} is not a valid reference (only {total} stash entries)"
        )));
    }
    // n=0 = newest = last in file order.
    let entry = &entries[total - 1 - n];
    Ok((entry.new_oid, n))
}

fn parse_stash_index(sel: Option<&str>) -> io::Result<usize> {
    let s = sel.unwrap_or("stash@{0}");
    // Accept "stash@{N}", "N", or "@{N}".
    let inner = if let Some(rest) = s.strip_prefix("stash@{").and_then(|r| r.strip_suffix('}')) {
        rest
    } else if let Some(rest) = s.strip_prefix("@{").and_then(|r| r.strip_suffix('}')) {
        rest
    } else {
        s
    };
    inner
        .parse::<usize>()
        .map_err(|_| io::Error::other(format!("bad stash selector {s:?}; expected stash@{{N}}")))
}

fn io_err<E: std::fmt::Display>(e: E) -> io::Error {
    io::Error::other(format!("{e}"))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_default_selector() {
        assert_eq!(parse_stash_index(None).unwrap(), 0);
    }

    #[test]
    fn parses_explicit_stash_at_n() {
        assert_eq!(parse_stash_index(Some("stash@{3}")).unwrap(), 3);
    }

    #[test]
    fn parses_short_at_n() {
        assert_eq!(parse_stash_index(Some("@{5}")).unwrap(), 5);
    }

    #[test]
    fn parses_bare_index() {
        assert_eq!(parse_stash_index(Some("2")).unwrap(), 2);
    }

    #[test]
    fn rejects_unparseable() {
        assert!(parse_stash_index(Some("garbage")).is_err());
    }
}
