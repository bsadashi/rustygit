//! `rustygit pack-objects` — read object names from stdin and produce a pack.
//!
//! Two input modes:
//!
//! - Default: every line on stdin is a hex oid. We pack exactly those objects
//!   (no transitive walk) — this matches `git pack-objects` without `--revs`.
//! - `--revs`: every line is a commit-ish (any rev-parse expression). We
//!   resolve each to an oid, then walk reachability from those starting
//!   points and pack everything we visit. Matches `git pack-objects --revs`.
//!
//! Output:
//! - By default the pack `.pack` and companion `.idx` are written under the
//!   current directory (or whatever `BASE_NAME` resolves to — we treat the
//!   positional as a directory prefix for compatibility, then the actual
//!   files are `pack-<hash>.pack` / `.idx`).
//! - With `--stdout`, no `.idx` is produced and the `.pack` bytes are dumped
//!   on stdout (git's behavior — the caller is expected to run `index-pack`
//!   over the dumped stream if they want a usable pair).
//!
//! On success we print the pack name (the SHA over the entry bytes — the
//! basename of the pair) to stdout, matching `git pack-objects`.

use std::fs::File;
use std::io::{self, BufRead, Read, Write};
use std::path::{Path, PathBuf};

use clap::Args;

use crate::hash::ObjectId;
use crate::pack::{write_pack, PackBuildError};
use crate::reachable::{ReachableError, ReachableSet};
use crate::repo::Repository;
use crate::revparse::{resolve, RevParseError};

#[derive(Debug, Args)]
pub struct PackObjectsArgs {
    /// Emit the pack data to stdout instead of writing files. The companion
    /// `.idx` is NOT produced in this mode (matches `git pack-objects --stdout`).
    #[arg(long = "stdout")]
    pub stdout: bool,

    /// Read commit-ish names from stdin and pack everything reachable from
    /// each. Without this flag, stdin is read as raw oid lines.
    #[arg(long = "revs")]
    pub revs: bool,

    /// Base path for the output. The actual `.pack`/`.idx` files are written
    /// at `<BASE>-<hash>.pack` / `.idx`, where `<BASE>` is the directory
    /// portion plus a "pack" prefix (i.e. `<dir>/pack-<hash>.pack`).
    /// Defaults to the current directory.
    #[arg(value_name = "BASE_NAME", default_value = "pack")]
    pub base_name: String,
}

pub fn run(args: PackObjectsArgs) -> io::Result<i32> {
    let repo = Repository::discover_from_cwd().map_err(io_err)?;

    // Collect the oids we'll pack — either from --revs reachability or from
    // raw stdin oid lines.
    let oids = match collect_oids(&repo, &args) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("rustygit: pack-objects: {e}");
            return Ok(128);
        }
    };

    if oids.is_empty() {
        eprintln!("rustygit: pack-objects: no objects to pack");
        return Ok(128);
    }

    if args.stdout {
        write_to_stdout(&repo, &oids)
    } else {
        write_to_disk(&repo, &oids, &args.base_name)
    }
}

/// Decide where to read inputs from, parse them, and (for `--revs`) walk
/// reachability to expand to the full pack set.
fn collect_oids(
    repo: &Repository,
    args: &PackObjectsArgs,
) -> Result<Vec<ObjectId>, PackObjectsError> {
    let stdin = io::stdin();
    let lines: Vec<String> = stdin
        .lock()
        .lines()
        .collect::<Result<Vec<_>, _>>()
        .map_err(PackObjectsError::Io)?;

    if args.revs {
        let mut starts: Vec<ObjectId> = Vec::with_capacity(lines.len());
        for raw in &lines {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let oid = resolve(repo.refs(), repo.odb(), line)?;
            starts.push(oid);
        }
        let set = ReachableSet::mark_from(repo, &starts)?;
        Ok(set.oids.into_iter().collect())
    } else {
        let mut oids = Vec::with_capacity(lines.len());
        for raw in &lines {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let oid =
                ObjectId::parse_hex(repo.hash_kind(), line).map_err(PackObjectsError::Hash)?;
            oids.push(oid);
        }
        Ok(oids)
    }
}

