//! `rustygit fetch <remote>` — download new objects and update
//! `refs/remotes/<remote>/*` without touching the working tree, the index,
//! or `refs/heads/`.
//!
//! M10 scope:
//!   - `<remote>` must be a literal URL (https://). Remote-name resolution
//!     (e.g. `origin` → URL from `.git/config`) lands in M11 once we have a
//!     config-writer to set up `[remote "origin"]` blocks.
//!   - `fetch <url>` updates `refs/remotes/origin/*` by convention. We don't
//!     yet honor a refspec.
//!   - No `--depth` (shallow) and no haves-negotiation. The current
//!     implementation re-downloads every advertised tip we don't already
//!     have locally; haves come in M11.

use std::io;

use clap::Args;

use crate::clone::network::{fetch_into_repo, NetworkCloneError};
use crate::repo::Repository;

#[derive(Debug, Args)]
pub struct FetchArgs {
    /// Suppress progress output.
    #[arg(short = 'q', long = "quiet")]
    pub quiet: bool,

    /// Remote name OR URL. For M10 this must be a URL — remote-name
    /// resolution requires the config-writer landing in M11.
    #[arg(value_name = "REMOTE")]
    pub remote: String,
}

pub fn run(args: FetchArgs) -> io::Result<i32> {
    let repo = match Repository::discover_from_cwd() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("fatal: {e}");
            return Ok(128);
        }
    };

    let (url, remote_name) = match parse_remote(&args.remote) {
        Ok(pair) => pair,
        Err(msg) => {
            eprintln!("rustygit: fetch: {msg}");
            return Ok(128);
        }
    };

    match fetch_into_repo(&repo, url, remote_name, args.quiet) {
        Ok(refs) => {
            if !args.quiet {
                println!("From {url}");
                for r in &refs {
                    if let Some(suffix) = r.name.strip_prefix("refs/heads/") {
                        // git's format: "<old>..<new> <branch> -> <remote>/<branch>".
                        // M10 doesn't compute old/new diffs yet — just print the
                        // updated tip line in a recognizable shape.
                        println!(" * branch                  {suffix} -> {remote_name}/{suffix}");
                    } else if let Some(tag_suffix) = r.name.strip_prefix("refs/tags/") {
                        println!(" * [new tag]               {tag_suffix} -> {tag_suffix}");
                    }
                }
            }
            Ok(0)
        }
        Err(e) => {
            eprintln!("fatal: {e}");
            Ok(match e {
                NetworkCloneError::DestNotEmpty(_) => 128,
                _ => 128,
            })
        }
    }
}

/// Resolve the user-supplied REMOTE to `(url, remote_name)`.
///
/// - If REMOTE looks like a URL (`https://…`), we use it directly and treat
///   the remote-tracking namespace as `origin`.
/// - Otherwise, refuse — remote-name resolution requires `[remote "<name>"]`
///   config, which we don't yet write or read.
fn parse_remote(remote: &str) -> Result<(&str, &str), &'static str> {
    if looks_like_url(remote) {
        Ok((remote, "origin"))
    } else {
        Err("remote name resolution requires config (M11); pass a full URL for now")
    }
}

fn looks_like_url(s: &str) -> bool {
    s.starts_with("https://")
        || s.starts_with("http://")
        || s.starts_with("git://")
        || s.starts_with("ssh://")
        || s.starts_with("file://")
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Debug, Parser)]
    struct Wrap {
        #[command(flatten)]
        args: FetchArgs,
    }

    #[test]
    fn parses_minimal() {
        let w = Wrap::try_parse_from(["x", "https://example.com/repo.git"]).unwrap();
        assert_eq!(w.args.remote, "https://example.com/repo.git");
        assert!(!w.args.quiet);
    }

    #[test]
    fn parses_quiet() {
        let w = Wrap::try_parse_from(["x", "-q", "https://e.com/r.git"]).unwrap();
        assert!(w.args.quiet);
    }

    #[test]
    fn url_detection() {
        assert!(looks_like_url("https://github.com/user/repo.git"));
        assert!(looks_like_url("http://example.com/repo.git"));
        assert!(looks_like_url("git://example.com/repo.git"));
        assert!(!looks_like_url("origin"));
        assert!(!looks_like_url("upstream"));
    }

    #[test]
    fn rejects_bare_remote_name_for_now() {
        match parse_remote("origin") {
            Err(msg) => assert!(msg.contains("M11")),
            Ok(_) => panic!("should reject bare names until M11"),
        }
    }
}
