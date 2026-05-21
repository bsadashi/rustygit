//! `rustygit diff-tree` — plumbing diff between two tree-ish OIDs.
//!
//! Minimum viable shape:
//!   `diff-tree [-r] <tree-ish> <tree-ish>`
//!
//! Real `git diff-tree` accepts a single commit and walks parents; that mode
//! is deferred. M5 covers the explicit two-tree case which is what callers
//! actually need to validate against `git diff <a> <b>`.

use std::io::{self, Write};

use clap::Args;

use crate::cli::{EXIT_DIFF_FOUND, EXIT_USAGE};
use crate::diff;
use crate::hash::ObjectId;
use crate::repo::Repository;
use crate::revparse::resolve;

#[derive(Debug, Args)]
pub struct DiffTreeArgs {
    /// Recurse into subtrees. We always recurse, but accept the flag for
    /// argv compatibility.
    #[arg(short = 'r')]
    pub recurse: bool,

    /// Make the program exit with code 1 if there are differences, 0 if not.
    #[arg(long = "exit-code")]
    pub exit_code: bool,

    /// Like `--exit-code`, but also suppress the diff output entirely.
    #[arg(long = "quiet", short = 'q')]
    pub quiet: bool,

    /// Two tree-ish revisions to compare.
    #[arg(value_name = "TREE-ISH", required = true, num_args = 2)]
    pub revs: Vec<String>,
}

pub fn run(args: DiffTreeArgs) -> io::Result<i32> {
    let repo = Repository::discover_from_cwd().map_err(io_err)?;
    if args.revs.len() != 2 {
        eprintln!("rustygit: diff-tree: expected exactly 2 tree-ish arguments");
        return Ok(EXIT_USAGE);
    }
    let a = resolve_rev(&repo, &args.revs[0])?;
    let b = resolve_rev(&repo, &args.revs[1])?;
    if args.exit_code || args.quiet {
        let mut buf: Vec<u8> = Vec::new();
        diff::diff_two_trees(&repo, a, b, &mut buf)?;
        let has_diff = !buf.is_empty();
        if !args.quiet {
            io::stdout().write_all(&buf)?;
        }
        return Ok(if has_diff { EXIT_DIFF_FOUND } else { 0 });
    }
    let stdout = io::stdout();
    let mut out = stdout.lock();
    diff::diff_two_trees(&repo, a, b, &mut out)?;
    Ok(0)
}

fn resolve_rev(repo: &Repository, rev: &str) -> io::Result<ObjectId> {
    resolve(repo.refs(), repo.odb(), rev).map_err(io_err)
}

fn io_err<E: std::fmt::Display>(e: E) -> io::Error {
    io::Error::other(format!("{e}"))
}
