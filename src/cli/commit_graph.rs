//! `rustygit commit-graph` — read/write the commit-graph cache.
//!
//! Subcommands:
//! - `write` walks every reachable commit and emits
//!   `<gitdir>/objects/info/commit-graph`, overwriting any existing file.
//! - `verify` opens that file and runs the structural / checksum checks
//!   matched by `git commit-graph verify`.
//!
//! Wiring into top-level `cli::dispatch` is left to the caller (M15 splits
//! the implementation from the dispatch hookup so two tracks can land
//! independently).

use std::io;
use std::path::Path;

use clap::Args;

use crate::commit_graph::{self, CommitGraph};
use crate::repo::Repository;

#[derive(Debug, Args)]
pub struct CommitGraphArgs {
    #[command(subcommand)]
    pub subcommand: CommitGraphSubcommand,
}

#[derive(Debug, clap::Subcommand)]
pub enum CommitGraphSubcommand {
    /// Write a commit-graph from all reachable commits.
    Write {
        /// Suppress the human-readable summary line.
        #[arg(short = 'q', long = "quiet")]
        quiet: bool,
    },
    /// Verify the on-disk commit-graph file.
    Verify,
}

pub fn run(args: CommitGraphArgs) -> io::Result<i32> {
    let repo = Repository::discover_from_cwd().map_err(io_err)?;
    match args.subcommand {
        CommitGraphSubcommand::Write { quiet } => {
            let result = commit_graph::write(&repo).map_err(io_err)?;
            if !quiet {
                eprintln!(
                    "rustygit: wrote commit-graph with {} commit{} ({} bytes) to {}",
                    result.commit_count,
                    if result.commit_count == 1 { "" } else { "s" },
                    result.bytes_written,
                    result.path.display(),
                );
            }
            Ok(0)
        }
        CommitGraphSubcommand::Verify => {
            let path = repo.objects_dir().join("info").join("commit-graph");
            if !path.exists() {
                eprintln!("rustygit: no commit-graph file at {}", path.display());
                return Ok(0);
            }
            let cg = CommitGraph::open(&path, repo.hash_kind()).map_err(io_err)?;
            match cg.verify() {
                Ok(()) => Ok(0),
                Err(e) => {
                    eprintln!("rustygit: commit-graph verify failed: {e}");
                    Ok(1)
                }
            }
        }
    }
}

fn io_err<E: std::fmt::Display>(e: E) -> io::Error {
    io::Error::other(format!("{e}"))
}

// Convenience helper used by `verify` integration tests outside this crate.
pub fn open_default(repo: &Repository) -> Result<CommitGraph, commit_graph::CommitGraphError> {
    let path = default_path(repo);
    CommitGraph::open(path, repo.hash_kind())
}

pub fn default_path(repo: &Repository) -> std::path::PathBuf {
    repo.objects_dir().join("info").join("commit-graph")
}

fn _ensure_static_path(_p: &Path) {} // keep `Path` import live for downstream

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Debug, Parser)]
    struct Wrap {
        #[command(flatten)]
        args: CommitGraphArgs,
    }

    #[test]
    fn parses_write_default() {
        let w = Wrap::try_parse_from(["x", "write"]).unwrap();
        match w.args.subcommand {
            CommitGraphSubcommand::Write { quiet } => assert!(!quiet),
            _ => panic!("wrong subcommand"),
        }
    }

    #[test]
    fn parses_write_quiet() {
        let w = Wrap::try_parse_from(["x", "write", "-q"]).unwrap();
        match w.args.subcommand {
            CommitGraphSubcommand::Write { quiet } => assert!(quiet),
            _ => panic!("wrong subcommand"),
        }
    }

    #[test]
    fn parses_verify() {
        let w = Wrap::try_parse_from(["x", "verify"]).unwrap();
        assert!(matches!(w.args.subcommand, CommitGraphSubcommand::Verify));
    }

    #[test]
    fn rejects_unknown_subcommand() {
        let r = Wrap::try_parse_from(["x", "bogus"]);
        assert!(r.is_err());
    }
}
