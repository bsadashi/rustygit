//! Multi-pack-index (MIDX) format reader & writer (M15).
//!
//! A `.git/objects/pack/multi-pack-index` lookup table maps oid → (pack-id,
//! offset within pack) across every pack in the directory. With one pack
//! present, an oid lookup is the same `O(log N)` you'd do against a single
//! `.idx`. With many packs, the midx collapses the per-pack search and the
//! cross-pack `O(N_packs)` selection into a single binary search on a
//! unified OID table.
//!
//! ## On-disk layout
//!
//! Little / big endian: all multi-byte integers are big-endian.
//!
//! ```text
//! HEADER (12 bytes):
//!   [0..4]   magic = b"MIDX"
//!   [4]      version       = 1
//!   [5]      hash version  = 1 (sha1) or 2 (sha256)
//!   [6]      num_chunks
//!   [7]      num_base_midx = 0 (we don't write chains)
//!   [8..12]  num_packs (u32 BE)
//!
//! CHUNK LOOKUP TABLE (12 bytes per entry; num_chunks+1 entries):
//!   [..4]    chunk id (b"PNAM", b"OIDF", ...)
//!   [..12]   offset of chunk's start, from the file's beginning, u64 BE
//!   The final entry has id = 0 and offset = (end of last chunk).
//!
//! CHUNKS (in order):
//!   PNAM: pack names. Null-terminated strings, packs in ascending byte order.
//!         Padded to a multiple of 4 with NULs.
//!   OIDF: 256 * u32 BE fanout. fanout[i] = #oids whose first byte is <= i.
//!   OIDL: N * raw_len bytes (the sorted OIDs themselves).
//!   OOFF: N * 8 bytes. Each entry is (pack_id u32 BE, offset u32 BE). The
//!         high bit of `offset` indicates "look in LOFF at index = lower 31".
//!   LOFF: present only if any pack offset >= 2^31. Each entry is u64 BE.
//!
//! TRAILER:
//!   raw_len bytes — hash of everything before it.
//! ```
//!
//! Pack id is an index into PNAM. Pack names in PNAM are the `.idx` filenames
//! (matching git's behaviour) — e.g. `pack-<hash>.idx`.
//!
//! ## Object-overlap policy
//!
//! When two packs contain the same oid, the midx keeps a single entry per
//! oid. We pick the pack that comes FIRST in PNAM ordering. This is a
//! deterministic, easy-to-explain rule, and matches what `git multi-pack-index
//! verify` accepts for plain (non-preferred-pack) writes.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use memmap2::Mmap;

use crate::hash::{hash_all, new_hasher, HashKind, Hasher, ObjectId};
use crate::pack::IdxFile;
use crate::repo::Repository;

// ---------- Format constants ------------------------------------------------

pub const MIDX_SIGNATURE: [u8; 4] = *b"MIDX";
pub const MIDX_VERSION: u8 = 1;
pub const MIDX_HEADER_SIZE: usize = 12;
pub const MIDX_CHUNK_ALIGNMENT: usize = 4;
const TOC_ENTRY_SIZE: usize = 12; // 4 byte id + 8 byte offset
const FANOUT_SIZE: usize = 256 * 4;
const OOFF_ENTRY_SIZE: usize = 8; // pack_id u32 + offset u32
const LARGE_OFFSET_NEEDED: u32 = 0x8000_0000;

pub const CHUNK_ID_PNAM: [u8; 4] = *b"PNAM";
pub const CHUNK_ID_OIDF: [u8; 4] = *b"OIDF";
pub const CHUNK_ID_OIDL: [u8; 4] = *b"OIDL";
pub const CHUNK_ID_OOFF: [u8; 4] = *b"OOFF";
pub const CHUNK_ID_LOFF: [u8; 4] = *b"LOFF";

pub const MIDX_FILENAME: &str = "multi-pack-index";

// ---------- Errors ----------------------------------------------------------

