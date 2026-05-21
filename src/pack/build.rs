//! Pack writer (M9).
//!
//! Emits non-delta packs only: every object is stored directly as `Direct(kind)`
//! with its zlib-compressed body. Output is `<out_dir>/pack-<hash>.{pack,idx}`,
//! readable by `git verify-pack -v`, `git index-pack --verify`, and the
//! [`PackFile`]/[`IdxFile`] readers in this crate.
//!
//! Deltification is a polish milestone (post-M16). Until then our packs are
//! larger than git's optimized output but byte-for-byte valid: the on-disk
//! format is identical, only the entry kinds differ.
//!
//! The format mirrors the readers exactly. See `src/pack/file.rs` (entry
//! decoding) and `src/pack/idx.rs` (idx v2 layout) for the structures we're
//! emitting.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use flate2::write::ZlibEncoder;
use flate2::Compression;

use crate::hash::{new_hasher, HashKind, Hasher, ObjectId};
use crate::object::ObjectKind;
use crate::odb::ObjectDb;

use super::PackError;

const PACK_SIGNATURE: &[u8; 4] = b"PACK";
const PACK_VERSION: u32 = 2;
const IDX_V2_MAGIC: [u8; 4] = [0xff, 0x74, 0x4f, 0x63];
const IDX_V2_VERSION: u32 = 2;

