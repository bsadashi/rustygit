//! `rustygit diff` — porcelain dispatcher between the four flavors of diff.
//!
//! Argv shapes mirror git's:
//!
//! ```text
//! rustygit diff                  # index vs. workdir
//! rustygit diff --cached         # HEAD-tree vs. index
//! rustygit diff --staged         # alias for --cached
//! rustygit diff <rev>            # <rev>-tree vs. workdir
//! rustygit diff --cached <rev>   # <rev>-tree vs. index
//! rustygit diff <rev> <rev>      # tree vs. tree
//! ```
//!
//! Out of scope for M5: pathspec filters after `--`, color output, the many
//! flags accepted by `git diff` (`--stat`, `--numstat`, `--name-only`, ...).
//! M6 polish.

use std::io::{self, Write};

use clap::Args;

use crate::cli::EXIT_DIFF_FOUND;
use crate::config::Config;
use crate::diff;
use crate::hash::ObjectId;
use crate::refs::FullName;
use crate::repo::Repository;
use crate::revparse::resolve;

#[derive(Debug, Args)]
pub struct DiffArgs {
    /// Compare the named tree (or HEAD) against the index instead of the workdir.
    #[arg(long = "cached", visible_alias = "staged")]
    pub cached: bool,

    /// Make the program exit with code 1 if there are differences, 0 if not.
    /// Useful for `if rustygit diff --exit-code; then ...` shell idioms.
    #[arg(long = "exit-code")]
    pub exit_code: bool,

    /// Like `--exit-code`, but also suppress the diff output entirely.
    #[arg(long = "quiet", short = 'q')]
    pub quiet: bool,

    /// Up to two revisions; what they mean depends on `--cached`.
    #[arg(value_name = "REV")]
    pub revs: Vec<String>,
}

pub fn run(args: DiffArgs) -> io::Result<i32> {
    let repo = Repository::discover_from_cwd().map_err(io_err)?;

    // Validate the revs count up front so we can surface 129 (usage error)
    // distinctly from a real I/O failure.
    if args.revs.len() > 2 {
        eprintln!("rustygit: diff: too many revisions ({})", args.revs.len());
        return Ok(129);
    }

    // For `--exit-code` / `--quiet` we need to know whether any bytes were
    // produced, so we render into a buffer and inspect it before forwarding
    // (or suppressing) the output. Without those flags we stream straight to
    // stdout — diffs can be megabytes and we'd rather not buffer them.
    if args.exit_code || args.quiet {
        let mut buffer: Vec<u8> = Vec::new();
        run_into(&repo, &args, &mut buffer)?;
        let has_diff = !buffer.is_empty();
        if !args.quiet {
            // Even the "buffered then print" path goes through the pager so
            // a large diff stays scrollable when --exit-code is passed
            // interactively. The buffer is already complete so a pager
            // close mid-stream doesn't cost us anything.
            let cfg = Config::from_repo_dir(repo.gitdir()).unwrap_or_else(|_| Config::empty());
            let mut out = crate::cli::pager::open(&cfg, false)?;
            out.write_all(&buffer)?;
        }
        Ok(if has_diff { EXIT_DIFF_FOUND } else { 0 })
    } else {
        let cfg = Config::from_repo_dir(repo.gitdir()).unwrap_or_else(|_| Config::empty());
        let mut out = crate::cli::pager::open(&cfg, false)?;
        run_into(&repo, &args, &mut out)?;
        Ok(0)
    }
}

/// Render whichever flavor of diff `args` selects into `out`.
fn run_into<W: Write>(repo: &Repository, args: &DiffArgs, out: &mut W) -> io::Result<()> {
    match (args.cached, args.revs.len()) {
        (false, 0) => diff::diff_index_workdir(repo, out),
        (true, 0) => match resolve_head_tree(repo)? {
            Some(tree) => diff::diff_tree_index(repo, tree, out),
            None => diff_unborn_head_against_index(repo, out),
        },
        (false, 1) => {
            let rev = resolve_rev(repo, &args.revs[0])?;
            diff::diff_tree_workdir(repo, rev, out)
        }
        (true, 1) => {
            let rev = resolve_rev(repo, &args.revs[0])?;
            diff::diff_tree_index(repo, rev, out)
        }
        (_, 2) => {
            let a = resolve_rev(repo, &args.revs[0])?;
            let b = resolve_rev(repo, &args.revs[1])?;
            diff::diff_two_trees(repo, a, b, out)
        }
        (_, _) => {
            // Pre-validated by `run`.
            unreachable!("revs count > 2 caught upstream");
        }
    }
}

fn resolve_rev(repo: &Repository, rev: &str) -> io::Result<ObjectId> {
    resolve(repo.refs(), repo.odb(), rev).map_err(io_err)
}

/// Try to resolve HEAD's commit, then peel to its tree. Returns `None` if HEAD
/// is unborn (refers to a branch that doesn't have a commit yet).
fn resolve_head_tree(repo: &Repository) -> io::Result<Option<ObjectId>> {
    let head_name = FullName::new("HEAD").map_err(io_err)?;
    let resolved = crate::refs::RefTarget::resolve(repo.refs(), &head_name).map_err(io_err)?;
    match resolved {
        Some((_, oid)) => {
            let tree = diff::peel_to_tree(repo, oid).map_err(io_err)?;
            Ok(Some(tree))
        }
        None => Ok(None),
    }
}

/// Handle `diff --cached` on a fresh repo where HEAD doesn't yet point at a
/// commit. Treat every index entry as Added.
fn diff_unborn_head_against_index<W: io::Write>(repo: &Repository, out: &mut W) -> io::Result<()> {
    use crate::diff::{diff_entries, flatten_index, format};
    use crate::index::Index;

    let index = Index::read(repo).map_err(io_err)?;
    let a_entries: Vec<crate::diff::DiffEntry> = Vec::new();
    let b_entries = flatten_index(&index);
    let pairs = diff_entries(&a_entries, &b_entries);
    for pair in &pairs {
        format::format_pair(repo, pair, out)?;
    }
    Ok(())
}

fn io_err<E: std::fmt::Display>(e: E) -> io::Error {
    io::Error::other(format!("{e}"))
}
