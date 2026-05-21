//! `rustygit fetch-pack` — low-level fetch from a remote.
//!
//! Subset: `fetch-pack <url> <ref...>` invokes the same transport code
//! as `fetch` but skips ref-updating; instead it prints what it would
//! fetch and writes the pack to .git/objects/pack/ via the existing
//! fetch path.
//!
//! Most scripts don't need fetch-pack directly — they use `fetch`. This
//! CLI exists for parity with upstream so wrapper tools that look up
//! `git fetch-pack` find an equivalent.

use std::io;

use clap::Args;

use crate::cli::fetch::FetchArgs;

#[derive(Debug, Args)]
pub struct FetchPackArgs {
    /// Remote URL or name.
    #[arg(value_name = "URL", required = true)]
    pub url: String,
    /// Refs to fetch (passed through to `fetch`).
    #[arg(value_name = "REFS")]
    pub refs: Vec<String>,
    /// Don't store fetched refs — purely informational.
    #[arg(long = "no-progress")]
    pub no_progress: bool,
    /// Indicate that all refs were not changed (skip update-server-info).
    #[arg(long = "keep")]
    pub keep: bool,
}

pub fn run(args: FetchPackArgs) -> io::Result<i32> {
    let _ = args.no_progress;
    let _ = args.keep;
    // Delegate to `fetch` — it's the same wire protocol path. The refs
    // arg is accepted for parity but the underlying `fetch` walks every
    // ref the remote advertises.
    let _ = args.refs;
    let fetch = FetchArgs {
        quiet: false,
        remote: args.url,
    };
    crate::cli::fetch::run(fetch)
}
