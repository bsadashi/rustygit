//! `rustygit clone` — routes to local or network clone based on URL scheme.
//!
//! Argv parsing + dispatch:
//!   - `https://`, `http://` → `crate::clone::network::clone_network` (M10)
//!   - anything else (file path, `file://`, bare absolute path) →
//!     `crate::clone::clone_local` (M8)
//!
//! We map errors to git's exit-code conventions: 128 for fatal errors,
//! 0 on success.

use std::io;
use std::path::{Path, PathBuf};

use clap::Args;

use crate::clone::network::{clone_network, NetworkCloneOpts};
use crate::clone::{clone_local, CloneError, CloneOpts};
use crate::hash::ObjectId;
use crate::hooks::{self, HookRunner};
use crate::refs::{FullName, RefTarget};
use crate::repo::Repository;

#[derive(Debug, Args)]
pub struct CloneArgs {
    /// Suppress progress messages.
    #[arg(short = 'q', long = "quiet")]
    pub quiet: bool,

    /// Don't check out a working tree.
    #[arg(long = "no-checkout", short = 'n')]
    pub no_checkout: bool,

    /// Source repository (file path or `file://` URL).
    #[arg(value_name = "REPOSITORY")]
    pub source: String,

    /// Destination directory. Defaults to a basename derived from the source.
    #[arg(value_name = "DIRECTORY")]
    pub dest: Option<String>,
}

pub fn run(args: CloneArgs) -> io::Result<i32> {
    let dst = match &args.dest {
        Some(d) => PathBuf::from(d),
        None => match default_dest_from_source(&args.source) {
            Some(d) => d,
            None => {
                eprintln!(
                    "rustygit: clone: cannot derive destination from source '{}'",
                    args.source
                );
                return Ok(128);
            }
        },
    };

    // Route by URL scheme. Network clones go through protocol-v2 over HTTPS
    // or SSH; anything else is treated as a local path source.
    let lower = args.source.to_ascii_lowercase();
    let is_http = lower.starts_with("https://") || lower.starts_with("http://");
    let is_ssh = crate::transport::ssh::is_ssh_url(&args.source);
    let clone_result = if is_http || is_ssh {
        let opts = NetworkCloneOpts {
            quiet: args.quiet,
            no_checkout: args.no_checkout,
        };
        match clone_network(&args.source, &dst, &opts) {
            Ok(()) => Ok(()),
            Err(e) => {
                eprintln!("fatal: {e}");
                return Ok(128);
            }
        }
    } else {
        let src = source_to_path(&args.source);
        let opts = CloneOpts {
            quiet: args.quiet,
            no_checkout: args.no_checkout,
        };
        match clone_local(&src, &dst, &opts) {
            Ok(()) => Ok::<(), CloneError>(()),
            Err(e) => {
                eprintln!("fatal: {e}");
                return Ok(match e {
                    CloneError::DestNotEmpty(_) => 128,
                    _ => 128,
                });
            }
        }
    };
    let _: Result<(), _> = clone_result;

    // post-checkout: fire on the freshly-cloned repo unless --no-checkout
    // was passed. argv = `<null-ref> <new-head> 1` per githooks(5).
    if !args.no_checkout {
        if let Ok(repo) = Repository::discover(&dst) {
            let head_name = FullName::new("HEAD").ok();
            let new_oid = head_name.and_then(|n| match repo.refs().read(&n).ok().flatten() {
                Some(r) => match r.target {
                    RefTarget::Direct(o) => Some(o),
                    RefTarget::Symbolic(target) => RefTarget::resolve(repo.refs(), &target)
                        .ok()
                        .flatten()
                        .map(|(_, o)| o),
                },
                None => None,
            });
            let runner = HookRunner::from_repo(&repo);
            let null = ObjectId::null(repo.hash_kind()).to_string();
            let new_s = new_oid
                .map(|o| o.to_string())
                .unwrap_or_else(|| null.clone());
            match runner.run("post-checkout", &[&null, &new_s, "1"], None) {
                Ok(crate::hooks::HookOutcome::Ran { exit_code }) if exit_code != 0 => {
                    hooks::print_warning("clone", "post-checkout", exit_code);
                }
                _ => {}
            }
        }
    }

    Ok(0)
}

/// Convert the user's REPOSITORY argument (which may include `file://`) into a
/// `PathBuf` for `clone_local` to chew on. The function also handles the form
/// without a scheme.
fn source_to_path(s: &str) -> PathBuf {
    PathBuf::from(s)
}

/// Derive a default destination directory from a source URL/path. Mirrors
/// git's `guess_dir_name` in builtin/clone.c with the simplifications M8
/// allows (local paths only).
///
/// Examples:
///   `/tmp/foo`           -> `foo`
///   `/tmp/foo.git`       -> `foo`
///   `/tmp/foo/.git`      -> `foo`
///   `file:///tmp/foo`    -> `foo`
fn default_dest_from_source(s: &str) -> Option<PathBuf> {
    // Drop `file://` if present.
    let body = s.strip_prefix("file://").unwrap_or(s);
    // Strip any trailing slashes git handled with rstrip.
    let trimmed = body.trim_end_matches('/');
    let path = Path::new(trimmed);
    let last = path.file_name()?.to_string_lossy().into_owned();
    // `<repo>/.git` -> use the parent's basename.
    let basename = if last == ".git" {
        let parent = path.parent()?;
        parent.file_name()?.to_string_lossy().into_owned()
    } else {
        last
    };
    // Strip a single trailing `.git` suffix if present.
    let basename = basename
        .strip_suffix(".git")
        .map(|s| s.to_string())
        .unwrap_or(basename);
    if basename.is_empty() {
        return None;
    }
    Some(PathBuf::from(basename))
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Debug, Parser)]
    struct Wrap {
        #[command(flatten)]
        args: CloneArgs,
    }

    #[test]
    fn parses_minimal() {
        let w = Wrap::try_parse_from(["x", "/tmp/source"]).unwrap();
        assert_eq!(w.args.source, "/tmp/source");
        assert!(w.args.dest.is_none());
        assert!(!w.args.quiet);
        assert!(!w.args.no_checkout);
    }

    #[test]
    fn parses_quiet_no_checkout_dest() {
        let w = Wrap::try_parse_from(["x", "-q", "-n", "src", "out"]).unwrap();
        assert!(w.args.quiet);
        assert!(w.args.no_checkout);
        assert_eq!(w.args.dest.as_deref(), Some("out"));
    }

    #[test]
    fn default_dest_strips_dot_git() {
        let p = default_dest_from_source("/tmp/foo.git").unwrap();
        assert_eq!(p, PathBuf::from("foo"));
    }

    #[test]
    fn default_dest_handles_dot_git_subdir() {
        let p = default_dest_from_source("/tmp/foo/.git").unwrap();
        assert_eq!(p, PathBuf::from("foo"));
    }

    #[test]
    fn default_dest_strips_file_scheme() {
        let p = default_dest_from_source("file:///tmp/foo").unwrap();
        assert_eq!(p, PathBuf::from("foo"));
    }

    #[test]
    fn default_dest_trailing_slash() {
        let p = default_dest_from_source("/tmp/foo/").unwrap();
        assert_eq!(p, PathBuf::from("foo"));
    }
}
