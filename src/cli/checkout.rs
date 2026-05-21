//! `rustygit checkout` — switch branches or restore a detached HEAD.
//!
//! Forms supported in M6:
//! - `checkout <branch>`: HEAD becomes a symbolic ref to `refs/heads/<branch>`,
//!   the index and working tree are updated to match the branch's tree.
//! - `checkout <oid>`: detached-HEAD checkout — HEAD becomes a direct ref to
//!   `<oid>`, the index and working tree are updated to that commit's tree.
//! - `checkout -b <name> [<start>]`: create `refs/heads/<name>` pointing at
//!   `<start>` (or HEAD when omitted) and check it out.
//! - `-f/--force`: skip the dirty-workdir conflict check.
//!
//! Pathspec form (`checkout <commit> -- <path>`) is deferred — see TODO at the
//! bottom of this file.

use std::io;

use clap::Args;

use crate::commit::Commit;
use crate::hash::ObjectId;
use crate::hooks::{self, HookRunner};
use crate::object::ObjectKind;
use crate::refs::{ExpectedOldValue, FullName, NewValue, RefTarget, ReflogMessage};
use crate::repo::Repository;
use crate::revparse::resolve;
use crate::unpack_trees::{self, UnpackError, UnpackOpts};

#[derive(Debug, Args)]
pub struct CheckoutArgs {
    /// Force checkout even if there are local modifications.
    #[arg(short = 'f', long = "force")]
    pub force: bool,

    /// Create and check out a new branch.
    #[arg(short = 'b', value_name = "NEW_BRANCH")]
    pub new_branch: Option<String>,

    /// Target: branch name, oid, or oid-ish.
    #[arg(value_name = "TARGET")]
    pub target: String,
}

pub fn run(args: CheckoutArgs) -> io::Result<i32> {
    let repo = Repository::discover_from_cwd().map_err(io_err)?;

    // Capture HEAD state before mutating anything — we need the "old name"
    // for the reflog message.
    let head_before = read_head_summary(&repo)?;

    // -b: create a new branch starting at <target> (or HEAD if no second arg).
    // The CheckoutArgs shape only takes one positional, so when -b is set
    // <target> is the new-branch start point. To match git's `checkout -b foo`
    // (no start point given), we treat the trailing `<target>` as the start
    // and require either a real start or "HEAD" — which `clap` can't enforce
    // for us cleanly; the conventional rustygit invocation is `-b name HEAD`.
    if let Some(new_branch_name) = &args.new_branch {
        return run_create_branch(
            &repo,
            &head_before,
            new_branch_name,
            &args.target,
            args.force,
        );
    }

    // Try as a branch name first: `refs/heads/<target>`.
    let branch_full = FullName::new(format!("refs/heads/{}", args.target)).ok();
    let branch_exists = match &branch_full {
        Some(name) => repo.refs().read(name).map_err(io_err)?.is_some(),
        None => false,
    };

    if branch_exists {
        let branch_full = branch_full.expect("checked above");
        let target_oid = resolve(repo.refs(), repo.odb(), &args.target).map_err(io_err)?;
        let target_tree = peel_to_tree(&repo, target_oid)?;

        let opts = UnpackOpts {
            force: args.force,
            keep_extra: false,
            update_workdir: true,
            update_index: true,
        };
        if let Err(e) = unpack_trees::checkout_tree(&repo, target_tree, &opts) {
            return handle_unpack_err("checkout", e);
        }

        // HEAD = symbolic to refs/heads/<branch>, with reflog.
        update_head_symbolic(
            &repo,
            &branch_full,
            target_oid,
            &format!(
                "checkout: moving from {} to {}",
                head_before.display_name, args.target
            ),
        )?;

        let branch_short = args.target.as_str();
        println!("Switched to branch '{branch_short}'");

        fire_post_checkout(&repo, head_before.direct_oid, Some(target_oid), true);
        return Ok(0);
    }

    // Otherwise, treat as a commit-ish for a detached-HEAD checkout.
    let target_oid = match resolve(repo.refs(), repo.odb(), &args.target) {
        Ok(o) => o,
        Err(e) => {
            eprintln!(
                "error: pathspec '{}' did not match any file(s) known to rustygit",
                args.target
            );
            eprintln!("rustygit: checkout: {e}");
            return Ok(1);
        }
    };
    let target_tree = peel_to_tree(&repo, target_oid)?;

    let opts = UnpackOpts {
        force: args.force,
        keep_extra: false,
        update_workdir: true,
        update_index: true,
    };
    if let Err(e) = unpack_trees::checkout_tree(&repo, target_tree, &opts) {
        return handle_unpack_err("checkout", e);
    }

    // Detached HEAD: HEAD = direct(target_oid).
    let head_name = FullName::new("HEAD").map_err(io_err)?;
    let mut tx = repo.refs().transaction();
    tx.update(
        &head_name,
        ExpectedOldValue::Any,
        NewValue::Direct(target_oid),
        ReflogMessage::from(format!(
            "checkout: moving from {} to {}",
            head_before.display_name, target_oid
        )),
    )
    .map_err(io_err)?;
    tx.commit().map_err(io_err)?;

    print_detached_summary(&repo, target_oid)?;
    fire_post_checkout(&repo, head_before.direct_oid, Some(target_oid), true);
    Ok(0)
}

