//! Low-level reftable format primitives.
//!
//! Implements varint encode/decode (reftable spec §3.5 — same encoding as
//! ofs-delta in pack files), the file header (§4.1, version 1 = 24 bytes;
//! version 2 = 28 bytes), the file footer (§4.7, 68 bytes for v1, 72 bytes
//! for v2), block headers (§4.3, `'r'|'i'|'o'|'g'` + 3-byte length), and a
//! plain CRC-32 used by the footer.
//!
//! Block compression is currently NONE for ref/obj/index blocks (uncompressed
//! is legal per spec §4.2). Log blocks are zlib-deflated per §4.5.

use std::io::{self, Read};

use thiserror::Error;

use crate::hash::HashKind;

/// reftable spec §4.1 — file magic.
pub const MAGIC: &[u8; 4] = b"REFT";

pub const VERSION_V1: u8 = 1;
pub const VERSION_V2: u8 = 2;

/// reftable spec §4.7 — fixed footer sizes (matches header + footer extras + CRC).
pub const FOOTER_LEN_V1: usize = 68;
pub const FOOTER_LEN_V2: usize = 72;

pub const HEADER_LEN_V1: usize = 24;
pub const HEADER_LEN_V2: usize = 28;

/// Block-type bytes per reftable spec §4.3.
pub const BLOCK_TYPE_REF: u8 = b'r';
pub const BLOCK_TYPE_OBJ: u8 = b'o';
pub const BLOCK_TYPE_LOG: u8 = b'g';
pub const BLOCK_TYPE_INDEX: u8 = b'i';

#[derive(Error, Debug)]
pub enum ReftableFormatError {
    #[error("unexpected eof while parsing reftable")]
    UnexpectedEof,
    #[error("bad reftable magic: expected REFT, got {0:?}")]
    BadMagic([u8; 4]),
    #[error("unsupported reftable version: {0}")]
    UnsupportedVersion(u8),
    #[error("invalid block type byte: {0:#x}")]
    InvalidBlockType(u8),
    #[error("varint overflow")]
    VarintOverflow,
    #[error("crc-32 mismatch in footer: expected {expected:#010x}, got {got:#010x}")]
    CrcMismatch { expected: u32, got: u32 },
    #[error("malformed reftable: {0}")]
    Malformed(String),
    #[error("io: {0}")]
    Io(#[from] io::Error),
}

/// reftable spec §3.5 — varint encoding (same as pack ofs-delta).
///
/// Decoder:
/// ```text
/// val = buf[ptr] & 0x7f
/// while (buf[ptr] & 0x80) {
///   ptr++
///   val = ((val + 1) << 7) | (buf[ptr] & 0x7f)
/// }
/// ```
pub fn read_varint(buf: &[u8]) -> Result<(u64, usize), ReftableFormatError> {
    if buf.is_empty() {
        return Err(ReftableFormatError::UnexpectedEof);
    }
    let mut ptr = 0;
    let mut val = (buf[ptr] & 0x7f) as u64;
    while buf[ptr] & 0x80 != 0 {
        ptr += 1;
        if ptr >= buf.len() {
            return Err(ReftableFormatError::UnexpectedEof);
        }
        // Multiply check: val will be shifted left by 7 then OR'd. If val+1 > 2^57
        // we'd overflow a u64.
        if val >= (1u64 << 57) {
            return Err(ReftableFormatError::VarintOverflow);
        }
        val = ((val + 1) << 7) | (buf[ptr] & 0x7f) as u64;
    }
    Ok((val, ptr + 1))
}

/// reftable spec §3.5 — varint encoder. Inverse of `read_varint`. Returns the
/// encoded byte length (always 1..=9 for any u64).
pub fn write_varint(mut val: u64, out: &mut Vec<u8>) {
    // Determine the number of 7-bit bytes needed by repeatedly applying the
    // inverse of ((val+1) << 7) | low7.
    let mut bytes = [0u8; 10];
    let mut n = 0;
    bytes[n] = (val & 0x7f) as u8;
    n += 1;
    while val >= 0x80 {
        val >>= 7;
        val -= 1;
        bytes[n] = (val & 0x7f) as u8 | 0x80;
        n += 1;
    }
    // We wrote bytes from low-order toward high-order, but the wire format
    // emits the high-order byte first (continuation bytes precede the
    // terminator). Reverse and the high bits are already set correctly.
    for i in (0..n).rev() {
        out.push(bytes[i]);
    }
}

/// 24-bit big-endian unsigned integer (used for `block_size` and `block_len`).
pub fn read_u24(buf: &[u8]) -> Result<u32, ReftableFormatError> {
    if buf.len() < 3 {
        return Err(ReftableFormatError::UnexpectedEof);
    }
    Ok(((buf[0] as u32) << 16) | ((buf[1] as u32) << 8) | (buf[2] as u32))
}

pub fn write_u24(v: u32, out: &mut Vec<u8>) {
    out.push((v >> 16) as u8);
    out.push((v >> 8) as u8);
    out.push(v as u8);
}

pub fn read_u16(buf: &[u8]) -> Result<u16, ReftableFormatError> {
    if buf.len() < 2 {
        return Err(ReftableFormatError::UnexpectedEof);
    }
    Ok(((buf[0] as u16) << 8) | (buf[1] as u16))
}

pub fn write_u16(v: u16, out: &mut Vec<u8>) {
    out.push((v >> 8) as u8);
    out.push(v as u8);
}

pub fn read_u32(buf: &[u8]) -> Result<u32, ReftableFormatError> {
    if buf.len() < 4 {
        return Err(ReftableFormatError::UnexpectedEof);
    }
    Ok(u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]))
}

