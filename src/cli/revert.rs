//! `rustygit revert` — apply the inverse of one or more commits.
//!
//! Implementation strategy: reuse the cherry-pick sequencer with
//! [`ApplyOpts::revert`] set to true. The sequencer swaps the base /
//! theirs trees so the 3-way merge applies the inverse diff, writes
//! `REVERT_HEAD` instead of `CHERRY_PICK_HEAD` on conflict, and uses
//! the canonical revert commit message
//! (`Revert "<title>"\n\nThis reverts commit <oid>.`).

use std::io;

use clap::Args;

use crate::hash::ObjectId;
use crate::refs::{FullName, RefTarget};
use crate::repo::Repository;
use crate::revparse;
use crate::sequencer::{
    abort, apply_commit, cont, ApplyOpts, ApplyOutcome, ContinueOutcome, SequencerError, State,
};

#[derive(Debug, Args)]
pub struct RevertArgs {
    /// Continue an in-progress revert after resolving conflicts.
    #[arg(long = "continue")]
    pub cont: bool,
    /// Abort an in-progress revert.
    #[arg(long = "abort")]
    pub abort: bool,
    /// `--mainline N` — when reverting a merge commit, pick which parent
    /// to keep (1-indexed). Required for any multi-parent commit.
    #[arg(short = 'm', long = "mainline", value_name = "N")]
    pub mainline: Option<usize>,
    /// Commits to revert. Accepts oids, ref names, OR range expressions
    /// like `A..B` (newest-first inclusive of B, exclusive of A).
    #[arg(value_name = "COMMIT")]
    pub commits: Vec<String>,
}

pub fn run(args: RevertArgs) -> io::Result<i32> {
    let repo = Repository::discover_from_cwd().map_err(io_err)?;

    if args.cont && args.abort {
        eprintln!("rustygit: revert: --continue and --abort are mutually exclusive");
        return Ok(129);
    }

    if args.abort {
        return run_abort(&repo);
    }

    if args.cont {
        return run_continue(&repo);
    }

    if args.commits.is_empty() {
        eprintln!("rustygit: revert: <commit>... required");
        return Ok(129);
    }
    run_fresh(&repo, &args.commits, args.mainline)
}

fn run_abort(repo: &Repository) -> io::Result<i32> {
    if !State::exists(repo) {
        eprintln!("rustygit: revert: no revert in progress");
        return Ok(128);
    }
    abort(repo).map_err(io_err)?;
    println!("Aborted revert.");
    Ok(0)
}

fn run_continue(repo: &Repository) -> io::Result<i32> {
    if !State::exists(repo) {
        eprintln!("rustygit: revert: no revert in progress");
        return Ok(128);
    }
    match cont(repo).map_err(io_err)? {
        ContinueOutcome::Done => {
            println!("Revert complete.");
            Ok(0)
        }
        ContinueOutcome::Conflicted {
            commit,
            offending_paths,
        } => {
            print_conflict_summary(commit, &offending_paths);
            Ok(1)
        }
    }
}

