//! `rustygit show-index` — dump the contents of a pack `.idx` file
//! read from stdin (or a path).
//!
//! Output, one entry per line:
//!   `<offset> <oid> [(<crc32-hex>)]`
//!
//! The CRC32 is only printed for v2 idx files (v1 doesn't carry one).

use std::io::{self, Read};

use clap::Args;

#[derive(Debug, Args)]
pub struct ShowIndexArgs {
    /// Hash function — accepted for upstream-parity. We currently only
    /// support `sha1`.
    #[arg(long = "object-format", default_value = "sha1")]
    pub object_format: String,
}

pub fn run(args: ShowIndexArgs) -> io::Result<i32> {
    if args.object_format != "sha1" {
        eprintln!(
            "rustygit: show-index: only sha1 idx files are supported (got {:?})",
            args.object_format
        );
        return Ok(128);
    }

    let mut bytes = Vec::new();
    io::stdin().read_to_end(&mut bytes)?;
    dump_idx(&bytes)
}

fn dump_idx(bytes: &[u8]) -> io::Result<i32> {
    // v2 magic: \377tOc + version (u32 BE).
    if bytes.len() < 8 {
        return Err(io::Error::other("show-index: input too short"));
    }
    let is_v2 = &bytes[..4] == b"\xfftOc";
    if !is_v2 {
        return Err(io::Error::other(
            "show-index: v1 .idx files are not supported",
        ));
    }
    let version = u32::from_be_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
    if version != 2 {
        return Err(io::Error::other(format!(
            "show-index: unknown idx version {version}"
        )));
    }
    let fanout_off = 8;
    if bytes.len() < fanout_off + 256 * 4 {
        return Err(io::Error::other("show-index: truncated fanout"));
    }
    let total = u32::from_be_bytes([
        bytes[fanout_off + 255 * 4],
        bytes[fanout_off + 255 * 4 + 1],
        bytes[fanout_off + 255 * 4 + 2],
        bytes[fanout_off + 255 * 4 + 3],
    ]) as usize;
    let oids_off = fanout_off + 256 * 4;
    let crcs_off = oids_off + total * 20;
    let small_offs_off = crcs_off + total * 4;
    let large_offs_off = small_offs_off + total * 4;
    if bytes.len() < small_offs_off + total * 4 {
        return Err(io::Error::other("show-index: truncated tables"));
    }

    let stdout = io::stdout();
    let mut out = stdout.lock();
    use std::io::Write as _;

    let mut large_offsets: Vec<u64> = Vec::new();
    // We discover how many large offsets we need by scanning small offsets;
    // but the file layout puts large offsets right after small ones.
    // Simplest: read all 64-bit entries between large_offs_off and trailer.
    // The trailer is 2 * 20 bytes (pack sha1 + idx sha1) for v2.
    let trailer_off = bytes.len().saturating_sub(40);
    if large_offs_off < trailer_off {
        let raw = &bytes[large_offs_off..trailer_off];
        for chunk in raw.chunks_exact(8) {
            let v = u64::from_be_bytes([
                chunk[0], chunk[1], chunk[2], chunk[3], chunk[4], chunk[5], chunk[6], chunk[7],
            ]);
            large_offsets.push(v);
        }
    }

    for i in 0..total {
        let oid_bytes = &bytes[oids_off + i * 20..oids_off + (i + 1) * 20];
        let mut hex = String::with_capacity(40);
        for &b in oid_bytes {
            hex.push_str(&format!("{b:02x}"));
        }
        let crc = u32::from_be_bytes([
            bytes[crcs_off + i * 4],
            bytes[crcs_off + i * 4 + 1],
            bytes[crcs_off + i * 4 + 2],
            bytes[crcs_off + i * 4 + 3],
        ]);
        let small_off = u32::from_be_bytes([
            bytes[small_offs_off + i * 4],
            bytes[small_offs_off + i * 4 + 1],
            bytes[small_offs_off + i * 4 + 2],
            bytes[small_offs_off + i * 4 + 3],
        ]);
        let offset = if small_off & 0x8000_0000 != 0 {
            let idx = (small_off & 0x7fff_ffff) as usize;
            *large_offsets
                .get(idx)
                .ok_or_else(|| io::Error::other("show-index: large-offset out of range"))?
        } else {
            small_off as u64
        };
        // git's format: "<offset> <oid> (<crc32 hex>)"
        writeln!(out, "{offset} {hex} ({crc:08x})")?;
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_input_errors() {
        let r = dump_idx(b"");
        assert!(r.is_err());
    }

    #[test]
    fn non_v2_magic_errors() {
        // 4 bytes that aren't the v2 magic, plus 4 of version.
        let mut bytes = vec![0u8; 8 + 256 * 4];
        // No magic prefix → routed to "v1 not supported" branch.
        let r = dump_idx(&bytes);
        assert!(r.is_err());
        bytes[..4].copy_from_slice(b"\xfftOc");
        bytes[4..8].copy_from_slice(&3u32.to_be_bytes());
        let r = dump_idx(&bytes);
        // Version 3 → error.
        assert!(r.is_err());
    }
}
