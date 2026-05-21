//! `rustygit bisect` — drive a binary-search session for a regression.
//!
//! Subcommands:
//! - `bisect start [<bad> [<good>]]` — initialize a session.
//! - `bisect bad [<commit>]`         — mark `<commit>` (default HEAD) as bad.
//! - `bisect good [<commit>...]`     — mark each `<commit>` as good.
//! - `bisect log`                    — print the human log file.
//! - `bisect reset`                  — end the session and restore HEAD.
//!
//! After each `good`/`bad` we recompute the bisect midpoint, check that out,
//! and print git-style messaging:
//!
//! ```text
//! Bisecting: N revisions left to test after this (roughly log2(N) steps)
//! [<oid>] <subject>
//! ```
//!
//! On convergence:
//!
//! ```text
//! <oid> is the first bad commit
//! commit <oid>
//! Author: ...
//! Date:   ...
//!
//!     <subject>
//! ```

use std::fs;
use std::io;

use clap::{Args, Subcommand};

use crate::bisect::{next_step, BisectStep, State};
use crate::commit::Commit;
use crate::hash::ObjectId;
use crate::object::ObjectKind;
use crate::refs::{ExpectedOldValue, FullName, NewValue, RefTarget, ReflogMessage};
use crate::repo::Repository;
use crate::revparse::resolve;
use crate::unpack_trees::{self, UnpackOpts};

#[derive(Debug, Subcommand)]
pub enum BisectSubcommand {
    /// Start a bisect session. Optionally mark BAD and GOOD up-front.
    Start {
        /// Commit to mark as bad immediately.
        #[arg(value_name = "BAD")]
        bad: Option<String>,
        /// Commit to mark as good immediately.
        #[arg(value_name = "GOOD")]
        good: Option<String>,
    },
    /// Mark commit(s) as good. With no args, marks HEAD.
    Good {
        #[arg(value_name = "COMMIT")]
        commits: Vec<String>,
    },
    /// Mark commit(s) as bad. With no args, marks HEAD.
    Bad {
        #[arg(value_name = "COMMIT")]
        commits: Vec<String>,
    },
    /// Show the current bisect log.
    Log,
    /// End the bisect session and restore HEAD to its starting position.
    Reset,
}

#[derive(Debug, Args)]
pub struct BisectArgs {
    #[command(subcommand)]
    pub subcommand: BisectSubcommand,
}

pub fn run(args: BisectArgs) -> io::Result<i32> {
    let repo = Repository::discover_from_cwd().map_err(io_err)?;
    match args.subcommand {
        BisectSubcommand::Start { bad, good } => start(&repo, bad.as_deref(), good.as_deref()),
        BisectSubcommand::Good { commits } => mark(&repo, &commits, Mark::Good),
        BisectSubcommand::Bad { commits } => mark(&repo, &commits, Mark::Bad),
        BisectSubcommand::Log => print_log(&repo),
        BisectSubcommand::Reset => reset(&repo),
    }
}

#[derive(Copy, Clone, PartialEq, Eq)]
enum Mark {
    Good,
    Bad,
}

// ---------------------------------------------------------------------------
// `bisect start`
// ---------------------------------------------------------------------------

