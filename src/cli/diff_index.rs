//! `rustygit diff-index` — plumbing diff between a tree and the index.
//!
//! Minimum viable shape: `diff-index [--cached] <tree-ish>`. Without
//! `--cached`, real git compares tree-vs-workdir; we follow that behavior.

use std::io::{self, Write};

use clap::Args;

use crate::cli::EXIT_DIFF_FOUND;
use crate::diff;
use crate::repo::Repository;
use crate::revparse::resolve;

#[derive(Debug, Args)]
pub struct DiffIndexArgs {
    /// Compare tree against the index (instead of the working tree).
    #[arg(long = "cached")]
    pub cached: bool,

    /// Make the program exit with code 1 if there are differences, 0 if not.
    #[arg(long = "exit-code")]
    pub exit_code: bool,

    /// Like `--exit-code`, but also suppress the diff output entirely.
    #[arg(long = "quiet", short = 'q')]
    pub quiet: bool,

    /// Tree-ish to use as the a-side.
    #[arg(value_name = "TREE-ISH")]
    pub tree: String,
}

pub fn run(args: DiffIndexArgs) -> io::Result<i32> {
    let repo = Repository::discover_from_cwd().map_err(io_err)?;
    let tree = resolve(repo.refs(), repo.odb(), &args.tree).map_err(io_err)?;
    if args.exit_code || args.quiet {
        let mut buf: Vec<u8> = Vec::new();
        if args.cached {
            diff::diff_tree_index(&repo, tree, &mut buf)?;
        } else {
            diff::diff_tree_workdir(&repo, tree, &mut buf)?;
        }
        let has_diff = !buf.is_empty();
        if !args.quiet {
            io::stdout().write_all(&buf)?;
        }
        return Ok(if has_diff { EXIT_DIFF_FOUND } else { 0 });
    }
    let stdout = io::stdout();
    let mut out = stdout.lock();
    if args.cached {
        diff::diff_tree_index(&repo, tree, &mut out)?;
    } else {
        diff::diff_tree_workdir(&repo, tree, &mut out)?;
    }
    Ok(0)
}

fn io_err<E: std::fmt::Display>(e: E) -> io::Error {
    io::Error::other(format!("{e}"))
}
