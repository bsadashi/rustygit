//! `rustygit push` — routes to local or network push based on the remote
//! URL scheme. Mirrors `clone`'s dispatch pattern.

use std::io;
use std::path::{Path, PathBuf};

use clap::Args;

use crate::hash::ObjectId;
use crate::hooks::{self, HookRunner};
use crate::push::local::{LocalPushReport, RefOutcome};
use crate::push::network::{NetworkPushReport, NetworkRefOutcome};
use crate::push::{parse_refspec, push_local, push_network, PushOpts, Refspec};
use crate::refs::{FullName, RefTarget};
use crate::repo::Repository;

#[derive(Debug, Args)]
pub struct PushArgs {
    /// Force-update (non-fast-forward).
    #[arg(short = 'f', long = "force")]
    pub force: bool,

    /// Atomic push (one fail rolls back all). Forwarded as a capability;
    /// only enforced server-side.
    #[arg(long = "atomic")]
    pub atomic: bool,

    /// Set upstream tracking for the pushed ref. (M11: ignored;
    /// remote-tracking is always updated for network push.)
    #[arg(short = 'u', long = "set-upstream")]
    pub set_upstream: bool,

    /// Quiet mode.
    #[arg(short = 'q', long = "quiet")]
    pub quiet: bool,

    /// Delete instead of pushing (alias for `:<refname>` refspec).
    #[arg(long = "delete")]
    pub delete: bool,

    /// Skip the `pre-push` hook. Mirrors `git push --no-verify`.
    #[arg(long = "no-verify")]
    pub no_verify: bool,

    /// Remote: a URL (HTTPS or a local bare path). M11 doesn't resolve
    /// named remotes — that needs config writing (M12+).
    #[arg(value_name = "REMOTE")]
    pub remote: String,

    /// Refspecs to push. If empty, push the current branch.
    #[arg(value_name = "REFSPEC")]
    pub refspecs: Vec<String>,
}

pub fn run(args: PushArgs) -> io::Result<i32> {
    let repo = match Repository::discover_from_cwd() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("fatal: {e}");
            return Ok(128);
        }
    };

    // Parse refspecs, applying --delete if no refspecs were given but
    // --delete was: that's a usage error.
    let mut refspecs: Vec<Refspec> = Vec::new();
    if args.refspecs.is_empty() {
        if args.delete {
            eprintln!("fatal: --delete requires a refspec");
            return Ok(129);
        }
        // Push current branch.
        match current_branch(&repo) {
            Some(branch) => match parse_refspec(&branch) {
                Ok(r) => refspecs.push(r),
                Err(e) => {
                    eprintln!("fatal: cannot build refspec for current branch: {e}");
                    return Ok(128);
                }
            },
            None => {
                eprintln!("fatal: HEAD is detached; no current branch to push");
                return Ok(128);
            }
        }
    } else {
        for spec in &args.refspecs {
            let raw = if args.delete && !spec.starts_with(':') {
                format!(":{spec}")
            } else {
                spec.clone()
            };
            match parse_refspec(&raw) {
                Ok(r) => refspecs.push(r),
                Err(e) => {
                    eprintln!("fatal: {e}");
                    return Ok(128);
                }
            }
        }
    }

    let opts = PushOpts {
        force: args.force,
        atomic: args.atomic,
        quiet: args.quiet,
    };

    // pre-push hook: takes <remote-name> <remote-url> as argv, and one line
    // of `<local-ref> SP <local-sha> SP <remote-ref> SP <remote-sha>` per
    // ref on stdin. For an unnamed remote (a URL only) git uses the URL for
    // both name and location. We don't yet resolve named remotes from
    // config, so we mirror that.
    if !args.no_verify {
        let hook_runner = HookRunner::from_repo(&repo);
        let stdin = build_pre_push_stdin(&repo, &args.remote, &refspecs);
        let outcome = hook_runner.run(
            "pre-push",
            &[&args.remote, &args.remote],
            Some(stdin.as_bytes()),
        )?;
        if outcome.aborts_parent() {
            let code = outcome.exit_code().unwrap_or(1);
            hooks::print_abort("push", "pre-push", code);
            return Ok(1);
        }
    }

    // Route by URL scheme. Anything else is treated as a local path.
    let lower = args.remote.to_ascii_lowercase();
    if lower.starts_with("https://") || lower.starts_with("http://") {
        match push_network(&repo, &args.remote, &refspecs, &opts) {
            Ok(report) => {
                if !args.quiet {
                    print_network_report(&report);
                }
                Ok(0)
            }
            Err(e) => {
                eprintln!("fatal: {e}");
                Ok(128)
            }
        }
    } else {
        let dst = remote_to_path(&args.remote);
        match push_local(&repo, &dst, &refspecs, &opts) {
            Ok(report) => {
                if !args.quiet {
                    print_local_report(&report);
                }
                Ok(0)
            }
            Err(e) => {
                eprintln!("fatal: {e}");
                Ok(128)
            }
        }
    }
}

