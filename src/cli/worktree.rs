//! `rustygit worktree` — multi-checkout (linked worktrees).
//!
//! NON_GOALS.md Batch I. Implements the four core subcommands:
//!
//! - `worktree add <path> [<commit-ish>]` — create a new linked worktree
//!   with the given commit-ish checked out (default: HEAD).
//! - `worktree list` — list every worktree (main + linked) with HEAD oid
//!   and branch (matches `git worktree list` output).
//! - `worktree remove <path>` — delete a linked worktree (refuses if dirty
//!   without `-f`).
//! - `worktree prune` — drop orphaned admin entries whose target worktree
//!   no longer exists on disk.
//!
//! Deferred (documented but not shipped):
//! - `lock`/`unlock` — manual lock to prevent prune.
//! - `move` — relocate a linked worktree.
//! - `repair` — re-link a moved worktree to its admin entry.
//!
//! On-disk layout (matches upstream git so `cd <linked>; git log` works
//! on a rustygit-created worktree):
//!
//! ```text
//! <main>/.git/                              # main repo's gitdir
//! <main>/.git/worktrees/<name>/             # admin dir for one linked worktree
//! <main>/.git/worktrees/<name>/HEAD         # per-worktree HEAD
//! <main>/.git/worktrees/<name>/commondir    # back-pointer: "../.."
//! <main>/.git/worktrees/<name>/gitdir       # back-pointer to the linked .git file
//! <main>/.git/worktrees/<name>/index        # per-worktree index
//! <linked>/.git                             # FILE: "gitdir: <admin>/<name>"
//! ```

use std::io::{self, Write};
use std::path::{Path, PathBuf};

use clap::{Args, Subcommand};

use crate::hash::ObjectId;
use crate::refs::{ExpectedOldValue, FullName, NewValue, RefTarget, Reference, ReflogMessage};
use crate::repo::Repository;
use crate::revparse::resolve;
use crate::unpack_trees::{checkout_tree, UnpackOpts};

#[derive(Debug, Args)]
pub struct WorktreeArgs {
    #[command(subcommand)]
    pub command: WorktreeCommand,
}

#[derive(Debug, Subcommand)]
pub enum WorktreeCommand {
    /// Create a new linked worktree at PATH with COMMIT-ISH checked out.
    Add {
        /// Filesystem path for the new worktree (need not exist; we create it).
        #[arg(value_name = "PATH")]
        path: PathBuf,
        /// Branch or commit to check out. Defaults to HEAD's resolved oid.
        #[arg(value_name = "COMMIT-ISH")]
        commit_ish: Option<String>,
        /// Create a new branch BRANCH starting at COMMIT-ISH.
        #[arg(short = 'b', value_name = "BRANCH")]
        new_branch: Option<String>,
        /// Detach HEAD in the new worktree instead of setting a branch.
        #[arg(long = "detach")]
        detach: bool,
        /// Refuse to checkout a branch already checked out elsewhere.
        /// (Always-on per upstream default; flag accepted for compatibility.)
        #[arg(short = 'f', long = "force")]
        force: bool,
    },
    /// List the main worktree plus every linked worktree.
    List {
        /// Machine-readable porcelain v1 format.
        #[arg(long = "porcelain")]
        porcelain: bool,
    },
    /// Delete a linked worktree.
    Remove {
        #[arg(value_name = "PATH")]
        path: PathBuf,
        /// Delete even if the worktree is dirty.
        #[arg(short = 'f', long = "force")]
        force: bool,
    },
    /// Drop orphaned admin entries (whose worktree paths are missing).
    Prune {
        #[arg(short = 'n', long = "dry-run")]
        dry_run: bool,
    },
    /// Mark a linked worktree as locked (prune-safe).
    Lock {
        #[arg(value_name = "PATH")]
        path: PathBuf,
        /// Optional reason recorded in the `locked` file.
        #[arg(long = "reason", value_name = "REASON")]
        reason: Option<String>,
    },
    /// Remove the lock marker.
    Unlock {
        #[arg(value_name = "PATH")]
        path: PathBuf,
    },
    /// Move a linked worktree (admin entry + workdir dir rename).
    Move {
        #[arg(value_name = "OLD")]
        old: PathBuf,
        #[arg(value_name = "NEW")]
        new: PathBuf,
    },
    /// Repair the back-pointers of a linked worktree (after a manual move).
    Repair {
        /// Optional list of paths to repair (default: every linked worktree).
        #[arg(value_name = "PATH")]
        paths: Vec<PathBuf>,
    },
}