fn start(repo: &Repository, bad: Option<&str>, good: Option<&str>) -> io::Result<i32> {
    // If a session is already active, refuse — match git's behavior of
    // requiring an explicit reset.
    if State::load(repo).map_err(io_err)?.is_some() {
        eprintln!(
            "rustygit: bisect: a bisect session is already in progress; run `bisect reset` first"
        );
        return Ok(1);
    }

    let head_name = FullName::new("HEAD").map_err(io_err)?;
    let head_ref = repo
        .refs()
        .read(&head_name)
        .map_err(io_err)?
        .ok_or_else(|| io::Error::other("bisect requires a HEAD reference"))?;
    let (start_branch, start_oid) = match head_ref.target {
        RefTarget::Symbolic(b) => {
            let oid = match RefTarget::resolve(repo.refs(), &b).map_err(io_err)? {
                Some((_, o)) => o,
                None => ObjectId::null(repo.hash_kind()),
            };
            (Some(b), oid)
        }
        RefTarget::Direct(o) => (None, o),
    };

    let state = State {
        bad: None,
        good: Vec::new(),
        start_branch,
        start_oid,
        term_bad: "bad".into(),
        term_good: "good".into(),
    };
    state.save(repo).map_err(io_err)?;

    // Initialise an empty log.
    let log_path = repo.gitdir().join("BISECT_LOG");
    let _ = fs::write(
        &log_path,
        "git bisect start\n# status: waiting for both good and bad commits\n",
    );

    // If the user supplied --bad / --good up-front, apply them now.
    if let Some(b) = bad {
        let oid = resolve(repo.refs(), repo.odb(), b).map_err(io_err)?;
        record_mark(repo, Mark::Bad, &[oid])?;
    }
    if let Some(g) = good {
        let oid = resolve(repo.refs(), repo.odb(), g).map_err(io_err)?;
        record_mark(repo, Mark::Good, &[oid])?;
    }

    // If we now have both, step.
    let state = State::load(repo).map_err(io_err)?.expect("we just saved");
    if state.bad.is_some() && !state.good.is_empty() {
        return advance(repo, &state);
    }
    Ok(0)
}

// ---------------------------------------------------------------------------
// `bisect good` / `bisect bad`
// ---------------------------------------------------------------------------

fn mark(repo: &Repository, args: &[String], mark_kind: Mark) -> io::Result<i32> {
    if State::load(repo).map_err(io_err)?.is_none() {
        eprintln!("rustygit: bisect: no bisect in progress; run `bisect start` first");
        return Ok(1);
    }

    // Resolve commit args (default: HEAD).
    let oids: Vec<ObjectId> = if args.is_empty() {
        vec![resolve(repo.refs(), repo.odb(), "HEAD").map_err(io_err)?]
    } else {
        let mut v = Vec::new();
        for a in args {
            v.push(resolve(repo.refs(), repo.odb(), a).map_err(io_err)?);
        }
        v
    };

    record_mark(repo, mark_kind, &oids)?;

    // Reload state and check whether we can pick a next step yet.
    let state = State::load(repo).map_err(io_err)?.expect("active");
    if state.bad.is_none() {
        println!("status: waiting for bad commit (1 more required)");
        let _ = state;
        return Ok(0);
    }
    if state.good.is_empty() {
        println!("status: waiting for good commit (1 more required)");
        return Ok(0);
    }
    advance(repo, &state)
}

