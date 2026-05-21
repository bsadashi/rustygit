//! `rustygit rerere` — Reuse Recorded Resolution.
//!
//! Minimal-but-real implementation. The on-disk shape matches upstream
//! git:
//!
//! ```text
//! .git/rr-cache/<conflict-id>/
//!     preimage     # the merge buffer WITH `<<<<<<<` markers
//!     postimage    # the resolved merge buffer (no markers)
//!     thisimage    # path-to-result-of-current-merge (for replay)
//! ```
//!
//! The `<conflict-id>` is a hash of the normalized preimage.
//!
//! This implementation ships:
//!   * `status`     — list paths with recorded resolutions.
//!   * `remaining`  — list paths still containing markers.
//!   * `diff`       — print the preimage→postimage diff per recorded ID.
//!   * `forget`     — drop a specific entry by path.
//!   * `clear`      — drop the whole cache.
//!   * `gc`         — drop entries older than `gc.rerereResolved`
//!                    (default 60 days).
//!
//! Automatic record-on-resolve and replay-on-merge are NOT yet wired
//! into the merge/cherry-pick/rebase paths. That requires plumbing
//! through `apply_commit` in the sequencer — a separate v0.3 task.

use std::io::{self, Write};
use std::path::Path;

use clap::{Args, Subcommand};

use crate::repo::Repository;

#[derive(Debug, Args)]
pub struct RerereArgs {
    #[command(subcommand)]
    pub command: Option<RerereCommand>,
}

#[derive(Debug, Subcommand)]
pub enum RerereCommand {
    Status,
    Diff,
    Forget {
        #[arg(value_name = "PATH")]
        paths: Vec<String>,
    },
    Gc,
    Clear,
    Remaining,
}

pub fn run(args: RerereArgs) -> io::Result<i32> {
    let repo = Repository::discover_from_cwd().map_err(io_err)?;
    let cache_dir = repo.gitdir().join("rr-cache");

    match args.command.unwrap_or(RerereCommand::Status) {
        RerereCommand::Status => status(&cache_dir),
        RerereCommand::Diff => diff(&cache_dir),
        RerereCommand::Forget { paths } => forget(&cache_dir, &paths),
        RerereCommand::Gc => gc(&cache_dir),
        RerereCommand::Clear => clear(&cache_dir),
        RerereCommand::Remaining => remaining(&repo),
    }
}

fn status(cache_dir: &Path) -> io::Result<i32> {
    let stdout = io::stdout();
    let mut out = stdout.lock();
    if !cache_dir.is_dir() {
        return Ok(0);
    }
    for entry in std::fs::read_dir(cache_dir)?.flatten() {
        let p = entry.path();
        if !p.is_dir() {
            continue;
        }
        if p.join("postimage").is_file() {
            // The original conflicted path lives in `thisimage` if recorded.
            let this = p.join("thisimage");
            if let Ok(s) = std::fs::read_to_string(&this) {
                writeln!(out, "{}", s.trim())?;
            } else {
                writeln!(out, "{}", p.file_name().unwrap().to_string_lossy())?;
            }
        }
    }
    Ok(0)
}

fn diff(cache_dir: &Path) -> io::Result<i32> {
    let stdout = io::stdout();
    let mut out = stdout.lock();
    if !cache_dir.is_dir() {
        return Ok(0);
    }
    for entry in std::fs::read_dir(cache_dir)?.flatten() {
        let p = entry.path();
        if !p.is_dir() {
            continue;
        }
        let pre = p.join("preimage");
        let post = p.join("postimage");
        if !pre.is_file() || !post.is_file() {
            continue;
        }
        writeln!(out, "--- a/{}", p.file_name().unwrap().to_string_lossy())?;
        writeln!(out, "+++ b/{}", p.file_name().unwrap().to_string_lossy())?;
        let pre_bytes = std::fs::read(&pre)?;
        let post_bytes = std::fs::read(&post)?;
        crate::xdiff::unified_diff(
            &pre_bytes,
            &post_bytes,
            &crate::xdiff::UnifiedDiffOpts::default(),
            &mut out,
        )
        .map_err(|e| io::Error::other(format!("{e}")))?;
    }
    Ok(0)
}

fn forget(cache_dir: &Path, paths: &[String]) -> io::Result<i32> {
    if paths.is_empty() {
        eprintln!("rustygit: rerere forget: missing <path>");
        return Ok(129);
    }
    for path in paths {
        // Look for any rr-cache entry whose thisimage matches.
        if !cache_dir.is_dir() {
            continue;
        }
        for entry in std::fs::read_dir(cache_dir)?.flatten() {
            let dir = entry.path();
            if !dir.is_dir() {
                continue;
            }
            let this = dir.join("thisimage");
            if let Ok(s) = std::fs::read_to_string(&this) {
                if s.trim() == path {
                    std::fs::remove_dir_all(&dir)?;
                }
            }
        }
    }
    Ok(0)
}

fn clear(cache_dir: &Path) -> io::Result<i32> {
    if cache_dir.is_dir() {
        std::fs::remove_dir_all(cache_dir)?;
    }
    Ok(0)
}

fn gc(cache_dir: &Path) -> io::Result<i32> {
    // gc.rerereResolved default = 60 days; gc.rerereUnresolved = 15 days.
    let now = std::time::SystemTime::now();
    let resolved_secs = 60 * 86400;
    let unresolved_secs = 15 * 86400;
    if !cache_dir.is_dir() {
        return Ok(0);
    }
    for entry in std::fs::read_dir(cache_dir)?.flatten() {
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        let mtime = match dir.metadata().and_then(|m| m.modified()) {
            Ok(t) => t,
            Err(_) => continue,
        };
        let age = now.duration_since(mtime).map(|d| d.as_secs()).unwrap_or(0);
        let resolved = dir.join("postimage").is_file();
        let cutoff = if resolved {
            resolved_secs
        } else {
            unresolved_secs
        };
        if age > cutoff {
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
    Ok(0)
}

fn remaining(repo: &Repository) -> io::Result<i32> {
    let stdout = io::stdout();
    let mut out = stdout.lock();
    // Walk the index for stage>0 entries.
    let index = match crate::index::Index::read(repo) {
        Ok(i) => i,
        Err(_) => return Ok(0),
    };
    use std::collections::BTreeSet;
    let mut paths: BTreeSet<Vec<u8>> = BTreeSet::new();
    for entry in &index.entries {
        if entry.stage != 0 {
            paths.insert(entry.path.clone());
        }
    }
    for p in paths {
        out.write_all(&p)?;
        out.write_all(b"\n")?;
    }
    Ok(0)
}

fn io_err<E: std::fmt::Display>(e: E) -> io::Error {
    io::Error::other(format!("{e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_subcommands() {
        use clap::Parser;
        #[derive(Parser, Debug)]
        struct Wrap {
            #[command(flatten)]
            args: RerereArgs,
        }
        let w = Wrap::try_parse_from(["rr", "status"]).unwrap();
        assert!(matches!(w.args.command, Some(RerereCommand::Status)));
    }
}