fn run_create_branch(
    repo: &Repository,
    head_before: &HeadSummary,
    new_name: &str,
    start_point: &str,
    force: bool,
) -> io::Result<i32> {
    // 1. Resolve the start point (a commit-ish — typically HEAD).
    let start_oid = resolve(repo.refs(), repo.odb(), start_point).map_err(io_err)?;
    let start_tree = peel_to_tree(repo, start_oid)?;

    // 2. Create refs/heads/<new_name> pointing at start_oid.
    let branch_full = FullName::new(format!("refs/heads/{}", new_name)).map_err(io_err)?;
    let mut tx = repo.refs().transaction();
    tx.update(
        &branch_full,
        ExpectedOldValue::Missing,
        NewValue::Direct(start_oid),
        ReflogMessage::from(format!("branch: Created from {start_point}")),
    )
    .map_err(io_err)?;
    tx.commit().map_err(io_err)?;

    // 3. Update workdir/index to match.
    let opts = UnpackOpts {
        force,
        keep_extra: false,
        update_workdir: true,
        update_index: true,
    };
    if let Err(e) = unpack_trees::checkout_tree(repo, start_tree, &opts) {
        return handle_unpack_err("checkout", e);
    }

    // 4. Move HEAD to point at the new branch.
    update_head_symbolic(
        repo,
        &branch_full,
        start_oid,
        &format!(
            "checkout: moving from {} to {}",
            head_before.display_name, new_name
        ),
    )?;

    println!("Switched to a new branch '{new_name}'");
    fire_post_checkout(repo, head_before.direct_oid, Some(start_oid), true);
    Ok(0)
}

/// Fire post-checkout with the canonical 3-arg signature.
/// Missing oids (e.g. unborn HEAD) are passed as the null oid, matching
/// upstream git's "first parameter given to the hook is the null-ref"
/// behavior for clone.
pub(crate) fn fire_post_checkout(
    repo: &Repository,
    old: Option<ObjectId>,
    new: Option<ObjectId>,
    is_branch_checkout: bool,
) {
    let runner = HookRunner::from_repo(repo);
    let null = ObjectId::null(repo.hash_kind()).to_string();
    let old_s = old.map(|o| o.to_string()).unwrap_or_else(|| null.clone());
    let new_s = new.map(|o| o.to_string()).unwrap_or_else(|| null.clone());
    let flag = if is_branch_checkout { "1" } else { "0" };
    match runner.run("post-checkout", &[&old_s, &new_s, flag], None) {
        Ok(crate::hooks::HookOutcome::Ran { exit_code }) if exit_code != 0 => {
            hooks::print_warning("checkout", "post-checkout", exit_code);
        }
        _ => {}
    }
}

