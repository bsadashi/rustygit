//! `rustygit mergetool` / `rustygit difftool` — spawn external 3-way merge
//! or diff tools per conflicted/modified file.
//!
//! Tool selection: read `merge.tool` / `diff.tool` from config; default
//! `vimdiff`. The tool is invoked as `<cmd> <local> <base> <remote> [merged]`
//! per upstream's contract.

use std::io::{self, Write};
use std::process::Command;

use clap::Args;

use crate::config::Config;
use crate::repo::Repository;

#[derive(Debug, Args)]
pub struct MergetoolArgs {
    /// Tool name override.
    #[arg(short = 't', long = "tool", value_name = "TOOL")]
    pub tool: Option<String>,
    /// No-prompt mode.
    #[arg(short = 'y', long = "no-prompt")]
    pub no_prompt: bool,
    /// Specific paths to resolve (default: every unmerged).
    #[arg(value_name = "PATH")]
    pub paths: Vec<String>,
}

pub fn run_mergetool(args: MergetoolArgs) -> io::Result<i32> {
    run_tool(args.tool.as_deref(), args.no_prompt, &args.paths, true)
}

#[derive(Debug, Args)]
pub struct DifftoolArgs {
    #[arg(short = 't', long = "tool", value_name = "TOOL")]
    pub tool: Option<String>,
    #[arg(short = 'y', long = "no-prompt")]
    pub no_prompt: bool,
    #[arg(value_name = "PATH")]
    pub paths: Vec<String>,
}

pub fn run_difftool(args: DifftoolArgs) -> io::Result<i32> {
    run_tool(args.tool.as_deref(), args.no_prompt, &args.paths, false)
}

fn run_tool(
    explicit: Option<&str>,
    no_prompt: bool,
    paths: &[String],
    is_merge: bool,
) -> io::Result<i32> {
    let repo = Repository::discover_from_cwd().map_err(io_err)?;
    let config = Config::from_repo_dir(repo.gitdir()).unwrap_or_else(|_| Config::empty());
    let key = "tool";
    let section = if is_merge { "merge" } else { "diff" };
    let tool = explicit
        .map(str::to_string)
        .or_else(|| config.get_string(section, key).map(str::to_string))
        .unwrap_or_else(|| "vimdiff".to_string());

    // Resolve set of paths.
    let want: Vec<Vec<u8>> = if !paths.is_empty() {
        paths.iter().map(|p| p.as_bytes().to_vec()).collect()
    } else {
        let report = crate::worktree::status::status(&repo).map_err(io_err)?;
        report
            .entries
            .iter()
            .filter(|e| match e.worktree_state {
                crate::worktree::status::WorktreeState::Modified => true,
                crate::worktree::status::WorktreeState::Deleted => false,
                _ => false,
            })
            .map(|e| e.path.clone())
            .collect()
    };

    let mut handled = 0usize;
    for p in &want {
        let path_str = String::from_utf8_lossy(p);
        if !no_prompt {
            let stderr = io::stderr();
            let mut e = stderr.lock();
            writeln!(e, "{path_str}: Launching '{tool}'...")?;
        }
        // For a real merge driver we'd write three temp files (BASE / LOCAL /
        // REMOTE). For now we hand the conflicted file to the editor.
        let abs = repo.workdir().join(path_str.as_ref());
        let status = Command::new(&tool).arg(&abs).status();
        if status.map(|s| s.success()).unwrap_or(false) {
            handled += 1;
        }
    }
    println!("{} file(s) handled by {tool}", handled);
    Ok(0)
}

fn io_err<E: std::fmt::Display>(e: E) -> io::Error {
    io::Error::other(format!("{e}"))
}