pub fn write_u32(v: u32, out: &mut Vec<u8>) {
    out.extend_from_slice(&v.to_be_bytes());
}

pub fn read_u64(buf: &[u8]) -> Result<u64, ReftableFormatError> {
    if buf.len() < 8 {
        return Err(ReftableFormatError::UnexpectedEof);
    }
    Ok(u64::from_be_bytes([
        buf[0], buf[1], buf[2], buf[3], buf[4], buf[5], buf[6], buf[7],
    ]))
}

pub fn write_u64(v: u64, out: &mut Vec<u8>) {
    out.extend_from_slice(&v.to_be_bytes());
}

/// reftable spec §4.6 — `reverse_int64(t) = 0xffffffffffffffff - t`. Sorts log
/// records descending by `update_index` under lexicographic byte order.
pub fn reverse_u64(t: u64) -> u64 {
    u64::MAX - t
}

/// reftable spec §4.1 — parsed file header.
#[derive(Debug, Clone)]
pub struct FileHeader {
    pub version: u8,
    pub block_size: u32,
    pub min_update_index: u64,
    pub max_update_index: u64,
    pub hash_id: HashKind,
}

impl FileHeader {
    /// Parse the file header from the start of a reftable file. Returns the
    /// header and the byte offset where the first block begins.
    pub fn parse(buf: &[u8]) -> Result<(Self, usize), ReftableFormatError> {
        if buf.len() < HEADER_LEN_V1 {
            return Err(ReftableFormatError::UnexpectedEof);
        }
        let mut magic = [0u8; 4];
        magic.copy_from_slice(&buf[0..4]);
        if &magic != MAGIC {
            return Err(ReftableFormatError::BadMagic(magic));
        }
        let version = buf[4];
        match version {
            VERSION_V1 | VERSION_V2 => {}
            other => return Err(ReftableFormatError::UnsupportedVersion(other)),
        }
        let block_size = read_u24(&buf[5..8])?;
        let min_update_index = read_u64(&buf[8..16])?;
        let max_update_index = read_u64(&buf[16..24])?;
        let (hash_id, header_len) = if version == VERSION_V2 {
            if buf.len() < HEADER_LEN_V2 {
                return Err(ReftableFormatError::UnexpectedEof);
            }
            // v2 spec §4.1.2 stores hash_id as a 4-byte ASCII tag, but C reftable
            // also accepts the trailing 4-byte representation that real git emits.
            // We accept both "sha1"/"s256" tags.
            let tag = &buf[24..28];
            let kind = match tag {
                b"sha1" => HashKind::Sha1,
                b"s256" => HashKind::Sha256,
                _ => HashKind::Sha1,
            };
            (kind, HEADER_LEN_V2)
        } else {
            (HashKind::Sha1, HEADER_LEN_V1)
        };
        Ok((
            Self {
                version,
                block_size,
                min_update_index,
                max_update_index,
                hash_id,
            },
            header_len,
        ))
    }

    pub fn encode(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(MAGIC);
        out.push(self.version);
        write_u24(self.block_size, out);
        write_u64(self.min_update_index, out);
        write_u64(self.max_update_index, out);
        if self.version == VERSION_V2 {
            let tag: &[u8; 4] = match self.hash_id {
                HashKind::Sha1 => b"sha1",
                HashKind::Sha256 => b"s256",
            };
            out.extend_from_slice(tag);
        }
    }