/// Update HEAD to a symbolic ref pointing at `branch`. The reflog entry uses
/// `target_oid` (the resolved tip the branch now points at) as the "new"
/// oid — symbolic-ref writes themselves don't get a reflog at the refs layer
/// (see `transaction::apply_update`), but we still record the move on HEAD's
/// own log via a Direct write... actually, since `RefTarget` is symbolic, the
/// transaction layer will skip the reflog. To match `git checkout`'s behavior
/// of logging on `HEAD`, we record an additional direct entry on HEAD as well.
///
/// In practice: the loose ref store's `apply_update` only writes a reflog
/// when `NewValue::Direct(new_oid)` was supplied. So for symbolic writes we
/// also append a HEAD reflog entry by hand here.
fn update_head_symbolic(
    repo: &Repository,
    branch: &FullName,
    target_oid: ObjectId,
    reflog_msg: &str,
) -> io::Result<()> {
    use crate::refs::reflog::{append, Identity, ReflogEntry};

    let head_name = FullName::new("HEAD").map_err(io_err)?;

    // Read the current direct oid HEAD resolves to (for the reflog "old" field).
    let old_oid = match RefTarget::resolve(repo.refs(), &head_name).map_err(io_err)? {
        Some((_, o)) => o,
        None => ObjectId::null(repo.hash_kind()),
    };

    let mut tx = repo.refs().transaction();
    tx.update(
        &head_name,
        ExpectedOldValue::Any,
        NewValue::Symbolic(branch.clone()),
        ReflogMessage::none(), // symbolic writes aren't logged at the txn layer
    )
    .map_err(io_err)?;
    tx.commit().map_err(io_err)?;

    // Append the HEAD reflog entry by hand.
    let identity = Identity::from_env_or_placeholder();
    let _ = append(
        repo.gitdir(),
        &head_name,
        ReflogEntry {
            old: old_oid,
            new: target_oid,
            identity: &identity,
            message: reflog_msg,
        },
    );

    Ok(())
}

fn handle_unpack_err(cmd: &str, e: UnpackError) -> io::Result<i32> {
    match e {
        UnpackError::Conflicts(conflicts) => {
            print_conflicts(cmd, &conflicts);
            Ok(1)
        }
        other => Err(io_err(other)),
    }
}

pub(crate) fn print_conflicts(cmd: &str, conflicts: &[crate::unpack_trees::UnpackConflict]) {
    use crate::unpack_trees::ConflictReason;

    // Bucket by reason so we emit git's familiar "Your local changes ..." vs.
    // "The following untracked working tree files would be overwritten ..." vs.
    // a generic message for type mismatches.
    let mut local: Vec<&[u8]> = Vec::new();
    let mut untracked: Vec<&[u8]> = Vec::new();
    let mut typed: Vec<&[u8]> = Vec::new();
    for c in conflicts {
        match c.reason {
            ConflictReason::LocalModifications => local.push(&c.path),
            ConflictReason::UntrackedClobber => untracked.push(&c.path),
            ConflictReason::TypeMismatch => typed.push(&c.path),
        }
    }

    if !local.is_empty() {
        eprintln!(
            "error: Your local changes to the following files would be overwritten by {cmd}:"
        );
        for p in &local {
            eprintln!("\t{}", String::from_utf8_lossy(p));
        }
        eprintln!("Please commit your changes or stash them before you switch branches.");
    }
    if !untracked.is_empty() {
        eprintln!(
            "error: The following untracked working tree files would be overwritten by {cmd}:"
        );
        for p in &untracked {
            eprintln!("\t{}", String::from_utf8_lossy(p));
        }
        eprintln!("Please move or remove them before you switch branches.");
    }
    if !typed.is_empty() {
        eprintln!("error: The following paths have type mismatches that prevent {cmd}:");
        for p in &typed {
            eprintln!("\t{}", String::from_utf8_lossy(p));
        }
    }
    eprintln!("Aborting");
}

