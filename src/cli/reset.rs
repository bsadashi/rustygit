//! `rustygit reset` — move HEAD's branch (and optionally the index/workdir)
//! to a target commit-ish.
//!
//! Modes (mutually exclusive; default is `--mixed`):
//! - `--soft`: move the current branch only. Index + working tree are
//!   untouched.
//! - `--mixed` (default): move the branch and replace the index with the
//!   target's tree. Working tree is untouched.
//! - `--hard`: move the branch, replace the index, AND replace the working
//!   tree with the target's tree.
//!
//! `reset` always operates on the *currently-checked-out branch*. When HEAD
//! is detached, soft/mixed/hard still work — they update HEAD itself
//! (direct) instead of a branch ref.

use std::io;

use clap::Args;

use crate::cli::checkout::{first_line_of, io_err, peel_to_tree};
use crate::commit::Commit;
use crate::hash::ObjectId;
use crate::object::ObjectKind;
use crate::refs::{ExpectedOldValue, FullName, NewValue, RefTarget, ReflogMessage};
use crate::repo::Repository;
use crate::revparse::resolve;
use crate::unpack_trees::{self, UnpackError, UnpackOpts};

#[derive(Debug, Args)]
pub struct ResetArgs {
    /// Update HEAD only; leave the index and working tree alone.
    #[arg(long = "soft", conflicts_with_all = ["mixed", "hard"])]
    pub soft: bool,

    /// Update HEAD and the index; leave the working tree alone. Default.
    #[arg(long = "mixed", conflicts_with_all = ["soft", "hard"])]
    pub mixed: bool,

    /// Update HEAD, the index, and the working tree.
    #[arg(long = "hard", conflicts_with_all = ["soft", "mixed"])]
    pub hard: bool,

    /// Target commit. Defaults to HEAD.
    #[arg(value_name = "COMMIT", default_value = "HEAD")]
    pub target: String,
}

pub fn run(args: ResetArgs) -> io::Result<i32> {
    let repo = Repository::discover_from_cwd().map_err(io_err)?;

    // Resolve the target to a commit-ish.
    let target_oid = match resolve(repo.refs(), repo.odb(), &args.target) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("fatal: ambiguous argument '{}': {}", args.target, e);
            return Ok(128);
        }
    };

    // Decide which ref we're moving.
    let head_name = FullName::new("HEAD").map_err(io_err)?;
    let head_ref = repo.refs().read(&head_name).map_err(io_err)?;
    let (ref_to_update, current_oid) = match head_ref {
        Some(r) => match r.target {
            RefTarget::Symbolic(branch) => {
                let cur = match RefTarget::resolve(repo.refs(), &branch).map_err(io_err)? {
                    Some((_, o)) => o,
                    None => {
                        eprintln!("fatal: failed to resolve '{}' as a tree", branch);
                        return Ok(128);
                    }
                };
                (branch, cur)
            }
            RefTarget::Direct(oid) => (head_name.clone(), oid),
        },
        None => {
            eprintln!("fatal: HEAD does not exist");
            return Ok(128);
        }
    };

    let mode = if args.soft {
        Mode::Soft
    } else if args.hard {
        Mode::Hard
    } else {
        Mode::Mixed
    };

    // For mixed/hard we need the target tree for the unpack-trees engine.
    let target_tree = if matches!(mode, Mode::Mixed | Mode::Hard) {
        Some(peel_to_tree(&repo, target_oid)?)
    } else {
        None
    };

    // Apply tree-level changes BEFORE moving the ref. If unpack fails we
    // haven't yet desynced HEAD from the workdir.
    match mode {
        Mode::Soft => {
            // Nothing to do; soft only moves HEAD.
            unpack_trees::reset_soft(&repo, target_oid).map_err(io_err)?;
        }
        Mode::Mixed => {
            let tree = target_tree.expect("set above");
            unpack_trees::reset_mixed(&repo, tree).map_err(io_err)?;
        }
        Mode::Hard => {
            let tree = target_tree.expect("set above");
            let opts = UnpackOpts {
                force: true,
                keep_extra: false,
                update_index: true,
                update_workdir: true,
            };
            if let Err(e) = unpack_trees::checkout_tree(&repo, tree, &opts) {
                return handle_unpack_err(e);
            }
        }
    }

    // Move the ref.
    let mut tx = repo.refs().transaction();
    tx.update(
        &ref_to_update,
        ExpectedOldValue::Direct(current_oid),
        NewValue::Direct(target_oid),
        ReflogMessage::from(format!("reset: moving to {}", args.target)),
    )
    .map_err(io_err)?;
    tx.commit().map_err(io_err)?;

    // Match git's stdout summary on `--hard`.
    if matches!(mode, Mode::Hard) {
        print_hard_summary(&repo, target_oid)?;
    } else if matches!(mode, Mode::Mixed) {
        // git --mixed says nothing on success unless paths are partially-
        // tracked, which we don't yet detect.
    }
    Ok(0)
}

#[derive(Debug, Clone, Copy)]
enum Mode {
    Soft,
    Mixed,
    Hard,
}

fn handle_unpack_err(e: UnpackError) -> io::Result<i32> {
    match e {
        UnpackError::Conflicts(conflicts) => {
            crate::cli::checkout::print_conflicts("reset", &conflicts);
            Ok(1)
        }
        other => Err(io_err(other)),
    }
}

fn print_hard_summary(repo: &Repository, oid: ObjectId) -> io::Result<()> {
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
        args: ResetArgs,
    }

    #[test]
    fn defaults_to_mixed() {
        let w = Wrap::try_parse_from(["x"]).unwrap();
        assert!(!w.args.soft);
        assert!(!w.args.mixed);
        assert!(!w.args.hard);
        assert_eq!(w.args.target, "HEAD");
    }

    #[test]
    fn parses_hard_with_target() {
        let w = Wrap::try_parse_from(["x", "--hard", "HEAD~1"]).unwrap();
        assert!(w.args.hard);
        assert_eq!(w.args.target, "HEAD~1");
    }

    #[test]
    fn rejects_conflicting_modes() {
        assert!(Wrap::try_parse_from(["x", "--soft", "--hard"]).is_err());
        assert!(Wrap::try_parse_from(["x", "--mixed", "--hard"]).is_err());
    }
}
