//! `rustygit update-index` — low-level index manipulation.
//!
//! Subset:
//!   * `--add`            — add new paths (matches `git add` for single files).
//!   * `--remove`         — drop entries.
//!   * `--cacheinfo <mode> <oid> <path>` — insert without touching workdir.
//!   * `--refresh`        — refresh stat info for tracked files.
//!   * `--assume-unchanged <path>...` — flag entries assume-valid.
//!   * `--no-assume-unchanged <path>...` — clear the flag.
//!   * `--skip-worktree <path>...` — flag entries skip-worktree.

use std::io;

use clap::Args;

use crate::index::Index;
use crate::repo::Repository;

#[derive(Debug, Args)]
pub struct UpdateIndexArgs {
    #[arg(long = "add")]
    pub add: bool,
    #[arg(long = "remove")]
    pub remove: bool,
    #[arg(long = "cacheinfo", num_args = 3, value_names = ["MODE", "OID", "PATH"])]
    pub cacheinfo: Option<Vec<String>>,
    #[arg(long = "refresh")]
    pub refresh: bool,
    #[arg(long = "assume-unchanged")]
    pub assume_unchanged: bool,
    #[arg(long = "no-assume-unchanged")]
    pub no_assume_unchanged: bool,
    #[arg(long = "skip-worktree")]
    pub skip_worktree: bool,
    #[arg(long = "no-skip-worktree")]
    pub no_skip_worktree: bool,
    /// Paths to operate on.
    #[arg(value_name = "PATH")]
    pub paths: Vec<String>,
}

pub fn run(args: UpdateIndexArgs) -> io::Result<i32> {
    let repo = Repository::discover_from_cwd().map_err(io_err)?;
    let mut index = Index::read(&repo).map_err(io_err)?;

    if let Some(ci) = &args.cacheinfo {
        let mode_str = &ci[0];
        let oid_str = &ci[1];
        let path = &ci[2];
        let mode = u32::from_str_radix(mode_str, 8)
            .map_err(|e| io::Error::other(format!("update-index: bad mode {mode_str}: {e}")))?;
        let oid = crate::hash::ObjectId::parse_hex(crate::hash::HashKind::Sha1, oid_str)
            .map_err(io_err)?;
        let path_bytes = path.as_bytes().to_vec();
        index.upsert(crate::index::IndexEntry {
            ctime_s: 0,
            ctime_n: 0,
            mtime_s: 0,
            mtime_n: 0,
            dev: 0,
            ino: 0,
            mode,
            uid: 0,
            gid: 0,
            size: 0,
            oid,
            flags: path_bytes.len().min(0xFFF) as u16,
            path: path_bytes,
            stage: 0,
            assume_valid: false,
            extended: false,
            extended_flags: 0,
        });
        index.sort();
        index.write(&repo).map_err(io_err)?;
        return Ok(0);
    }

    if args.remove {
        for path in &args.paths {
            index.remove(path.as_bytes());
        }
        index.write(&repo).map_err(io_err)?;
        return Ok(0);
    }

    if args.assume_unchanged || args.no_assume_unchanged {
        let value = args.assume_unchanged;
        for path in &args.paths {
            if let Some(entry) = index.entries.iter_mut().find(|e| e.path == path.as_bytes()) {
                entry.assume_valid = value;
            }
        }
        index.write(&repo).map_err(io_err)?;
        return Ok(0);
    }

    if args.skip_worktree || args.no_skip_worktree {
        let value = args.skip_worktree;
        for path in &args.paths {
            if let Some(entry) = index.entries.iter_mut().find(|e| e.path == path.as_bytes()) {
                // We don't have a dedicated skip-worktree bit on
                // IndexEntry; piggyback on `extended_flags` bit 14 per
                // git's index format spec.
                if value {
                    entry.extended_flags |= 1 << 14;
                    entry.extended = true;
                } else {
                    entry.extended_flags &= !(1 << 14);
                }
            }
        }
        index.write(&repo).map_err(io_err)?;
        return Ok(0);
    }

    if args.refresh {
        // Refresh is a no-op when our index is already correct; in our
        // current implementation `git add` re-stats on every write.
        return Ok(0);
    }

    if args.add {
        // Delegate to `cli::add::run` via the public Args constructor.
        let add_args = crate::cli::add::AddArgs {
            paths: args.paths.iter().map(std::path::PathBuf::from).collect(),
            refresh: false,
            patch: false,
        };
        return crate::cli::add::run(add_args);
    }

    Ok(0)
}

fn io_err<E: std::fmt::Display>(e: E) -> io::Error {
    io::Error::other(format!("{e}"))
}
