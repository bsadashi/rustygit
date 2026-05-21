//! `.pack` file reader.
//!
//! A pack file is laid out as:
//!   - 12-byte header: `PACK` + u32 BE version + u32 BE object count.
//!   - N entries, each: variable-length type+size header, optional ofs/ref
//!     delta header, then a zlib stream of either the object body (non-delta)
//!     or the delta-instruction stream.
//!   - Trailing hash (sha1 = 20, sha256 = 32 bytes) over all preceding bytes.
//!
//! We mmap the whole file and let everything borrow from the mapping. The
//! sole concession to the unsafe-ness of mmap is the call to `Mmap::map`
//! itself; thereafter all reads go through bounds-checked slicing.

use std::path::{Path, PathBuf};

use flate2::{Decompress, FlushDecompress, Status};
use memmap2::Mmap;

use crate::hash::{hash_all, HashKind, ObjectId};
use crate::object::ObjectKind;

use super::PackError;

const PACK_HEADER_LEN: usize = 12;
const PACK_SIGNATURE: &[u8; 4] = b"PACK";

#[derive(Debug, Clone)]
pub enum PackEntryKind {
    Direct(ObjectKind),
    /// Offset is the ABSOLUTE offset in the same packfile of the base entry.
    OfsDelta {
        base_offset: u64,
    },
    /// Base oid resides in some object store (this packfile or another).
    RefDelta {
        base_oid: ObjectId,
    },
}

#[derive(Debug)]
pub struct RawPackEntry {
    /// Offset of this entry's first byte (the type+size header) in the pack.
    pub offset: u64,
    /// What kind of entry this is.
    pub kind: PackEntryKind,
    /// The "size" field from the entry header. For direct objects this is the
    /// uncompressed body size; for deltas it's the size of the delta-instruction
    /// stream (i.e. inflate output size).
    pub declared_size: u64,
    /// Decompressed (zlib-inflated) payload. For direct objects this is the
    /// object body; for deltas it's the raw delta-instruction stream.
    pub data: Vec<u8>,
    /// Total compressed bytes consumed by this entry (header + zlib stream).
    /// Useful for iteration.
    pub size_in_pack: u64,
}

pub struct PackFile {
    mmap: Mmap,
    object_count: u32,
    hash_kind: HashKind,
    path: PathBuf,
}

