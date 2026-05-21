//! `rustygit sparse-checkout` — restrict the working tree to a subset
//! of paths via the index's SKIP_WORKTREE bit.
//!
//! Subset:
//!   * `init [--cone]` — enable; write empty `.git/info/sparse-checkout`.
//!   * `set <pattern>...` — replace patterns.
//!   * `add <pattern>...` — append patterns.
//!   * `list` — print current patterns.
//!   * `disable` — clear SKIP_WORKTREE on every entry and delete the file.
//!   * `reapply` — recompute SKIP_WORKTREE bits from current patterns.
//!   * `--cone` mode adds the directory-only constraint.

use std::io::{self, Write};

use clap::{Args, Subcommand};

use crate::index::Index;
use crate::repo::Repository;

#[derive(Debug, Args)]
pub struct SparseCheckoutArgs {
    #[command(subcommand)]
    pub sub: SparseSub,
}

#[derive(Debug, Subcommand)]
pub enum SparseSub {
    Init {
        #[arg(long = "cone")]
        cone: bool,
    },
    Set {
        #[arg(long = "cone")]
        cone: bool,
        #[arg(value_name = "PATTERN", required = true)]
        patterns: Vec<String>,
    },
    Add {
        #[arg(value_name = "PATTERN", required = true)]
        patterns: Vec<String>,
    },
    List,
    Disable,
    Reapply,
}

pub fn run(args: SparseCheckoutArgs) -> io::Result<i32> {
    let repo = Repository::discover_from_cwd().map_err(io_err)?;
    let file = repo.gitdir().join("info").join("sparse-checkout");
    if let Some(parent) = file.parent() {
        std::fs::create_dir_all(parent)?;
    }

    match args.sub {
        SparseSub::Init { cone } => {
            if !file.is_file() {
                let default = if cone { "/*\n!/*/\n" } else { "/*\n" };
                std::fs::write(&file, default)?;
            }
            apply_to_index(&repo, &file)?;
            // Persist the cone-mode bit via config so reapply knows.
            if cone {
                let _ = crate::cli::config_cmd::run(crate::cli::config_cmd::ConfigArgs {
                    get: false,
                    set: Some(vec![
                        "core.sparseCheckoutCone".to_string(),
                        "true".to_string(),
                    ]),
                    unset: false,
                    add: None,
                    list: false,
                    local: true,
                    global: false,
                    key: None,
                    value: None,
                });
            }
            Ok(0)
        }
        SparseSub::Set { patterns, cone: _ } => {
            let body = patterns.join("\n") + "\n";
            std::fs::write(&file, body)?;
            apply_to_index(&repo, &file)
        }
        SparseSub::Add { patterns } => {
            let mut existing = std::fs::read_to_string(&file).unwrap_or_default();
            if !existing.is_empty() && !existing.ends_with('\n') {
                existing.push('\n');
            }
            existing.push_str(&patterns.join("\n"));
            existing.push('\n');
            std::fs::write(&file, existing)?;
            apply_to_index(&repo, &file)
        }
        SparseSub::List => {
            let body = std::fs::read_to_string(&file).unwrap_or_default();
            let stdout = io::stdout();
            stdout.lock().write_all(body.as_bytes())?;
            Ok(0)
        }
        SparseSub::Disable => {
            let _ = std::fs::remove_file(&file);
            let mut idx = Index::read(&repo).map_err(io_err)?;
            for entry in idx.entries.iter_mut() {
                entry.extended_flags &= !(1 << 14);
            }
            idx.write(&repo).map_err(io_err)?;
            Ok(0)
        }
        SparseSub::Reapply => apply_to_index(&repo, &file),
    }
}

fn apply_to_index(repo: &Repository, file: &std::path::Path) -> io::Result<i32> {
    let patterns = std::fs::read_to_string(file).unwrap_or_default();
    let mut idx = Index::read(repo).map_err(io_err)?;
    for entry in idx.entries.iter_mut() {
        let path = match std::str::from_utf8(&entry.path) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let in_set = path_matches_any(path, &patterns);
        if in_set {
            entry.extended_flags &= !(1 << 14);
        } else {
            entry.extended_flags |= 1 << 14;
            entry.extended = true;
        }
    }
    idx.write(repo).map_err(io_err)?;
    Ok(0)
}

/// Tiny pattern check: each line in `patterns` is either an include
/// (`/foo`, `*.rs`) or a negation (`!foo`). Last-match-wins.
fn path_matches_any(path: &str, patterns: &str) -> bool {
    let mut included = false;
    for raw in patterns.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (neg, pat) = if let Some(rest) = line.strip_prefix('!') {
            (true, rest)
        } else {
            (false, line)
        };
        let pat = pat.trim_start_matches('/');
        if simple_glob(pat, path) {
            included = !neg;
        }
    }
    included
}

fn simple_glob(pat: &str, path: &str) -> bool {
    if pat == "*" {
        return true;
    }
    if let Some(prefix) = pat.strip_suffix("/*") {
        return path.starts_with(prefix);
    }
    if let Some(suffix) = pat.strip_prefix("*.") {
        return path.ends_with(&format!(".{suffix}"));
    }
    path == pat
}

fn io_err<E: std::fmt::Display>(e: E) -> io::Error {
    io::Error::other(format!("{e}"))
}