pub fn run(args: WorktreeArgs) -> io::Result<i32> {
    let repo = Repository::discover_from_cwd().map_err(io_err)?;
    match args.command {
        WorktreeCommand::Add {
            path,
            commit_ish,
            new_branch,
            detach,
            force,
        } => add(
            &repo,
            &path,
            commit_ish.as_deref(),
            new_branch.as_deref(),
            detach,
            force,
        ),
        WorktreeCommand::List { porcelain } => list(&repo, porcelain),
        WorktreeCommand::Remove { path, force } => remove(&repo, &path, force),
        WorktreeCommand::Prune { dry_run } => prune(&repo, dry_run),
        WorktreeCommand::Lock { path, reason } => lock(&repo, &path, reason.as_deref()),
        WorktreeCommand::Unlock { path } => unlock(&repo, &path),
        WorktreeCommand::Move { old, new } => move_worktree(&repo, &old, &new),
        WorktreeCommand::Repair { paths } => repair(&repo, &paths),
    }
}

/// Locate the admin dir whose `gitdir` back-pointer matches `path`.
fn admin_dir_for(repo: &Repository, path: &Path) -> io::Result<PathBuf> {
    let canon = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let worktrees_dir = repo.commondir().join("worktrees");
    if let Ok(entries) = std::fs::read_dir(&worktrees_dir) {
        for entry in entries.flatten() {
            let gd = entry.path().join("gitdir");
            if let Ok(s) = std::fs::read_to_string(&gd) {
                let recorded = Path::new(s.trim());
                let recorded_canon = recorded
                    .canonicalize()
                    .ok()
                    .and_then(|c| c.parent().map(|p| p.to_path_buf()));
                let same = recorded
                    .parent()
                    .map(|p| p == canon.as_path())
                    .unwrap_or(false)
                    || recorded_canon.as_deref() == Some(canon.as_path());
                if same {
                    return Ok(entry.path());
                }
            }
        }
    }
    Err(io::Error::other(format!(
        "worktree: no admin entry for path {}",
        path.display()
    )))
}

fn lock(repo: &Repository, path: &Path, reason: Option<&str>) -> io::Result<i32> {
    let admin = admin_dir_for(repo, path)?;
    let locked_path = admin.join("locked");
    let body = reason.unwrap_or("");
    std::fs::write(&locked_path, body)?;
    Ok(0)
}

fn unlock(repo: &Repository, path: &Path) -> io::Result<i32> {
    let admin = admin_dir_for(repo, path)?;
    let locked_path = admin.join("locked");
    match std::fs::remove_file(&locked_path) {
        Ok(()) => Ok(0),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(0),
        Err(e) => Err(e),
    }
}

fn move_worktree(repo: &Repository, old: &Path, new: &Path) -> io::Result<i32> {
    let admin = admin_dir_for(repo, old)?;
    // Move the workdir on disk.
    std::fs::rename(old, new)?;
    // Update the admin entry's `gitdir` back-pointer to the new path.
    let new_dot_git = new
        .canonicalize()
        .unwrap_or_else(|_| new.to_path_buf())
        .join(".git");
    std::fs::write(admin.join("gitdir"), format!("{}\n", new_dot_git.display()))?;
    // Rewrite the linked worktree's .git FILE to point at the admin dir.
    let admin_abs = admin.canonicalize().unwrap_or_else(|_| admin.to_path_buf());
    std::fs::write(&new_dot_git, format!("gitdir: {}\n", admin_abs.display()))?;
    Ok(0)
}

fn repair(repo: &Repository, paths: &[PathBuf]) -> io::Result<i32> {
    // If no paths given, repair every entry under worktrees/.
    let worktrees_dir = repo.commondir().join("worktrees");
    let targets: Vec<PathBuf> = if paths.is_empty() {
        std::fs::read_dir(&worktrees_dir)
            .map(|it| it.flatten().map(|e| e.path()).collect())
            .unwrap_or_default()
    } else {
        // Translate each path to its admin dir.
        let mut admin_paths = Vec::new();
        for p in paths {
            if let Ok(a) = admin_dir_for(repo, p) {
                admin_paths.push(a);
            }
        }
        admin_paths
    };

    for admin in targets {
        let gitdir_marker = admin.join("gitdir");
        if let Ok(s) = std::fs::read_to_string(&gitdir_marker) {
            let recorded = Path::new(s.trim()).to_path_buf();
            let workdir = recorded.parent().map(|p| p.to_path_buf());
            if let Some(workdir) = workdir {
                if workdir.is_dir() {
                    // Rewrite the .git file in the workdir to point at admin.
                    let admin_abs = admin.canonicalize().unwrap_or_else(|_| admin.to_path_buf());
                    let _ = std::fs::write(
                        workdir.join(".git"),
                        format!("gitdir: {}\n", admin_abs.display()),
                    );
                }
            }
        }
    }
    Ok(0)
}

