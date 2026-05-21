//! Delta application for pack-encoded objects.
//!
//! Pack entries of kind OFS_DELTA / REF_DELTA store a *patch* against some
//! earlier object (the "base"). The patch begins with two varint sizes — the
//! expected source size and the produced target size — followed by a stream of
//! COPY / INSERT instructions. We replay those instructions to reconstruct the
//! target buffer.
//!
//! Reference implementation: `git/patch-delta.c` and `get_delta_hdr_size`
//! from `git/delta.h`. The varint is 7-bit little-endian groups with the
//! continuation bit in the high bit; COPY commands have the high bit set, with
//! the low 4 bits choosing which offset bytes follow and bits 4-6 choosing
//! which size bytes follow; INSERT commands have the high bit clear and the
//! low 7 bits encode the literal byte count to copy from the delta stream.

use thiserror::Error;

#[derive(Error, Debug, PartialEq, Eq)]
pub enum DeltaError {
    #[error("malformed delta: {0}")]
    Malformed(&'static str),
    #[error("delta size mismatch: declared {declared}, produced {produced}")]
    SizeMismatch { declared: u64, produced: u64 },
    #[error("delta source size mismatch: declared {declared}, base is {actual}")]
    SourceMismatch { declared: u64, actual: u64 },
}

/// Decode one of the two header varints (source size or target size). Returns
/// the decoded value and the number of bytes consumed.
fn read_header_varint(data: &[u8]) -> Result<(u64, usize), DeltaError> {
    let mut size: u64 = 0;
    let mut shift: u32 = 0;
    let mut i = 0;
    loop {
        if i >= data.len() {
            return Err(DeltaError::Malformed("truncated varint"));
        }
        let byte = data[i];
        i += 1;
        // Guard against overflow on a pathologically long varint.
        let chunk = (byte & 0x7f) as u64;
        let shifted = chunk
            .checked_shl(shift)
            .ok_or(DeltaError::Malformed("varint overflow"))?;
        size = size
            .checked_add(shifted)
            .ok_or(DeltaError::Malformed("varint overflow"))?;
        if byte & 0x80 == 0 {
            return Ok((size, i));
        }
        shift = shift
            .checked_add(7)
            .ok_or(DeltaError::Malformed("varint overflow"))?;
        if shift >= 64 {
            return Err(DeltaError::Malformed("varint too long"));
        }
    }
}

/// Apply a delta against a base buffer.
///
/// `delta_instrs` is the *uncompressed* delta payload — caller has already
/// inflated it. The first bytes encode `(source_size, target_size)`; the rest
/// is a sequence of COPY/INSERT instructions.
pub fn apply_delta(base: &[u8], delta_instrs: &[u8]) -> Result<Vec<u8>, DeltaError> {
    let mut cursor = 0usize;

    // Header: source size, then target size.
    let (src_size, used) = read_header_varint(&delta_instrs[cursor..])?;
    cursor += used;
    if src_size as usize != base.len() {
        return Err(DeltaError::SourceMismatch {
            declared: src_size,
            actual: base.len() as u64,
        });
    }

    let (tgt_size, used) = read_header_varint(&delta_instrs[cursor..])?;
    cursor += used;

    let mut out: Vec<u8> = Vec::with_capacity(tgt_size as usize);

    while cursor < delta_instrs.len() {
        let cmd = delta_instrs[cursor];
        cursor += 1;

        if cmd & 0x80 != 0 {
            // COPY from base.
            let mut cp_off: u32 = 0;
            let mut cp_size: u32 = 0;
            for (bit, shift) in [(0x01u8, 0u32), (0x02, 8), (0x04, 16), (0x08, 24)] {
                if cmd & bit != 0 {
                    if cursor >= delta_instrs.len() {
                        return Err(DeltaError::Malformed("truncated copy offset"));
                    }
                    cp_off |= (delta_instrs[cursor] as u32) << shift;
                    cursor += 1;
                }
            }
            for (bit, shift) in [(0x10u8, 0u32), (0x20, 8), (0x40, 16)] {
                if cmd & bit != 0 {
                    if cursor >= delta_instrs.len() {
                        return Err(DeltaError::Malformed("truncated copy size"));
                    }
                    cp_size |= (delta_instrs[cursor] as u32) << shift;
                    cursor += 1;
                }
            }
            if cp_size == 0 {
                cp_size = 0x10000;
            }
            let cp_off = cp_off as usize;
            let cp_size = cp_size as usize;
            let end = cp_off
                .checked_add(cp_size)
                .ok_or(DeltaError::Malformed("copy range overflow"))?;
            if end > base.len() {
                return Err(DeltaError::Malformed("copy range exceeds base"));
            }
            out.extend_from_slice(&base[cp_off..end]);
        } else if cmd != 0 {
            // INSERT literal of `cmd` bytes from the delta stream.
            let n = cmd as usize;
            let end = cursor
                .checked_add(n)
                .ok_or(DeltaError::Malformed("insert range overflow"))?;
            if end > delta_instrs.len() {
                return Err(DeltaError::Malformed("truncated insert literal"));
            }
            out.extend_from_slice(&delta_instrs[cursor..end]);
            cursor = end;
        } else {
            return Err(DeltaError::Malformed("reserved opcode 0x00"));
        }
    }

    if out.len() as u64 != tgt_size {
        return Err(DeltaError::SizeMismatch {
            declared: tgt_size,
            produced: out.len() as u64,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: encode a value as a 7-bit varint as used in the delta header.
    fn encode_varint(mut v: u64) -> Vec<u8> {
        let mut out = Vec::new();
        loop {
            let byte = (v & 0x7f) as u8;
            v >>= 7;
            if v == 0 {
                out.push(byte);
                break;
            }
            out.push(byte | 0x80);
        }
        out
    }

    #[test]
    fn empty_delta_over_empty_base() {
        // Source size 0, target size 0, no instructions.
        let delta = [0x00u8, 0x00];
        let out = apply_delta(&[], &delta).unwrap();
        assert_eq!(out, Vec::<u8>::new());
    }

    #[test]
    fn pure_insert() {
        // Base is "" (size 0), target is "abc" (size 3).
        // INSERT cmd = 3, then b"abc".
        let delta = [0x00u8, 0x03, 0x03, b'a', b'b', b'c'];
        let out = apply_delta(&[], &delta).unwrap();
        assert_eq!(out, b"abc");
    }

    #[test]
    fn pure_copy_entire_base() {
        // Base = "hello world" (size 11), target = same (size 11).
        // COPY: high bit set, offset 0 with size flags 0x10 (size0 byte = 11).
        // cmd byte = 0x80 | 0x10 = 0x90, then size byte = 11.
        // No offset bits → offset is 0.
        let base = b"hello world";
        let mut delta = Vec::new();
        delta.extend_from_slice(&encode_varint(11));
        delta.extend_from_slice(&encode_varint(11));
        delta.push(0x90); // COPY, no offset bytes, size0 only
        delta.push(11); // size byte
        let out = apply_delta(base, &delta).unwrap();
        assert_eq!(out, base);
    }

    #[test]
    fn copy_with_offset_and_size() {
        // Base = "abcdefghij", copy "cdef" (offset 2, size 4) — single bytes for both.
        let base = b"abcdefghij";
        let mut delta = Vec::new();
        delta.extend_from_slice(&encode_varint(base.len() as u64));
        delta.extend_from_slice(&encode_varint(4));
        // cmd: copy, offset0 + size0 → 0x80 | 0x01 | 0x10 = 0x91
        delta.push(0x91);
        delta.push(2); // offset0
        delta.push(4); // size0
        let out = apply_delta(base, &delta).unwrap();
        assert_eq!(out, b"cdef");
    }

    #[test]
    fn copy_size_zero_means_64k() {
        // 64 KiB base, copy entire base with cmd that has zero size flags.
        let base = vec![0x42u8; 0x10000];
        let mut delta = Vec::new();
        delta.extend_from_slice(&encode_varint(0x10000));
        delta.extend_from_slice(&encode_varint(0x10000));
        // cmd: copy, no flags → 0x80. size resolved as 0 → 0x10000.
        delta.push(0x80);
        let out = apply_delta(&base, &delta).unwrap();
        assert_eq!(out, base);
    }

    #[test]
    fn mixed_copy_insert() {
        // Base = "the quick brown fox"; target = "the lazy fox".
        // We want to: COPY "the " (offset 0, size 4), INSERT "lazy ", COPY "fox" (offset 16, size 3).
        let base = b"the quick brown fox";
        let mut delta = Vec::new();
        delta.extend_from_slice(&encode_varint(base.len() as u64));
        delta.extend_from_slice(&encode_varint(12)); // "the lazy fox"
                                                     // COPY [0..4]
        delta.push(0x91); // copy with offset0 + size0
        delta.push(0); // offset0
        delta.push(4); // size0
                       // INSERT "lazy "
        delta.push(5);
        delta.extend_from_slice(b"lazy ");
        // COPY [16..19]
        delta.push(0x91);
        delta.push(16);
        delta.push(3);
        let out = apply_delta(base, &delta).unwrap();
        assert_eq!(out, b"the lazy fox");
    }

    #[test]
    fn large_offset_and_size() {
        // Base bigger than 256 bytes — exercise multi-byte offset/size.
        let base: Vec<u8> = (0..1000u32).map(|i| i as u8).collect();
        // Copy bytes [300..900] — offset=300, size=600.
        // offset 300 = 0x012c → low byte 0x2c, high byte 0x01 → flags 0x01|0x02
        // size 600 = 0x0258 → low byte 0x58, high byte 0x02 → flags 0x10|0x20
        let mut delta = Vec::new();
        delta.extend_from_slice(&encode_varint(base.len() as u64));
        delta.extend_from_slice(&encode_varint(600));
        delta.push(0x80 | 0x01 | 0x02 | 0x10 | 0x20);
        delta.push(0x2c); // offset0
        delta.push(0x01); // offset1
        delta.push(0x58); // size0
        delta.push(0x02); // size1
        let out = apply_delta(&base, &delta).unwrap();
        assert_eq!(out, &base[300..900]);
    }

    #[test]
    fn target_size_mismatch_detected() {
        // Declare target size 5 but instructions only produce 3 bytes.
        let delta = [0x00u8, 0x05, 0x03, b'x', b'y', b'z'];
        let err = apply_delta(&[], &delta).unwrap_err();
        match err {
            DeltaError::SizeMismatch { declared, produced } => {
                assert_eq!(declared, 5);
                assert_eq!(produced, 3);
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn source_size_mismatch_detected() {
        // Declared source size 100 but base is empty.
        let delta = [0x64u8, 0x00];
        let err = apply_delta(&[], &delta).unwrap_err();
        match err {
            DeltaError::SourceMismatch { declared, actual } => {
                assert_eq!(declared, 100);
                assert_eq!(actual, 0);
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn reserved_opcode_zero_rejected() {
        // Header (sizes 0, 1) then a 0x00 opcode.
        let delta = [0x00u8, 0x01, 0x00];
        let err = apply_delta(&[], &delta).unwrap_err();
        assert!(matches!(err, DeltaError::Malformed(_)));
    }

    #[test]
    fn copy_past_base_rejected() {
        // Base of 5 bytes, attempt to copy 100 bytes from offset 0.
        let base = b"hello";
        let mut delta = Vec::new();
        delta.extend_from_slice(&encode_varint(5));
        delta.extend_from_slice(&encode_varint(100));
        delta.push(0x90); // copy, size0 only
        delta.push(100);
        let err = apply_delta(base, &delta).unwrap_err();
        assert!(matches!(err, DeltaError::Malformed(_)));
    }

    #[test]
    fn truncated_insert_rejected() {
        // INSERT 5 bytes but only 2 follow.
        let delta = [0x00u8, 0x05, 0x05, b'a', b'b'];
        let err = apply_delta(&[], &delta).unwrap_err();
        assert!(matches!(err, DeltaError::Malformed(_)));
    }

    #[test]
    fn varint_multibyte_decode() {
        // Build a delta with a multi-byte source size: 200 = 0xc8.
        // 200 in 7-bit groups: low 7 = 0x48, high = 0x01 → bytes [0xc8, 0x01].
        let base = vec![0u8; 200];
        let delta: Vec<u8> = vec![
            0xc8, 0x01, // source size = 200
            0x00, // target size = 0
        ];
        let out = apply_delta(&base, &delta).unwrap();
        assert_eq!(out.len(), 0);
    }

    #[test]
    fn truncated_varint_rejected() {
        // High bit set on every byte, no terminator.
        let delta = [0x80u8, 0x80, 0x80];
        let err = apply_delta(&[], &delta).unwrap_err();
        assert!(matches!(err, DeltaError::Malformed(_)));
    }

    /// Round-trip delta resolution against a real packfile produced by system
    /// git. Skips when git isn't on PATH.
    ///
    /// Strategy: build a small repo with `git gc --aggressive`, walk the pack
    /// looking for an OFS_DELTA entry, resolve it via our `apply_delta`, then
    /// compare to what `git cat-file -p <oid>` produces.
    #[test]
    fn round_trip_against_system_git() {
        use crate::hash::{hash_all, HashKind};
        use crate::pack::{IdxFile, PackEntryKind, PackFile};
        use std::process::Command;

        let git_ok = matches!(
            Command::new("git").arg("--version").output(),
            Ok(o) if o.status.success()
        );
        if !git_ok {
            eprintln!("skipping: no system git");
            return;
        }

        let dir = tempfile::tempdir().expect("tempdir");
        let work = dir.path();
        let run = |args: &[&str]| {
            let out = Command::new("git")
                .args(args)
                .current_dir(work)
                .env("GIT_AUTHOR_NAME", "T")
                .env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "T")
                .env("GIT_COMMITTER_EMAIL", "t@t")
                .output()
                .expect("git");
            assert!(
                out.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        run(&["init", "-q", "-b", "main"]);
        // First commit: long content. Subsequent commits append small tails so
        // most of each version is delta-able from the previous (the "shared
        // prefix" pattern git's pack heuristic likes).
        let mut base = String::new();
        for i in 0..2000u32 {
            base.push_str(&format!("line {i}\n"));
        }
        std::fs::write(work.join("a.txt"), &base).unwrap();
        run(&["add", "a.txt"]);
        run(&["commit", "-q", "-m", "base"]);
        for v in 1..8u32 {
            let mut content = base.clone();
            content.push_str(&format!("tail {v}\n"));
            std::fs::write(work.join("a.txt"), content).unwrap();
            run(&["add", "a.txt"]);
            run(&["commit", "-q", "-m", &format!("v{v}")]);
        }
        run(&["gc", "-q", "--aggressive"]);

        let pack_dir = work.join(".git/objects/pack");
        let mut pack_path = None;
        let mut idx_path = None;
        for ent in std::fs::read_dir(&pack_dir).unwrap().flatten() {
            let p = ent.path();
            if p.extension().map(|e| e == "pack").unwrap_or(false) {
                pack_path = Some(p);
            } else if p.extension().map(|e| e == "idx").unwrap_or(false) {
                idx_path = Some(p);
            }
        }
        let pack_path = pack_path.expect("pack present");
        let idx_path = idx_path.expect("idx present");

        let pack = PackFile::open(&pack_path, HashKind::Sha1).expect("open pack");
        let idx = IdxFile::open(&idx_path, HashKind::Sha1).expect("open idx");

        // Find an OFS_DELTA entry and walk the chain to a base.
        let mut found = false;
        for (oid, off) in idx.iter() {
            let entry = pack.read_entry_at(off).expect("read entry");
            let base_offset = match entry.kind {
                PackEntryKind::OfsDelta { base_offset } => base_offset,
                _ => continue,
            };

            // Walk chain to a non-delta base.
            let mut bases: Vec<Vec<u8>> = vec![entry.data.clone()];
            let mut cur_off = base_offset;
            let base_kind = loop {
                let e = pack.read_entry_at(cur_off).expect("read base");
                match e.kind {
                    PackEntryKind::Direct(k) => break (k, e.data),
                    PackEntryKind::OfsDelta { base_offset } => {
                        bases.push(e.data);
                        cur_off = base_offset;
                    }
                    PackEntryKind::RefDelta { .. } => panic!("did not expect ref delta"),
                }
            };
            let (kind, mut buf) = base_kind;
            // Apply patches from base towards leaf — bases is in leaf-first
            // order, so iterate in reverse.
            for patch in bases.iter().rev() {
                buf = apply_delta(&buf, patch).expect("apply_delta");
            }

            // Verify the OID matches: hash should equal `kind size\0buf`.
            let mut framed = format!("{} {}\0", kind.as_str(), buf.len()).into_bytes();
            framed.extend_from_slice(&buf);
            let recomputed = hash_all(HashKind::Sha1, &framed);
            assert_eq!(recomputed, oid, "delta-resolved OID mismatch");

            // Cross-check against `git cat-file -p <oid>` for blob/tree.
            // We use cat-file with the type so it works for any kind.
            let cat = Command::new("git")
                .args(["cat-file", kind.as_str(), &oid.to_string()])
                .current_dir(work)
                .output()
                .expect("git cat-file");
            assert!(cat.status.success());
            assert_eq!(buf, cat.stdout, "byte-for-byte mismatch with git for {oid}");

            found = true;
            break;
        }
        assert!(found, "expected at least one OFS_DELTA in test pack");
    }
}