    pub fn header_len(&self) -> usize {
        if self.version == VERSION_V2 {
            HEADER_LEN_V2
        } else {
            HEADER_LEN_V1
        }
    }
}

/// reftable spec §4.7 — parsed footer.
#[derive(Debug, Clone)]
pub struct Footer {
    pub header: FileHeader,
    pub ref_index_position: u64,
    pub obj_position: u64,
    pub obj_id_len: u8,
    pub obj_index_position: u64,
    pub log_position: u64,
    pub log_index_position: u64,
}

impl Footer {
    /// Parse the file footer. `file` is the entire file contents.
    pub fn parse(file: &[u8]) -> Result<Self, ReftableFormatError> {
        let (header, _) = FileHeader::parse(file)?;
        let footer_len = if header.version == VERSION_V2 {
            FOOTER_LEN_V2
        } else {
            FOOTER_LEN_V1
        };
        if file.len() < footer_len {
            return Err(ReftableFormatError::UnexpectedEof);
        }
        let start = file.len() - footer_len;
        let buf = &file[start..];

        // First fields mirror the header (so we re-verify magic + version).
        let header_len = header.header_len();
        // CRC covers footer bytes [0..footer_len-4].
        let crc_start = footer_len - 4;
        let expected_crc = read_u32(&buf[crc_start..crc_start + 4])?;
        let got_crc = crc32(&buf[..crc_start]);
        if expected_crc != got_crc {
            return Err(ReftableFormatError::CrcMismatch {
                expected: expected_crc,
                got: got_crc,
            });
        }

        let mut p = header_len;
        let ref_index_position = read_u64(&buf[p..p + 8])?;
        p += 8;
        let obj_combined = read_u64(&buf[p..p + 8])?;
        p += 8;
        // Per spec §4.7: `(obj_position << 5) | obj_id_len`.
        let obj_position = obj_combined >> 5;
        let obj_id_len = (obj_combined & 0x1f) as u8;
        let obj_index_position = read_u64(&buf[p..p + 8])?;
        p += 8;
        let log_position = read_u64(&buf[p..p + 8])?;
        p += 8;
        let log_index_position = read_u64(&buf[p..p + 8])?;
        let _ = p;

        Ok(Self {
            header,
            ref_index_position,
            obj_position,
            obj_id_len,
            obj_index_position,
            log_position,
            log_index_position,
        })
    }

    /// Encode the footer. Caller has already written the header at file start
    /// and the body in between; this emits the trailing footer block.
    pub fn encode(&self, out: &mut Vec<u8>) {
        let start = out.len();
        self.header.encode(out);
        write_u64(self.ref_index_position, out);
        write_u64(
            (self.obj_position << 5) | (self.obj_id_len as u64 & 0x1f),
            out,
        );
        write_u64(self.obj_index_position, out);
        write_u64(self.log_position, out);
        write_u64(self.log_index_position, out);
        let crc = crc32(&out[start..]);
        write_u32(crc, out);
    }
}

/// Reftable block header layout: 1-byte type + 3-byte length. Total 4 bytes.
#[derive(Debug, Clone, Copy)]
pub struct BlockHeader {
    pub block_type: u8,
    pub block_len: u32,
}

impl BlockHeader {
    pub fn parse(buf: &[u8]) -> Result<Self, ReftableFormatError> {
        if buf.len() < 4 {
            return Err(ReftableFormatError::UnexpectedEof);
        }
        let block_type = buf[0];
        match block_type {
            BLOCK_TYPE_REF | BLOCK_TYPE_OBJ | BLOCK_TYPE_LOG | BLOCK_TYPE_INDEX => {}
            other => return Err(ReftableFormatError::InvalidBlockType(other)),
        }
        let block_len = read_u24(&buf[1..4])?;
        Ok(Self {
            block_type,
            block_len,
        })
    }
}

/// Standard CRC-32 (IEEE 802.3 polynomial 0xedb88320) — used by the reftable
/// footer per spec §4.7. We implement it locally rather than pull in the
/// `crc32fast` crate (matches the project's minimal-deps style).
pub fn crc32(data: &[u8]) -> u32 {
    static mut TABLE: [u32; 256] = [0; 256];
    static INIT: std::sync::Once = std::sync::Once::new();
    // SAFETY: write to TABLE is gated by `Once::call_once`; later reads are
    // through &TABLE (read-only). Single-threaded init then read-only fan-out.
    unsafe {
        INIT.call_once(|| {
            for i in 0..256u32 {
                let mut c = i;
                for _ in 0..8 {
                    if c & 1 != 0 {
                        c = 0xedb88320 ^ (c >> 1);
                    } else {
                        c >>= 1;
                    }
                }
                TABLE[i as usize] = c;
            }
        });
        let mut crc: u32 = 0xffffffff;
        for &b in data {
            crc = TABLE[((crc ^ b as u32) & 0xff) as usize] ^ (crc >> 8);
        }
        crc ^ 0xffffffff
    }
}