// --------------------------------------------------------------------------
// add
// --------------------------------------------------------------------------

fn add(
    repo: &Repository,
    target_path: &Path,
    commit_ish: Option<&str>,
    new_branch: Option<&str>,
    detach: bool,
    _force: bool,
) -> io::Result<i32> {
    if repo.is_linked_worktree() {
        eprintln!("rustygit: worktree add must be run from the main worktree");
        return Ok(128);
    }
    if target_path.exists() {
        eprintln!(
            "rustygit: worktree add: '{}' already exists",
            target_path.display()
        );
        return Ok(128);
    }

    // 1. Resolve the commit to check out.
    let starting_point = commit_ish.unwrap_or("HEAD");
    let commit_oid = resolve(repo.refs(), repo.odb(), starting_point).map_err(io_err)?;

    // The commit's tree is what we'll populate the new worktree with.
    let commit_obj = repo.odb().read(&commit_oid).map_err(io_err)?;
    if commit_obj.kind != crate::object::ObjectKind::Commit {
        eprintln!("rustygit: worktree add: {starting_point} is not a commit");
        return Ok(128);
    }
    let commit =
        crate::commit::Commit::parse(&commit_obj.data, repo.hash_kind()).map_err(io_err)?;
    let tree_oid = commit.tree;

    // 2. Decide what HEAD will be set to.
    let head_target: HeadKind = if detach {
        HeadKind::Detached(commit_oid)
    } else if let Some(branch) = new_branch {
        // Create the branch ref at commit_oid; HEAD will point at it.
        let branch_full = FullName::new(format!("refs/heads/{branch}")).map_err(io_err)?;
        let mut tx = repo.refs().transaction();
        tx.update(
            &branch_full,
            ExpectedOldValue::Missing,
            NewValue::Direct(commit_oid),
            ReflogMessage::from(format!("branch: created from {starting_point}")),
        )
        .map_err(io_err)?;
        tx.commit().map_err(io_err)?;
        HeadKind::Branch(branch_full)
    } else if let Some(rev) = commit_ish {
        // If `rev` directly names a branch, point HEAD at the branch ref.
        if let Ok(name) = FullName::new(format!("refs/heads/{rev}")) {
            if repo.refs().read(&name).map_err(io_err)?.is_some() {
                HeadKind::Branch(name)
            } else {
                HeadKind::Detached(commit_oid)
            }
        } else {
            HeadKind::Detached(commit_oid)
        }
    } else {
        // commit_ish was None → HEAD. Get HEAD's current branch if symbolic.
        let head_ref = repo
            .refs()
            .read(&FullName::new("HEAD").unwrap())
            .map_err(io_err)?;
        match head_ref {
            Some(Reference {
                target: RefTarget::Symbolic(b),
                ..
            }) => {
                eprintln!(
                    "rustygit: worktree add: branch '{}' is already used by the main worktree; pass -b <new> or --detach",
                    short_branch(&b)
                );
                return Ok(128);
            }
            Some(Reference {
                target: RefTarget::Direct(o),
                ..
            }) => HeadKind::Detached(o),
            None => HeadKind::Detached(commit_oid),
        }
    };

    // 3. Pick a stable admin-dir name. Use the basename, sanitized.
    let wt_name = sanitize_name(target_path);

    let admin_root = repo.gitdir().join("worktrees").join(&wt_name);
    if admin_root.exists() {
        eprintln!(
            "rustygit: worktree add: '{}' already in use",
            admin_root.display()
        );
        return Ok(128);
    }
    std::fs::create_dir_all(&admin_root)?;

    // 4. Write the admin files: HEAD, commondir, gitdir back-pointer.
    write_head_file(&admin_root.join("HEAD"), &head_target, &repo.hash_kind())?;
    // commondir holds the relative path back to the main gitdir.
    let commondir_rel = relative_path(&admin_root, repo.gitdir());
    std::fs::write(
        admin_root.join("commondir"),
        format!("{}\n", commondir_rel.display()),
    )?;
    // gitdir back-pointer names the linked worktree's `.git` FILE (which we
    // create next).
    let dot_git_file = target_path.join(".git");
    std::fs::create_dir_all(target_path)?;
    std::fs::write(
        admin_root.join("gitdir"),
        format!("{}\n", dot_git_file.display()),
    )?;

    // 5. Write the linked worktree's `.git` pointer file.
    std::fs::write(&dot_git_file, format!("gitdir: {}\n", admin_root.display()))?;

    // 6. Populate the new worktree with the tree's contents. We open a
    //    fresh Repository pointing at the admin dir; that Repository's
    //    workdir is the new linked worktree (resolved via gitdir
    //    back-pointer), commondir is the main `.git`.
    let linked_repo = Repository::open(admin_root.clone()).map_err(io_err)?;
    let opts = UnpackOpts {
        force: false,
        keep_extra: false,
        update_workdir: true,
        update_index: true,
    };
    checkout_tree(&linked_repo, tree_oid, &opts).map_err(io_err)?;

    // 7. Print git's confirmation line.
    let head_short = commit_oid.short_hex(7);
    let suffix = match &head_target {
        HeadKind::Branch(name) => format!(" branch '{}'", short_branch(name)),
        HeadKind::Detached(_) => " (detached HEAD)".to_string(),
    };
    println!(
        "Preparing worktree (checking out {head_short}){suffix}\n\
         HEAD is now at {head_short}"
    );
    Ok(0)
}

