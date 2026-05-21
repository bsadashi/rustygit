//! `.idx` v2 file reader.
//!
//! Layout (everything network/big-endian unless noted):
//!   [0..4]      magic = `\xff\x74\x4f\x63`
//!   [4..8]      version = 2 (u32 BE)
//!   [8..1032]   256-entry fanout table (each u32 BE).
//!               `fanout[i]` = number of OIDs whose first byte is <= i.
//!               `fanout[255]` = total object count N.
//!   then        N raw OIDs in sorted order (raw_len bytes each)
//!   then        N CRC32 values  (u32 BE)
//!   then        N pack-offsets  (u32 BE; high bit set => index into the
//!               64-bit table, low 31 bits = the index)
//!   optionally  64-bit offset table (only present if any small offset's
//!               high bit was set). Size = (#large) * 8.
//!   then        pack hash + idx hash, each raw_len bytes.
//!
//! We mmap the file and serve all reads from it. Lookups are binary search
//! within the fanout-narrowed range.

use std::path::{Path, PathBuf};

use memmap2::Mmap;

use crate::hash::{hash_all, HashKind, ObjectId};

use super::PackError;

const IDX_V2_MAGIC: [u8; 4] = [0xff, 0x74, 0x4f, 0x63];
const FANOUT_OFF: usize = 8;
const FANOUT_BYTES: usize = 256 * 4;

pub struct IdxFile {
    mmap: Mmap,
    hash_kind: HashKind,
    object_count: u32,
    fanout_off: usize,
    oid_table_off: usize,
    crc_table_off: usize,
    offset_table_off: usize,
    /// Offset of the 64-bit large-offset table. Equal to `trailer_start` when
    /// the table is absent.
    large_offset_table_off: usize,
    path: PathBuf,
}