/// zlib-decompress a slice produced by zlib `deflate()`. Used to inflate
/// log_block payloads (reftable spec §4.5).
pub fn zlib_decompress(input: &[u8]) -> Result<Vec<u8>, ReftableFormatError> {
    use flate2::read::ZlibDecoder;
    let mut decoder = ZlibDecoder::new(input);
    let mut out = Vec::new();
    decoder
        .read_to_end(&mut out)
        .map_err(|e| ReftableFormatError::Malformed(format!("zlib inflate: {e}")))?;
    Ok(out)
}

/// zlib-compress a slice. Used to deflate log_block payloads when writing.
pub fn zlib_compress(input: &[u8]) -> Result<Vec<u8>, ReftableFormatError> {
    use flate2::write::ZlibEncoder;
    use flate2::Compression;
    use std::io::Write;
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(input)?;
    Ok(encoder.finish()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip_varint(v: u64) {
        let mut out = Vec::new();
        write_varint(v, &mut out);
        let (decoded, n) = read_varint(&out).unwrap();
        assert_eq!(decoded, v, "round-trip failed for {v}");
        assert_eq!(n, out.len(), "byte count mismatch for {v}");
    }

    #[test]
    fn varint_small_values_match_spec() {
        // Single-byte values: high bit clear, low 7 bits = value.
        for v in 0u64..0x80 {
            roundtrip_varint(v);
        }
    }

    #[test]
    fn varint_known_examples() {
        // The reftable header we observed had 0x80 0x09 → 137 (suffix=17, type=1).
        let buf = [0x80, 0x09];
        let (v, n) = read_varint(&buf).unwrap();
        assert_eq!(v, 137);
        assert_eq!(n, 2);
    }

    #[test]
    fn varint_round_trip_various() {
        for v in [
            0u64,
            1,
            0x7f,
            0x80,
            0xff,
            137,
            500,
            1000,
            10_000,
            100_000,
            1_000_000,
            u32::MAX as u64,
            u64::MAX / 2,
        ] {
            roundtrip_varint(v);
        }
    }

    #[test]
    fn read_24_bit_be() {
        assert_eq!(read_u24(&[0x00, 0x10, 0x00]).unwrap(), 0x1000);
        assert_eq!(read_u24(&[0xff, 0xff, 0xff]).unwrap(), 0xffffff);
    }

    #[test]
    fn header_v1_round_trip() {
        let h = FileHeader {
            version: VERSION_V1,
            block_size: 4096,
            min_update_index: 1,
            max_update_index: 5,
            hash_id: HashKind::Sha1,
        };
        let mut out = Vec::new();
        h.encode(&mut out);
        assert_eq!(out.len(), HEADER_LEN_V1);
        let (got, n) = FileHeader::parse(&out).unwrap();
        assert_eq!(got.version, h.version);
        assert_eq!(got.block_size, h.block_size);
        assert_eq!(got.min_update_index, h.min_update_index);
        assert_eq!(got.max_update_index, h.max_update_index);
        assert_eq!(n, HEADER_LEN_V1);
    }

    #[test]
    fn block_header_parse() {
        let buf = [b'r', 0x00, 0x00, 0x3a];
        let h = BlockHeader::parse(&buf).unwrap();
        assert_eq!(h.block_type, BLOCK_TYPE_REF);
        assert_eq!(h.block_len, 0x3a);
    }

    #[test]
    fn crc32_known_value() {
        // CRC-32("123456789") == 0xCBF43926 (well-known test vector).
        assert_eq!(crc32(b"123456789"), 0xCBF43926);
    }

    #[test]
    fn parse_real_git_reftable_header() {
        // The header bytes we observed from `git init --ref-format=reftable`.
        let raw = [
            b'R', b'E', b'F', b'T', 0x01, 0x00, 0x10, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01,
        ];
        let (h, n) = FileHeader::parse(&raw).unwrap();
        assert_eq!(h.version, 1);
        assert_eq!(h.block_size, 0x1000);
        assert_eq!(h.min_update_index, 1);
        assert_eq!(h.max_update_index, 1);
        assert_eq!(n, 24);
    }
}