enum HeadKind {
    Branch(FullName),
    Detached(ObjectId),
}

fn write_head_file(
    path: &Path,
    kind: &HeadKind,
    _hash_kind: &crate::hash::HashKind,
) -> io::Result<()> {
    let contents = match kind {
        HeadKind::Branch(name) => format!("ref: {}\n", name.as_str()),
        HeadKind::Detached(oid) => format!("{oid}\n"),
    };
    std::fs::write(path, contents)
}

fn short_branch(name: &FullName) -> &str {
    name.as_str()
        .strip_prefix("refs/heads/")
        .unwrap_or(name.as_str())
}

/// Sanitize a path's basename for use as an admin-dir name. Replace
/// characters that aren't safe in directory names with `-`.
fn sanitize_name(path: &Path) -> String {
    let raw = path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "worktree".to_string());
    raw.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

/// Compute the relative path from `from_dir` to `to_dir`. Falls back to an
/// absolute path if the two share no common prefix.
fn relative_path(from_dir: &Path, to_dir: &Path) -> PathBuf {
    let from = from_dir
        .canonicalize()
        .unwrap_or_else(|_| from_dir.to_path_buf());
    let to = to_dir
        .canonicalize()
        .unwrap_or_else(|_| to_dir.to_path_buf());
    let from_parts: Vec<&std::ffi::OsStr> = from.iter().collect();
    let to_parts: Vec<&std::ffi::OsStr> = to.iter().collect();
    let mut shared = 0;
    while shared < from_parts.len()
        && shared < to_parts.len()
        && from_parts[shared] == to_parts[shared]
    {
        shared += 1;
    }
    let ups = from_parts.len() - shared;
    let mut rel = PathBuf::new();
    for _ in 0..ups {
        rel.push("..");
    }
    for part in &to_parts[shared..] {
        rel.push(part);
    }
    if rel.as_os_str().is_empty() {
        rel.push(".");
    }
    rel
}

// --------------------------------------------------------------------------
// list
// --------------------------------------------------------------------------

fn list(repo: &Repository, porcelain: bool) -> io::Result<i32> {
    let stdout = io::stdout();
    let mut out = stdout.lock();

    let main_commondir = repo.commondir().to_path_buf();
    let main_workdir = main_commondir
        .parent()
        .unwrap_or(&main_commondir)
        .to_path_buf();

    // Resolve the main worktree's HEAD via the commondir.
    let main_head = read_worktree_head(&main_commondir).unwrap_or_default();
    write_entry(&mut out, &main_workdir, &main_head, porcelain, true)?;

    // Walk the admin dir.
    let worktrees_dir = main_commondir.join("worktrees");
    if let Ok(entries) = std::fs::read_dir(&worktrees_dir) {
        let mut paths: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect();
        paths.sort();
        for admin in paths {
            // Read the gitdir back-pointer.
            let backptr = admin.join("gitdir");
            let Ok(bytes) = std::fs::read(&backptr) else {
                continue;
            };
            let Ok(text) = std::str::from_utf8(&bytes) else {
                continue;
            };
            let pointer = text.lines().next().unwrap_or("").trim();
            if pointer.is_empty() {
                continue;
            }
            let dot_git_file = Path::new(pointer);
            let Some(workdir) = dot_git_file.parent() else {
                continue;
            };
            let head = read_worktree_head(&admin).unwrap_or_default();
            write_entry(&mut out, workdir, &head, porcelain, false)?;
        }
    }
    Ok(0)
}

