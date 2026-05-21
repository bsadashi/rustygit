//! `rustygit verify-pack` — walk a pack and print one line per object.
//!
//! Output format matches `git verify-pack -v <pack>`:
//!
//! ```text
//! <oid> <type>    <size> <size-in-pack> <pack-offset>[ <depth> <base-oid>]
//! ...
//! non delta: N objects
//! chain length = 1: M objects
//! ...
//! <pack-path>: ok
//! ```
//!
//! The `<type>` column is left-justified to width 6 so the rest of the columns
//! line up between delta and non-delta entries.
//!
//! M7 scope: a single pack at a time. `git verify-pack` accepts multiple
//! packs; we add that once the porcelain integration test suite proves the
//! single-pack output already matches.

use std::collections::BTreeMap;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

use clap::Args;

use crate::hash::{HashKind, ObjectId};
use crate::object::ObjectKind;
use crate::pack::{IdxFile, PackEntryKind, PackFile};

#[derive(Debug, Args)]
pub struct VerifyPackArgs {
    /// Verbose: print one line per object plus chain-length histogram.
    #[arg(short = 'v', long = "verbose")]
    pub verbose: bool,

    /// Path to the pack (with or without the `.pack`/`.idx` extension).
    #[arg(value_name = "PACK")]
    pub pack: PathBuf,
}