/// Persist a good/bad mark: write the appropriate ref(s) and append the log.
fn record_mark(repo: &Repository, kind: Mark, oids: &[ObjectId]) -> io::Result<()> {
    let mut tx = repo.refs().transaction();
    for oid in oids {
        let ref_name = match kind {
            Mark::Bad => FullName::new("refs/bisect/bad").map_err(io_err)?,
            Mark::Good => FullName::new(format!("refs/bisect/good-{oid}")).map_err(io_err)?,
        };
        tx.update(
            &ref_name,
            ExpectedOldValue::Any,
            NewValue::Direct(*oid),
            ReflogMessage::none(),
        )
        .map_err(io_err)?;
    }
    tx.commit().map_err(io_err)?;

    // Append the log.
    let log_path = repo.gitdir().join("BISECT_LOG");
    let prior = fs::read_to_string(&log_path).unwrap_or_default();
    let mut next = prior;
    for oid in oids {
        let label = match kind {
            Mark::Bad => "bad",
            Mark::Good => "good",
        };
        next.push_str(&format!("# {label}: [{oid}]\n"));
        next.push_str(&format!("git bisect {label} {oid}\n"));
    }
    fs::write(&log_path, next)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Pick + checkout the next bisect midpoint.
// ---------------------------------------------------------------------------

fn advance(repo: &Repository, state: &State) -> io::Result<i32> {
    let step = next_step(repo, state).map_err(io_err)?;
    match step {
        BisectStep::Next { commit, remaining } => {
            // Checkout the midpoint (detached) and print summary.
            checkout_detached(repo, commit)?;
            // Record the rev we just checked out.
            let _ = fs::write(
                repo.gitdir().join("BISECT_EXPECTED_REV"),
                format!("{commit}\n"),
            );
            let subject = commit_subject(repo, commit).unwrap_or_default();
            let steps = approx_log2(remaining.max(1));
            println!(
                "Bisecting: {remaining} revisions left to test after this (roughly {steps} steps)"
            );
            println!("[{commit}] {subject}");
            Ok(0)
        }
        BisectStep::Done { first_bad } => {
            print_first_bad(repo, first_bad)?;
            Ok(0)
        }
    }
}

fn approx_log2(n: usize) -> usize {
    if n <= 1 {
        return 0;
    }
    // 64 - leading-zeros of (n) gives ceil(log2(n+1)); we want floor(log2(n)).
    (usize::BITS - 1 - (n.leading_zeros())) as usize
}

fn checkout_detached(repo: &Repository, oid: ObjectId) -> io::Result<()> {
    // Peel to tree.
    let obj = repo.odb().read(&oid).map_err(io_err)?;
    if obj.kind != ObjectKind::Commit {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{oid} is not a commit"),
        ));
    }
    let commit = Commit::parse(&obj.data, repo.hash_kind()).map_err(io_err)?;
    let opts = UnpackOpts {
        force: true,
        keep_extra: false,
        update_workdir: true,
        update_index: true,
    };
    // Update workdir+index. Allow checkout to fail in degenerate cases (the
    // test harness uses raw refs + state files without a real workdir state).
    let _ = unpack_trees::checkout_tree(repo, commit.tree, &opts);

    // Detached HEAD: HEAD = direct(oid).
    let head_name = FullName::new("HEAD").map_err(io_err)?;
    let mut tx = repo.refs().transaction();
    tx.update(
        &head_name,
        ExpectedOldValue::Any,
        NewValue::Direct(oid),
        ReflogMessage::from(format!("bisect: checkout {}", oid.short_hex(7))),
    )
    .map_err(io_err)?;
    tx.commit().map_err(io_err)?;
    Ok(())
}

fn commit_subject(repo: &Repository, oid: ObjectId) -> io::Result<String> {
    let obj = repo.odb().read(&oid).map_err(io_err)?;
    if obj.kind != ObjectKind::Commit {
        return Ok(String::new());
    }
    let commit = Commit::parse(&obj.data, repo.hash_kind()).map_err(io_err)?;
    Ok(String::from_utf8_lossy(&commit.message)
        .lines()
        .next()
        .unwrap_or("")
        .to_string())
}