impl PackFile {
    /// Open and validate the pack header. Does NOT verify the trailer
    /// checksum (that's expensive; expose `verify_checksum()` separately).
    pub fn open(path: impl AsRef<Path>, hash_kind: HashKind) -> Result<Self, PackError> {
        let path = path.as_ref().to_path_buf();
        let file = std::fs::File::open(&path).map_err(|e| PackError::Io {
            path: path.clone(),
            source: e,
        })?;
        // SAFETY: we do not mutate the file via this mapping, and pack files
        // in `objects/pack/` are immutable in practice (atomic rename on write).
        let mmap = unsafe { Mmap::map(&file) }.map_err(|e| PackError::Io {
            path: path.clone(),
            source: e,
        })?;

        // Header: 12 bytes minimum, plus the trailing hash.
        let trailer_len = hash_kind.raw_len();
        if mmap.len() < PACK_HEADER_LEN + trailer_len {
            return Err(PackError::Malformed("file shorter than header+trailer"));
        }

        let mut sig = [0u8; 4];
        sig.copy_from_slice(&mmap[0..4]);
        if &sig != PACK_SIGNATURE {
            return Err(PackError::BadPackSignature(sig));
        }
        let version = read_u32_be(&mmap, 4);
        if version != 2 && version != 3 {
            return Err(PackError::UnsupportedPackVersion(version));
        }
        let object_count = read_u32_be(&mmap, 8);

        Ok(Self {
            mmap,
            object_count,
            hash_kind,
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

    /// Read and decompress the entry at `offset`. The offset is the byte
    /// offset of the entry header in the pack (i.e. what idx files store).
    pub fn read_entry_at(&self, offset: u64) -> Result<RawPackEntry, PackError> {
        let off_usize = offset as usize;
        let trailer_start = self.mmap.len() - self.hash_kind.raw_len();
        if off_usize >= trailer_start {
            return Err(PackError::Malformed(
                "entry offset is in or past the trailer",
            ));
        }

        // Parse the type+size header.
        let (raw_type, declared_size, hdr_len) = parse_type_size(&self.mmap[off_usize..])?;
        let mut cursor = off_usize + hdr_len;

        let kind = match raw_type {
            0 => return Err(PackError::Malformed("invalid object type 0")),
            1 => PackEntryKind::Direct(ObjectKind::Commit),
            2 => PackEntryKind::Direct(ObjectKind::Tree),
            3 => PackEntryKind::Direct(ObjectKind::Blob),
            4 => PackEntryKind::Direct(ObjectKind::Tag),
            5 => return Err(PackError::Malformed("reserved object type 5")),
            6 => {
                // OBJ_OFS_DELTA: variable-length backward offset, then zlib.
                let (neg_offset, n) = read_offset_varint(&self.mmap[cursor..])
                    .ok_or(PackError::Malformed("truncated ofs-delta offset"))?;
                cursor += n;
                if neg_offset == 0 || neg_offset > offset {
                    return Err(PackError::Malformed("ofs-delta offset escapes packfile"));
                }
                PackEntryKind::OfsDelta {
                    base_offset: offset - neg_offset,
                }
            }
            7 => {
                // OBJ_REF_DELTA: raw oid (sha1=20 / sha256=32), then zlib.
                let raw_len = self.hash_kind.raw_len();
                if cursor + raw_len > trailer_start {
                    return Err(PackError::Malformed("truncated ref-delta header"));
                }
                let oid =
                    ObjectId::from_bytes(self.hash_kind, &self.mmap[cursor..cursor + raw_len])?;
                cursor += raw_len;
                PackEntryKind::RefDelta { base_oid: oid }
            }
            _ => return Err(PackError::Malformed("unknown object type")),
        };

        // Inflate the zlib stream. We size the output buffer using the
        // declared size to avoid pathological growth on a malformed entry.
        let zlib_start = cursor;
        if zlib_start >= trailer_start {
            return Err(PackError::Malformed("entry zlib stream past EOF"));
        }
        let (data, consumed) =
            inflate_zlib(&self.mmap[zlib_start..trailer_start], declared_size, offset)?;
        let size_in_pack = (zlib_start - off_usize) as u64 + consumed;

        Ok(RawPackEntry {
            offset,
            kind,
            declared_size,
            data,
            size_in_pack,
        })
    }

    /// Iterate every entry in the pack in disk order.
    pub fn iter_entries(&self) -> EntryIter<'_> {
        EntryIter {
            pack: self,
            offset: PACK_HEADER_LEN as u64,
            remaining: self.object_count,
        }
    }

    /// Validate the trailing hash by re-computing over all preceding bytes.
    pub fn verify_checksum(&self) -> Result<(), PackError> {
        let trailer_len = self.hash_kind.raw_len();
        let body_end = self.mmap.len() - trailer_len;
        let computed = hash_all(self.hash_kind, &self.mmap[..body_end]);
        let stored = &self.mmap[body_end..];
        if computed.as_bytes() != stored {
            return Err(PackError::ChecksumMismatch);
        }
        Ok(())
    }
}

pub struct EntryIter<'a> {
    pack: &'a PackFile,
    offset: u64,
    remaining: u32,
}

impl<'a> Iterator for EntryIter<'a> {
    type Item = Result<RawPackEntry, PackError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 {
            return None;
        }
        let result = self.pack.read_entry_at(self.offset);
        match result {
            Ok(entry) => {
                self.offset += entry.size_in_pack;
                self.remaining -= 1;
                Some(Ok(entry))
            }
            Err(e) => {
                self.remaining = 0;
                Some(Err(e))
            }
        }
    }
}

// ---- helpers --------------------------------------------------------------

fn read_u32_be(buf: &[u8], at: usize) -> u32 {
    u32::from_be_bytes([buf[at], buf[at + 1], buf[at + 2], buf[at + 3]])
}

