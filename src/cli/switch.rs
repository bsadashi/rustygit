//! `rustygit switch` — branch switching with stricter semantics than
//! `checkout`.
//!
//! Differences from `checkout`:
//! - The positional target *must* be a branch unless `--detach` is supplied.
//! - There is no pathspec form, so this command never restores individual
//!   paths.
//! - `-c/-C` create a new branch and switch to it (we accept `-c` only —
//!   `-C` would force-overwrite, which is a porcelain-polish item).
//!
//! Refusal to clobber dirty state is the unpack-trees engine's job. We pass
//! `force: false` by default; `--force` flips it.

use std::io;

use clap::Args;

use crate::cli::checkout::{
    fire_post_checkout, first_line_of, io_err, peel_to_tree, print_conflicts, read_head_summary,
};
use crate::commit::Commit;
use crate::object::ObjectKind;
use crate::refs::{ExpectedOldValue, FullName, NewValue, ReflogMessage};
use crate::repo::Repository;
use crate::revparse::resolve;
use crate::unpack_trees::{self, UnpackError, UnpackOpts};

#[derive(Debug, Args)]
pub struct SwitchArgs {
    /// Force switch even if there are local modifications.
    #[arg(short = 'f', long = "force")]
    pub force: bool,

    /// Create and switch to a new branch.
    #[arg(short = 'c', value_name = "NEW_BRANCH")]
    pub create: Option<String>,

    /// Switch to a detached HEAD at the given commit.
    #[arg(long = "detach")]
    pub detach: bool,

    /// Target: branch name (or commit-ish if `--detach`). With `-c` defaults
    /// to HEAD (i.e. branch from current commit).
    #[arg(value_name = "TARGET", default_value = "HEAD")]
    pub target: String,
}

pub fn run(args: SwitchArgs) -> io::Result<i32> {
    let repo = Repository::discover_from_cwd().map_err(io_err)?;
    let head_before = read_head_summary(&repo)?;

    // -c <new-branch>: create from <target> (typically HEAD), then switch.
    if let Some(new_name) = &args.create {
        return run_create(&repo, &head_before, new_name, &args.target, args.force);
    }

    // --detach <commit-ish>
    if args.detach {
        let oid = match resolve(repo.refs(), repo.odb(), &args.target) {
            Ok(o) => o,
            Err(e) => {
                eprintln!("fatal: invalid reference: {}", args.target);
                eprintln!("rustygit: switch: {e}");
                return Ok(128);
            }
        };
        let tree = peel_to_tree(&repo, oid)?;

        let opts = UnpackOpts {
            force: args.force,
            keep_extra: false,
            update_workdir: true,
            update_index: true,
        };
        if let Err(e) = unpack_trees::checkout_tree(&repo, tree, &opts) {
            return handle_unpack_err(e);
        }

        let head_name = FullName::new("HEAD").map_err(io_err)?;
        let mut tx = repo.refs().transaction();
        tx.update(
            &head_name,
            ExpectedOldValue::Any,
            NewValue::Direct(oid),
            ReflogMessage::from(format!(
                "checkout: moving from {} to {}",
                head_before.display_name, oid
            )),
        )
        .map_err(io_err)?;
        tx.commit().map_err(io_err)?;

        print_detached_summary(&repo, oid)?;
        fire_post_checkout(&repo, head_before.direct_oid, Some(oid), true);
        return Ok(0);
    }

    // Plain `switch <branch>`: target must be a branch.
    let branch_full = match FullName::new(format!("refs/heads/{}", args.target)) {
        Ok(n) => n,
        Err(e) => {
            eprintln!("fatal: invalid branch name '{}': {}", args.target, e);
            return Ok(128);
        }
    };

    if repo.refs().read(&branch_full).map_err(io_err)?.is_none() {
        eprintln!("fatal: invalid reference: {}", args.target);
        eprintln!(
            "rustygit: switch: did you mean `--detach` or `-c {}`?",
            args.target
        );
        return Ok(128);
    }

    let target_oid = resolve(repo.refs(), repo.odb(), &args.target).map_err(io_err)?;
    let target_tree = peel_to_tree(&repo, target_oid)?;

    let opts = UnpackOpts {
        force: args.force,
        keep_extra: false,
        update_workdir: true,
        update_index: true,
    };
    if let Err(e) = unpack_trees::checkout_tree(&repo, target_tree, &opts) {
        return handle_unpack_err(e);
    }

    update_head_symbolic_with_log(
        &repo,
        &branch_full,
        target_oid,
        &format!(
            "checkout: moving from {} to {}",
            head_before.display_name, args.target
        ),
    )?;

    println!("Switched to branch '{}'", args.target);
    fire_post_checkout(&repo, head_before.direct_oid, Some(target_oid), true);
    Ok(0)
}

fn run_create(
    repo: &Repository,
    head_before: &crate::cli::checkout::HeadSummary,
    new_name: &str,
    start_point: &str,
    force: bool,
) -> io::Result<i32> {
    let start_oid = resolve(repo.refs(), repo.odb(), start_point).map_err(io_err)?;
    let start_tree = peel_to_tree(repo, start_oid)?;

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

    let opts = UnpackOpts {
        force,
        keep_extra: false,
        update_workdir: true,
        update_index: true,
    };
    if let Err(e) = unpack_trees::checkout_tree(repo, start_tree, &opts) {
        return handle_unpack_err(e);
    }

    update_head_symbolic_with_log(
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

fn update_head_symbolic_with_log(
    repo: &Repository,
    branch: &FullName,
    target_oid: crate::hash::ObjectId,
    reflog_msg: &str,
) -> io::Result<()> {
    use crate::refs::reflog::{append, Identity, ReflogEntry};
    use crate::refs::RefTarget;

    let head_name = FullName::new("HEAD").map_err(io_err)?;
    let old_oid = match RefTarget::resolve(repo.refs(), &head_name).map_err(io_err)? {
        Some((_, o)) => o,
        None => crate::hash::ObjectId::null(repo.hash_kind()),
    };

    let mut tx = repo.refs().transaction();
    tx.update(
        &head_name,
        ExpectedOldValue::Any,
        NewValue::Symbolic(branch.clone()),
        ReflogMessage::none(),
    )
    .map_err(io_err)?;
    tx.commit().map_err(io_err)?;

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

fn handle_unpack_err(e: UnpackError) -> io::Result<i32> {
    match e {
        UnpackError::Conflicts(conflicts) => {
            print_conflicts("checkout", &conflicts);
            Ok(1)
        }
        other => Err(io_err(other)),
    }
}

fn print_detached_summary(repo: &Repository, oid: crate::hash::ObjectId) -> io::Result<()> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Debug, Parser)]
    struct Wrap {
        #[command(flatten)]
        args: SwitchArgs,
    }

    #[test]
    fn parses_branch() {
        let w = Wrap::try_parse_from(["x", "main"]).unwrap();
        assert_eq!(w.args.target, "main");
        assert!(!w.args.detach);
        assert!(w.args.create.is_none());
    }

    #[test]
    fn parses_create() {
        let w = Wrap::try_parse_from(["x", "-c", "topic", "HEAD"]).unwrap();
        assert_eq!(w.args.create.as_deref(), Some("topic"));
        assert_eq!(w.args.target, "HEAD");
    }

    #[test]
    fn parses_detach() {
        let w = Wrap::try_parse_from(["x", "--detach", "HEAD~1"]).unwrap();
        assert!(w.args.detach);
        assert_eq!(w.args.target, "HEAD~1");
    }
}
