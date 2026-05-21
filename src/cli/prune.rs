//! `rustygit prune` — remove unreachable loose objects.
//! `rustygit prune-packed` — remove loose objects already in a pack.

use std::collections::HashSet;
use std::io::{self, Write};

use clap::Args;

use crate::hash::ObjectId;
use crate::repo::Repository;

#[derive(Debug, Args)]
pub struct PruneArgs {
    /// Print what would be removed; don't actually remove.
    #[arg(short = 'n', long = "dry-run")]
    pub dry_run: bool,
    /// Print every loose object that's being kept.
    #[arg(short = 'v', long = "verbose")]
    pub verbose: bool,
}

pub fn run(args: PruneArgs) -> io::Result<i32> {
    let repo = Repository::discover_from_cwd().map_err(|e| io::Error::other(format!("{e}")))?;
    let reachable = compute_reachable(&repo)?;
    let mut removed = 0u64;
    let mut kept = 0u64;

    let objects_dir = repo.gitdir().join("objects");
    let dirs = match std::fs::read_dir(&objects_dir) {
        Ok(d) => d,
        Err(_) => return Ok(0),
    };
    let stdout = io::stdout();
    let mut out = stdout.lock();

    for entry in dirs.flatten() {
        let name = entry.file_name();
        let s = name.to_string_lossy();
        if s.len() != 2 || !s.chars().all(|c| c.is_ascii_hexdigit()) {
            continue;
        }
        if let Ok(inner) = std::fs::read_dir(entry.path()) {
            for f in inner.flatten() {
                let fname = f.file_name();
                let fname_str = fname.to_string_lossy();
                if fname_str.len() != 38 || !fname_str.chars().all(|c| c.is_ascii_hexdigit()) {
                    continue;
                }
                let hex = format!("{s}{fname_str}");
                let oid = match ObjectId::parse_hex(crate::hash::HashKind::Sha1, &hex) {
                    Ok(o) => o,
                    Err(_) => continue,
                };
                if reachable.contains(&oid) {
                    kept += 1;
                    if args.verbose {
                        let _ = writeln!(out, "keep {oid}");
                    }
                    continue;
                }
                if args.dry_run {
                    let _ = writeln!(out, "would remove {oid}");
                } else {
                    let _ = std::fs::remove_file(f.path());
                }
                removed += 1;
            }
        }
    }
    let _ = (kept, removed); // unused-warning suppressor when verbose is off
    Ok(0)
}

// ---------------------------------------------------------------------------
// prune-packed
// ---------------------------------------------------------------------------

#[derive(Debug, Args)]
pub struct PrunePackedArgs {
    #[arg(short = 'n', long = "dry-run")]
    pub dry_run: bool,
    #[arg(short = 'q', long = "quiet")]
    pub quiet: bool,
}

pub fn run_prune_packed(args: PrunePackedArgs) -> io::Result<i32> {
    let repo = Repository::discover_from_cwd().map_err(|e| io::Error::other(format!("{e}")))?;

    // Collect every oid present in any pack.
    let mut in_pack: HashSet<ObjectId> = HashSet::new();
    let pack_dir = repo.gitdir().join("objects").join("pack");
    if let Ok(entries) = std::fs::read_dir(&pack_dir) {
        for e in entries.flatten() {
            let p = e.path();
            if p.extension().and_then(|s| s.to_str()) == Some("idx") {
                if let Ok(bytes) = std::fs::read(&p) {
                    collect_oids_from_idx(&bytes, &mut in_pack);
                }
            }
        }
    }

    let objects_dir = repo.gitdir().join("objects");
    let dirs = match std::fs::read_dir(&objects_dir) {
        Ok(d) => d,
        Err(_) => return Ok(0),
    };
    let stdout = io::stdout();
    let mut out = stdout.lock();

    for entry in dirs.flatten() {
        let name = entry.file_name();
        let s = name.to_string_lossy();
        if s.len() != 2 || !s.chars().all(|c| c.is_ascii_hexdigit()) {
            continue;
        }
        if let Ok(inner) = std::fs::read_dir(entry.path()) {
            for f in inner.flatten() {
                let fname = f.file_name();
                let fname_str = fname.to_string_lossy();
                if fname_str.len() != 38 || !fname_str.chars().all(|c| c.is_ascii_hexdigit()) {
                    continue;
                }
                let hex = format!("{s}{fname_str}");
                let oid = match ObjectId::parse_hex(crate::hash::HashKind::Sha1, &hex) {
                    Ok(o) => o,
                    Err(_) => continue,
                };
                if in_pack.contains(&oid) {
                    if args.dry_run {
                        let _ = writeln!(out, "would remove {oid}");
                    } else {
                        let _ = std::fs::remove_file(f.path());
                        if !args.quiet {
                            let _ = writeln!(out, "removed {oid}");
                        }
                    }
                }
            }
        }
    }
    Ok(0)
}

fn collect_oids_from_idx(bytes: &[u8], out: &mut HashSet<ObjectId>) {
    // v2 idx layout: magic(4) + version(4) + 256 u32 fanout + oid table.
    if bytes.len() < 8 + 256 * 4 || &bytes[..4] != b"\xfftOc" {
        return;
    }
    let total = u32::from_be_bytes([
        bytes[8 + 255 * 4],
        bytes[8 + 255 * 4 + 1],
        bytes[8 + 255 * 4 + 2],
        bytes[8 + 255 * 4 + 3],
    ]) as usize;
    let oids_off = 8 + 256 * 4;
    for i in 0..total {
        let off = oids_off + i * 20;
        if off + 20 > bytes.len() {
            break;
        }
        if let Ok(oid) = ObjectId::from_bytes(crate::hash::HashKind::Sha1, &bytes[off..off + 20]) {
            out.insert(oid);
        }
    }
}

/// Walk every ref tip + HEAD, transitively visit commits, trees, blobs,
/// and tags, and return every oid we touched.
fn compute_reachable(repo: &Repository) -> io::Result<HashSet<ObjectId>> {
    let mut out: HashSet<ObjectId> = HashSet::new();
    let mut roots: Vec<ObjectId> = Vec::new();
    for r in repo.refs().iter(None) {
        let r = r.map_err(|e| io::Error::other(format!("{e}")))?;
        if let crate::refs::RefTarget::Direct(o) = r.target {
            roots.push(o);
        }
    }
    let mut stack = roots;
    while let Some(oid) = stack.pop() {
        if !out.insert(oid) {
            continue;
        }
        let raw = match repo.odb().read(&oid) {
            Ok(r) => r,
            Err(_) => continue,
        };
        match raw.kind {
            crate::object::ObjectKind::Commit => {
                if let Ok(c) = crate::commit::Commit::parse(&raw.data, repo.hash_kind()) {
                    stack.push(c.tree);
                    for p in &c.parents {
                        stack.push(*p);
                    }
                }
            }
            crate::object::ObjectKind::Tree => {
                if let Ok(t) = crate::tree::Tree::parse(&raw.data, repo.hash_kind()) {
                    for e in &t.entries {
                        stack.push(e.oid);
                    }
                }
            }
            crate::object::ObjectKind::Tag => {
                if let Ok(tg) = crate::tag::Tag::parse(&raw.data, repo.hash_kind()) {
                    stack.push(tg.object);
                }
            }
            crate::object::ObjectKind::Blob => {}
        }
    }
    Ok(out)
}
