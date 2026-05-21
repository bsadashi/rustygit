//! `rustygit diff-files` — plumbing diff between the index and the workdir.
//!
//! Takes no revisions (by definition).

use std::io::{self, Write};

use clap::Args;

use crate::cli::EXIT_DIFF_FOUND;
use crate::diff;
use crate::repo::Repository;

#[derive(Debug, Args)]
pub struct DiffFilesArgs {
    /// Accepted for argv compatibility; we always recurse.
    #[arg(short = 'r')]
    pub recurse: bool,

    /// Make the program exit with code 1 if there are differences, 0 if not.
    #[arg(long = "exit-code")]
    pub exit_code: bool,

    /// Like `--exit-code`, but also suppress the diff output entirely.
    #[arg(long = "quiet", short = 'q')]
    pub quiet: bool,
}

pub fn run(args: DiffFilesArgs) -> io::Result<i32> {
    let repo = Repository::discover_from_cwd().map_err(io_err)?;
    if args.exit_code || args.quiet {
        let mut buf: Vec<u8> = Vec::new();
        diff::diff_index_workdir(&repo, &mut buf)?;
        let has_diff = !buf.is_empty();
        if !args.quiet {
            io::stdout().write_all(&buf)?;
        }
        return Ok(if has_diff { EXIT_DIFF_FOUND } else { 0 });
    }
    let stdout = io::stdout();
    let mut out = stdout.lock();
    diff::diff_index_workdir(&repo, &mut out)?;
    Ok(0)
}

fn io_err<E: std::fmt::Display>(e: E) -> io::Error {
    io::Error::other(format!("{e}"))
}
