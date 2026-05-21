//! `rustygit pull <remote>` — stub for M10.
//!
//! Real `git pull` is `git fetch` followed by either `git merge` or `git
//! rebase`. We have fetch (this milestone), but no merge yet — merge ships in
//! M13. Until then, `pull` runs fetch and then errors with a deferred-feature
//! message, so users discover the limitation early rather than after a clean
//! fetch has updated remote-tracking refs without applying the work locally.

use std::io;

use clap::Args;

use super::fetch::{self, FetchArgs};

#[derive(Debug, Args)]
pub struct PullArgs {
    /// Remote name or URL. M10: only URLs are accepted (see `fetch`).
    #[arg(value_name = "REMOTE", default_value = "origin")]
    pub remote: String,
}

pub fn run(args: PullArgs) -> io::Result<i32> {
    // Dispatch to fetch first. If fetch fails, propagate its exit code — we
    // don't want to mislead the user about why pull failed.
    let fetch_args = FetchArgs {
        quiet: false,
        remote: args.remote,
    };
    let code = fetch::run(fetch_args)?;
    if code != 0 {
        return Ok(code);
    }

    // Fetch succeeded — but the merge step isn't here yet.
    eprintln!("rustygit: pull: automatic merge not implemented (M13); use fetch + merge manually");
    Ok(1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Debug, Parser)]
    struct Wrap {
        #[command(flatten)]
        args: PullArgs,
    }

    #[test]
    fn parses_with_default() {
        let w = Wrap::try_parse_from(["x"]).unwrap();
        assert_eq!(w.args.remote, "origin");
    }

    #[test]
    fn parses_with_explicit_remote() {
        let w = Wrap::try_parse_from(["x", "https://example.com/r.git"]).unwrap();
        assert_eq!(w.args.remote, "https://example.com/r.git");
    }
}