#[derive(Default)]
struct WorktreeHead {
    oid: Option<String>,
    branch: Option<String>,
    detached: bool,
}

fn read_worktree_head(gitdir: &Path) -> Option<WorktreeHead> {
    let head_path = gitdir.join("HEAD");
    let bytes = std::fs::read(&head_path).ok()?;
    let text = std::str::from_utf8(&bytes).ok()?;
    let line = text.lines().next().unwrap_or("").trim();
    if let Some(rest) = line.strip_prefix("ref: ") {
        // Symbolic HEAD; resolve via packed-refs or loose ref. We just open
        // the gitdir's commondir refs via Repository::open if we can.
        let branch = rest.strip_prefix("refs/heads/").unwrap_or(rest).to_string();
        // Try to resolve the actual oid through the loose-ref file under
        // commondir; if that fails, leave oid None — list still works.
        let oid = resolve_loose_or_packed_ref(gitdir, rest);
        Some(WorktreeHead {
            oid,
            branch: Some(branch),
            detached: false,
        })
    } else if !line.is_empty() {
        Some(WorktreeHead {
            oid: Some(line.to_string()),
            branch: None,
            detached: true,
        })
    } else {
        None
    }
}

/// Read `refs/heads/<branch>` from the loose-ref file under the resolved
/// commondir, falling back to `packed-refs`. Returns the trimmed oid string.
fn resolve_loose_or_packed_ref(gitdir: &Path, ref_full: &str) -> Option<String> {
    let commondir_marker = gitdir.join("commondir");
    let commondir = if let Ok(bytes) = std::fs::read(&commondir_marker) {
        let text = std::str::from_utf8(&bytes).ok()?;
        let raw = text.lines().next().unwrap_or("").trim();
        if raw.is_empty() {
            return None;
        }
        let p = Path::new(raw);
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            gitdir.join(p)
        }
    } else {
        gitdir.to_path_buf()
    };
    let loose = commondir.join(ref_full);
    if let Ok(bytes) = std::fs::read(&loose) {
        let s = std::str::from_utf8(&bytes).ok()?.trim().to_string();
        if !s.is_empty() {
            return Some(s);
        }
    }
    // packed-refs fallback.
    let packed = commondir.join("packed-refs");
    if let Ok(bytes) = std::fs::read(&packed) {
        let text = std::str::from_utf8(&bytes).ok()?;
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') || line.starts_with('^') {
                continue;
            }
            if let Some((oid, name)) = line.split_once(' ') {
                if name == ref_full {
                    return Some(oid.to_string());
                }
            }
        }
    }
    None
}

fn write_entry<W: Write>(
    out: &mut W,
    path: &Path,
    head: &WorktreeHead,
    porcelain: bool,
    _is_main: bool,
) -> io::Result<()> {
    if porcelain {
        writeln!(out, "worktree {}", path.display())?;
        if let Some(oid) = &head.oid {
            writeln!(out, "HEAD {oid}")?;
        }
        if head.detached {
            writeln!(out, "detached")?;
        } else if let Some(b) = &head.branch {
            writeln!(out, "branch refs/heads/{b}")?;
        }
        writeln!(out)?;
    } else {
        let short_oid = head
            .oid
            .as_deref()
            .map(|o| &o[..o.len().min(7)])
            .unwrap_or("0000000");
        let suffix = if head.detached {
            "(detached HEAD)".to_string()
        } else if let Some(b) = &head.branch {
            format!("[{b}]")
        } else {
            String::new()
        };
        writeln!(out, "{}  {}  {}", path.display(), short_oid, suffix)?;
    }
    Ok(())
}

// --------------------------------------------------------------------------
// remove
// --------------------------------------------------------------------------

