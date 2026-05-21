//! `rustygit repack` — consolidate the object store into a single fresh pack.
//!
//! M9 scope is intentionally narrow: we always pack every reachable object
//! (no incremental mode, no `--keep-pack`, no `--max-pack-size`). The flag
//! surface here matches what porcelain `gc` needs.
//!
//! Algorithm:
//! 1. Compute the full reachable set via `ReachableSet::mark_all`.
//! 2. Write a new pack at `.git/objects/pack/pack-<hash>.{pack,idx}` via
//!    `pack::write_pack`.
//! 3. If `-d`: delete every other `.pack` (and its `.idx`) in that directory,
//!    plus every loose object whose oid is now in our pack. Bare-bones
//!    "redundant loose object" pruning — anything not in the new pack stays
//!    on disk (so we don't clobber unreachable-but-still-recent objects).
//!
//! Reflog/object-grace handling is M14+; for now `gc.reflogExpire` and
//! `gc.pruneExpire` are effectively ignored.

use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use clap::Args;

use crate::pack::{write_pack, PackBuildError, PackBuildResult};
use crate::reachable::{ReachableError, ReachableSet};
use crate::repo::Repository;

#[derive(Debug, Args)]
pub struct RepackArgs {
    /// After writing the new pack, delete every old pack and every loose
    /// object that is now redundant (i.e. exists in the new pack).
    #[arg(short = 'd')]
    pub delete: bool,

    /// Pack all reachable objects (not just loose ones). Currently the only
    /// mode supported; the flag is accepted for compatibility.
    #[arg(short = 'a')]
    pub all: bool,

    /// Quiet output (suppress the "Counting objects" line).
    #[arg(short = 'q', long = "quiet")]
    pub quiet: bool,
}

pub fn run(args: RepackArgs) -> io::Result<i32> {
    let repo = Repository::discover_from_cwd().map_err(io_err)?;
    match repack(&repo, &args) {
        Ok(()) => Ok(0),
        Err(e) => {
            eprintln!("rustygit: repack: {e}");
            Ok(128)
        }
    }
}

/// Library entry point: callable directly by `gc`.
pub fn repack(repo: &Repository, args: &RepackArgs) -> Result<(), RepackError> {
    let pack_dir = repo.gitdir().join("objects").join("pack");
    fs::create_dir_all(&pack_dir).map_err(|e| RepackError::Io {
        path: pack_dir.clone(),
        source: e,
    })?;

    let reachable = ReachableSet::mark_all(repo)?;

    if reachable.oids.is_empty() {
        // Nothing to do. git's `repack -d` is a noop here; we follow suit.
        if !args.quiet {
            println!("Counting objects: 0, done.");
            println!("Nothing new to pack.");
        }
        return Ok(());
    }

    let oids: Vec<_> = reachable.oids.iter().copied().collect();
    let count = oids.len();

    if !args.quiet {
        println!("Counting objects: {count}, done.");
    }

    let result = write_pack(&oids, repo.odb(), &pack_dir, repo.hash_kind())?;

    if args.delete {
        prune_redundant(repo, &pack_dir, &result, &reachable.oids)?;
    }
    Ok(())
}

/// Delete every existing pack except the one we just wrote, plus every loose
/// object that's now packed (its oid is in `packed_oids`).
fn prune_redundant(
    repo: &Repository,
    pack_dir: &Path,
    keep: &PackBuildResult,
    packed_oids: &BTreeSet<crate::hash::ObjectId>,
) -> Result<(), RepackError> {
    // Delete old `.pack` + `.idx` pairs. We identify the "keep" pair by exact
    // path, not by hash, so a same-content rewrite still drops the original.
    let keep_pack = keep
        .pack_path
        .canonicalize()
        .unwrap_or_else(|_| keep.pack_path.clone());
    let keep_idx = keep
        .idx_path
        .canonicalize()
        .unwrap_or_else(|_| keep.idx_path.clone());
    let entries = fs::read_dir(pack_dir).map_err(|e| RepackError::Io {
        path: pack_dir.to_path_buf(),
        source: e,
    })?;
    for entry in entries {
        let entry = entry.map_err(|e| RepackError::Io {
            path: pack_dir.to_path_buf(),
            source: e,
        })?;
        let path = entry.path();
        let ext = path.extension().and_then(|s| s.to_str());
        if !matches!(ext, Some("pack" | "idx")) {
            continue;
        }
        let canon = path.canonicalize().unwrap_or_else(|_| path.clone());
        if canon == keep_pack || canon == keep_idx {
            continue;
        }
        if let Err(e) = fs::remove_file(&path) {
            // Best-effort — log to stderr but don't abort. A pack we can't
            // delete still leaves the repo correct, just slightly bloated.
            eprintln!("rustygit: repack: failed to remove {}: {e}", path.display());
        }
    }

    // Delete loose objects covered by the new pack. We use the loose store's
    // path layout (`objects/aa/bbbb...`) directly — going through the odb
    // wouldn't help because there's no "delete" method on `ObjectStore`.
    let objects_dir = repo.gitdir().join("objects");
    for oid in packed_oids {
        let hex = oid.to_string();
        let (dir, file) = hex.split_at(2);
        let loose_path = objects_dir.join(dir).join(file);
        if loose_path.is_file() {
            if let Err(e) = fs::remove_file(&loose_path) {
                eprintln!(
                    "rustygit: repack: failed to remove loose {}: {e}",
                    loose_path.display()
                );
            }
        }
    }

    // Try to clean up emptied shard directories (`aa/`). Failure is harmless.
    if let Ok(shards) = fs::read_dir(&objects_dir) {
        for shard in shards.flatten() {
            let p = shard.path();
            if !p.is_dir() {
                continue;
            }
            // Only remove the two-hex-digit shard dirs; never info/, pack/, etc.
            let name = match p.file_name().and_then(|s| s.to_str()) {
                Some(n) => n.to_string(),
                None => continue,
            };
            if name.len() != 2 || !name.chars().all(|c| c.is_ascii_hexdigit()) {
                continue;
            }
            // remove_dir only succeeds when the dir is empty — exactly what
            // we want here.
            let _ = fs::remove_dir(&p);
        }
    }
    Ok(())
}

#[derive(thiserror::Error, Debug)]
pub enum RepackError {
    #[error("io on {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error(transparent)]
    Pack(#[from] PackBuildError),
    #[error(transparent)]
    Reachable(#[from] ReachableError),
}

fn io_err<E: std::fmt::Display>(e: E) -> io::Error {
    io::Error::other(format!("{e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Debug, Parser)]
    struct Wrap {
        #[command(flatten)]
        args: RepackArgs,
    }

    #[test]
    fn parses_defaults() {
        let w = Wrap::try_parse_from(["x"]).unwrap();
        assert!(!w.args.delete);
        assert!(!w.args.all);
        assert!(!w.args.quiet);
    }

    #[test]
    fn parses_full_set() {
        let w = Wrap::try_parse_from(["x", "-a", "-d", "-q"]).unwrap();
        assert!(w.args.delete);
        assert!(w.args.all);
        assert!(w.args.quiet);
    }

    #[test]
    fn parses_long_quiet() {
        let w = Wrap::try_parse_from(["x", "--quiet"]).unwrap();
        assert!(w.args.quiet);
    }

    #[test]
    fn rejects_unknown_flag() {
        let r = Wrap::try_parse_from(["x", "--no-such-flag"]);
        assert!(r.is_err());
    }
}