impl IdxFile {
    pub fn open(path: impl AsRef<Path>, hash_kind: HashKind) -> Result<Self, PackError> {
        let path = path.as_ref().to_path_buf();
        let file = std::fs::File::open(&path).map_err(|e| PackError::Io {
            path: path.clone(),
            source: e,
        })?;
        // SAFETY: pack/idx files are immutable in practice.
        let mmap = unsafe { Mmap::map(&file) }.map_err(|e| PackError::Io {
            path: path.clone(),
            source: e,
        })?;

        let raw_len = hash_kind.raw_len();
        let trailer_total = raw_len * 2;
        let header_len = 4 + 4 + FANOUT_BYTES;
        if mmap.len() < header_len + trailer_total {
            return Err(PackError::MalformedIdx("idx shorter than header+trailer"));
        }

        if mmap[0..4] != IDX_V2_MAGIC {
            let mut s = [0u8; 4];
            s.copy_from_slice(&mmap[0..4]);
            return Err(PackError::BadIdxSignature(s));
        }
        let version = read_u32_be(&mmap, 4);
        if version != 2 {
            return Err(PackError::UnsupportedIdxVersion(version));
        }

        let object_count = read_u32_be(&mmap, FANOUT_OFF + (255 * 4));

        // Validate fanout monotonicity and that fanout[255] == count.
        {
            let fanout = &mmap[FANOUT_OFF..FANOUT_OFF + FANOUT_BYTES];
            let mut prev: u32 = 0;
            for i in 0..256 {
                let v = u32::from_be_bytes([
                    fanout[i * 4],
                    fanout[i * 4 + 1],
                    fanout[i * 4 + 2],
                    fanout[i * 4 + 3],
                ]);
                if v < prev {
                    return Err(PackError::MalformedIdx("non-monotonic fanout"));
                }
                prev = v;
            }
            if prev != object_count {
                return Err(PackError::MalformedIdx("fanout[255] != object count"));
            }
        }

        let n = object_count as usize;
        let oid_table_off = FANOUT_OFF + FANOUT_BYTES;
        let crc_table_off = oid_table_off + n * raw_len;
        let offset_table_off = crc_table_off + n * 4;
        let small_offsets_end = offset_table_off + n * 4;

        let trailer_start = mmap.len() - trailer_total;
        if small_offsets_end > trailer_start {
            return Err(PackError::MalformedIdx("idx shorter than primary tables"));
        }

        // The 64-bit table sits between the small-offset table and the trailer.
        let large_offset_table_off = small_offsets_end;
        let large_table_bytes = trailer_start - large_offset_table_off;
        // `is_multiple_of` is MSRV 1.87; spell it manually until we move past 1.85.
        #[allow(clippy::manual_is_multiple_of)]
        if large_table_bytes % 8 != 0 {
            return Err(PackError::MalformedIdx(
                "64-bit offset table size not a multiple of 8",
            ));
        }

        Ok(Self {
            mmap,
            hash_kind,
            object_count,
            fanout_off: FANOUT_OFF,
            oid_table_off,
            crc_table_off,
            offset_table_off,
            large_offset_table_off,
            path,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn object_count(&self) -> u32 {
        self.object_count
    }

    pub fn hash_kind(&self) -> HashKind {
        self.hash_kind
    }

    /// Look up an oid in the idx; returns the pack file offset on hit.
    pub fn lookup(&self, oid: &ObjectId) -> Option<u64> {
        if oid.kind() != self.hash_kind {
            return None;
        }
        let idx = self.find_index(oid)?;
        Some(self.offset_at_index(idx))
    }

    /// CRC32 of the corresponding pack entry's compressed bytes.
    pub fn crc32_at_index(&self, idx: usize) -> Option<u32> {
        if idx >= self.object_count as usize {
            return None;
        }
        let off = self.crc_table_off + idx * 4;
        Some(read_u32_be(&self.mmap, off))
    }

    /// Iterate (oid, pack_offset) pairs in idx order (sorted by oid).
    pub fn iter(&self) -> IdxIter<'_> {
        IdxIter {
            idx: self,
            position: 0,
        }
    }

    /// Find all oids whose hex prefix matches; for `resolve_prefix` integration.
    pub fn resolve_prefix(&self, hex_prefix: &str) -> Vec<ObjectId> {
        let lower = hex_prefix.to_lowercase();
        if lower.is_empty() || lower.len() > self.hash_kind.hex_len() {
            return Vec::new();
        }
        if !lower.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Vec::new();
        }
        // Decode an even-length raw prefix, plus an optional half-byte tail.
        let even_len = lower.len() & !1;
        let raw_prefix = match hex::decode(&lower[..even_len]) {
            Ok(v) => v,
            Err(_) => return Vec::new(),
        };
        let half_byte = if lower.len() & 1 == 1 {
            // Single trailing nibble that must equal the high nibble of the next byte.
            let nibble = lower.as_bytes()[lower.len() - 1];
            let n = match nibble {
                b'0'..=b'9' => nibble - b'0',
                b'a'..=b'f' => nibble - b'a' + 10,
                _ => return Vec::new(),
            };
            Some(n)
        } else {
            None
        };

        // Use the fanout to narrow the candidate range by first byte.
        let first_byte = if !raw_prefix.is_empty() {
            raw_prefix[0]
        } else if let Some(h) = half_byte {
            h << 4
        } else {
            return Vec::new();
        };
        let lo = if first_byte == 0 {
            0
        } else {
            self.fanout(first_byte - 1) as usize
        };
        let hi = self.fanout(first_byte) as usize;

        let raw_len = self.hash_kind.raw_len();
        let mut matches = Vec::new();
        for i in lo..hi {
            let oid_bytes = &self.mmap[self.oid_table_off + i * raw_len..][..raw_len];
            if !oid_bytes.starts_with(&raw_prefix) {
                continue;
            }
            if let Some(nib) = half_byte {
                let next = oid_bytes[raw_prefix.len()];
                if (next >> 4) != nib {
                    continue;
                }
            }
            if let Ok(oid) = ObjectId::from_bytes(self.hash_kind, oid_bytes) {
                matches.push(oid);
            }
        }
        matches
    }

    /// Validate the trailing idx hash.
    pub fn verify_checksum(&self) -> Result<(), PackError> {
        let raw_len = self.hash_kind.raw_len();
        let body_end = self.mmap.len() - raw_len;
        let computed = hash_all(self.hash_kind, &self.mmap[..body_end]);
        let stored = &self.mmap[body_end..];
        if computed.as_bytes() != stored {
            return Err(PackError::ChecksumMismatch);
        }
        Ok(())
    }

    // ---- internals --------------------------------------------------------

    fn fanout(&self, i: u8) -> u32 {
        read_u32_be(&self.mmap, self.fanout_off + (i as usize) * 4)
    }

    /// Binary-search the OID table for `oid`, narrowed by the fanout.
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
            let mid_oid = &self.mmap[self.oid_table_off + mid * raw_len..][..raw_len];
            match mid_oid.cmp(bytes) {
                std::cmp::Ordering::Less => left = mid + 1,
                std::cmp::Ordering::Greater => right = mid,
                std::cmp::Ordering::Equal => return Some(mid),
            }
        }
        None
    }

    fn oid_at_index(&self, idx: usize) -> ObjectId {
        let raw_len = self.hash_kind.raw_len();
        let bytes = &self.mmap[self.oid_table_off + idx * raw_len..][..raw_len];
        ObjectId::from_bytes(self.hash_kind, bytes)
            .expect("idx oid bytes always have correct length")
    }

    fn offset_at_index(&self, idx: usize) -> u64 {
        let raw = read_u32_be(&self.mmap, self.offset_table_off + idx * 4);
        if raw & 0x8000_0000 == 0 {
            raw as u64
        } else {
            let large_idx = (raw & 0x7fff_ffff) as usize;
            // Each entry in the large table is 8 bytes BE.
            let off = self.large_offset_table_off + large_idx * 8;
            let mut buf = [0u8; 8];
            buf.copy_from_slice(&self.mmap[off..off + 8]);
            u64::from_be_bytes(buf)
        }
    }
}

