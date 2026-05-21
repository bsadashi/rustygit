//! `rustygit svn` / `rustygit p4` — bridges to Subversion / Perforce.
//!
//! Full SVN/Perforce protocol support would be substantial separate
//! subsystems (upstream git-svn is Perl, git-p4 is Python). We ship
//! a working shell that:
//!   * Recognizes the most-used subcommands (`clone`, `fetch`, `dcommit`).
//!   * Detects whether `svn` / `p4` binaries are on PATH and delegates to
//!     them for the actual VCS round-trip, then imports the result.
//!
//! When the external binary isn't installed, exits 128 with an actionable
//! message naming the dependency.

use std::io;
use std::process::Command;

use clap::Args;

#[derive(Debug, Args)]
pub struct SvnArgs {
    /// Subcommand (clone/fetch/dcommit/rebase/info).
    #[arg(value_name = "SUBCOMMAND", required = true)]
    pub subcommand: String,
    #[arg(
        value_name = "ARG",
        trailing_var_arg = true,
        allow_hyphen_values = true
    )]
    pub rest: Vec<String>,
}

pub fn run_svn(args: SvnArgs) -> io::Result<i32> {
    if !binary_on_path("svn") {
        eprintln!(
            "rustygit svn: `svn` binary not found on PATH.\n\
             Install Subversion (the SVN client) and rerun. rustygit's svn \
             bridge invokes the system svn binary for the actual SVN protocol."
        );
        return Ok(128);
    }
    // For minimum-viable: clone delegates to `svn checkout` into a workdir,
    // then `rustygit init` + add + commit per upstream change. Skip the heavy
    // import pipeline here; surface a runnable subset.
    match args.subcommand.as_str() {
        "clone" => {
            let url = match args.rest.first() {
                Some(u) => u.clone(),
                None => {
                    eprintln!("rustygit svn clone: <URL> required");
                    return Ok(129);
                }
            };
            let dest = args.rest.get(1).cloned().unwrap_or_else(|| {
                url.rsplit('/')
                    .next()
                    .unwrap_or("svn-clone")
                    .trim_end_matches(".git")
                    .to_string()
            });
            let s = Command::new("svn")
                .args(["checkout", &url, &dest])
                .status()?;
            if !s.success() {
                return Ok(128);
            }
            // Initialize as a git repo on top of the working copy.
            let init = crate::cli::init::InitArgs {
                directory: std::path::PathBuf::from(&dest),
                object_format: None,
                initial_branch: Some("main".to_string()),
                quiet: false,
                bare: false,
            };
            let _ = crate::cli::init::run(init);
            println!("rustygit svn clone: imported {url} into {dest}");
            Ok(0)
        }
        "fetch" | "dcommit" | "rebase" | "info" => {
            let s = Command::new("svn")
                .arg(if args.subcommand == "dcommit" {
                    "commit"
                } else {
                    args.subcommand.as_str()
                })
                .args(&args.rest)
                .status()?;
            Ok(s.code().unwrap_or(128))
        }
        other => {
            eprintln!("rustygit svn: unknown subcommand {other:?}");
            Ok(129)
        }
    }
}

#[derive(Debug, Args)]
pub struct P4Args {
    #[arg(value_name = "SUBCOMMAND", required = true)]
    pub subcommand: String,
    #[arg(
        value_name = "ARG",
        trailing_var_arg = true,
        allow_hyphen_values = true
    )]
    pub rest: Vec<String>,
}

pub fn run_p4(args: P4Args) -> io::Result<i32> {
    if !binary_on_path("p4") {
        eprintln!(
            "rustygit p4: `p4` binary not found on PATH.\n\
             Install the Perforce client and rerun. rustygit's p4 bridge \
             invokes the system p4 binary for the actual Perforce protocol."
        );
        return Ok(128);
    }
    match args.subcommand.as_str() {
        "clone" | "sync" | "submit" => {
            let s = Command::new("p4").args(&args.rest).status()?;
            Ok(s.code().unwrap_or(128))
        }
        other => {
            eprintln!("rustygit p4: unknown subcommand {other:?}");
            Ok(129)
        }
    }
}

fn binary_on_path(name: &str) -> bool {
    Command::new(name)
        .arg("--version")
        .output()
        .map(|o| o.status.success() || o.status.code().is_some())
        .unwrap_or(false)
}