/// Read HEAD and return the branch name suffix (e.g. `main` for
/// `refs/heads/main`). Returns None if HEAD is detached or unborn.
fn current_branch(repo: &Repository) -> Option<String> {
    use crate::refs::FullName;
    let head = FullName::new("HEAD").ok()?;
    let r = repo.refs().read(&head).ok()??;
    match r.target {
        RefTarget::Symbolic(target) => {
            let s = target.as_str().strip_prefix("refs/heads/")?;
            Some(s.to_string())
        }
        RefTarget::Direct(_) => None,
    }
}

fn remote_to_path(s: &str) -> PathBuf {
    if let Some(rest) = s.strip_prefix("file://") {
        PathBuf::from(rest)
    } else {
        PathBuf::from(s)
    }
}

/// Build the pre-push hook's stdin payload from the planned refspecs.
///
/// Format (per githooks(5) `pre-push`):
///   `<local-ref> SP <local-sha> SP <remote-ref> SP <remote-sha> LF`
///
/// For deletes, `local-ref` is `(delete)` and `local-sha` is all zeroes.
/// For the `remote-sha` we do a best-effort lookup: for local destinations
/// we read the destination repo's ref directly; for network we use zeros
/// (a pre-push hook that needs accurate remote-sha for HTTPS pushes is
/// rare and we'd need an extra round trip for it).
fn build_pre_push_stdin(repo: &Repository, remote: &str, refspecs: &[Refspec]) -> String {
    let null_oid = ObjectId::null(repo.hash_kind()).to_string();
    // Resolve the destination repo lazily once for local pushes.
    let dst_repo = if !is_url(remote) {
        let dst_path = remote_to_path(remote);
        Repository::open(resolve_dst_gitdir_best_effort(&dst_path)).ok()
    } else {
        None
    };

    let mut out = String::new();
    for rs in refspecs {
        let (local_ref, local_sha) = if rs.is_delete() {
            ("(delete)".to_string(), null_oid.clone())
        } else {
            let sha = match FullName::new(rs.src.clone())
                .ok()
                .and_then(|n| repo.refs().read(&n).ok().flatten())
            {
                Some(r) => match r.target {
                    RefTarget::Direct(o) => o.to_string(),
                    RefTarget::Symbolic(target) => {
                        match crate::refs::RefTarget::resolve(repo.refs(), &target)
                            .ok()
                            .flatten()
                        {
                            Some((_, o)) => o.to_string(),
                            None => null_oid.clone(),
                        }
                    }
                },
                None => null_oid.clone(),
            };
            (rs.src.clone(), sha)
        };

        let remote_sha = match dst_repo.as_ref() {
            Some(dst) => match FullName::new(rs.dst.clone())
                .ok()
                .and_then(|n| dst.refs().read(&n).ok().flatten())
            {
                Some(r) => match r.target {
                    RefTarget::Direct(o) => o.to_string(),
                    _ => null_oid.clone(),
                },
                None => null_oid.clone(),
            },
            None => null_oid.clone(),
        };

        out.push_str(&format!(
            "{local_ref} {local_sha} {} {remote_sha}\n",
            rs.dst
        ));
    }
    out
}