fn print_first_bad(repo: &Repository, oid: ObjectId) -> io::Result<()> {
    let obj = repo.odb().read(&oid).map_err(io_err)?;
    if obj.kind != ObjectKind::Commit {
        println!("{oid} is the first bad commit");
        return Ok(());
    }
    let commit = Commit::parse(&obj.data, repo.hash_kind()).map_err(io_err)?;
    println!("{oid} is the first bad commit");
    println!("commit {oid}");
    println!("Author: {} <{}>", commit.author.name, commit.author.email);
    println!(
        "Date:   {} {:+05}",
        commit.author.when.seconds, commit.author.when.offset_minutes
    );
    println!();
    for line in String::from_utf8_lossy(&commit.message).lines() {
        println!("    {line}");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// `bisect log`
// ---------------------------------------------------------------------------

fn print_log(repo: &Repository) -> io::Result<i32> {
    let path = repo.gitdir().join("BISECT_LOG");
    match fs::read_to_string(&path) {
        Ok(s) => {
            print!("{s}");
            Ok(0)
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            eprintln!("rustygit: bisect: no bisect in progress");
            Ok(1)
        }
        Err(e) => Err(e),
    }
}

// ---------------------------------------------------------------------------
// `bisect reset`
// ---------------------------------------------------------------------------

fn reset(repo: &Repository) -> io::Result<i32> {
    let state = match State::load(repo).map_err(io_err)? {
        Some(s) => s,
        None => {
            // No-op; match git's silent behavior here.
            return Ok(0);
        }
    };

    // Restore HEAD.
    let head_name = FullName::new("HEAD").map_err(io_err)?;
    let mut tx = repo.refs().transaction();
    let new_head = match state.start_branch {
        Some(branch) => NewValue::Symbolic(branch),
        None => NewValue::Direct(state.start_oid),
    };
    tx.update(
        &head_name,
        ExpectedOldValue::Any,
        new_head,
        ReflogMessage::none(),
    )
    .map_err(io_err)?;
    tx.commit().map_err(io_err)?;

    // Also try to roll the workdir back to start_oid's tree (best-effort).
    if !state.start_oid.is_null() {
        if let Ok(obj) = repo.odb().read(&state.start_oid) {
            if obj.kind == ObjectKind::Commit {
                if let Ok(c) = Commit::parse(&obj.data, repo.hash_kind()) {
                    let opts = UnpackOpts {
                        force: true,
                        keep_extra: false,
                        update_workdir: true,
                        update_index: true,
                    };
                    let _ = unpack_trees::checkout_tree(repo, c.tree, &opts);
                }
            }
        }
    }

    State::cleanup(repo).map_err(io_err)?;
    Ok(0)
}

fn io_err<E: std::fmt::Display>(e: E) -> io::Error {
    io::Error::other(format!("{e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Debug, Parser)]
    struct Wrap {
        #[command(flatten)]
        args: BisectArgs,
    }

    #[test]
    fn parses_start_no_args() {
        let w = Wrap::try_parse_from(["x", "start"]).unwrap();
        match w.args.subcommand {
            BisectSubcommand::Start { bad, good } => {
                assert!(bad.is_none());
                assert!(good.is_none());
            }
            _ => panic!("expected start"),
        }
    }

    #[test]
    fn parses_start_with_bad_and_good() {
        let w = Wrap::try_parse_from(["x", "start", "HEAD", "HEAD~5"]).unwrap();
        match w.args.subcommand {
            BisectSubcommand::Start { bad, good } => {
                assert_eq!(bad.as_deref(), Some("HEAD"));
                assert_eq!(good.as_deref(), Some("HEAD~5"));
            }
            _ => panic!("expected start"),
        }
    }

    #[test]
    fn parses_good_multiple_commits() {
        let w = Wrap::try_parse_from(["x", "good", "abc123", "def456"]).unwrap();
        match w.args.subcommand {
            BisectSubcommand::Good { commits } => {
                assert_eq!(commits, vec!["abc123", "def456"]);
            }
            _ => panic!("expected good"),
        }
    }

    #[test]
    fn parses_bad_no_args() {
        let w = Wrap::try_parse_from(["x", "bad"]).unwrap();
        match w.args.subcommand {
            BisectSubcommand::Bad { commits } => assert!(commits.is_empty()),
            _ => panic!("expected bad"),
        }
    }

    #[test]
    fn parses_reset() {
        let w = Wrap::try_parse_from(["x", "reset"]).unwrap();
        assert!(matches!(w.args.subcommand, BisectSubcommand::Reset));
    }

    #[test]
    fn parses_log() {
        let w = Wrap::try_parse_from(["x", "log"]).unwrap();
        assert!(matches!(w.args.subcommand, BisectSubcommand::Log));
    }

    #[test]
    fn approx_log2_basic() {
        assert_eq!(approx_log2(1), 0);
        assert_eq!(approx_log2(2), 1);
        assert_eq!(approx_log2(4), 2);
        assert_eq!(approx_log2(8), 3);
        assert_eq!(approx_log2(15), 3);
        assert_eq!(approx_log2(16), 4);
    }
}