pub fn run(args: VerifyPackArgs) -> io::Result<i32> {
    // Default to sha1 — we don't have repo context here. If the pack/idx use
    // sha256, the caller can extend this later via `--object-format`.
    let hash_kind = HashKind::Sha1;
    let (pack_path, idx_path) = pair_paths(&args.pack);

    let pack = PackFile::open(&pack_path, hash_kind).map_err(io_err)?;
    let idx = IdxFile::open(&idx_path, hash_kind).map_err(io_err)?;

    if pack.object_count() != idx.object_count() {
        eprintln!(
            "rustygit verify-pack: pack/idx disagree on object count ({} vs {})",
            pack.object_count(),
            idx.object_count()
        );
        return Ok(1);
    }

    let stdout = io::stdout();
    let mut out = stdout.lock();

    // Build up the full per-object table first. We need:
    //   - real type (after delta resolution)
    //   - real size (after delta resolution)
    //   - size in pack (next-offset minus this-offset)
    //   - delta depth + base oid
    //
    // Index → entries indexed by pack offset, so we can compute size-in-pack
    // from sorted offsets and look up oids by offset.
    let mut by_offset: BTreeMap<u64, ObjectId> = BTreeMap::new();
    for (oid, off) in idx.iter() {
        by_offset.insert(off, oid);
    }
    let sorted_offsets: Vec<u64> = by_offset.keys().copied().collect();

    // The "end of objects" marker is the pack file size minus the trailer
    // (which is `hash_kind.raw_len()` bytes). This is what git uses for the
    // final entry's size-in-pack.
    let pack_total_len = std::fs::metadata(pack.path()).map_err(io_err)?.len();
    let trailer_len = hash_kind.raw_len() as u64;
    let end_of_objects = pack_total_len.saturating_sub(trailer_len);

    let mut entries: Vec<EntryInfo> = Vec::with_capacity(sorted_offsets.len());

    // First pass: read each raw entry. We need the raw kind to know if it's
    // a delta. For deltas, we track its delta-base offset (resolving REF_DELTA
    // to offset via the idx); for direct entries, depth = 0.
    let mut raw_kind_at: BTreeMap<u64, RawKind> = BTreeMap::new();
    for &off in &sorted_offsets {
        let entry = pack.read_entry_at(off).map_err(io_err)?;
        let raw_kind = match entry.kind {
            PackEntryKind::Direct(k) => RawKind::Direct(k),
            PackEntryKind::OfsDelta { base_offset } => RawKind::OfsDelta { base_offset },
            PackEntryKind::RefDelta { base_oid } => RawKind::RefDelta { base_oid },
        };
        raw_kind_at.insert(off, raw_kind);
    }

    // Second pass: resolve final type/size by walking the chain, and compute
    // depth + base-oid for delta entries. We use a small memoization map so a
    // long shared base isn't re-walked once per leaf.
    let mut resolved: BTreeMap<u64, (ObjectKind, u64)> = BTreeMap::new();
    let mut depth_at: BTreeMap<u64, (u32, ObjectId)> = BTreeMap::new();

    for &off in &sorted_offsets {
        // Resolve final kind+size by walking up to a non-delta base.
        let (kind, size) =
            resolve_kind_size(&pack, &raw_kind_at, &idx, off, &mut resolved).map_err(io_err)?;
        // Compute delta depth and root base oid.
        let mut depth = 0u32;
        let mut base_offset_for_delta: Option<u64> = None;
        let mut cur = off;
        loop {
            let kind = raw_kind_at
                .get(&cur)
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing offset"))?;
            match kind {
                RawKind::Direct(_) => break,
                RawKind::OfsDelta { base_offset } => {
                    depth += 1;
                    if base_offset_for_delta.is_none() {
                        base_offset_for_delta = Some(*base_offset);
                    }
                    cur = *base_offset;
                }
                RawKind::RefDelta { base_oid } => {
                    depth += 1;
                    let Some(boff) = idx.lookup(base_oid) else {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!("ref delta base {base_oid} not in this pack"),
                        ));
                    };
                    if base_offset_for_delta.is_none() {
                        base_offset_for_delta = Some(boff);
                    }
                    cur = boff;
                }
            }
        }
        // The "base-oid" git prints is the *immediate* base of the delta —
        // i.e. for a chain a->b->c, c's row prints b's oid (and depth 2).
        if depth > 0 {
            let imm = base_offset_for_delta.expect("delta has immediate base");
            let imm_oid = by_offset.get(&imm).copied().ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "base offset not in idx")
            })?;
            depth_at.insert(off, (depth, imm_oid));
        }
        entries.push(EntryInfo {
            offset: off,
            oid: by_offset[&off],
            kind,
            size,
            depth,
        });
    }

    if args.verbose {
        for e in &entries {
            // size-in-pack: next entry's offset minus this one's, or
            // end-of-objects minus this for the last.
            let next_off = next_offset_after(&sorted_offsets, e.offset, end_of_objects);
            let size_in_pack = next_off.saturating_sub(e.offset);
            // Match git's column widths: `%s %-6s %d %d %d`. ObjectKind's
            // Display uses `f.write_str` which ignores the width specifier, so
            // format into a String first to get proper padding.
            let kind_str = e.kind.to_string();
            if e.depth > 0 {
                let (depth, base_oid) =
                    depth_at.get(&e.offset).cloned().unwrap_or((e.depth, e.oid));
                writeln!(
                    out,
                    "{} {:<6} {} {} {} {} {}",
                    e.oid, kind_str, e.size, size_in_pack, e.offset, depth, base_oid
                )?;
            } else {
                writeln!(
                    out,
                    "{} {:<6} {} {} {}",
                    e.oid, kind_str, e.size, size_in_pack, e.offset
                )?;
            }
        }

        // Stats: count base objects + chain-length histogram.
        let mut base_count: u64 = 0;
        let mut chain_hist: BTreeMap<u32, u64> = BTreeMap::new();
        for e in &entries {
            if e.depth == 0 {
                base_count += 1;
            } else {
                *chain_hist.entry(e.depth).or_insert(0) += 1;
            }
        }

        if base_count > 0 {
            let label = if base_count == 1 { "object" } else { "objects" };
            writeln!(out, "non delta: {} {}", base_count, label)?;
        }
        for (depth, count) in &chain_hist {
            let label = if *count == 1 { "object" } else { "objects" };
            writeln!(out, "chain length = {}: {} {}", depth, count, label)?;
        }
    }

    // git verify-pack closes with "<pack>: ok" on success.
    writeln!(out, "{}: ok", pack_path.display())?;
    Ok(0)
}

#[derive(Debug, Clone)]
enum RawKind {
    Direct(ObjectKind),
    OfsDelta { base_offset: u64 },
    RefDelta { base_oid: ObjectId },
}