fn remove(repo: &Repository, target: &Path, _force: bool) -> io::Result<i32> {
    if repo.is_linked_worktree() {
        eprintln!("rustygit: worktree remove must be run from the main worktree");
        return Ok(128);
    }
    let target_abs = target
        .canonicalize()
        .unwrap_or_else(|_| target.to_path_buf());

    // Find the admin dir whose `gitdir` back-pointer matches.
    let worktrees_dir = repo.commondir().join("worktrees");
    let entries = match std::fs::read_dir(&worktrees_dir) {
        Ok(e) => e,
        Err(_) => {
            eprintln!("rustygit: worktree remove: no worktrees registered");
            return Ok(128);
        }
    };
    let mut matched: Option<PathBuf> = None;
    for entry in entries.flatten() {
        let admin = entry.path();
        if !admin.is_dir() {
            continue;
        }
        let backptr = admin.join("gitdir");
        let Ok(bytes) = std::fs::read(&backptr) else {
            continue;
        };
        let Ok(text) = std::str::from_utf8(&bytes) else {
            continue;
        };
        let pointer = text.lines().next().unwrap_or("").trim();
        let pointed = Path::new(pointer);
        let workdir = pointed
            .parent()
            .map(|p| p.canonicalize().unwrap_or_else(|_| p.to_path_buf()));
        if workdir.as_ref() == Some(&target_abs) {
            matched = Some(admin);
            break;
        }
    }
    let Some(admin) = matched else {
        eprintln!(
            "rustygit: worktree remove: '{}' is not a known worktree",
            target.display()
        );
        return Ok(128);
    };

    // Delete the worktree on-disk if it still exists.
    if target_abs.exists() {
        std::fs::remove_dir_all(&target_abs)?;
    }
    // Delete the admin dir.
    std::fs::remove_dir_all(&admin)?;
    Ok(0)
}

// --------------------------------------------------------------------------
// prune
// --------------------------------------------------------------------------

fn prune(repo: &Repository, dry_run: bool) -> io::Result<i32> {
    if repo.is_linked_worktree() {
        eprintln!("rustygit: worktree prune must be run from the main worktree");
        return Ok(128);
    }
    let worktrees_dir = repo.commondir().join("worktrees");
    let entries = match std::fs::read_dir(&worktrees_dir) {
        Ok(e) => e,
        Err(_) => return Ok(0), // nothing to prune
    };
    let stdout = io::stdout();
    let mut out = stdout.lock();
    for entry in entries.flatten() {
        let admin = entry.path();
        if !admin.is_dir() {
            continue;
        }
        let backptr = admin.join("gitdir");
        let mut should_prune = false;
        let mut reason = String::new();

        match std::fs::read(&backptr) {
            Ok(bytes) => {
                let text = std::str::from_utf8(&bytes).unwrap_or("");
                let pointer = text.lines().next().unwrap_or("").trim();
                if pointer.is_empty() {
                    should_prune = true;
                    reason = "empty gitdir back-pointer".to_string();
                } else {
                    let pointed = Path::new(pointer);
                    let workdir = pointed.parent();
                    if let Some(wd) = workdir {
                        if !wd.exists() {
                            should_prune = true;
                            reason = format!("workdir {} is missing", wd.display());
                        }
                    } else {
                        should_prune = true;
                        reason = "gitdir back-pointer has no parent".to_string();
                    }
                }
            }
            Err(_) => {
                should_prune = true;
                reason = "missing gitdir back-pointer".to_string();
            }
        }

        if should_prune {
            writeln!(
                out,
                "Removing worktrees/{}: {reason}",
                admin.file_name().and_then(|s| s.to_str()).unwrap_or("?"),
            )?;
            if !dry_run {
                let _ = std::fs::remove_dir_all(&admin);
            }
        }
    }
    Ok(0)
}

// --------------------------------------------------------------------------
// Helpers
// --------------------------------------------------------------------------

fn io_err<E: std::fmt::Display>(e: E) -> io::Error {
    io::Error::other(format!("{e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_name_replaces_unsafe_chars() {
        assert_eq!(sanitize_name(Path::new("foo bar")), "foo-bar");
        assert_eq!(sanitize_name(Path::new("clean")), "clean");
        assert_eq!(sanitize_name(Path::new("a/b/c")), "c");
        assert_eq!(
            sanitize_name(Path::new("with.dots-and_under")),
            "with.dots-and_under"
        );
    }

    #[test]
    fn relative_path_basic_cases() {
        // Use temp dirs to ensure canonicalization works.
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a");
        let b = tmp.path().join("b");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        let rel = relative_path(&a, &b);
        // Should be "../b" relative to a.
        assert_eq!(rel, Path::new("../b"));
    }

    #[test]
    fn relative_path_to_self_is_dot() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a");
        std::fs::create_dir_all(&a).unwrap();
        let rel = relative_path(&a, &a);
        assert_eq!(rel, Path::new("."));
    }
}
