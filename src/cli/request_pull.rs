//! `rustygit request-pull` — emit a "please pull from X" summary.
//!
//! Args: `<start> <url> [<end>]`
//!
//! Output: text body suitable for emailing — names the branch, summarizes
//! commits, lists diffstat highlights.

use std::io::{self, Write};

use clap::Args;

use crate::repo::Repository;

#[derive(Debug, Args)]
pub struct RequestPullArgs {
    /// Tip everyone has (the diverging point).
    #[arg(value_name = "START", required = true)]
    pub start: String,
    /// Remote URL where the new commits live.
    #[arg(value_name = "URL", required = true)]
    pub url: String,
    /// Local end of the range (default: HEAD).
    #[arg(value_name = "END")]
    pub end: Option<String>,
}

pub fn run(args: RequestPullArgs) -> io::Result<i32> {
    let repo = Repository::discover_from_cwd().map_err(io_err)?;
    let start = crate::revparse::resolve(repo.refs(), repo.odb(), &args.start).map_err(io_err)?;
    let end_rev = args.end.as_deref().unwrap_or("HEAD");
    let end = crate::revparse::resolve(repo.refs(), repo.odb(), end_rev).map_err(io_err)?;

    let stdout = io::stdout();
    let mut out = stdout.lock();
    writeln!(
        out,
        "The following changes since commit {start}:\n\
         \n\
         are available in the Git repository at:\n\
         \n\
           {} {end_rev}\n\
         \n\
         for you to fetch changes up to {end}:\n",
        args.url
    )?;
    // shortlog of commits in start..end
    let walk = crate::revparse::resolve_range(
        repo.refs(),
        repo.odb(),
        &format!("{}..{end_rev}", args.start),
    )
    .map_err(io_err)?
    .unwrap_or_default();
    writeln!(
        out,
        "----------------------------------------------------------------"
    )?;
    for oid in &walk {
        let raw = repo.odb().read(oid).map_err(io_err)?;
        let commit = crate::commit::Commit::parse(&raw.data, repo.hash_kind()).map_err(io_err)?;
        let subject = String::from_utf8_lossy(&commit.message)
            .lines()
            .next()
            .unwrap_or("")
            .to_string();
        writeln!(out, "  {} ({})", subject, oid.short_hex(7))?;
    }
    writeln!(
        out,
        "----------------------------------------------------------------"
    )?;
    Ok(0)
}

fn io_err<E: std::fmt::Display>(e: E) -> io::Error {
    io::Error::other(format!("{e}"))
}