#[derive(thiserror::Error, Debug)]
pub enum PackBuildError {
    #[error("io on {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error(transparent)]
    Odb(#[from] crate::odb::OdbError),
    #[error(transparent)]
    Hash(#[from] crate::hash::HashError),
    #[error(transparent)]
    Pack(#[from] PackError),
    #[error("empty pack: nothing to write")]
    Empty,
}

#[derive(Debug, Clone)]
pub struct PackBuildResult {
    /// Hex of the SHA over the pack file's entry-bytes (NOT the file hash).
    /// This is the value used as the pack file's basename (`pack-<this>.pack`).
    pub pack_name: String,
    /// Path of the written .pack file.
    pub pack_path: PathBuf,
    /// Path of the written .idx file.
    pub idx_path: PathBuf,
    /// Total objects written.
    pub object_count: u32,
}

/// Write a pack containing the given object ids, sourced from `odb`.
///
/// See module-level docs for the format. Each oid is read from `odb`, encoded
/// as a `Direct` (non-delta) entry, then indexed in the companion `.idx`.
pub fn write_pack(
    oids: &[ObjectId],
    odb: &ObjectDb,
    out_dir: &Path,
    hash_kind: HashKind,
) -> Result<PackBuildResult, PackBuildError> {
    if oids.is_empty() {
        return Err(PackBuildError::Empty);
    }

    // Pre-fetch every object from the odb. Doing this up front keeps the
    // pack-writing pass straightforward (no failure mid-stream after the
    // header is written) and makes the streaming variant share the same core.
    let mut objects: Vec<(ObjectId, ObjectKind, Vec<u8>)> = Vec::with_capacity(oids.len());
    for oid in oids {
        let raw = odb.read(oid)?;
        objects.push((*oid, raw.kind, raw.data));
    }
    write_pack_from_objects(&objects, out_dir, hash_kind)
}

/// Streaming variant for callers that already have `(oid, kind, bytes)` triples.
pub fn write_pack_from_objects(
    objects: &[(ObjectId, ObjectKind, Vec<u8>)],
    out_dir: &Path,
    hash_kind: HashKind,
) -> Result<PackBuildResult, PackBuildError> {
    if objects.is_empty() {
        return Err(PackBuildError::Empty);
    }

    std::fs::create_dir_all(out_dir).map_err(|e| PackBuildError::Io {
        path: out_dir.to_path_buf(),
        source: e,
    })?;

    let object_count = objects.len() as u32;
    crate::trace!("pack", "writing pack of {} objects", object_count);

    // ---- Phase 1: write the pack to a temp file. -------------------------

    let tmp_pack = out_dir.join(".tmp-pack.pack");
    let pack_offsets: Vec<u64>;
    let crc32s: Vec<u32>;
    let pack_hash: ObjectId;
    {
        let file = File::create(&tmp_pack).map_err(|e| PackBuildError::Io {
            path: tmp_pack.clone(),
            source: e,
        })?;
        let mut w = HashingWriter::new(BufWriter::new(file), new_hasher(hash_kind));

        // Header: PACK + version + count.
        w.write_all(PACK_SIGNATURE)
            .map_err(|e| io_err(&tmp_pack, e))?;
        w.write_all(&PACK_VERSION.to_be_bytes())
            .map_err(|e| io_err(&tmp_pack, e))?;
        w.write_all(&object_count.to_be_bytes())
            .map_err(|e| io_err(&tmp_pack, e))?;

        // Entries.
        let mut offsets = Vec::with_capacity(objects.len());
        let mut crcs = Vec::with_capacity(objects.len());
        for (_oid, kind, body) in objects {
            let offset = w.bytes_written();
            // Offsets up to 2^32 fit in a u32 (and most fit in 2^31 — the idx
            // distinguishes those). Anything beyond stays a u64 in `offsets`.
            offsets.push(offset);

            let entry_bytes = encode_entry(*kind, body)?;
            w.write_all(&entry_bytes)
                .map_err(|e| io_err(&tmp_pack, e))?;

            // CRC32 over the raw on-pack bytes of this entry — that's what
            // the idx records for `git index-pack --verify` to cross-check.
            let mut crc = flate2::Crc::new();
            crc.update(&entry_bytes);
            crcs.push(crc.sum());
        }

        // Trailer: SHA over everything we've written so far.
        let inner_writer = w.finish_into_inner();
        let computed_hash = inner_writer.hasher.finalize();
        let mut bw = inner_writer.inner;
        bw.write_all(computed_hash.as_bytes())
            .map_err(|e| io_err(&tmp_pack, e))?;
        bw.flush().map_err(|e| io_err(&tmp_pack, e))?;
        let file = bw
            .into_inner()
            .map_err(|e| io_err(&tmp_pack, e.into_error()))?;
        file.sync_all().map_err(|e| io_err(&tmp_pack, e))?;

        pack_offsets = offsets;
        crc32s = crcs;
        pack_hash = computed_hash;
    }

    // Rename pack to its final name.
    let pack_name = pack_hash.to_string();
    let final_pack = out_dir.join(format!("pack-{pack_name}.pack"));
    std::fs::rename(&tmp_pack, &final_pack).map_err(|e| PackBuildError::Io {
        path: final_pack.clone(),
        source: e,
    })?;

    // ---- Phase 2: write the idx to a temp file. --------------------------

    // Build the sort order: ascending by raw oid bytes. We carry along the
    // original index so we can pull each oid's offset and crc.
    let mut order: Vec<usize> = (0..objects.len()).collect();
    order.sort_by(|&a, &b| objects[a].0.as_bytes().cmp(objects[b].0.as_bytes()));

    let tmp_idx = out_dir.join(".tmp-pack.idx");
    {
        let file = File::create(&tmp_idx).map_err(|e| PackBuildError::Io {
            path: tmp_idx.clone(),
            source: e,
        })?;
        let mut w = HashingWriter::new(BufWriter::new(file), new_hasher(hash_kind));

        // Magic + version.
        w.write_all(&IDX_V2_MAGIC)
            .map_err(|e| io_err(&tmp_idx, e))?;
        w.write_all(&IDX_V2_VERSION.to_be_bytes())
            .map_err(|e| io_err(&tmp_idx, e))?;

        // Fanout: fanout[i] = count of oids whose first byte is <= i.
        let mut fanout = [0u32; 256];
        for &idx in &order {
            let first = objects[idx].0.as_bytes()[0] as usize;
            // Bump every bucket >= first by one.
            for bucket in fanout.iter_mut().skip(first) {
                *bucket += 1;
            }
        }
        for v in &fanout {
            w.write_all(&v.to_be_bytes())
                .map_err(|e| io_err(&tmp_idx, e))?;
        }

        // OIDs in sorted order.
        for &idx in &order {
            w.write_all(objects[idx].0.as_bytes())
                .map_err(|e| io_err(&tmp_idx, e))?;
        }

        // CRC32s, one per sorted oid.
        for &idx in &order {
            w.write_all(&crc32s[idx].to_be_bytes())
                .map_err(|e| io_err(&tmp_idx, e))?;
        }

        // Offsets — small (u32 BE) with high bit set for "look in the 64-bit
        // table", followed by the 64-bit table itself.
        let mut large_offsets: Vec<u64> = Vec::new();
        for &idx in &order {
            let off = pack_offsets[idx];
            if off < 0x8000_0000 {
                // Fits in 31 bits; high bit clear -> read directly.
                w.write_all(&(off as u32).to_be_bytes())
                    .map_err(|e| io_err(&tmp_idx, e))?;
            } else {
                // High bit set, low 31 bits = index into the 64-bit table.
                let large_idx = large_offsets.len() as u32;
                let encoded = 0x8000_0000 | large_idx;
                w.write_all(&encoded.to_be_bytes())
                    .map_err(|e| io_err(&tmp_idx, e))?;
                large_offsets.push(off);
            }
        }
        for off in &large_offsets {
            w.write_all(&off.to_be_bytes())
                .map_err(|e| io_err(&tmp_idx, e))?;
        }

        // Trailer part 1: copy of the pack hash. Feed it into the idx hash too.
        w.write_all(pack_hash.as_bytes())
            .map_err(|e| io_err(&tmp_idx, e))?;

        // Trailer part 2: the idx hash itself. Computed over everything above.
        let inner_writer = w.finish_into_inner();
        let idx_hash = inner_writer.hasher.finalize();
        let mut bw = inner_writer.inner;
        bw.write_all(idx_hash.as_bytes())
            .map_err(|e| io_err(&tmp_idx, e))?;
        bw.flush().map_err(|e| io_err(&tmp_idx, e))?;
        let file = bw
            .into_inner()
            .map_err(|e| io_err(&tmp_idx, e.into_error()))?;
        file.sync_all().map_err(|e| io_err(&tmp_idx, e))?;
    }

    let final_idx = out_dir.join(format!("pack-{pack_name}.idx"));
    std::fs::rename(&tmp_idx, &final_idx).map_err(|e| PackBuildError::Io {
        path: final_idx.clone(),
        source: e,
    })?;

    Ok(PackBuildResult {
        pack_name,
        pack_path: final_pack,
        idx_path: final_idx,
        object_count,
    })
}

// ---- low-level helpers ----------------------------------------------------

fn io_err(path: &Path, source: std::io::Error) -> PackBuildError {
    PackBuildError::Io {
        path: path.to_path_buf(),
        source,
    }
}

/// Encode one pack entry: variable-length type+size header followed by the
/// zlib-compressed body. The encoding here MUST round-trip through
/// `parse_type_size` in `src/pack/file.rs`.
fn encode_entry(kind: ObjectKind, body: &[u8]) -> Result<Vec<u8>, PackBuildError> {
    let type_id: u8 = match kind {
        ObjectKind::Commit => 1,
        ObjectKind::Tree => 2,
        ObjectKind::Blob => 3,
        ObjectKind::Tag => 4,
    };
    let size = body.len() as u64;

    // Header.
    let mut out = Vec::with_capacity(8 + body.len());
    encode_type_size_header(&mut out, type_id, size);

    // Body: zlib-compressed at the default level (matches LooseStore).
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(body).map_err(|e| PackBuildError::Io {
        path: PathBuf::from("<zlib-encode>"),
        source: e,
    })?;
    let compressed = encoder.finish().map_err(|e| PackBuildError::Io {
        path: PathBuf::from("<zlib-encode>"),
        source: e,
    })?;
    out.extend_from_slice(&compressed);
    Ok(out)
}

/// Push the variable-length type+size bytes onto `out`.
///
/// Layout (must round-trip through `parse_type_size`):
///   - byte 0: bit 7 = continue?, bits 4-6 = type_id (3 bits), bits 0-3 = size & 0x0f
///   - subsequent bytes: bit 7 = continue?, bits 0-6 = next 7 bits of size, LE-order
fn encode_type_size_header(out: &mut Vec<u8>, type_id: u8, size: u64) {
    debug_assert!(type_id <= 7);
    let mut rem = size >> 4;
    let cont0: u8 = if rem > 0 { 0x80 } else { 0 };
    let first = cont0 | ((type_id & 0x07) << 4) | ((size & 0x0f) as u8);
    out.push(first);
    while rem > 0 {
        let chunk = (rem & 0x7f) as u8;
        rem >>= 7;
        let cont: u8 = if rem > 0 { 0x80 } else { 0 };
        out.push(cont | chunk);
    }
}

/// A `Write` wrapper that hashes every byte that passes through and tracks the
/// total written count. We use this for the pack and idx files so the trailer
/// hash is just `hasher.finalize()` after the entries are flushed.
struct HashingWriter<W: Write> {
    inner: W,
    hasher: Box<dyn Hasher>,
    written: u64,
}

impl<W: Write> HashingWriter<W> {
    fn new(inner: W, hasher: Box<dyn Hasher>) -> Self {
        Self {
            inner,
            hasher,
            written: 0,
        }
    }

    fn bytes_written(&self) -> u64 {
        self.written
    }

    fn write_all(&mut self, buf: &[u8]) -> std::io::Result<()> {
        self.inner.write_all(buf)?;
        self.hasher.update(buf);
        self.written += buf.len() as u64;
        Ok(())
    }

    /// Surrender the wrapped writer + hasher so we can flush and finalize them
    /// separately. The trailer write happens on `inner` AFTER `hasher.finalize()`,
    /// so they have to come apart at this point.
    fn finish_into_inner(self) -> FinishedHashingWriter<W> {
        FinishedHashingWriter {
            inner: self.inner,
            hasher: self.hasher,
        }
    }
}

struct FinishedHashingWriter<W: Write> {
    inner: W,
    hasher: Box<dyn Hasher>,
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::process::Command;
    use std::sync::Arc;

    use crate::hash::hash_all;
    use crate::object::RawObject;
    use crate::odb::{LooseStore, ObjectDb};
    use crate::pack::{IdxFile, PackEntryKind, PackFile};

    use tempfile::tempdir;

    fn make_odb(dir: &Path) -> ObjectDb {
        let loose = LooseStore::new(dir.to_path_buf(), HashKind::Sha1);
        ObjectDb::new(vec![Arc::new(loose)], 0, HashKind::Sha1)
    }

    fn three_canonical_objects(odb: &ObjectDb) -> Vec<ObjectId> {
        // 1 blob, 1 tree referring to that blob, 1 commit referring to the tree.
        let blob = RawObject::new(ObjectKind::Blob, b"hello world\n".to_vec());
        let blob_oid = odb.write(&blob).unwrap();

        // Tree with one entry: 100644 file.txt -> blob_oid.
        let mut tree_body = Vec::new();
        tree_body.extend_from_slice(b"100644 file.txt\0");
        tree_body.extend_from_slice(blob_oid.as_bytes());
        let tree = RawObject::new(ObjectKind::Tree, tree_body);
        let tree_oid = odb.write(&tree).unwrap();

        // Commit referencing the tree.
        let commit_body = format!(
            "tree {tree_oid}\nauthor T <t@t> 1700000000 +0000\ncommitter T <t@t> 1700000000 +0000\n\nmsg\n"
        )
        .into_bytes();
        let commit = RawObject::new(ObjectKind::Commit, commit_body);
        let commit_oid = odb.write(&commit).unwrap();

        vec![blob_oid, tree_oid, commit_oid]
    }

    #[test]
    fn round_trip_via_our_readers() {
        let work = tempdir().unwrap();
        let odb_dir = work.path().join("objects");
        std::fs::create_dir_all(&odb_dir).unwrap();
        let odb = make_odb(&odb_dir);
        let oids = three_canonical_objects(&odb);

        let out_dir = work.path().join("pack-out");
        let r = write_pack(&oids, &odb, &out_dir, HashKind::Sha1).expect("write_pack");
        assert_eq!(r.object_count, oids.len() as u32);
        assert!(r.pack_path.exists());
        assert!(r.idx_path.exists());
        assert!(r
            .pack_path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("pack-"));

        // Open with our own readers.
        let pf = PackFile::open(&r.pack_path, HashKind::Sha1).expect("open pack");
        assert_eq!(pf.object_count(), oids.len() as u32);
        pf.verify_checksum().expect("pack checksum");

        let idx = IdxFile::open(&r.idx_path, HashKind::Sha1).expect("open idx");
        assert_eq!(idx.object_count(), oids.len() as u32);
        idx.verify_checksum().expect("idx checksum");

        for oid in &oids {
            let off = idx.lookup(oid).expect("oid in idx");
            let entry = pf.read_entry_at(off).expect("read entry");
            // Re-frame and re-hash to confirm we got the right body back.
            let kind = match &entry.kind {
                PackEntryKind::Direct(k) => *k,
                other => panic!("expected Direct, got {other:?}"),
            };
            let mut framed = format!("{} {}\0", kind.as_str(), entry.data.len()).into_bytes();
            framed.extend_from_slice(&entry.data);
            let got_oid = hash_all(HashKind::Sha1, &framed);
            assert_eq!(got_oid, *oid, "round-tripped oid mismatch");
        }
    }

    #[test]
    fn git_verify_pack_accepts_our_output() {
        let Some(_git) = git_available() else {
            eprintln!("skipping: no system git");
            return;
        };
        let work = tempdir().unwrap();
        let odb_dir = work.path().join("objects");
        std::fs::create_dir_all(&odb_dir).unwrap();
        let odb = make_odb(&odb_dir);
        let oids = three_canonical_objects(&odb);

        let out_dir = work.path().join("pack-out");
        let r = write_pack(&oids, &odb, &out_dir, HashKind::Sha1).expect("write_pack");

        let out = Command::new("git")
            .args(["verify-pack", "-v"])
            .arg(&r.pack_path)
            .output()
            .expect("git verify-pack ran");
        assert!(
            out.status.success(),
            "git verify-pack failed: stdout={:?} stderr={:?}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        let stdout = String::from_utf8_lossy(&out.stdout);
        let object_lines = stdout
            .lines()
            .filter(|l| l.len() >= 40 && l.as_bytes()[..40].iter().all(|b| b.is_ascii_hexdigit()))
            .count();
        assert_eq!(
            object_lines,
            oids.len(),
            "verify-pack didn't see every object"
        );
    }

    #[test]
    fn git_index_pack_verify_accepts_our_output() {
        let Some(_git) = git_available() else {
            eprintln!("skipping: no system git");
            return;
        };
        let work = tempdir().unwrap();
        let odb_dir = work.path().join("objects");
        std::fs::create_dir_all(&odb_dir).unwrap();
        let odb = make_odb(&odb_dir);
        let oids = three_canonical_objects(&odb);

        let out_dir = work.path().join("pack-out");
        let r = write_pack(&oids, &odb, &out_dir, HashKind::Sha1).expect("write_pack");

        let out = Command::new("git")
            .args(["index-pack", "--verify"])
            .arg(&r.pack_path)
            .output()
            .expect("git index-pack ran");
        assert!(
            out.status.success(),
            "git index-pack --verify failed: stdout={:?} stderr={:?}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    #[test]
    fn output_is_deterministic() {
        let work1 = tempdir().unwrap();
        let work2 = tempdir().unwrap();
        let r1 = build_canonical_pack(work1.path());
        let r2 = build_canonical_pack(work2.path());

        let p1 = std::fs::read(&r1.pack_path).unwrap();
        let p2 = std::fs::read(&r2.pack_path).unwrap();
        assert_eq!(p1, p2, "pack bytes differ across runs");

        let i1 = std::fs::read(&r1.idx_path).unwrap();
        let i2 = std::fs::read(&r2.idx_path).unwrap();
        assert_eq!(i1, i2, "idx bytes differ across runs");

        assert_eq!(r1.pack_name, r2.pack_name);
    }

    fn build_canonical_pack(root: &Path) -> PackBuildResult {
        let odb_dir = root.join("objects");
        std::fs::create_dir_all(&odb_dir).unwrap();
        let odb = make_odb(&odb_dir);
        let oids = three_canonical_objects(&odb);
        let out_dir = root.join("pack-out");
        write_pack(&oids, &odb, &out_dir, HashKind::Sha1).expect("write_pack")
    }

    #[test]
    fn empty_input_errors() {
        let work = tempdir().unwrap();
        let odb_dir = work.path().join("objects");
        std::fs::create_dir_all(&odb_dir).unwrap();
        let odb = make_odb(&odb_dir);
        let out_dir = work.path().join("pack-out");
        match write_pack(&[], &odb, &out_dir, HashKind::Sha1) {
            Err(PackBuildError::Empty) => {}
            other => panic!("expected Empty, got {other:?}"),
        }
        match write_pack_from_objects(&[], &out_dir, HashKind::Sha1) {
            Err(PackBuildError::Empty) => {}
            other => panic!("expected Empty, got {other:?}"),
        }
    }

    #[test]
    fn large_object_multi_byte_header() {
        let work = tempdir().unwrap();
        let odb_dir = work.path().join("objects");
        std::fs::create_dir_all(&odb_dir).unwrap();
        let odb = make_odb(&odb_dir);

        // 1 MB blob. The size header alone needs 3+ bytes.
        let big = vec![0x55u8; 1024 * 1024];
        let blob = RawObject::new(ObjectKind::Blob, big.clone());
        let blob_oid = odb.write(&blob).unwrap();

        let out_dir = work.path().join("pack-out");
        let r = write_pack(&[blob_oid], &odb, &out_dir, HashKind::Sha1).expect("write_pack");

        let pf = PackFile::open(&r.pack_path, HashKind::Sha1).expect("open pack");
        let idx = IdxFile::open(&r.idx_path, HashKind::Sha1).expect("open idx");
        let off = idx.lookup(&blob_oid).expect("blob in idx");
        let entry = pf.read_entry_at(off).expect("read entry");
        assert_eq!(entry.declared_size, big.len() as u64);
        assert_eq!(entry.data, big);
    }

    #[test]
    fn idx_oids_are_in_ascending_order() {
        // Use enough random-ish blobs that ascending order is non-trivial.
        let work = tempdir().unwrap();
        let odb_dir = work.path().join("objects");
        std::fs::create_dir_all(&odb_dir).unwrap();
        let odb = make_odb(&odb_dir);

        let mut oids = Vec::new();
        for i in 0..16u8 {
            let blob = RawObject::new(ObjectKind::Blob, vec![i; 32]);
            oids.push(odb.write(&blob).unwrap());
        }

        let out_dir = work.path().join("pack-out");
        let r = write_pack(&oids, &odb, &out_dir, HashKind::Sha1).expect("write_pack");

        let idx = IdxFile::open(&r.idx_path, HashKind::Sha1).expect("open idx");
        let mut prev: Option<ObjectId> = None;
        for (oid, _off) in idx.iter() {
            if let Some(p) = prev {
                assert!(
                    p.as_bytes() < oid.as_bytes(),
                    "idx oids not strictly ascending: {p} >= {oid}"
                );
            }
            prev = Some(oid);
        }
    }

    #[test]
    fn header_encoder_round_trips_size_4096() {
        // A size just past the 4-bit window — the first byte holds bits 0..4
        // and we continue with another byte holding bits 4..11.
        let mut out = Vec::new();
        encode_type_size_header(&mut out, 3, 4096);
        // Decode it back via the reader's logic (open-coded here, since
        // parse_type_size is private to file.rs).
        let (t, s, n) = decode_type_size(&out).unwrap();
        assert_eq!(t, 3);
        assert_eq!(s, 4096);
        assert_eq!(n, out.len());
    }

    /// Mini decoder mirroring `parse_type_size` in file.rs — kept here so tests
    /// don't need access to its private helper.
    fn decode_type_size(buf: &[u8]) -> Option<(u8, u64, usize)> {
        if buf.is_empty() {
            return None;
        }
        let first = buf[0];
        let raw_type = (first >> 4) & 0x07;
        let mut size: u64 = (first & 0x0f) as u64;
        let mut shift: u32 = 4;
        let mut idx = 1usize;
        let mut more = (first & 0x80) != 0;
        while more {
            if idx >= buf.len() {
                return None;
            }
            let b = buf[idx];
            size |= ((b & 0x7f) as u64) << shift;
            shift += 7;
            more = (b & 0x80) != 0;
            idx += 1;
        }
        Some((raw_type, size, idx))
    }

    fn git_available() -> Option<()> {
        let out = Command::new("git").arg("--version").output().ok()?;
        out.status.success().then_some(())
    }
}