#[derive(Debug)]
struct EntryInfo {
    offset: u64,
    oid: ObjectId,
    kind: ObjectKind,
    size: u64,
    depth: u32,
}

/// Walk the delta chain from `offset` to find the *kind* of the eventual base
/// (deltas inherit their type from the base). The `size` we report matches
/// what `git verify-pack -v` prints — i.e. the entry's declared size (object
/// size for direct entries, delta-payload size for deltas), NOT the resolved
/// post-delta size. Memoizes in `cache` so each unique offset is resolved at
/// most once.
fn resolve_kind_size(
    pack: &PackFile,
    raw: &BTreeMap<u64, RawKind>,
    idx: &IdxFile,
    offset: u64,
    cache: &mut BTreeMap<u64, (ObjectKind, u64)>,
) -> io::Result<(ObjectKind, u64)> {
    if let Some(hit) = cache.get(&offset) {
        return Ok(*hit);
    }
    let kind = raw
        .get(&offset)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing offset"))?;

    let entry = pack.read_entry_at(offset).map_err(io_err)?;
    let value = match kind {
        RawKind::Direct(k) => (*k, entry.declared_size),
        RawKind::OfsDelta { base_offset } => {
            let (base_kind, _) = resolve_kind_size(pack, raw, idx, *base_offset, cache)?;
            (base_kind, entry.declared_size)
        }
        RawKind::RefDelta { base_oid } => {
            let Some(boff) = idx.lookup(base_oid) else {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("ref-delta base {base_oid} not in pack"),
                ));
            };
            let (base_kind, _) = resolve_kind_size(pack, raw, idx, boff, cache)?;
            (base_kind, entry.declared_size)
        }
    };

    cache.insert(offset, value);
    Ok(value)
}

/// Find the smallest offset strictly greater than `offset` in `sorted`.
/// Falls back to `end_of_objects` for the last entry.
fn next_offset_after(sorted: &[u64], offset: u64, end_of_objects: u64) -> u64 {
    match sorted.binary_search(&offset) {
        Ok(i) => sorted.get(i + 1).copied().unwrap_or(end_of_objects),
        Err(_) => end_of_objects,
    }
}

fn pair_paths(input: &Path) -> (PathBuf, PathBuf) {
    let s = input.to_string_lossy();
    let stripped: &str = if let Some(rest) = s.strip_suffix(".pack") {
        rest
    } else if let Some(rest) = s.strip_suffix(".idx") {
        rest
    } else {
        s.as_ref()
    };
    let pack = PathBuf::from(format!("{stripped}.pack"));
    let idx = PathBuf::from(format!("{stripped}.idx"));
    (pack, idx)
}

fn io_err<E: std::fmt::Display>(e: E) -> io::Error {
    io::Error::other(format!("{e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pair_paths_handles_pack() {
        let (p, i) = pair_paths(Path::new("/a/b/foo.pack"));
        assert_eq!(p.to_str().unwrap(), "/a/b/foo.pack");
        assert_eq!(i.to_str().unwrap(), "/a/b/foo.idx");
    }

    #[test]
    fn pair_paths_handles_idx() {
        let (p, i) = pair_paths(Path::new("/a/b/foo.idx"));
        assert_eq!(p.to_str().unwrap(), "/a/b/foo.pack");
        assert_eq!(i.to_str().unwrap(), "/a/b/foo.idx");
    }

    #[test]
    fn pair_paths_handles_basename() {
        let (p, i) = pair_paths(Path::new("/a/b/foo"));
        assert_eq!(p.to_str().unwrap(), "/a/b/foo.pack");
        assert_eq!(i.to_str().unwrap(), "/a/b/foo.idx");
    }

    #[test]
    fn next_offset_finds_next() {
        let s = vec![10u64, 20, 30, 40];
        assert_eq!(next_offset_after(&s, 10, 100), 20);
        assert_eq!(next_offset_after(&s, 30, 100), 40);
        assert_eq!(next_offset_after(&s, 40, 100), 100);
    }
}
