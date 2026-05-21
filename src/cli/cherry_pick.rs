//! `rustygit cherry-pick` — apply commits from another branch on top of HEAD.
//!
//! Algorithm:
//!   1. If `--abort`: run `sequencer::abort` and exit.
//!   2. If `--continue`: assert the user has resolved + committed the
//!      previously-conflicted apply, then call `sequencer::cont`.
//!   3. Otherwise: resolve each `<commit>` argv to an oid, build a `State`
//!      describing the run, save it, then drain commit-by-commit. On the
//!      first conflict we stop (leaving state on disk).
//!
//! Out of scope for M14:
//! * `--no-commit` (apply changes but don't commit).
//! * `-X<strategy-option>`.
//! * Range expressions `A..B` — only single oids/revspecs.
//! * `-x` (append `(cherry picked from commit <oid>)` to the message).
//! * `--mainline` for cherry-picking a merge commit.

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
pub struct CherryPickArgs {
    /// Continue an in-progress cherry-pick after resolving conflicts.
    #[arg(long = "continue")]
    pub cont: bool,
    /// Abort an in-progress cherry-pick.
    #[arg(long = "abort")]
    pub abort: bool,
    /// Don't preserve original authorship (use current identity).
    #[arg(long = "reset-author")]
    pub reset_author: bool,
    /// Commits to apply, in order.
    #[arg(value_name = "COMMIT")]
    pub commits: Vec<String>,
}

pub fn run(args: CherryPickArgs) -> io::Result<i32> {
    let repo = Repository::discover_from_cwd().map_err(io_err)?;

    // Validate mutually-exclusive modes.
    if args.cont && args.abort {
        eprintln!("rustygit: cherry-pick: --continue and --abort are mutually exclusive");
        return Ok(129);
    }

    if args.abort {
        return run_abort(&repo);
    }

    if args.cont {
        return run_continue(&repo, args.reset_author);
    }

    // Fresh cherry-pick.
    if args.commits.is_empty() {
        eprintln!("rustygit: cherry-pick: <commit>... required");
        return Ok(129);
    }
    run_fresh(&repo, &args.commits, args.reset_author)
}

fn run_abort(repo: &Repository) -> io::Result<i32> {
    if !State::exists(repo) {
        eprintln!("rustygit: cherry-pick: no cherry-pick in progress");
        return Ok(128);
    }
    abort(repo).map_err(io_err)?;
    println!("Aborted cherry-pick.");
    Ok(0)
}

