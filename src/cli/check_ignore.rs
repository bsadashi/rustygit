//! `rustygit check-ignore` — for each input path, print it on stdout
//! if it would be gitignored.
//!
//! Exit: 0 if at least one path matched a rule; 1 if no paths matched.
//!
//! `-v`/`--verbose` is accepted for upstream-flag parity but currently
//! emits the same single-column form (full source/line/pattern
//! attribution is a deferred enhancement; the underlying
//! `IgnoreStack::is_ignored` doesn't surface match metadata yet).

use std::io::{self, Write};

use clap::Args;

use crate::ignore::IgnoreStack;
use crate::repo::Repository;

#[derive(Debug, Args)]
pub struct CheckIgnoreArgs {
    /// Accepted for parity; today emits the same output as the default.
    #[arg(short = 'v', long = "verbose")]
    pub verbose: bool,
    /// NUL-terminate output records.
    #[arg(short = 'z')]
    pub nul_terminate: bool,
    /// Don't consult any worktree `.gitignore` files (only `info/exclude`).
    #[arg(long = "no-index")]
    pub no_index: bool,
    /// Paths to classify.
    #[arg(value_name = "PATH", required = true)]
    pub paths: Vec<String>,
}

pub fn run(args: CheckIgnoreArgs) -> io::Result<i32> {
    let repo = Repository::discover_from_cwd().map_err(io_err)?;
    let stack = build_stack(&repo, args.no_index)?;
    let term = if args.nul_terminate { 0u8 } else { b'\n' };

    let stdout = io::stdout();
    let mut out = stdout.lock();
    let mut any_match = false;

    for path in &args.paths {
        if stack.is_ignored(path.as_bytes(), false) {
            any_match = true;
            out.write_all(path.as_bytes())?;
            out.write_all(std::slice::from_ref(&term))?;
        }
    }

    if any_match {
        Ok(0)
    } else {
        Ok(1)
    }
}

fn build_stack(repo: &Repository, no_index: bool) -> io::Result<IgnoreStack> {
    let mut stack = IgnoreStack::empty();
    let info_exclude = repo.gitdir().join("info").join("exclude");
    if let Ok(bytes) = std::fs::read(&info_exclude) {
        stack.push_file(&bytes, b"");
    }
    if !no_index {
        let root_ignore = repo.workdir().join(".gitignore");
        if let Ok(bytes) = std::fs::read(&root_ignore) {
            stack.push_file(&bytes, b"");
        }
    }
    Ok(stack)
}

fn io_err<E: std::fmt::Display>(e: E) -> io::Error {
    io::Error::other(format!("{e}"))
}