pub struct IdxIter<'a> {
    idx: &'a IdxFile,
    position: usize,
}

impl<'a> Iterator for IdxIter<'a> {
    type Item = (ObjectId, u64);

    fn next(&mut self) -> Option<Self::Item> {
        if self.position >= self.idx.object_count as usize {
            return None;
        }
        let oid = self.idx.oid_at_index(self.position);
        let off = self.idx.offset_at_index(self.position);
        self.position += 1;
        Some((oid, off))
    }
}

fn read_u32_be(buf: &[u8], at: usize) -> u32 {
    u32::from_be_bytes([buf[at], buf[at + 1], buf[at + 2], buf[at + 3]])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    use super::super::file::tests::make_test_repo;
    use super::super::file::{PackEntryKind, PackFile};

    #[test]
    fn fanout_consistency() {
        let Some((_dir, _pack, idx_path)) = make_test_repo() else {
            eprintln!("skipping: no system git");
            return;
        };
        let idx = IdxFile::open(&idx_path, HashKind::Sha1).expect("open idx");
        // fanout[255] == count
        assert_eq!(idx.fanout(255), idx.object_count());
        // For each oid, fanout[first_byte - 1] <= idx < fanout[first_byte].
        for (i, (oid, _off)) in idx.iter().enumerate() {
            let first = oid.as_bytes()[0];
            let lower = if first == 0 { 0 } else { idx.fanout(first - 1) };
            let upper = idx.fanout(first);
            assert!(
                (lower as usize) <= i && i < (upper as usize),
                "fanout violated at index {i} for first_byte {first:#x}"
            );
        }
    }

    #[test]
    fn idx_lookup_round_trip() {
        let Some((dir, pack_path, idx_path)) = make_test_repo() else {
            eprintln!("skipping: no system git");
            return;
        };
        let idx = IdxFile::open(&idx_path, HashKind::Sha1).expect("open idx");
        let pf = PackFile::open(&pack_path, HashKind::Sha1).expect("open pack");

        // Use HEAD's commit oid: must be present in the pack.
        let out = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(dir.path())
            .output()
            .expect("rev-parse");
        assert!(out.status.success());
        let head_hex = String::from_utf8(out.stdout).unwrap().trim().to_string();
        let head_oid = ObjectId::parse_hex(HashKind::Sha1, &head_hex).expect("parse oid");
        let off = idx.lookup(&head_oid).expect("HEAD must be in pack");
        let entry = pf.read_entry_at(off).expect("read at offset");
        match entry.kind {
            PackEntryKind::Direct(crate::object::ObjectKind::Commit) => {}
            other => panic!("expected commit at HEAD offset, got {other:?}"),
        }
    }

    #[test]
    fn idx_iter_matches_verify_pack() {
        let Some((dir, pack_path, idx_path)) = make_test_repo() else {
            eprintln!("skipping: no system git");
            return;
        };
        let idx = IdxFile::open(&idx_path, HashKind::Sha1).expect("open idx");
        let mut from_idx: Vec<String> = idx.iter().map(|(o, _)| o.to_string()).collect();
        from_idx.sort();

        let out = Command::new("git")
            .args(["verify-pack", "-v"])
            .arg(&pack_path)
            .current_dir(dir.path())
            .output()
            .expect("verify-pack");
        assert!(out.status.success());
        let stdout = String::from_utf8_lossy(&out.stdout);
        let mut from_git: Vec<String> = stdout
            .lines()
            .filter_map(|l| {
                if l.len() >= 40 && l.as_bytes()[..40].iter().all(|b| b.is_ascii_hexdigit()) {
                    Some(l[..40].to_string())
                } else {
                    None
                }
            })
            .collect();
        from_git.sort();
        from_git.dedup();

        assert_eq!(from_idx, from_git);
    }

    #[test]
    fn idx_verify_checksum_passes_then_fails_after_tamper() {
        let Some((dir, _pack, idx_path)) = make_test_repo() else {
            eprintln!("skipping: no system git");
            return;
        };
        let idx = IdxFile::open(&idx_path, HashKind::Sha1).expect("open idx");
        idx.verify_checksum().expect("idx checksum should be valid");

        // Tamper one byte inside the OID table.
        let bytes = std::fs::read(&idx_path).unwrap();
        let mut tampered = bytes.clone();
        // Flip a byte at the start of the oid table — well past the header,
        // before the trailer.
        let flip_at = FANOUT_OFF + FANOUT_BYTES + 4;
        if flip_at < tampered.len() - 40 {
            tampered[flip_at] ^= 0x55;
        }
        let tpath = dir.path().join("tampered.idx");
        std::fs::write(&tpath, &tampered).unwrap();
        let idx2 = IdxFile::open(&tpath, HashKind::Sha1).expect("opens header still");
        assert!(matches!(
            idx2.verify_checksum(),
            Err(PackError::ChecksumMismatch)
        ));
    }

    #[test]
    fn idx_crc_and_offsets_present() {
        let Some((_dir, _pack, idx_path)) = make_test_repo() else {
            eprintln!("skipping: no system git");
            return;
        };
        let idx = IdxFile::open(&idx_path, HashKind::Sha1).expect("open idx");
        // Each entry should expose a crc32; offsets should be < 2^31 for
        // a small test pack (no large-offset table needed).
        for (i, (_oid, off)) in idx.iter().enumerate() {
            assert!(idx.crc32_at_index(i).is_some());
            assert!(off < (1u64 << 31));
        }
        // out-of-range crc returns None
        assert!(idx.crc32_at_index(idx.object_count() as usize).is_none());
    }

    #[test]
    fn idx_resolve_prefix_finds_known_oid() {
        let Some((_dir, _pack, idx_path)) = make_test_repo() else {
            eprintln!("skipping: no system git");
            return;
        };
        let idx = IdxFile::open(&idx_path, HashKind::Sha1).expect("open idx");
        let (oid, _) = idx.iter().next().expect("at least one object");
        let hex = oid.to_string();
        // 7-char prefix should at minimum hit our oid (maybe others too).
        let matches = idx.resolve_prefix(&hex[..7]);
        assert!(matches.contains(&oid));
        // Empty / invalid prefix gives empty results.
        assert!(idx.resolve_prefix("").is_empty());
        assert!(idx.resolve_prefix("xyz").is_empty());
    }

    #[test]
    fn idx_lookup_miss_returns_none() {
        let Some((_dir, _pack, idx_path)) = make_test_repo() else {
            eprintln!("skipping: no system git");
            return;
        };
        let idx = IdxFile::open(&idx_path, HashKind::Sha1).expect("open idx");
        let bogus = ObjectId::parse_hex(HashKind::Sha1, &"a".repeat(40)).unwrap();
        // Astronomically unlikely the test pack hashes to all-a's.
        assert!(idx.lookup(&bogus).is_none());
    }

    #[test]
    fn rejects_bad_idx_signature() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("not-an-idx.idx");
        // Write enough bytes to pass length check but with bogus magic.
        let bytes = vec![0u8; 8 + FANOUT_BYTES + 40];
        std::fs::write(&path, &bytes).unwrap();
        match IdxFile::open(&path, HashKind::Sha1) {
            Err(PackError::BadIdxSignature(_)) => {}
            Ok(_) => panic!("expected BadIdxSignature, got Ok"),
            Err(other) => panic!("expected BadIdxSignature, got {other:?}"),
        }
    }
}
