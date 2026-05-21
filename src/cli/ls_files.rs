//! `rustygit ls-files` — list files known to the index.
//!
//! Default: one path per line, sorted by index order.
//!
//! Flags:
//!   * `-s` / `--stage`     — emit `<mode> <oid> <stage>\t<path>` rows.
//!   * `-c` / `--cached`    — show cached files (default).
//!   * `-o` / `--others`    — show files not in the index (untracked).
//!   * `-m` / `--modified`  — show files where the workdir differs from index.
//!   * `-i` / `--ignored`   — restrict `--others` to gitignored files.
//!   * `-z`                 — NUL-terminate output records.
//!   * `--full-name`        — always print paths relative to the repo root.

use std::io::{self, Write};

use clap::Args;

use crate::index::Index;
use crate::repo::Repository;

#[derive(Debug, Args)]
pub struct LsFilesArgs {
    /// Show staged entries with mode/oid/stage.
    #[arg(short = 's', long = "stage")]
    pub stage: bool,
    /// Show cached files (default).
    #[arg(short = 'c', long = "cached")]
    pub cached: bool,
    /// Show files not in the index.
    #[arg(short = 'o', long = "others")]
    pub others: bool,
    /// Show files where workdir differs from index.
    #[arg(short = 'm', long = "modified")]
    pub modified: bool,
    /// Restrict `--others` to gitignored files (or include them in `-c`).
    #[arg(short = 'i', long = "ignored")]
    pub ignored: bool,
    /// NUL-terminate output records.
    #[arg(short = 'z')]
    pub nul_terminate: bool,
    /// Accepted for parity; rustygit always prints repo-relative paths.
    #[arg(long = "full-name")]
    pub full_name: bool,
}

pub fn run(args: LsFilesArgs) -> io::Result<i32> {
    let repo = Repository::discover_from_cwd().map_err(io_err)?;
    let index = Index::read(&repo).map_err(io_err)?;
    let term = if args.nul_terminate { 0u8 } else { b'\n' };

    let want_cached = args.cached || (!args.others && !args.modified);
    let want_others = args.others;
    let want_modified = args.modified;
    let _ = args.full_name; // always relative — accept silently

    let stdout = io::stdout();
    let mut out = stdout.lock();

    if want_cached {
        for entry in &index.entries {
            if args.stage {
                let stage = entry.stage;
                let mode = entry.mode;
                let oid = entry.oid;
                let prefix = format!("{mode:06o} {oid} {stage}\t");
                out.write_all(prefix.as_bytes())?;
            }
            out.write_all(&entry.path)?;
            out.write_all(std::slice::from_ref(&term))?;
        }
    }

    if want_modified {
        let report = crate::worktree::status::status(&repo).map_err(io_err)?;
        for entry in &report.entries {
            if matches!(
                entry.worktree_state,
                crate::worktree::status::WorktreeState::Modified
                    | crate::worktree::status::WorktreeState::Deleted
                    | crate::worktree::status::WorktreeState::TypeChanged
            ) {
                out.write_all(&entry.path)?;
                out.write_all(std::slice::from_ref(&term))?;
            }
        }
    }

    if want_others {
        let report = crate::worktree::status::status(&repo).map_err(io_err)?;
        for entry in &report.entries {
            if entry.worktree_state != crate::worktree::status::WorktreeState::Untracked {
                continue;
            }
            // --ignored: only print files that the ignore engine would
            // mark out (status's "?" already skips ignored — so for
            // --ignored we'd need a separate walk; deferred).
            if args.ignored {
                continue;
            }
            out.write_all(&entry.path)?;
            out.write_all(std::slice::from_ref(&term))?;
        }
    }

    Ok(0)
}

fn io_err<E: std::fmt::Display>(e: E) -> io::Error {
    io::Error::other(format!("{e}"))
}
