//! `rustygit status` — working-tree status.
//!
//! Two output formats:
//! - **Human (default)** — the verbose, multi-section form `git status` emits
//!   when given no flags. Includes "On branch …", "Changes to be committed:",
//!   "Untracked files:", etc. Stable enough for users; not for scripts.
//! - **Porcelain v1** — selected by `--porcelain` or `-s`/`--short`. The
//!   stable `XY <path>` machine format documented by `git status --porcelain`.
//!
//! Both formats run the same underlying [`status`] engine; only the renderer
//! differs.

use std::io::{self, Write};

use clap::Args;

use crate::repo::Repository;
use crate::worktree::{status, Human, PorcelainV1};

#[derive(Debug, Args)]
pub struct StatusArgs {
    /// Use the porcelain v1 format (machine-readable, stable).
    #[arg(long = "porcelain")]
    pub porcelain: bool,

    /// Short format. Currently aliases porcelain v1 (we don't ship a separate
    /// `-s` renderer — git's short form differs from porcelain v1 only in
    /// branch-header handling, which we don't emit either way without
    /// `--branch`).
    #[arg(short = 's', long = "short")]
    pub short: bool,
}

pub fn run(args: StatusArgs) -> io::Result<i32> {
    let repo = Repository::discover_from_cwd().map_err(io_err)?;
    let report = status(&repo).map_err(io_err)?;

    let stdout = io::stdout();
    let mut out = stdout.lock();
    let bytes = if args.porcelain || args.short {
        PorcelainV1::new(&report).to_bytes()
    } else {
        Human::new(&report).with_upstream_from(&repo).to_bytes()
    };
    out.write_all(&bytes)?;
    Ok(0)
}

fn io_err<E: std::fmt::Display>(e: E) -> io::Error {
    io::Error::other(format!("{e}"))
}