fn run_fresh(repo: &Repository, commits: &[String], mainline: Option<usize>) -> io::Result<i32> {
    if State::exists(repo) {
        eprintln!(
            "rustygit: revert: a sequencer operation is already in progress.\n\
             hint: use --continue after resolving, or --abort to give up."
        );
        return Ok(128);
    }

    // Resolve every input. Range expressions (`A..B`, `A...B`) expand
    // into the list of commits reachable from B but not from A. Single
    // refs/oids resolve directly. Order is preserved across args, and
    // within a range it's newest-first (matches `git revert A..B`).
    let mut oids: Vec<ObjectId> = Vec::with_capacity(commits.len());
    for c in commits {
        match revparse::resolve_range(repo.refs(), repo.odb(), c) {
            Ok(Some(range)) => oids.extend(range),
            Ok(None) => {
                let oid = revparse::resolve(repo.refs(), repo.odb(), c)
                    .map_err(|e| io::Error::other(format!("bad revision {c:?}: {e}")))?;
                oids.push(oid);
            }
            Err(e) => return Err(io::Error::other(format!("bad range {c:?}: {e}"))),
        }
    }

    let (branch, head_oid) = read_head_branch(repo)?;

    let mut state = State {
        head_branch: branch,
        orig_head: head_oid,
        onto: head_oid,
        todo: oids.clone(),
        done: Vec::new(),
        in_progress: None,
        revert: true,
    };
    state.save(repo).map_err(io_err)?;

    let opts = ApplyOpts {
        preserve_author: false, // revert commits get the current identity
        override_message: None,
        theirs_label: "revert".into(),
        revert: true,
        mainline,
    };

    while let Some(next) = state.todo.first().copied() {
        match apply_commit(repo, next, &opts) {
            Ok(ApplyOutcome::Done { new_commit }) => {
                println!(
                    "[{}] Revert \"{}\"",
                    new_commit.short_hex(7),
                    short_subject(repo, next)?,
                );
                state.todo.remove(0);
                state.done.push(next);
                state.save(repo).map_err(io_err)?;
            }
            Ok(ApplyOutcome::Empty) => {
                println!(
                    "Skipping revert of {}: change already absent from HEAD.",
                    next.short_hex(7)
                );
                state.todo.remove(0);
                state.save(repo).map_err(io_err)?;
            }
            Ok(ApplyOutcome::Conflicted { offending_paths }) => {
                print_conflict_summary(next, &offending_paths);
                state.in_progress = Some(next);
                state.todo.remove(0);
                state.save(repo).map_err(io_err)?;
                return Ok(1);
            }
            Err(SequencerError::DetachedHead) => {
                eprintln!("rustygit: revert: detached HEAD is not supported");
                let _ = State::cleanup(repo);
                return Ok(128);
            }
            Err(SequencerError::MergeNeedsMainline(oid)) => {
                eprintln!(
                    "rustygit: revert: {} is a merge commit — pass -m <N> to pick the parent to keep",
                    oid.short_hex(7)
                );
                let _ = State::cleanup(repo);
                return Ok(128);
            }
            Err(e) => return Err(io_err(e)),
        }
    }

    State::cleanup(repo).map_err(io_err)?;
    if state.done.len() > 1 {
        println!("Reverted {} commits.", state.done.len());
    }
    Ok(0)
}

fn short_subject(repo: &Repository, oid: ObjectId) -> io::Result<String> {
    let raw = repo.odb().read(&oid).map_err(io_err)?;
    let commit = crate::commit::Commit::parse(&raw.data, repo.hash_kind()).map_err(io_err)?;
    let nl = commit
        .message
        .iter()
        .position(|&b| b == b'\n')
        .unwrap_or(commit.message.len());
    Ok(String::from_utf8_lossy(&commit.message[..nl]).into_owned())
}

fn read_head_branch(repo: &Repository) -> io::Result<(FullName, ObjectId)> {
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

fn print_conflict_summary(commit: ObjectId, paths: &[Vec<u8>]) {
    eprintln!("error: could not revert {}", commit.short_hex(7));
    for p in paths {
        eprintln!(
            "CONFLICT (content): Merge conflict in {}",
            String::from_utf8_lossy(p)
        );
    }
    eprintln!(
        "hint: after resolving the conflicts, mark them with 'rustygit add'\n\
         hint: then run 'rustygit revert --continue'.\n\
         hint: To abort, run 'rustygit revert --abort'."
    );
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
    use clap::Parser;

    #[derive(Parser, Debug)]
    struct Wrap {
        #[command(flatten)]
        args: RevertArgs,
    }

    #[test]
    fn parses_single_commit() {
        let w = Wrap::try_parse_from(["test", "deadbeef"]).unwrap();
        assert_eq!(w.args.commits, vec!["deadbeef".to_string()]);
        assert!(!w.args.cont);
        assert!(!w.args.abort);
    }

    #[test]
    fn parses_continue_no_args() {
        let w = Wrap::try_parse_from(["test", "--continue"]).unwrap();
        assert!(w.args.cont);
        assert!(w.args.commits.is_empty());
    }

    #[test]
    fn parses_abort() {
        let w = Wrap::try_parse_from(["test", "--abort"]).unwrap();
        assert!(w.args.abort);
    }

    #[test]
    fn parses_multiple_commits() {
        let w = Wrap::try_parse_from(["test", "A", "B", "C"]).unwrap();
        assert_eq!(
            w.args.commits,
            vec!["A".to_string(), "B".to_string(), "C".to_string()]
        );
    }
}