fn is_url(s: &str) -> bool {
    let l = s.to_ascii_lowercase();
    l.starts_with("http://") || l.starts_with("https://")
}

/// Mirror `push::local::resolve_dst_gitdir`'s rules without re-exporting it:
/// strip `file://`, then try `<p>/.git` if `<p>` is a working tree.
fn resolve_dst_gitdir_best_effort(p: &Path) -> PathBuf {
    let s = p.to_string_lossy();
    let stripped: PathBuf = if let Some(rest) = s.strip_prefix("file://") {
        PathBuf::from(rest)
    } else {
        p.to_path_buf()
    };
    let dot_git = stripped.join(".git");
    if dot_git.is_dir() {
        dot_git
    } else {
        stripped
    }
}

// ---------------------------------------------------------------------------
// Output formatting
// ---------------------------------------------------------------------------

fn print_local_report(report: &LocalPushReport) {
    println!("To {}", report.dst_display);
    for outcome in &report.outcomes {
        match outcome {
            RefOutcome::Created { dst, new } => {
                println!(" * [new branch]      {} -> {dst}", new.short_hex(7));
            }
            RefOutcome::Updated { dst, old, new } => {
                println!(
                    "   {}..{}  {} -> {dst}",
                    old.short_hex(7),
                    new.short_hex(7),
                    dst
                );
            }
            RefOutcome::Forced { dst, old, new } => {
                println!(
                    " + {}...{}  {} -> {dst} (forced update)",
                    old.short_hex(7),
                    new.short_hex(7),
                    dst
                );
            }
            RefOutcome::Deleted { dst, .. } => {
                println!(" - [deleted]         {dst}");
            }
            RefOutcome::UpToDate { dst, .. } => {
                println!("   [up to date]      {dst}");
            }
        }
    }
}

fn print_network_report(report: &NetworkPushReport) {
    println!("To {}", report.url);
    for outcome in &report.outcomes {
        match outcome {
            NetworkRefOutcome::Created { dst, new } => {
                println!(" * [new branch]      {} -> {dst}", new.short_hex(7));
            }
            NetworkRefOutcome::Updated { dst, old, new } => {
                println!(
                    "   {}..{}  {} -> {dst}",
                    old.short_hex(7),
                    new.short_hex(7),
                    dst
                );
            }
            NetworkRefOutcome::Forced { dst, old, new } => {
                println!(
                    " + {}...{}  {} -> {dst} (forced update)",
                    old.short_hex(7),
                    new.short_hex(7),
                    dst
                );
            }
            NetworkRefOutcome::Deleted { dst, .. } => {
                println!(" - [deleted]         {dst}");
            }
            NetworkRefOutcome::UpToDate { dst, .. } => {
                println!("   [up to date]      {dst}");
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Debug, Parser)]
    struct Wrap {
        #[command(flatten)]
        args: PushArgs,
    }

    #[test]
    fn parses_minimal() {
        let w = Wrap::try_parse_from(["x", "origin"]).unwrap();
        assert_eq!(w.args.remote, "origin");
        assert!(w.args.refspecs.is_empty());
        assert!(!w.args.force);
        assert!(!w.args.atomic);
    }

    #[test]
    fn parses_force_and_refspec() {
        let w = Wrap::try_parse_from(["x", "-f", "origin", "main"]).unwrap();
        assert!(w.args.force);
        assert_eq!(w.args.remote, "origin");
        assert_eq!(w.args.refspecs, vec!["main".to_string()]);
    }

    #[test]
    fn parses_delete_flag() {
        let w = Wrap::try_parse_from(["x", "--delete", "origin", "topic"]).unwrap();
        assert!(w.args.delete);
    }

    #[test]
    fn parses_multiple_refspecs() {
        let w = Wrap::try_parse_from(["x", "origin", "main", "topic"]).unwrap();
        assert_eq!(
            w.args.refspecs,
            vec!["main".to_string(), "topic".to_string()]
        );
    }

    #[test]
    fn parses_set_upstream() {
        let w = Wrap::try_parse_from(["x", "-u", "origin", "main"]).unwrap();
        assert!(w.args.set_upstream);
    }
}