/// Parse the variable-length type+size header at the start of `buf`.
///
/// Layout: first byte's bits 4-6 are the type (3 bits), bits 0-3 are the LOW
/// 4 bits of the size; bit 7 is the "more bytes" flag. Subsequent bytes use
/// bits 0-6 as the next 7 size bits, with bit 7 again as continuation. The
/// 7-bit chunks are concatenated little-endian.
///
/// Returns `(type, size, bytes_consumed)`.
fn parse_type_size(buf: &[u8]) -> Result<(u8, u64, usize), PackError> {
    if buf.is_empty() {
        return Err(PackError::Malformed("truncated entry header"));
    }
    let first = buf[0];
    let raw_type = (first >> 4) & 0x07;
    let mut size: u64 = (first & 0x0f) as u64;
    let mut shift: u32 = 4;
    let mut idx = 1usize;
    let mut more = (first & 0x80) != 0;
    while more {
        if idx >= buf.len() {
            return Err(PackError::Malformed("truncated entry header"));
        }
        let b = buf[idx];
        // Guard against pathological shift overflow on a malformed entry.
        if shift >= 64 {
            return Err(PackError::Malformed("entry size overflow"));
        }
        size |= ((b & 0x7f) as u64) << shift;
        shift += 7;
        more = (b & 0x80) != 0;
        idx += 1;
    }
    Ok((raw_type, size, idx))
}

/// Read the variable-length OFS_DELTA backward offset. This is NOT the same as
/// the size encoding above — it's a "compact offset" form that adds 1 to every
/// non-final 7-bit chunk to recover the values the gap leaves unrepresentable.
///
/// Pseudocode (from git's source):
///   c = read_byte();
///   ofs = c & 0x7f;
///   while (c & 0x80) {
///       ofs += 1;
///       c = read_byte();
///       ofs = (ofs << 7) | (c & 0x7f);
///   }
///
/// Returns `(offset, bytes_consumed)` or `None` on truncation.
fn read_offset_varint(buf: &[u8]) -> Option<(u64, usize)> {
    if buf.is_empty() {
        return None;
    }
    let mut idx = 0usize;
    let mut c = buf[idx];
    idx += 1;
    let mut ofs: u64 = (c & 0x7f) as u64;
    while c & 0x80 != 0 {
        if idx >= buf.len() {
            return None;
        }
        ofs = ofs.checked_add(1)?;
        c = buf[idx];
        idx += 1;
        ofs = ofs.checked_shl(7)?;
        ofs |= (c & 0x7f) as u64;
    }
    Some((ofs, idx))
}

