//! `rustygit var` — print internal git variables.
//!
//! Supported names (matches `git var --list`):
//!   * `GIT_AUTHOR_IDENT`     — author identity used for new commits
//!   * `GIT_COMMITTER_IDENT`  — committer identity used for new commits
//!   * `GIT_EDITOR`           — editor used for editor flows
//!   * `GIT_PAGER`            — pager program
//!   * `GIT_DEFAULT_BRANCH`   — `init.defaultBranch` or `main`
//!
//! `-l` / `--list` prints every variable, one per line, in the form
//! `NAME=VALUE`. A bare `<name>` prints just the value.

use std::io;

use clap::Args;

use crate::config::Config;
use crate::identity::{Signature, Time};
use crate::repo::Repository;

#[derive(Debug, Args)]
pub struct VarArgs {
    /// List all variables.
    #[arg(short = 'l', long = "list")]
    pub list: bool,
    /// Variable name (when not listing).
    #[arg(value_name = "NAME")]
    pub name: Option<String>,
}

pub fn run(args: VarArgs) -> io::Result<i32> {
    // Repo is optional — `var GIT_EDITOR` should work outside a repo too.
    let repo = Repository::discover_from_cwd().ok();
    let config = match &repo {
        Some(r) => Config::from_repo_dir(r.gitdir()).unwrap_or_else(|_| Config::empty()),
        None => Config::empty(),
    };

    let vars = collect_vars(&config);

    if args.list {
        for (name, value) in &vars {
            println!("{name}={value}");
        }
        return Ok(0);
    }

    let name = match args.name {
        Some(n) => n,
        None => {
            eprintln!("rustygit: var: missing variable name (or pass --list)");
            return Ok(129);
        }
    };

    match vars.iter().find(|(n, _)| *n == name.as_str()) {
        Some((_, v)) => {
            println!("{v}");
            Ok(0)
        }
        None => {
            eprintln!("rustygit: var: '{name}' is not a known variable");
            Ok(128)
        }
    }
}

fn collect_vars(config: &Config) -> Vec<(&'static str, String)> {
    let now = Time::now_local();

    let author = Signature::author_from_env_or_config(config, now)
        .map(|s| s.serialize())
        .unwrap_or_default();
    let committer = Signature::committer_from_env_or_config(config, now)
        .map(|s| s.serialize())
        .unwrap_or_default();

    let editor = pick_editor(config);
    let pager = pick_pager(config);
    let default_branch = config
        .get_string("init", "defaultBranch")
        .map(str::to_string)
        .unwrap_or_else(|| "main".to_string());

    vec![
        ("GIT_AUTHOR_IDENT", author),
        ("GIT_COMMITTER_IDENT", committer),
        ("GIT_EDITOR", editor),
        ("GIT_PAGER", pager),
        ("GIT_DEFAULT_BRANCH", default_branch),
    ]
}

/// Pick an editor following git's precedence:
/// `$GIT_EDITOR` > `core.editor` > `$VISUAL` > `$EDITOR` > `vi`.
pub(crate) fn pick_editor(config: &Config) -> String {
    if let Ok(v) = std::env::var("GIT_EDITOR") {
        if !v.is_empty() {
            return v;
        }
    }
    if let Some(v) = config.get_string("core", "editor") {
        if !v.is_empty() {
            return v.to_string();
        }
    }
    if let Ok(v) = std::env::var("VISUAL") {
        if !v.is_empty() {
            return v;
        }
    }
    if let Ok(v) = std::env::var("EDITOR") {
        if !v.is_empty() {
            return v;
        }
    }
    "vi".to_string()
}

/// `$GIT_PAGER` > `core.pager` > `$PAGER` > `less`.
fn pick_pager(config: &Config) -> String {
    if let Ok(v) = std::env::var("GIT_PAGER") {
        if !v.is_empty() {
            return v;
        }
    }
    if let Some(v) = config.get_string("core", "pager") {
        if !v.is_empty() {
            return v.to_string();
        }
    }
    if let Ok(v) = std::env::var("PAGER") {
        if !v.is_empty() {
            return v;
        }
    }
    "less".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser, Debug)]
    struct Wrap {
        #[command(flatten)]
        args: VarArgs,
    }

    #[test]
    fn parses_list() {
        let w = Wrap::try_parse_from(["test", "-l"]).unwrap();
        assert!(w.args.list);
    }

    #[test]
    fn parses_name() {
        let w = Wrap::try_parse_from(["test", "GIT_EDITOR"]).unwrap();
        assert_eq!(w.args.name, Some("GIT_EDITOR".to_string()));
    }
}
