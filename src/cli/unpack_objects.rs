//! `rustygit unpack-objects` — read a pack from stdin and explode every object
//! into the loose object store.
//!
//! Algorithm:
//!   1. Read stdin into a temp file (PackFile is mmap-backed and needs a path).
//!   2. Open as `PackFile`. Walk entries in disk order.
//!   3. Maintain an in-memory cache of every entry's resolved `(kind, body)`
//!      keyed by pack offset, so OFS_DELTA chains can apply against earlier
//!      entries we've already resolved.
//!   4. For REF_DELTA, look up the base in the existing object database (the
//!      pack might be a thin pack — but for M8 we don't really expect that
//!      from a `git pack-objects` invocation, just from network protocols).
//!   5. Write each resolved object as a loose blob/tree/commit/tag.
//!
//! Out of scope for M8: thin-pack completion (M10), `--strict` cycle checking,
//! `--max-input-size` byte cap.

use std::collections::HashMap;
use std::fs::File;
use std::io;

use clap::Args;

use crate::object::{ObjectKind, RawObject};
use crate::pack::file::{PackEntryKind, PackFile, RawPackEntry};
use crate::pack::{apply_delta, PackError};
use crate::repo::Repository;

#[derive(Debug, Args)]
pub struct UnpackObjectsArgs {
    /// Don't actually write objects; just report what would happen. Reserved.
    #[arg(short = 'n', long = "dry-run")]
    pub dry_run: bool,

    /// Quiet mode.
    #[arg(short = 'q', long = "quiet")]
    pub quiet: bool,
}

pub fn run(args: UnpackObjectsArgs) -> io::Result<i32> {
    let repo = Repository::discover_from_cwd().map_err(io_err)?;

    // Spool stdin to a temp file under .git/objects/pack/. We can't mmap stdin
    // directly. The temp file gets removed on success.
    let tmp_path = repo
        .gitdir()
        .join("objects")
        .join("pack")
        .join(".tmp-unpack-objects.pack");
    if let Some(parent) = tmp_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    {
        let mut tmp = File::create(&tmp_path)?;
        let mut stdin = io::stdin().lock();
        io::copy(&mut stdin, &mut tmp)?;
        tmp.sync_all()?;
    }

    let result = unpack_into(&repo, &tmp_path, args.dry_run, args.quiet);

    // Best-effort cleanup. Even on error, we don't want a stray temp pack.
    let _ = std::fs::remove_file(&tmp_path);

    match result {
        Ok(count) => {
            if !args.quiet {
                println!("Unpacking objects: {count} done.");
            }
            Ok(0)
        }
        Err(e) => {
            eprintln!("rustygit: unpack-objects: {e}");
            Ok(128)
        }
    }
}

fn unpack_into(
    repo: &Repository,
    pack_path: &std::path::Path,
    dry_run: bool,
    quiet: bool,
) -> Result<usize, UnpackObjectsError> {
    let pack = PackFile::open(pack_path, repo.hash_kind())?;
    let mut count = 0usize;
    let mut cache: HashMap<u64, (ObjectKind, Vec<u8>)> = HashMap::new();

    for entry in pack.iter_entries() {
        let entry = entry?;
        let (kind, body) = resolve_entry(repo, &pack, &entry, &mut cache)?;

        if !dry_run {
            let oid = repo
                .odb()
                .write(&RawObject::new(kind, body.clone()))
                .map_err(UnpackObjectsError::Odb)?;
            if !quiet {
                let _ = oid; // suppress unused if we later hide non-quiet output behind progress
            }
        }
        cache.insert(entry.offset, (kind, body));
        count += 1;
    }
    Ok(count)
}

fn resolve_entry(
    repo: &Repository,
    pack: &PackFile,
    entry: &RawPackEntry,
    cache: &mut HashMap<u64, (ObjectKind, Vec<u8>)>,
) -> Result<(ObjectKind, Vec<u8>), UnpackObjectsError> {
    match &entry.kind {
        PackEntryKind::Direct(kind) => Ok((*kind, entry.data.clone())),
        PackEntryKind::OfsDelta { base_offset } => {
            // Cache hit?
            if let Some(base) = cache.get(base_offset) {
                let resolved =
                    apply_delta(&base.1, &entry.data).map_err(UnpackObjectsError::Delta)?;
                return Ok((base.0, resolved));
            }
            // Cache miss → re-read the base entry. For pathological pack
            // orders this could be expensive, but `git pack-objects` always
            // emits bases-before-deltas in disk order, so the cache hit is
            // the common path.
            let base_entry = pack.read_entry_at(*base_offset)?;
            let base = resolve_entry(repo, pack, &base_entry, cache)?;
            let resolved = apply_delta(&base.1, &entry.data).map_err(UnpackObjectsError::Delta)?;
            // Memo the base so siblings of the same delta-base hit the cache.
            cache.insert(*base_offset, base.clone());
            Ok((base.0, resolved))
        }
        PackEntryKind::RefDelta { base_oid } => {
            // Try existing odb first (this lets us complete a thin pack whose
            // base lives in pre-existing loose objects).
            let base_obj = repo.odb().read(base_oid).map_err(UnpackObjectsError::Odb)?;
            let resolved =
                apply_delta(&base_obj.data, &entry.data).map_err(UnpackObjectsError::Delta)?;
            Ok((base_obj.kind, resolved))
        }
    }
}

#[derive(thiserror::Error, Debug)]
pub enum UnpackObjectsError {
    #[error(transparent)]
    Pack(#[from] PackError),
    #[error(transparent)]
    Delta(crate::pack::DeltaError),
    #[error(transparent)]
    Odb(crate::odb::OdbError),
}

fn io_err<E: std::fmt::Display>(e: E) -> io::Error {
    io::Error::other(format!("{e}"))
}