#[derive(thiserror::Error, Debug)]
pub enum MidxError {
    #[error(transparent)]
    Pack(#[from] crate::pack::PackError),
    #[error(transparent)]
    Hash(#[from] crate::hash::HashError),
    #[error("io on {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("malformed midx: {0}")]
    Malformed(&'static str),
    #[error("bad signature: expected 'MIDX', got {0:?}")]
    BadSignature([u8; 4]),
    #[error("unsupported midx version: {0}")]
    UnsupportedVersion(u8),
    #[error("checksum mismatch")]
    ChecksumMismatch,
}

fn io_err(path: &Path, source: std::io::Error) -> MidxError {
    MidxError::Io {
        path: path.to_path_buf(),
        source,
    }
}

// ---------- Write result ----------------------------------------------------

#[derive(Debug)]
pub struct WriteResult {
    pub pack_count: u32,
    pub object_count: u32,
    pub path: PathBuf,
    pub bytes_written: u64,
}

// ---------- Writer ----------------------------------------------------------

/// Write a multi-pack-index over every `.pack`/`.idx` pair under
/// `<gitdir>/objects/pack/`.
pub fn write(repo: &Repository) -> Result<WriteResult, MidxError> {
    let pack_dir = repo.gitdir().join("objects").join("pack");
    let hash_kind = repo.hash_kind();
    write_in_dir(&pack_dir, hash_kind)
}

/// Lower-level entry point: same as [`write`] but takes the pack directory
/// explicitly. Useful for tests that build packs in scratch directories.
pub fn write_in_dir(pack_dir: &Path, hash_kind: HashKind) -> Result<WriteResult, MidxError> {
    std::fs::create_dir_all(pack_dir).map_err(|e| io_err(pack_dir, e))?;

    // Discover (.idx, .pack) pairs. We key off `.idx` files since that's what
    // the MIDX stores in PNAM (matching git).
    let mut idx_names: Vec<String> = Vec::new();
    let entries = std::fs::read_dir(pack_dir).map_err(|e| io_err(pack_dir, e))?;
    for ent in entries.flatten() {
        let path = ent.path();
        if path.extension().and_then(|s| s.to_str()) == Some("idx") {
            // Confirm a sibling .pack exists.
            let stem = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or_default();
            let pack_path = pack_dir.join(format!("{stem}.pack"));
            if pack_path.exists() {
                if let Some(name) = path.file_name().and_then(|s| s.to_str()) {
                    idx_names.push(name.to_string());
                }
            }
        }
    }
    // PNAM is sorted ascending byte order.
    idx_names.sort();
    let pack_count = idx_names.len() as u32;

    // Walk each idx, collect (oid, pack_idx, offset). On oid collisions
    // across packs, keep the one whose pack_idx is the lowest (= earliest
    // in PNAM order).
    let mut by_oid: BTreeMap<ObjectId, (u32, u64)> = BTreeMap::new();
    for (pack_idx, idx_name) in idx_names.iter().enumerate() {
        let idx_path = pack_dir.join(idx_name);
        let idx = IdxFile::open(&idx_path, hash_kind)?;
        for (oid, offset) in idx.iter() {
            // First-seen wins; iteration is in ascending oid order, but
            // we still need the earlier-pack-wins semantics across packs.
            by_oid
                .entry(oid)
                .or_insert_with(|| (pack_idx as u32, offset));
        }
    }

    // Sort entries by oid (BTreeMap iteration is sorted already).
    let mut entries: Vec<(ObjectId, u32, u64)> = by_oid
        .into_iter()
        .map(|(oid, (p, o))| (oid, p, o))
        .collect();
    entries.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
    let object_count = entries.len() as u32;

    let final_path = pack_dir.join(MIDX_FILENAME);
    let tmp_path = pack_dir.join(format!("{MIDX_FILENAME}.tmp"));

    let bytes_written = write_midx_bytes(&tmp_path, hash_kind, &idx_names, &entries)?;

    std::fs::rename(&tmp_path, &final_path).map_err(|e| io_err(&final_path, e))?;
    Ok(WriteResult {
        pack_count,
        object_count,
        path: final_path,
        bytes_written,
    })
}

/// Emit the raw MIDX file bytes at `path`. Returns total bytes written.
fn write_midx_bytes(
    path: &Path,
    hash_kind: HashKind,
    pack_names: &[String],
    entries: &[(ObjectId, u32, u64)],
) -> Result<u64, MidxError> {
    let raw_len = hash_kind.raw_len();

    // ---- Compute PNAM chunk: each name followed by NUL; padded to 4. ----
    let mut pnam_bytes: Vec<u8> = Vec::new();
    for name in pack_names {
        pnam_bytes.extend_from_slice(name.as_bytes());
        pnam_bytes.push(0);
    }
    let pad =
        (MIDX_CHUNK_ALIGNMENT - (pnam_bytes.len() % MIDX_CHUNK_ALIGNMENT)) % MIDX_CHUNK_ALIGNMENT;
    pnam_bytes.extend(std::iter::repeat_n(0u8, pad));

    // ---- Compute OIDF: 256 u32 BE fanout. ----
    let mut fanout = [0u32; 256];
    for (oid, _p, _o) in entries {
        let first = oid.as_bytes()[0] as usize;
        for bucket in fanout.iter_mut().skip(first) {
            *bucket += 1;
        }
    }
    let mut oidf_bytes: Vec<u8> = Vec::with_capacity(FANOUT_SIZE);
    for v in &fanout {
        oidf_bytes.extend_from_slice(&v.to_be_bytes());
    }
    debug_assert_eq!(oidf_bytes.len(), FANOUT_SIZE);

    // ---- Compute OIDL: raw oid bytes, sorted. ----
    let mut oidl_bytes: Vec<u8> = Vec::with_capacity(entries.len() * raw_len);
    for (oid, _p, _o) in entries {
        oidl_bytes.extend_from_slice(oid.as_bytes());
    }

    // ---- Compute OOFF + LOFF. ----
    let mut large_offsets: Vec<u64> = Vec::new();
    let mut ooff_bytes: Vec<u8> = Vec::with_capacity(entries.len() * OOFF_ENTRY_SIZE);
    for (_oid, pack_idx, offset) in entries {
        ooff_bytes.extend_from_slice(&pack_idx.to_be_bytes());
        if *offset < LARGE_OFFSET_NEEDED as u64 {
            ooff_bytes.extend_from_slice(&(*offset as u32).to_be_bytes());
        } else {
            let large_idx = large_offsets.len() as u32;
            let encoded = LARGE_OFFSET_NEEDED | large_idx;
            ooff_bytes.extend_from_slice(&encoded.to_be_bytes());
            large_offsets.push(*offset);
        }
    }
    let mut loff_bytes: Vec<u8> = Vec::with_capacity(large_offsets.len() * 8);
    for off in &large_offsets {
        loff_bytes.extend_from_slice(&off.to_be_bytes());
    }

    // ---- Lay out chunks: order is PNAM, OIDF, OIDL, OOFF, [LOFF]. ----
    let has_loff = !loff_bytes.is_empty();
    let num_chunks: u8 = if has_loff { 5 } else { 4 };

    // The TOC has num_chunks+1 entries. Compute offsets accordingly.
    let toc_total = (num_chunks as usize + 1) * TOC_ENTRY_SIZE;
    let mut cur: u64 = (MIDX_HEADER_SIZE + toc_total) as u64;

    let pnam_off = cur;
    cur += pnam_bytes.len() as u64;
    let oidf_off = cur;
    cur += oidf_bytes.len() as u64;
    let oidl_off = cur;
    cur += oidl_bytes.len() as u64;
    let ooff_off = cur;
    cur += ooff_bytes.len() as u64;
    let loff_off = if has_loff {
        let v = cur;
        cur += loff_bytes.len() as u64;
        Some(v)
    } else {
        None
    };
    let end_off = cur;

    // ---- Open the file and write through a hashing wrapper. ----
    let file = File::create(path).map_err(|e| io_err(path, e))?;
    let mut w = HashingWriter::new(BufWriter::new(file), new_hasher(hash_kind));

    // Header.
    w.write_all(&MIDX_SIGNATURE).map_err(|e| io_err(path, e))?;
    w.write_all(&[MIDX_VERSION]).map_err(|e| io_err(path, e))?;
    let hash_version: u8 = match hash_kind {
        HashKind::Sha1 => 1,
        HashKind::Sha256 => 2,
    };
    w.write_all(&[hash_version]).map_err(|e| io_err(path, e))?;
    w.write_all(&[num_chunks]).map_err(|e| io_err(path, e))?;
    w.write_all(&[0]).map_err(|e| io_err(path, e))?; // num base midx
    let pack_count = pack_names.len() as u32;
    w.write_all(&pack_count.to_be_bytes())
        .map_err(|e| io_err(path, e))?;

    // TOC.
    write_toc_entry(&mut w, path, &CHUNK_ID_PNAM, pnam_off)?;
    write_toc_entry(&mut w, path, &CHUNK_ID_OIDF, oidf_off)?;
    write_toc_entry(&mut w, path, &CHUNK_ID_OIDL, oidl_off)?;
    write_toc_entry(&mut w, path, &CHUNK_ID_OOFF, ooff_off)?;
    if let Some(off) = loff_off {
        write_toc_entry(&mut w, path, &CHUNK_ID_LOFF, off)?;
    }
    // Trailing TOC entry (id=0, offset=end-of-chunks).
    write_toc_entry(&mut w, path, &[0u8; 4], end_off)?;

    // Chunks.
    w.write_all(&pnam_bytes).map_err(|e| io_err(path, e))?;
    w.write_all(&oidf_bytes).map_err(|e| io_err(path, e))?;
    w.write_all(&oidl_bytes).map_err(|e| io_err(path, e))?;
    w.write_all(&ooff_bytes).map_err(|e| io_err(path, e))?;
    if has_loff {
        w.write_all(&loff_bytes).map_err(|e| io_err(path, e))?;
    }

    // Trailer: hash of everything we just wrote.
    let body_bytes = w.bytes_written();
    let inner = w.finish_into_inner();
    let trailer = inner.hasher.finalize();
    let mut bw = inner.inner;
    bw.write_all(trailer.as_bytes())
        .map_err(|e| io_err(path, e))?;
    bw.flush().map_err(|e| io_err(path, e))?;
    let f = bw.into_inner().map_err(|e| io_err(path, e.into_error()))?;
    f.sync_all().map_err(|e| io_err(path, e))?;
    Ok(body_bytes + raw_len as u64)
}

fn write_toc_entry<W: Write>(
    w: &mut HashingWriter<W>,
    path: &Path,
    id: &[u8; 4],
    offset: u64,
) -> Result<(), MidxError> {
    w.write_all(id).map_err(|e| io_err(path, e))?;
    w.write_all(&offset.to_be_bytes())
        .map_err(|e| io_err(path, e))?;
    Ok(())
}

// ---------- Reader ----------------------------------------------------------

pub struct MultiPackIndex {
    bytes: Mmap,
    hash_kind: HashKind,
    #[allow(dead_code)]
    pnam_off: usize,
    #[allow(dead_code)]
    pnam_len: usize,
    oidf_off: usize,
    oidl_off: usize,
    ooff_off: usize,
    loff_off: Option<usize>,
    pack_count: u32,
    object_count: u32,
    pack_names: Vec<String>,
    path: PathBuf,
}

impl MultiPackIndex {
    pub fn open(path: impl AsRef<Path>, hash_kind: HashKind) -> Result<Self, MidxError> {
        let path = path.as_ref().to_path_buf();
        let file = File::open(&path).map_err(|e| io_err(&path, e))?;
        // SAFETY: the midx file is treated as immutable for the duration of
        // the mmap (same convention as IdxFile/PackFile).
        let bytes = unsafe { Mmap::map(&file) }.map_err(|e| io_err(&path, e))?;
        let raw_len = hash_kind.raw_len();

        if bytes.len() < MIDX_HEADER_SIZE + raw_len {
            return Err(MidxError::Malformed("midx shorter than header+trailer"));
        }
        if bytes[0..4] != MIDX_SIGNATURE {
            let mut s = [0u8; 4];
            s.copy_from_slice(&bytes[0..4]);
            return Err(MidxError::BadSignature(s));
        }
        let version = bytes[4];
        if version != MIDX_VERSION {
            return Err(MidxError::UnsupportedVersion(version));
        }
        let hash_version = bytes[5];
        let expected_hash_version: u8 = match hash_kind {
            HashKind::Sha1 => 1,
            HashKind::Sha256 => 2,
        };
        if hash_version != expected_hash_version {
            return Err(MidxError::Malformed("hash version mismatch"));
        }
        let num_chunks = bytes[6] as usize;
        // bytes[7] is num_base_midx; we only support 0 (non-chained).
        let pack_count = read_u32_be(&bytes, 8);

        // Read the TOC.
        let toc_off = MIDX_HEADER_SIZE;
        let toc_entries = num_chunks + 1;
        let toc_end = toc_off + toc_entries * TOC_ENTRY_SIZE;
        if toc_end + raw_len > bytes.len() {
            return Err(MidxError::Malformed("midx TOC out of bounds"));
        }

        let mut pnam_off = None;
        let mut pnam_end = None;
        let mut oidf_off = None;
        let mut oidl_off = None;
        let mut ooff_off = None;
        let mut loff_off = None;

        // Read all chunk-ids + offsets, plus the trailing sentinel.
        let mut toc: Vec<([u8; 4], u64)> = Vec::with_capacity(toc_entries);
        for i in 0..toc_entries {
            let base = toc_off + i * TOC_ENTRY_SIZE;
            let mut id = [0u8; 4];
            id.copy_from_slice(&bytes[base..base + 4]);
            let off = read_u64_be(&bytes, base + 4);
            toc.push((id, off));
        }
        let end_of_chunks_off = toc.last().map(|(_, o)| *o).unwrap_or(0);
        if end_of_chunks_off as usize > bytes.len() - raw_len {
            return Err(MidxError::Malformed("midx TOC end past file body"));
        }

        // Build a quick chunk-id → (start, end) view, using the next entry's
        // offset as `end` (works because chunks are written contiguously).
        for i in 0..num_chunks {
            let (id, start) = toc[i];
            let end = toc[i + 1].1;
            let s = start as usize;
            let e = end as usize;
            match &id {
                b"PNAM" => {
                    pnam_off = Some(s);
                    pnam_end = Some(e);
                }
                b"OIDF" => {
                    oidf_off = Some(s);
                    if e - s != FANOUT_SIZE {
                        return Err(MidxError::Malformed("OIDF chunk wrong size"));
                    }
                }
                b"OIDL" => oidl_off = Some(s),
                b"OOFF" => ooff_off = Some(s),
                b"LOFF" => loff_off = Some(s),
                _ => { /* unknown chunk — ignore for forward-compat */ }
            }
        }

        let pnam_off = pnam_off.ok_or(MidxError::Malformed("PNAM chunk missing"))?;
        let pnam_end = pnam_end.ok_or(MidxError::Malformed("PNAM chunk missing"))?;
        let oidf_off = oidf_off.ok_or(MidxError::Malformed("OIDF chunk missing"))?;
        let oidl_off = oidl_off.ok_or(MidxError::Malformed("OIDL chunk missing"))?;
        let ooff_off = ooff_off.ok_or(MidxError::Malformed("OOFF chunk missing"))?;

        // object_count = fanout[255].
        let object_count = read_u32_be(&bytes, oidf_off + 255 * 4);

        // Parse pack names out of PNAM (NUL-separated, padded).
        let pnam_len = pnam_end - pnam_off;
        let pnam_slice = &bytes[pnam_off..pnam_off + pnam_len];
        let mut pack_names: Vec<String> = Vec::with_capacity(pack_count as usize);
        let mut start = 0;
        for i in 0..pnam_slice.len() {
            if pnam_slice[i] == 0 {
                if pack_names.len() == pack_count as usize {
                    // Anything after the last name is padding — stop.
                    break;
                }
                let name = std::str::from_utf8(&pnam_slice[start..i])
                    .map_err(|_| MidxError::Malformed("PNAM not UTF-8"))?;
                pack_names.push(name.to_string());
                start = i + 1;
            }
        }
        if pack_names.len() != pack_count as usize {
            return Err(MidxError::Malformed("PNAM has wrong number of names"));
        }

        Ok(Self {
            bytes,
            hash_kind,
            pnam_off,
            pnam_len,
            oidf_off,
            oidl_off,
            ooff_off,
            loff_off,
            pack_count,
            object_count,
            pack_names,
            path,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn pack_count(&self) -> u32 {
        self.pack_count
    }

    pub fn object_count(&self) -> u32 {
        self.object_count
    }

    pub fn hash_kind(&self) -> HashKind {
        self.hash_kind
    }

    pub fn pack_name(&self, idx: u32) -> Option<&str> {
        self.pack_names.get(idx as usize).map(|s| s.as_str())
    }

    /// Lookup an oid → (pack_index, offset_in_pack). `None` if not present.
    pub fn lookup(&self, oid: &ObjectId) -> Option<(u32, u64)> {
        if oid.kind() != self.hash_kind {
            return None;
        }
        let idx = self.find_index(oid)?;
        Some(self.entry_at(idx))
    }

    /// Iterate (oid, pack_idx, offset) in sorted-by-oid order.
    pub fn iter(&self) -> MidxIter<'_> {
        MidxIter {
            midx: self,
            position: 0,
        }
    }

    /// Verify file structure + trailer hash. Returns Ok(()) when the file is
    /// internally consistent.
    pub fn verify(&self) -> Result<(), MidxError> {
        // Trailer hash over everything before the last raw_len bytes.
        let raw_len = self.hash_kind.raw_len();
        let body_end = self.bytes.len() - raw_len;
        let computed = hash_all(self.hash_kind, &self.bytes[..body_end]);
        let stored = &self.bytes[body_end..];
        if computed.as_bytes() != stored {
            return Err(MidxError::ChecksumMismatch);
        }
        // Fanout monotonicity + agreement with object_count.
        let mut prev: u32 = 0;
        for i in 0..256 {
            let v = read_u32_be(&self.bytes, self.oidf_off + i * 4);
            if v < prev {
                return Err(MidxError::Malformed("non-monotonic fanout"));
            }
            prev = v;
        }
        if prev != self.object_count {
            return Err(MidxError::Malformed("fanout[255] != object_count"));
        }
        // OIDL strictly ascending.
        if self.object_count >= 2 {
            let mut prev_oid = self.oid_at(0);
            for i in 1..self.object_count as usize {
                let cur = self.oid_at(i);
                if cur.as_bytes() <= prev_oid.as_bytes() {
                    return Err(MidxError::Malformed("OIDL not strictly ascending"));
                }
                prev_oid = cur;
            }
        }
        // Each OOFF entry's pack_id is in range.
        for i in 0..self.object_count as usize {
            let (pack_idx, _) = self.entry_at(i);
            if pack_idx >= self.pack_count {
                return Err(MidxError::Malformed("OOFF pack_id out of range"));
            }
        }
        Ok(())
    }

    // ---- internals --------------------------------------------------------

    fn fanout(&self, i: u8) -> u32 {
        read_u32_be(&self.bytes, self.oidf_off + (i as usize) * 4)
    }

    fn find_index(&self, oid: &ObjectId) -> Option<usize> {
        let bytes = oid.as_bytes();
        let first = bytes[0];
        let lo = if first == 0 {
            0
        } else {
            self.fanout(first - 1) as usize
        };
        let hi = self.fanout(first) as usize;
        let raw_len = self.hash_kind.raw_len();
        let mut left = lo;
        let mut right = hi;
        while left < right {
            let mid = left + (right - left) / 2;
            let mid_oid = &self.bytes[self.oidl_off + mid * raw_len..][..raw_len];
            match mid_oid.cmp(bytes) {
                std::cmp::Ordering::Less => left = mid + 1,
                std::cmp::Ordering::Greater => right = mid,
                std::cmp::Ordering::Equal => return Some(mid),
            }
        }
        None
    }

    fn oid_at(&self, idx: usize) -> ObjectId {
        let raw_len = self.hash_kind.raw_len();
        let bytes = &self.bytes[self.oidl_off + idx * raw_len..][..raw_len];
        ObjectId::from_bytes(self.hash_kind, bytes).expect("oid bytes always correct length")
    }

    fn entry_at(&self, idx: usize) -> (u32, u64) {
        let base = self.ooff_off + idx * OOFF_ENTRY_SIZE;
        let pack_idx = read_u32_be(&self.bytes, base);
        let raw = read_u32_be(&self.bytes, base + 4);
        let offset: u64 = if raw & LARGE_OFFSET_NEEDED == 0 {
            raw as u64
        } else {
            let large_idx = (raw & 0x7fff_ffff) as usize;
            let loff = self
                .loff_off
                .expect("LOFF table must be present when large offsets are used");
            read_u64_be(&self.bytes, loff + large_idx * 8)
        };
        (pack_idx, offset)
    }
}

pub struct MidxIter<'a> {
    midx: &'a MultiPackIndex,
    position: usize,
}

impl<'a> Iterator for MidxIter<'a> {
    type Item = (ObjectId, u32, u64);
    fn next(&mut self) -> Option<Self::Item> {
        if self.position >= self.midx.object_count as usize {
            return None;
        }
        let oid = self.midx.oid_at(self.position);
        let (pack_idx, offset) = self.midx.entry_at(self.position);
        self.position += 1;
        Some((oid, pack_idx, offset))
    }
}

// ---------- Hashing writer (same shape as pack/build.rs) --------------------

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

// ---------- Byte helpers ----------------------------------------------------

fn read_u32_be(buf: &[u8], at: usize) -> u32 {
    u32::from_be_bytes([buf[at], buf[at + 1], buf[at + 2], buf[at + 3]])
}
fn read_u64_be(buf: &[u8], at: usize) -> u64 {
    u64::from_be_bytes([
        buf[at],
        buf[at + 1],
        buf[at + 2],
        buf[at + 3],
        buf[at + 4],
        buf[at + 5],
        buf[at + 6],
        buf[at + 7],
    ])
}

// ---------- Tests -----------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    use std::process::Command;
    use std::sync::Arc;

    use crate::hash::HashKind;
    use crate::object::{ObjectKind, RawObject};
    use crate::odb::{LooseStore, ObjectDb};
    use crate::pack::{write_pack, PackBuildResult};

    use tempfile::tempdir;

    fn make_odb(dir: &Path, hash_kind: HashKind) -> ObjectDb {
        let loose = LooseStore::new(dir.to_path_buf(), hash_kind);
        ObjectDb::new(vec![Arc::new(loose)], 0, hash_kind)
    }

    /// Build a small in-temp odb + pack-out directory. Returns `(odb, pack_dir)`.
    fn fresh_pack_dir() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let work = tempdir().unwrap();
        let odb_dir = work.path().join("objects");
        let pack_dir = odb_dir.join("pack");
        std::fs::create_dir_all(&pack_dir).unwrap();
        (work, odb_dir, pack_dir)
    }

    fn three_blobs(odb: &ObjectDb, salt: &str) -> Vec<ObjectId> {
        let mut out = Vec::new();
        for i in 0..3 {
            let body = format!("body {salt} {i}\n");
            let blob = RawObject::new(ObjectKind::Blob, body.into_bytes());
            out.push(odb.write(&blob).unwrap());
        }
        out
    }

    /// Convenience: write a pack containing `oids` into `pack_dir`, returning the result.
    fn write_one_pack(
        oids: &[ObjectId],
        odb: &ObjectDb,
        pack_dir: &Path,
        hash_kind: HashKind,
    ) -> PackBuildResult {
        write_pack(oids, odb, pack_dir, hash_kind).expect("write_pack")
    }

    #[test]
    fn write_with_zero_packs_errors_or_produces_empty() {
        let (_work, _odb_dir, pack_dir) = fresh_pack_dir();
        // No packs in directory — we should still successfully write a header-
        // only midx (pack_count=0, object_count=0).
        let result = write_in_dir(&pack_dir, HashKind::Sha1).expect("write empty midx");
        assert_eq!(result.pack_count, 0);
        assert_eq!(result.object_count, 0);
        assert!(result.path.exists());

        let midx = MultiPackIndex::open(&result.path, HashKind::Sha1).expect("open empty midx");
        assert_eq!(midx.pack_count(), 0);
        assert_eq!(midx.object_count(), 0);
        midx.verify().expect("empty midx verifies");
    }

    #[test]
    fn write_with_one_pack_round_trips() {
        let (_work, odb_dir, pack_dir) = fresh_pack_dir();
        let odb = make_odb(&odb_dir, HashKind::Sha1);
        let oids = three_blobs(&odb, "a");
        let pr = write_one_pack(&oids, &odb, &pack_dir, HashKind::Sha1);

        let result = write_in_dir(&pack_dir, HashKind::Sha1).expect("write midx");
        assert_eq!(result.pack_count, 1);
        assert_eq!(result.object_count, oids.len() as u32);

        let midx = MultiPackIndex::open(&result.path, HashKind::Sha1).expect("open midx");
        // PNAM should hold the .idx filename.
        let want_name = pr
            .idx_path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string();
        assert_eq!(midx.pack_name(0).unwrap(), want_name);

        // Each oid → (pack_idx=0, offset matching the per-pack idx lookup).
        let idx = IdxFile::open(&pr.idx_path, HashKind::Sha1).unwrap();
        for oid in &oids {
            let (pack_idx, offset) = midx.lookup(oid).expect("oid in midx");
            assert_eq!(pack_idx, 0);
            assert_eq!(offset, idx.lookup(oid).unwrap());
        }
        midx.verify().expect("verify");
    }

    #[test]
    fn write_with_two_disjoint_packs() {
        let (_work, odb_dir, pack_dir) = fresh_pack_dir();
        let odb = make_odb(&odb_dir, HashKind::Sha1);
        let a = three_blobs(&odb, "alpha");
        let b = three_blobs(&odb, "beta");

        let pr_a = write_one_pack(&a, &odb, &pack_dir, HashKind::Sha1);
        let pr_b = write_one_pack(&b, &odb, &pack_dir, HashKind::Sha1);

        let result = write_in_dir(&pack_dir, HashKind::Sha1).expect("midx");
        assert_eq!(result.pack_count, 2);
        assert_eq!(result.object_count as usize, a.len() + b.len());

        let midx = MultiPackIndex::open(&result.path, HashKind::Sha1).expect("open midx");

        // PNAM is sorted ascending → pack_idx is the lexicographic order of idx
        // basenames, not the order we wrote them.
        let mut names: Vec<String> = vec![
            pr_a.idx_path
                .file_name()
                .unwrap()
                .to_string_lossy()
                .to_string(),
            pr_b.idx_path
                .file_name()
                .unwrap()
                .to_string_lossy()
                .to_string(),
        ];
        names.sort();
        let pack_a_idx = names
            .iter()
            .position(|n| *n == pr_a.idx_path.file_name().unwrap().to_string_lossy())
            .unwrap() as u32;
        let pack_b_idx = names
            .iter()
            .position(|n| *n == pr_b.idx_path.file_name().unwrap().to_string_lossy())
            .unwrap() as u32;

        for oid in &a {
            let (p, _) = midx.lookup(oid).expect("a oid present");
            assert_eq!(p, pack_a_idx);
        }
        for oid in &b {
            let (p, _) = midx.lookup(oid).expect("b oid present");
            assert_eq!(p, pack_b_idx);
        }
        midx.verify().expect("verify");
    }

    #[test]
    fn write_with_two_overlapping_packs_picks_earliest_pack() {
        // Two packs share the same blob. MIDX should keep one entry, mapped to
        // whichever pack sorts FIRST in PNAM order.
        let (_work, odb_dir, pack_dir) = fresh_pack_dir();
        let odb = make_odb(&odb_dir, HashKind::Sha1);

        let shared_blob = RawObject::new(ObjectKind::Blob, b"shared\n".to_vec());
        let shared_oid = odb.write(&shared_blob).unwrap();
        let unique_a = RawObject::new(ObjectKind::Blob, b"only-a\n".to_vec());
        let unique_a_oid = odb.write(&unique_a).unwrap();
        let unique_b = RawObject::new(ObjectKind::Blob, b"only-b\n".to_vec());
        let unique_b_oid = odb.write(&unique_b).unwrap();

        let pr_a = write_one_pack(&[shared_oid, unique_a_oid], &odb, &pack_dir, HashKind::Sha1);
        let pr_b = write_one_pack(&[shared_oid, unique_b_oid], &odb, &pack_dir, HashKind::Sha1);

        let result = write_in_dir(&pack_dir, HashKind::Sha1).expect("midx");
        assert_eq!(result.pack_count, 2);
        // Three unique oids — the shared one is recorded once.
        assert_eq!(result.object_count, 3);

        let midx = MultiPackIndex::open(&result.path, HashKind::Sha1).expect("open");
        // pack_idx for shared should match the lexicographically-earlier pack.
        let name_a = pr_a
            .idx_path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string();
        let name_b = pr_b
            .idx_path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string();
        // Both sort positions == 0 since sorted PNAM regardless of name order.
        let earliest = 0u32;
        let expected_pack_idx = if name_a <= name_b { 0 } else { 1 };
        let _ = earliest;
        let (got_pack_idx, _) = midx.lookup(&shared_oid).expect("shared present");
        assert_eq!(
            got_pack_idx, expected_pack_idx,
            "shared oid should map to lexicographically earliest pack"
        );
    }

    #[test]
    fn round_trip_iterates_in_sorted_order() {
        let (_work, odb_dir, pack_dir) = fresh_pack_dir();
        let odb = make_odb(&odb_dir, HashKind::Sha1);
        let oids = three_blobs(&odb, "iter");
        let _ = write_one_pack(&oids, &odb, &pack_dir, HashKind::Sha1);

        let r = write_in_dir(&pack_dir, HashKind::Sha1).unwrap();
        let midx = MultiPackIndex::open(&r.path, HashKind::Sha1).unwrap();

        let collected: Vec<_> = midx.iter().collect();
        assert_eq!(collected.len(), oids.len());
        for w in collected.windows(2) {
            assert!(w[0].0.as_bytes() < w[1].0.as_bytes(), "iter not sorted");
        }
    }

    #[test]
    fn sha256_mode_round_trip() {
        let (_work, odb_dir, pack_dir) = fresh_pack_dir();
        let odb = make_odb(&odb_dir, HashKind::Sha256);
        let oids = three_blobs(&odb, "256");
        let pr = write_pack(&oids, &odb, &pack_dir, HashKind::Sha256).expect("write_pack sha256");

        let r = write_in_dir(&pack_dir, HashKind::Sha256).expect("midx sha256");
        assert_eq!(r.object_count, oids.len() as u32);
        assert_eq!(r.pack_count, 1);

        let midx = MultiPackIndex::open(&r.path, HashKind::Sha256).expect("open sha256 midx");
        midx.verify().expect("sha256 midx verifies");

        let idx = IdxFile::open(&pr.idx_path, HashKind::Sha256).unwrap();
        for oid in &oids {
            let (p, off) = midx.lookup(oid).expect("oid in midx");
            assert_eq!(p, 0);
            assert_eq!(off, idx.lookup(oid).unwrap());
        }
    }

    #[test]
    fn tampering_trailer_triggers_checksum_mismatch() {
        let (_work, odb_dir, pack_dir) = fresh_pack_dir();
        let odb = make_odb(&odb_dir, HashKind::Sha1);
        let oids = three_blobs(&odb, "tamper");
        let _ = write_one_pack(&oids, &odb, &pack_dir, HashKind::Sha1);
        let r = write_in_dir(&pack_dir, HashKind::Sha1).unwrap();

        let mut bytes = std::fs::read(&r.path).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 0xff;
        std::fs::write(&r.path, &bytes).unwrap();

        let midx = MultiPackIndex::open(&r.path, HashKind::Sha1).expect("opens fine");
        match midx.verify() {
            Err(MidxError::ChecksumMismatch) => {}
            other => panic!("expected ChecksumMismatch, got {other:?}"),
        }
    }

    #[test]
    fn git_multi_pack_index_verify_accepts_our_output() {
        // Set up a real .git dir, drop one of our packs into objects/pack/, run
        // our midx writer, then ask system git to verify it.
        let Some(_) = git_available() else {
            eprintln!("skipping: no system git");
            return;
        };
        let work = tempdir().unwrap();
        let git_init = Command::new("git")
            .args(["init", "-q", "-b", "master", "."])
            .current_dir(work.path())
            .output()
            .unwrap();
        assert!(git_init.status.success());

        // Make a few commits so we have a real history to pack.
        for (i, body) in ["one", "two", "three"].iter().enumerate() {
            std::fs::write(work.path().join("f.txt"), body).unwrap();
            run_git(work.path(), &["config", "user.email", "t@t"]);
            run_git(work.path(), &["config", "user.name", "t"]);
            run_git(work.path(), &["add", "."]);
            run_git(work.path(), &["commit", "-q", "-m", &format!("c{i}")]);
        }
        // Pack everything into one pack via git so the format is its own.
        run_git(work.path(), &["repack", "-a", "-d", "-q"]);
        // Create an additional pack so we have >=2.
        std::fs::write(work.path().join("extra.txt"), "extra").unwrap();
        run_git(work.path(), &["add", "."]);
        run_git(work.path(), &["commit", "-q", "-m", "extra"]);
        run_git(work.path(), &["repack", "-q"]);

        let pack_dir = work.path().join(".git/objects/pack");

        // Write the midx via our impl.
        let r = write_in_dir(&pack_dir, HashKind::Sha1).expect("write midx");
        assert!(r.pack_count >= 1);

        let out = Command::new("git")
            .args(["multi-pack-index", "verify"])
            .current_dir(work.path())
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git multi-pack-index verify failed:\nstdout={}\nstderr={}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn run_git(cwd: &Path, args: &[&str]) {
        let out = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn git_available() -> Option<()> {
        let out = Command::new("git").arg("--version").output().ok()?;
        out.status.success().then_some(())
    }
}