pub(crate) fn peel_to_tree(repo: &Repository, oid: ObjectId) -> io::Result<ObjectId> {
    let obj = repo.odb().read(&oid).map_err(io_err)?;
    match obj.kind {
        ObjectKind::Tree => Ok(oid),
        ObjectKind::Commit => {
            let c = Commit::parse(&obj.data, repo.hash_kind()).map_err(io_err)?;
            Ok(c.tree)
        }
        ObjectKind::Tag => {
            // Walk one level into the tag and recurse.
            let body = std::str::from_utf8(&obj.data).map_err(io_err)?;
            for line in body.lines() {
                if let Some(rest) = line.strip_prefix("object ") {
                    let next =
                        ObjectId::parse_hex(repo.hash_kind(), rest.trim()).map_err(io_err)?;
                    return peel_to_tree(repo, next);
                }
                if line.is_empty() {
                    break;
                }
            }
            Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("tag {oid} missing 'object' line"),
            ))
        }
        other => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{oid} is a {other}, not commit-ish"),
        )),
    }
}

#[derive(Debug, Clone)]
pub(crate) struct HeadSummary {
    /// What to put in the reflog as the "from" name. Branch short name when
    /// HEAD is symbolic; full oid otherwise; the literal `(unborn)` when there
    /// is no commit yet.
    pub display_name: String,
    /// Resolved direct oid HEAD currently points at, when one exists.
    /// (Kept for future use by callers that want to e.g. set the
    /// `ExpectedOldValue` of a HEAD-direct update.)
    #[allow(dead_code)]
    pub direct_oid: Option<ObjectId>,
}

pub(crate) fn read_head_summary(repo: &Repository) -> io::Result<HeadSummary> {
    let head_name = FullName::new("HEAD").map_err(io_err)?;
    let head = repo.refs().read(&head_name).map_err(io_err)?;
    match head {
        None => Ok(HeadSummary {
            display_name: "(unborn)".into(),
            direct_oid: None,
        }),
        Some(r) => match r.target {
            RefTarget::Symbolic(name) => {
                let display = name
                    .as_str()
                    .strip_prefix("refs/heads/")
                    .unwrap_or(name.as_str())
                    .to_string();
                let direct = RefTarget::resolve(repo.refs(), &name)
                    .map_err(io_err)?
                    .map(|(_, o)| o);
                Ok(HeadSummary {
                    display_name: display,
                    direct_oid: direct,
                })
            }
            RefTarget::Direct(oid) => Ok(HeadSummary {
                display_name: oid.to_string(),
                direct_oid: Some(oid),
            }),
        },
    }
}

fn print_detached_summary(repo: &Repository, oid: ObjectId) -> io::Result<()> {
    let obj = repo.odb().read(&oid).map_err(io_err)?;
    let summary = if obj.kind == ObjectKind::Commit {
        match Commit::parse(&obj.data, repo.hash_kind()) {
            Ok(c) => first_line_of(&c.message),
            Err(_) => String::new(),
        }
    } else {
        String::new()
    };
    let short = oid.short_hex(7);
    if summary.is_empty() {
        println!("HEAD is now at {short}");
    } else {
        println!("HEAD is now at {short} {summary}");
    }
    Ok(())
}

pub(crate) fn first_line_of(msg: &[u8]) -> String {
    let s = String::from_utf8_lossy(msg);
    s.lines().next().unwrap_or("").to_string()
}

pub(crate) fn io_err<E: std::fmt::Display>(e: E) -> io::Error {
    io::Error::other(format!("{e}"))
}

// TODO(M6+): pathspec form `checkout [<commit>] -- <pathspec>...` to
// restore individual files. The shape lives in `restore`; we'll either
// dispatch into `restore::run` here or share its core function once the
// argv shape is finalized.

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Debug, Parser)]
    struct Wrap {
        #[command(flatten)]
        args: CheckoutArgs,
    }

    #[test]
    fn parses_branch_target() {
        let w = Wrap::try_parse_from(["x", "main"]).unwrap();
        assert_eq!(w.args.target, "main");
        assert!(!w.args.force);
        assert!(w.args.new_branch.is_none());
    }

    #[test]
    fn parses_force_and_create() {
        let w = Wrap::try_parse_from(["x", "-f", "-b", "feature", "HEAD"]).unwrap();
        assert!(w.args.force);
        assert_eq!(w.args.new_branch.as_deref(), Some("feature"));
        assert_eq!(w.args.target, "HEAD");
    }
}
