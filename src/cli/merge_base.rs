//! `rustygit merge-base` — compute the merge base (LCA) of two commits.

use std::io;

use clap::Args;

use crate::merge::base::{is_ancestor, merge_base, merge_bases};
use crate::repo::Repository;
use crate::revparse;

#[derive(Debug, Args)]
pub struct MergeBaseArgs {
    /// Print all merge bases when there are multiple (criss-cross history).
    #[arg(long = "all")]
    pub all: bool,

    /// Exit 0 if the first arg is an ancestor of the second; 1 otherwise.
    /// (Other flags are ignored when this is set.)
    #[arg(long = "is-ancestor")]
    pub is_ancestor: bool,

    /// Commits to compute the merge base of.
    #[arg(value_name = "COMMIT", required = true, num_args = 2..=2)]
    pub commits: Vec<String>,
}

pub fn run(args: MergeBaseArgs) -> io::Result<i32> {
    let repo = Repository::discover_from_cwd().map_err(io_err)?;
    let a = resolve(&repo, &args.commits[0])?;
    let b = resolve(&repo, &args.commits[1])?;

    if args.is_ancestor {
        let ok = is_ancestor(&repo, a, b).map_err(io_err)?;
        return Ok(if ok { 0 } else { 1 });
    }

    if args.all {
        let bases = merge_bases(&repo, a, b).map_err(io_err)?;
        if bases.is_empty() {
            return Ok(1);
        }
        for oid in bases {
            println!("{oid}");
        }
        Ok(0)
    } else {
        match merge_base(&repo, a, b).map_err(io_err)? {
            Some(oid) => {
                println!("{oid}");
                Ok(0)
            }
            None => Ok(1),
        }
    }
}

fn resolve(repo: &Repository, expr: &str) -> io::Result<crate::hash::ObjectId> {
    revparse::resolve(repo.refs(), repo.odb(), expr)
        .map_err(|e| io::Error::other(format!("not a valid object name: {e}")))
}

fn io_err<E: std::fmt::Display>(e: E) -> io::Error {
    io::Error::other(format!("{e}"))
}