/// Inflate a zlib stream from `buf`, expecting roughly `expected_out` bytes
/// of output. Returns the decompressed bytes plus the number of input bytes
/// consumed (the size of the zlib stream).
fn inflate_zlib(
    buf: &[u8],
    expected_out: u64,
    entry_offset: u64,
) -> Result<(Vec<u8>, u64), PackError> {
    // For deltas, `expected_out` is small; for big blobs it can be GBs in
    // theory but in practice fits in memory. We allocate up to expected then
    // grow if zlib disagrees.
    let cap = expected_out.min(1 << 28) as usize; // cap initial alloc at 256MiB
    let mut out = Vec::with_capacity(cap);
    let mut decoder = Decompress::new(true);

    let mut in_pos: usize = 0;
    loop {
        // Grow output buffer if needed. We extend by either the remaining
        // expected size or 16KiB, whichever is larger.
        if out.len() == out.capacity() {
            let extra = (expected_out as usize)
                .saturating_sub(out.len())
                .max(16 * 1024);
            out.reserve(extra);
        }
        let before_in = decoder.total_in();
        let before_out = decoder.total_out();
        let status = decoder
            .decompress_vec(&buf[in_pos..], &mut out, FlushDecompress::None)
            .map_err(|e| PackError::Inflate {
                offset: entry_offset,
                source: std::io::Error::other(e.to_string()),
            })?;
        let consumed_in = (decoder.total_in() - before_in) as usize;
        in_pos += consumed_in;
        let _ = before_out; // total_out tracked via out.len()

        match status {
            Status::StreamEnd => break,
            Status::Ok | Status::BufError => {
                // BufError can mean "need more input" or "need more output".
                // If we made no input progress AND output buffer isn't full,
                // we hit the end of the input slice prematurely.
                if consumed_in == 0 && out.len() < out.capacity() {
                    return Err(PackError::Inflate {
                        offset: entry_offset,
                        source: std::io::Error::other("zlib stream truncated"),
                    });
                }
                continue;
            }
        }
    }

    Ok((out, decoder.total_in()))
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use std::process::Command;
    use tempfile::TempDir;

    /// Create a tempdir, run `git init` + a few commits + `git gc` to produce
    /// a real .pack/.idx pair. Returns `None` if system `git` isn't usable
    /// (so tests skip cleanly on CI without git).
    pub(in crate::pack) fn make_test_repo() -> Option<(TempDir, PathBuf, PathBuf)> {
        let git_ok = Command::new("git").arg("--version").output().ok()?;
        if !git_ok.status.success() {
            return None;
        }
        let dir = tempfile::tempdir().ok()?;
        let work = dir.path();
        let run = |args: &[&str]| {
            let status = Command::new("git")
                .args(args)
                .current_dir(work)
                .env("GIT_AUTHOR_NAME", "T")
                .env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "T")
                .env("GIT_COMMITTER_EMAIL", "t@t")
                .env("GIT_AUTHOR_DATE", "1700000000 +0000")
                .env("GIT_COMMITTER_DATE", "1700000000 +0000")
                .output()
                .expect("git ran");
            assert!(
                status.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&status.stderr)
            );
        };
        run(&["init", "-q", "-b", "main"]);
        // Produce enough content (and variation) to ensure deltas appear.
        for i in 0..40 {
            let mut content = String::new();
            for line in 0..(20 + i % 7) {
                content.push_str(&format!("file {i} line {line}\n"));
            }
            std::fs::write(work.join(format!("f{i:02}.txt")), content).unwrap();
        }
        run(&["add", "."]);
        run(&["commit", "-q", "-m", "initial"]);
        // Mutate a few files and commit again to give git more material.
        for i in 0..40 {
            std::fs::write(work.join(format!("f{i:02}.txt")), format!("v2-{i}\n")).unwrap();
        }
        run(&["add", "."]);
        run(&["commit", "-q", "-m", "second"]);
        run(&["gc", "-q", "--aggressive"]);
        let pack_dir = work.join(".git/objects/pack");
        let mut pack_path = None;
        let mut idx_path = None;
        for entry in std::fs::read_dir(&pack_dir).ok()? {
            let entry = entry.ok()?;
            let p = entry.path();
            match p.extension().and_then(|s| s.to_str()) {
                Some("pack") => pack_path = Some(p),
                Some("idx") => idx_path = Some(p),
                _ => {}
            }
        }
        Some((dir, pack_path?, idx_path?))
    }

    #[test]
    fn parse_type_size_min_byte() {
        // type=3 (blob), size=5, no continuation: 0b0011_0101.
        let buf = [0b0011_0101u8];
        let (t, s, n) = parse_type_size(&buf).unwrap();
        assert_eq!(t, 3);
        assert_eq!(s, 5);
        assert_eq!(n, 1);
    }

    #[test]
    fn parse_type_size_two_bytes() {
        // type=2 (tree), low4=0xc, high7=0x01 -> size = 0xc | (1 << 4) = 0x1c.
        let buf = [0b1010_1100u8, 0b0000_0001u8];
        let (t, s, n) = parse_type_size(&buf).unwrap();
        assert_eq!(t, 2);
        assert_eq!(s, 0x1c);
        assert_eq!(n, 2);
    }

    #[test]
    fn ofs_delta_offset_single_byte() {
        // 0x05 -> offset 5, no continuation.
        let buf = [0x05u8];
        assert_eq!(read_offset_varint(&buf), Some((5, 1)));
    }

    #[test]
    fn ofs_delta_offset_two_bytes() {
        // git's encoding: bytes (0xc1, 0x05) -> ofs starts at 0x41,
        // continuation: ofs = (0x41 + 1) << 7 | 0x05 = (0x42 << 7) | 5 = 8453.
        let buf = [0xc1u8, 0x05u8];
        assert_eq!(read_offset_varint(&buf), Some((8453, 2)));
    }

    #[test]
    fn ofs_delta_offset_truncated() {
        // Continuation bit set but no follow-up byte.
        let buf = [0x80u8];
        assert!(read_offset_varint(&buf).is_none());
    }

    #[test]
    fn pack_header_sanity() {
        let Some((dir, pack_path, _idx)) = make_test_repo() else {
            eprintln!("skipping: no system git");
            return;
        };
        let pf = PackFile::open(&pack_path, HashKind::Sha1).expect("open pack");
        assert!(pf.object_count() > 0);
        // Cross-check with `git verify-pack -v`.
        let out = Command::new("git")
            .args(["verify-pack", "-v"])
            .arg(&pack_path)
            .current_dir(dir.path())
            .output()
            .expect("verify-pack");
        assert!(out.status.success());
        let stdout = String::from_utf8_lossy(&out.stdout);
        // Lines that start with 40 hex chars are object lines.
        let count = stdout
            .lines()
            .filter(|l| l.len() >= 40 && l.as_bytes()[..40].iter().all(|b| b.is_ascii_hexdigit()))
            .count();
        assert_eq!(count as u32, pf.object_count());
        let _ = dir;
    }

    #[test]
    fn iter_every_entry() {
        let Some((dir, pack_path, idx_path)) = make_test_repo() else {
            eprintln!("skipping: no system git");
            return;
        };
        let pf = PackFile::open(&pack_path, HashKind::Sha1).expect("open pack");
        let idx = super::super::idx::IdxFile::open(&idx_path, HashKind::Sha1).expect("open idx");
        // Build the set of oids the idx claims are in this pack.
        let oids: std::collections::HashSet<ObjectId> = idx.iter().map(|(o, _)| o).collect();

        let mut seen = 0u32;
        for entry in pf.iter_entries() {
            let entry = entry.expect("entry parses");
            seen += 1;
            match &entry.kind {
                PackEntryKind::Direct(kind) => {
                    // Re-frame the object and verify the OID is present.
                    let mut framed =
                        format!("{} {}\0", kind.as_str(), entry.data.len()).into_bytes();
                    framed.extend_from_slice(&entry.data);
                    let oid = hash_all(HashKind::Sha1, &framed);
                    assert!(oids.contains(&oid), "direct-entry oid {} not in idx", oid);
                }
                PackEntryKind::OfsDelta { base_offset } => {
                    assert!(
                        *base_offset < entry.offset,
                        "ofs-delta base must precede the entry"
                    );
                }
                PackEntryKind::RefDelta { .. } => {
                    // Ref-delta bases may live elsewhere; we don't check here.
                }
            }
        }
        assert_eq!(seen, pf.object_count());
        let _ = dir;
    }

    #[test]
    fn verify_checksum_passes_then_fails_after_tamper() {
        let Some((dir, pack_path, _idx)) = make_test_repo() else {
            eprintln!("skipping: no system git");
            return;
        };
        let pf = PackFile::open(&pack_path, HashKind::Sha1).expect("open pack");
        pf.verify_checksum().expect("checksum should be valid");

        // Make a tampered copy.
        let bytes = std::fs::read(&pack_path).unwrap();
        let mut tampered = bytes.clone();
        // Flip a byte well inside the entry region (not header, not trailer).
        let flip_at = bytes.len() / 2;
        tampered[flip_at] ^= 0xff;
        let tampered_path = dir.path().join("tampered.pack");
        std::fs::write(&tampered_path, &tampered).unwrap();
        let pf2 = PackFile::open(&tampered_path, HashKind::Sha1).expect("opens header still");
        assert!(matches!(
            pf2.verify_checksum(),
            Err(PackError::ChecksumMismatch)
        ));
        let _ = dir;
    }

    #[test]
    fn rejects_bad_signature() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("not-a-pack.pack");
        // Write a 32-byte file with the wrong signature.
        let bytes = vec![0u8; 32];
        std::fs::write(&path, &bytes).unwrap();
        match PackFile::open(&path, HashKind::Sha1) {
            Err(PackError::BadPackSignature(_)) => {}
            Ok(_) => panic!("expected BadPackSignature, got Ok"),
            Err(other) => panic!("expected BadPackSignature, got {other:?}"),
        }
    }

    #[test]
    fn rejects_short_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("short.pack");
        std::fs::write(&path, b"PACK").unwrap();
        match PackFile::open(&path, HashKind::Sha1) {
            Err(PackError::Malformed(_)) => {}
            Ok(_) => panic!("expected Malformed, got Ok"),
            Err(other) => panic!("expected Malformed, got {other:?}"),
        }
    }
}
