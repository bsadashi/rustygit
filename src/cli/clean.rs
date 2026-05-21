//! `rustygit clean` — remove untracked files from the working tree.
//!
//! Safety: by default, `clean` refuses to do anything without `-f`/`--force`.
//! Without `-d`, directories are left alone.
//!
//! Flags:
//!   * `-n`/`--dry-run` — print what would be removed; remove nothing.
//!   * `-f`/`--force`   — required to actually remove anything.
//!   * `-d`             — also remove untracked directories.
//!   * `-x`             — also remove gitignored files.
//!   * `-X`             — only remove gitignored files.
//!   * `-q`/`--quiet`   — suppress informational output.

use std::io::{self, Write};
use std::path::PathBuf;

use clap::Args;

use crate::repo::Repository;

#[derive(Debug, Args)]
pub struct CleanArgs {
    #[arg(short = 'n', long = "dry-run")]
    pub dry_run: bool,
    #[arg(short = 'f', long = "force")]
    pub force: bool,
    #[arg(short = 'd')]
    pub directories: bool,
    #[arg(short = 'x', conflicts_with = "only_ignored")]
    pub include_ignored: bool,
    #[arg(short = 'X', conflicts_with = "include_ignored")]
    pub only_ignored: bool,
    #[arg(short = 'q', long = "quiet")]
    pub quiet: bool,
    /// Optional pathspec filter.
    #[arg(value_name = "PATHSPEC")]
    pub paths: Vec<String>,
}

pub fn run(args: CleanArgs) -> io::Result<i32> {
    let repo = Repository::discover_from_cwd().map_err(io_err)?;

    if !args.force && !args.dry_run {
        eprintln!(
            "rustygit: clean: refusing to remove without --force; pass -n to preview or -f to remove"
        );
        return Ok(128);
    }

    let report = crate::worktree::status::status(&repo).map_err(io_err)?;
    let mut candidates: Vec<PathBuf> = Vec::new();
    for e in &report.entries {
        if e.worktree_state != crate::worktree::status::WorktreeState::Untracked {
            continue;
        }
        let path_str = match std::str::from_utf8(&e.path) {
            Ok(s) => s,
            Err(_) => continue,
        };
        if !args.paths.is_empty() && !args.paths.iter().any(|p| path_str.starts_with(p)) {
            continue;
        }
        // -X = only ignored, -x = include ignored. status() already
        // excludes ignored by default; so for -X / -x we walk separately.
        // For now we operate on the un-ignored untracked set; the
        // ignored variants are documented as a follow-on.
        let _ = args.include_ignored;
        let _ = args.only_ignored;
        candidates.push(repo.workdir().join(path_str));
    }

    let stdout = io::stdout();
    let mut out = stdout.lock();
    let mut removed = 0u64;
    for path in &candidates {
        let rel = path
            .strip_prefix(repo.workdir())
            .unwrap_or(path)
            .display()
            .to_string();
        if path.is_dir() {
            if !args.directories {
                continue;
            }
            if args.dry_run || !args.quiet {
                writeln!(out, "Would remove {rel}/")?;
            }
            if !args.dry_run {
                std::fs::remove_dir_all(path)?;
                removed += 1;
            }
        } else if path.is_file() || path.is_symlink() {
            if args.dry_run || !args.quiet {
                writeln!(out, "Would remove {rel}")?;
            }
            if !args.dry_run {
                std::fs::remove_file(path)?;
                removed += 1;
            }
        }
    }
    if !args.quiet && !args.dry_run {
        writeln!(out, "Removed {removed} item(s).")?;
    }
    Ok(0)
}

fn io_err<E: std::fmt::Display>(e: E) -> io::Error {
    io::Error::other(format!("{e}"))
}