/// Write a pack to disk under `<base>` (resolved relative to cwd) and print
/// the pack basename hash.
fn write_to_disk(repo: &Repository, oids: &[ObjectId], base: &str) -> io::Result<i32> {
    // The positional acts like a directory prefix. We split into dir + filename
    // prefix — the prefix itself is mostly cosmetic in our world because
    // `write_pack` always produces `pack-<hash>.pack`. We honor the leading
    // dir portion only.
    let base_path = PathBuf::from(base);
    let out_dir = match base_path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => PathBuf::from("."),
    };

    let result = write_pack(oids, repo.odb(), &out_dir, repo.hash_kind()).map_err(io_err)?;
    // git prints the pack-name hash (the hex used in `pack-<hash>.pack`).
    println!("{}", result.pack_name);
    Ok(0)
}

/// Write the pack bytes to stdout. `write_pack` always writes to disk, so we
/// route through a tempdir under the repo's `objects/pack/` and then stream
/// the resulting `.pack` to stdout. The temp `.idx` and `.pack` are deleted
/// afterwards (matching `git pack-objects --stdout`'s "no idx, no leftover").
fn write_to_stdout(repo: &Repository, oids: &[ObjectId]) -> io::Result<i32> {
    let staging = repo
        .gitdir()
        .join("objects")
        .join("pack")
        .join(".tmp-pack-objects-stdout");
    if let Err(e) = std::fs::create_dir_all(&staging) {
        eprintln!("rustygit: pack-objects: create staging dir: {e}");
        return Ok(128);
    }
    let result = match write_pack(oids, repo.odb(), &staging, repo.hash_kind()) {
        Ok(r) => r,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&staging);
            eprintln!("rustygit: pack-objects: {e}");
            return Ok(128);
        }
    };
    let copy_result = stream_to_stdout(&result.pack_path);
    // Best-effort cleanup. We do this whether the copy succeeded or not.
    let _ = std::fs::remove_dir_all(&staging);
    copy_result?;
    // Per git: even in --stdout mode the pack name still goes to stdout —
    // wait, actually `git pack-objects --stdout` writes the pack to stdout
    // and the name doesn't appear (it'd interleave with binary data).
    // We follow git: don't emit the name here.
    Ok(0)
}

fn stream_to_stdout(pack_path: &Path) -> io::Result<()> {
    let mut file = File::open(pack_path)?;
    let stdout = io::stdout();
    let mut out = stdout.lock();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        out.write_all(&buf[..n])?;
    }
    out.flush()
}

#[derive(thiserror::Error, Debug)]
enum PackObjectsError {
    #[error(transparent)]
    Io(std::io::Error),
    #[error(transparent)]
    RevParse(#[from] RevParseError),
    #[error(transparent)]
    Reachable(#[from] ReachableError),
    #[error(transparent)]
    Hash(crate::hash::HashError),
    #[error(transparent)]
    Pack(#[from] PackBuildError),
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
        args: PackObjectsArgs,
    }

    #[test]
    fn parses_defaults() {
        let w = Wrap::try_parse_from(["x"]).unwrap();
        assert!(!w.args.stdout);
        assert!(!w.args.revs);
        assert_eq!(w.args.base_name, "pack");
    }

    #[test]
    fn parses_stdout_and_revs() {
        let w = Wrap::try_parse_from(["x", "--stdout", "--revs"]).unwrap();
        assert!(w.args.stdout);
        assert!(w.args.revs);
        assert_eq!(w.args.base_name, "pack");
    }

    #[test]
    fn parses_positional_base() {
        let w = Wrap::try_parse_from(["x", "/tmp/out/objects/pack/foo"]).unwrap();
        assert_eq!(w.args.base_name, "/tmp/out/objects/pack/foo");
    }

    #[test]
    fn rejects_unknown_flag() {
        // Unknown long flags should fail parsing — keeps the surface area
        // tight (we don't want clap silently consuming `--nonsense` as a
        // positional).
        let r = Wrap::try_parse_from(["x", "--no-such-flag"]);
        assert!(r.is_err());
    }
}
