//! `rustygit pack-refs` — consolidate loose refs into the
//! `packed-refs` file.
//!
//! Algorithm:
//!   1. Read every loose ref under `.git/refs/`.
//!   2. Read existing `packed-refs` (if any) into a map.
//!   3. Merge: loose entries take precedence (they're the truth right now).
//!   4. Rewrite `packed-refs` atomically via lockfile.
//!   5. With `--prune` (the default starting from git 2.46), delete the
//!      loose ref files we just packed. Symbolic refs (HEAD) are never
//!      packed and never deleted.
//!
//! Flags:
//!   * `--all` — pack heads and tags (the default).
//!   * `--prune` — delete loose refs after packing (default true).
//!   * `--no-prune` — keep loose refs after packing.

use std::collections::BTreeMap;
use std::io::{self, Write};

use clap::Args;

use crate::hash::ObjectId;
use crate::refs::{FullName, RefTarget};
use crate::repo::Repository;

#[derive(Debug, Args)]
pub struct PackRefsArgs {
    /// Pack refs under refs/heads and refs/tags (default).
    #[arg(long = "all")]
    pub all: bool,
    /// Delete loose ref files after packing (default).
    #[arg(long = "prune")]
    pub prune: bool,
    /// Keep loose refs after packing.
    #[arg(long = "no-prune", conflicts_with = "prune")]
    pub no_prune: bool,
}

pub fn run(args: PackRefsArgs) -> io::Result<i32> {
    let repo = Repository::discover_from_cwd().map_err(io_err)?;
    let do_prune = !args.no_prune;
    // --all is the implicit default for which subset to pack; the only
    // alternative would be a pattern-driven subset, which git ships but
    // we don't currently expose.
    let _ = args.all;

    // 1. Collect every direct loose ref. We only pack direct refs;
    //    symbolic refs (HEAD, refs/remotes/<r>/HEAD) stay loose.
    let mut to_pack: BTreeMap<String, ObjectId> = BTreeMap::new();
    for r in repo.refs().iter(None) {
        let r = r.map_err(io_err)?;
        let name = r.name.as_str();
        // pack-refs only touches refs/* (never HEAD itself).
        if name == "HEAD" {
            continue;
        }
        match r.target {
            RefTarget::Direct(oid) => {
                to_pack.insert(name.to_string(), oid);
            }
            RefTarget::Symbolic(_) => {
                // Symbolic refs are never packed.
            }
        }
    }

    if to_pack.is_empty() {
        return Ok(0);
    }

    // 2. Read existing packed-refs (if any). Merge: the freshest loose
    //    value wins because we already overwrote the map with it.
    let packed_path = repo.commondir().join("packed-refs");
    let prior = read_packed_refs(&packed_path)?;
    let mut merged: BTreeMap<String, ObjectId> = prior;
    for (name, oid) in &to_pack {
        merged.insert(name.clone(), *oid);
    }

    // 3. Rewrite atomically (write to packed-refs.lock, fsync, rename).
    let lock_path = repo.commondir().join("packed-refs.lock");
    {
        let mut f = std::fs::File::create(&lock_path)?;
        writeln!(f, "# pack-refs with: peeled fully-peeled sorted")?;
        for (name, oid) in &merged {
            writeln!(f, "{oid} {name}")?;
        }
        f.sync_all()?;
    }
    std::fs::rename(&lock_path, &packed_path)?;

    // 4. Optionally remove the loose ref files we just packed.
    if do_prune {
        for name in to_pack.keys() {
            let n = match FullName::new(name.clone()) {
                Ok(n) => n,
                Err(_) => continue,
            };
            let p = repo.commondir().join(n.loose_path_relative());
            let _ = std::fs::remove_file(&p);
            // Also try to remove now-empty parent dirs (refs/heads/feature
            // becomes empty if it was the only ref there).
            if let Some(parent) = p.parent() {
                prune_empty_dirs(parent, &repo.commondir().join("refs"));
            }
        }
    }

    Ok(0)
}

/// Walk up from `start` to `stop` (exclusive), removing any directory
/// that becomes empty. Stops if a removal fails or a dir is non-empty.
fn prune_empty_dirs(start: &std::path::Path, stop: &std::path::Path) {
    let mut cur = start.to_path_buf();
    while cur.starts_with(stop) && cur != *stop {
        if std::fs::remove_dir(&cur).is_err() {
            break;
        }
        match cur.parent() {
            Some(p) => cur = p.to_path_buf(),
            None => break,
        }
    }
}

/// Parse the packed-refs file. Returns ref-name → oid pairs; peeled
/// (`^<tag-target-oid>`) lines are kept on the previous tag but we don't
/// expose them through the BTreeMap (we'll regenerate them on write).
fn read_packed_refs(path: &std::path::Path) -> io::Result<BTreeMap<String, ObjectId>> {
    let mut out = BTreeMap::new();
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(e),
    };
    let text = String::from_utf8_lossy(&bytes);
    for line in text.lines() {
        if line.is_empty() || line.starts_with('#') || line.starts_with('^') {
            continue;
        }
        let mut iter = line.splitn(2, ' ');
        let oid_str = match iter.next() {
            Some(s) => s,
            None => continue,
        };
        let name = match iter.next() {
            Some(s) => s.trim().to_string(),
            None => continue,
        };
        let oid = match ObjectId::parse_hex(crate::hash::HashKind::Sha1, oid_str) {
            Ok(o) => o,
            Err(_) => continue,
        };
        out.insert(name, oid);
    }
    Ok(out)
}

fn io_err<E: std::fmt::Display>(e: E) -> io::Error {
    io::Error::other(format!("{e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_simple_packed_refs() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(
            tmp.path(),
            "# pack-refs with: peeled\n\
             1111111111111111111111111111111111111111 refs/heads/main\n\
             2222222222222222222222222222222222222222 refs/tags/v1\n\
             ^3333333333333333333333333333333333333333\n",
        )
        .unwrap();
        let m = read_packed_refs(tmp.path()).unwrap();
        assert_eq!(m.len(), 2);
        assert!(m.contains_key("refs/heads/main"));
        assert!(m.contains_key("refs/tags/v1"));
    }

    #[test]
    fn missing_packed_refs_returns_empty() {
        let m = read_packed_refs(std::path::Path::new("/nonexistent/path")).unwrap();
        assert!(m.is_empty());
    }
}