fn run_continue(repo: &Repository, reset_author: bool) -> io::Result<i32> {
    let _ = reset_author; // not currently respected during --continue resume
    if !State::exists(repo) {
        eprintln!("rustygit: cherry-pick: no cherry-pick in progress");
        return Ok(128);
    }
    match cont(repo).map_err(io_err)? {
        ContinueOutcome::Done => {
            println!("Cherry-pick complete.");
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

fn run_fresh(repo: &Repository, commits: &[String], reset_author: bool) -> io::Result<i32> {
    if State::exists(repo) {
        eprintln!(
            "rustygit: cherry-pick: a cherry-pick is already in progress.\n\
             hint: use --continue after resolving, or --abort to give up."
        );
        return Ok(128);
    }

    // Resolve every input to an oid up-front so we fail fast on a typo.
    let mut oids: Vec<ObjectId> = Vec::with_capacity(commits.len());
    for c in commits {
        let oid = revparse::resolve(repo.refs(), repo.odb(), c)
            .map_err(|e| io::Error::other(format!("bad revision {c:?}: {e}")))?;
        oids.push(oid);
    }

    // Snapshot HEAD before we start. Used as orig_head + onto.
    let (branch, head_oid) = read_head_branch(repo)?;

    // Save initial state so abort/continue work even if we crash mid-loop.
    let mut state = State {
        head_branch: branch,
        orig_head: head_oid,
        onto: head_oid,
        todo: oids.clone(),
        done: Vec::new(),
        in_progress: None,
        revert: false,
    };
    state.save(repo).map_err(io_err)?;

    let opts = ApplyOpts {
        preserve_author: !reset_author,
        override_message: None,
        theirs_label: "cherry-pick".into(),
        revert: false,
        mainline: None,
    };

    while let Some(next) = state.todo.first().copied() {
        match apply_commit(repo, next, &opts) {
            Ok(ApplyOutcome::Done { new_commit }) => {
                println!("[{}] picked {}", new_commit.short_hex(7), next.short_hex(7));
                state.todo.remove(0);
                state.done.push(next);
                state.save(repo).map_err(io_err)?;
            }
            Ok(ApplyOutcome::Empty) => {
                println!(
                    "Skipping {}: change already present in HEAD.",
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
                eprintln!("rustygit: cherry-pick: detached HEAD is not supported");
                let _ = State::cleanup(repo);
                return Ok(128);
            }
            Err(e) => return Err(io_err(e)),
        }
    }

    State::cleanup(repo).map_err(io_err)?;
    if state.done.len() > 1 {
        println!("Cherry-picked {} commits.", state.done.len());
    }
    Ok(0)
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
    eprintln!("error: could not apply {}", commit.short_hex(7));
    for p in paths {
        eprintln!(
            "CONFLICT (content): Merge conflict in {}",
            String::from_utf8_lossy(p)
        );
    }
    eprintln!(
        "hint: after resolving the conflicts, mark them with 'git add'\n\
         hint: then run 'rustygit cherry-pick --continue'.\n\
         hint: To abort and get back to the state before, run 'rustygit cherry-pick --abort'."
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

    /// Wrap so we can invoke clap parsing for just CherryPickArgs.
    #[derive(Parser, Debug)]
    struct Wrap {
        #[command(flatten)]
        args: CherryPickArgs,
    }

    /// `cherry-pick <oid>` — basic single-commit form.
    #[test]
    fn parses_single_commit() {
        let w = Wrap::try_parse_from(["test", "deadbeef"]).unwrap();
        assert_eq!(w.args.commits, vec!["deadbeef".to_string()]);
        assert!(!w.args.cont);
        assert!(!w.args.abort);
        assert!(!w.args.reset_author);
    }

    /// `cherry-pick --continue` — no positional args needed.
    #[test]
    fn parses_continue() {
        let w = Wrap::try_parse_from(["test", "--continue"]).unwrap();
        assert!(w.args.cont);
        assert!(!w.args.abort);
        assert!(w.args.commits.is_empty());
    }

    /// `cherry-pick --abort`.
    #[test]
    fn parses_abort() {
        let w = Wrap::try_parse_from(["test", "--abort"]).unwrap();
        assert!(w.args.abort);
        assert!(!w.args.cont);
    }

    /// `cherry-pick A B C` — multiple commits, preserves order.
    #[test]
    fn parses_multiple_commits() {
        let w = Wrap::try_parse_from(["test", "A", "B", "C"]).unwrap();
        assert_eq!(
            w.args.commits,
            vec!["A".to_string(), "B".to_string(), "C".to_string()]
        );
    }

    /// `cherry-pick --reset-author <oid>` — flag flips preserve_author off.
    #[test]
    fn parses_reset_author() {
        let w = Wrap::try_parse_from(["test", "--reset-author", "abc"]).unwrap();
        assert!(w.args.reset_author);
        assert_eq!(w.args.commits, vec!["abc".to_string()]);
    }

    /// `cherry-pick --abort --continue` parses (we check the conflict
    /// programmatically in `run`).
    #[test]
    fn parses_abort_and_continue_both_set() {
        let w = Wrap::try_parse_from(["test", "--abort", "--continue"]).unwrap();
        assert!(w.args.cont);
        assert!(w.args.abort);
    }
}
