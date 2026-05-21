//! Reftable writer.
//!
//! Builds a single `.ref` file from a list of `TableUpdate`s. We emit a
//! minimal file: one ref block (no indexes), optionally one log block, and a
//! footer. This is enough for round-tripping with real git for small
//! transactions (≤ ~100 refs per commit fits in a single 4 KiB ref block).
//!
//! Spec references:
//!   * §4.1 — header (v1).
//!   * §4.3 — ref block + ref record format.
//!   * §4.5/§4.6 — log block + log record format.
//!   * §4.7 — footer.

use std::path::Path;

use crate::hash::{HashKind, ObjectId};

use super::super::FullName;
use super::format::{
    self, write_u16, write_u24, write_varint, BLOCK_TYPE_LOG, BLOCK_TYPE_REF, FOOTER_LEN_V1,
    FOOTER_LEN_V2, HEADER_LEN_V1, VERSION_V1, VERSION_V2,
};

/// In-memory description of a single ref/log update to bake into a new table.
#[derive(Debug, Clone)]
pub struct TableUpdate {
    pub name: FullName,
    pub value: WriteRefValue,
    /// `None` = don't write a reflog entry for this update.
    pub reflog: Option<WriteLogEntry>,
}

#[derive(Debug, Clone)]
pub enum WriteRefValue {
    Deletion,
    Direct(ObjectId),
    Symbolic(FullName),
    // We never write peeled here; peeling is a M-future enhancement.
}

#[derive(Debug, Clone)]
pub struct WriteLogEntry {
    pub old_oid: ObjectId,
    pub new_oid: ObjectId,
    pub committer_name: String,
    pub committer_email: String,
    pub time_seconds: u64,
    pub tz_offset_minutes: i16,
    pub message: String,
}

/// Default block size matching git's behavior (4 KiB aligned).
const DEFAULT_BLOCK_SIZE: u32 = 4096;

/// Write a brand-new reftable file at `path`. Updates must be sorted (we sort
/// internally too, but caller can rely on the on-disk order matching name
/// ordering). All updates share `update_index`; reflog records use the same
/// update_index as their associated ref record (per spec §5).
pub fn write_table_file(
    path: &Path,
    hash_kind: HashKind,
    update_index: u64,
    mut updates: Vec<TableUpdate>,
) -> Result<(), super::super::RefError> {
    // Sort by name — reftable spec requires ref records sorted ascending.
    updates.sort_by(|a, b| a.name.as_str().cmp(b.name.as_str()));

    // Build the file in memory then write atomically.
    let mut file: Vec<u8> = Vec::with_capacity(8 * 1024);

    // Header (v1, SHA-1 path).
    let version = match hash_kind {
        HashKind::Sha1 => VERSION_V1,
        HashKind::Sha256 => VERSION_V2,
    };
    let block_size = DEFAULT_BLOCK_SIZE;
    let header = format::FileHeader {
        version,
        block_size,
        min_update_index: update_index,
        max_update_index: update_index,
        hash_id: hash_kind,
    };
    header.encode(&mut file);
    debug_assert_eq!(
        file.len(),
        header.header_len(),
        "header encoded to unexpected length"
    );

    // Build the ref block payload. For a single-block file we have only one
    // ref block, and the first block shares the file header. Per §4.3:
    //   * block_len for the first block INCLUDES the 24-byte header.
    //   * restart_offsets in the first block are relative to file start
    //     (not block start), so the first restart_offset is `header_len`
    //     plus 4 (the block-header bytes within the first block).

    let header_len = header.header_len();
    let body_start = file.len();
    // We will write the block header inline: 'r' + uint24(block_len).
    // Block_len is unknown until we've encoded records, so we patch it later.
    file.push(BLOCK_TYPE_REF);
    file.push(0);
    file.push(0);
    file.push(0);

    // restart_offsets: we put a restart at every record for simplicity (small
    // files; restart_count rarely exceeds 65535). Each restart offset must
    // point at a record where prefix_length = 0.
    let mut restart_offsets: Vec<u32> = Vec::new();
    // Prefix compression is intentionally disabled (every record is a restart
    // point). The `prior_name` placeholder is kept in case we re-enable it.

    let only_ref_updates: Vec<_> = updates.iter().collect();

    for upd in &only_ref_updates {
        // Restart offset measured from file start (first block) or block
        // start (later blocks). We only have one block, so from file start.
        let record_offset = file.len();
        // First restart point for the first block is `header_len + 4`
        // (right after the block header). Per spec §4.3 we record restart
        // offsets pointing at records with prefix_length=0.
        // For simplicity we make every record a restart point.
        restart_offsets.push(record_offset as u32);

        let name_str = upd.name.as_str();
        let name_bytes = name_str.as_bytes();
        // We choose prefix_length=0 at every restart point (we mark every
        // record as a restart). Prefix compression therefore isn't applied.
        let prefix_length: u64 = 0;
        let suffix_bytes: &[u8] = name_bytes;
        let suffix_length: u64 = suffix_bytes.len() as u64;
        let value_type: u8 = match &upd.value {
            WriteRefValue::Deletion => 0,
            WriteRefValue::Direct(_) => 1,
            WriteRefValue::Symbolic(_) => 3,
        };
        write_varint(prefix_length, &mut file);
        write_varint((suffix_length << 3) | value_type as u64, &mut file);
        file.extend_from_slice(suffix_bytes);
        let update_index_delta: u64 = 0; // single update_index in this file
        write_varint(update_index_delta, &mut file);
        match &upd.value {
            WriteRefValue::Deletion => { /* no payload */ }
            WriteRefValue::Direct(oid) => {
                file.extend_from_slice(oid.as_bytes());
            }
            WriteRefValue::Symbolic(target) => {
                let t = target.as_str();
                write_varint(t.len() as u64, &mut file);
                file.extend_from_slice(t.as_bytes());
            }
        }
        let _ = name_str; // suppressed: prior_name elided since every record is a restart.
    }

    // Restart table at the tail.
    if restart_offsets.is_empty() {
        // No refs: empty ref block. Reftable spec §4.3 says restart table
        // "must not be empty", but a zero-record block can't have a restart.
        // For safety we never emit a ref block in this case — caller should
        // ensure at least one update. If we get here, push a synthetic empty
        // restart count of 0.
        write_u16(0, &mut file);
    } else {
        for off in &restart_offsets {
            write_u24(*off, &mut file);
        }
        write_u16(restart_offsets.len() as u16, &mut file);
    }

    // Patch the block_len in the first block: for the first block, this
    // is the absolute byte count from file start to end of restart table
    // (i.e., the entire file so far). Per spec §4.3 the first block's
    // block_len includes the file header.
    let block_end = file.len();
    let first_block_len = (block_end as u32) & 0xff_ffff;
    file[body_start + 1] = (first_block_len >> 16) as u8;
    file[body_start + 2] = (first_block_len >> 8) as u8;
    file[body_start + 3] = first_block_len as u8;

    // Pad up to block_size if block alignment is on (block_size != 0).
    if block_size > 0 {
        let padding = block_size as usize - (file.len() % block_size as usize);
        if padding != block_size as usize {
            file.resize(file.len() + padding, 0);
        }
    }

    // Optional log block. Only emit if any update carries a reflog entry.
    let log_position = {
        let mut log_pos = 0u64;
        let mut log_payload: Vec<u8> = Vec::new();
        let mut log_restarts: Vec<u32> = Vec::new();
        let mut any_log = false;
        // Spec §4.6: keys are `refname \0 reverse_int64(update_index)`. Records
        // sort lexicographically — newest first thanks to reverse_int64.
        // We pre-sort updates by (refname asc, then by reverse_int64 of
        // update_index — but everything is the same update_index in one
        // table, so name-asc suffices).
        for upd in &updates {
            let log = match &upd.reflog {
                Some(l) => l,
                None => continue,
            };
            any_log = true;
            // Restart at every record (small files; matches reader assumptions).
            log_restarts.push(log_payload.len() as u32 + 4 /* block header */);

            let name_bytes = upd.name.as_str().as_bytes();
            let rev_idx = format::reverse_u64(update_index);
            let mut key: Vec<u8> = Vec::with_capacity(name_bytes.len() + 9);
            key.extend_from_slice(name_bytes);
            key.push(0);
            key.extend_from_slice(&rev_idx.to_be_bytes());
            // No prefix compression (every record a restart).
            let prefix_length: u64 = 0;
            let suffix_bytes: &[u8] = &key;
            let suffix_length: u64 = suffix_bytes.len() as u64;
            let log_type: u8 = 1; // standard reflog entry
            write_varint(prefix_length, &mut log_payload);
            write_varint((suffix_length << 3) | log_type as u64, &mut log_payload);
            log_payload.extend_from_slice(suffix_bytes);
            // log_data
            log_payload.extend_from_slice(log.old_oid.as_bytes());
            log_payload.extend_from_slice(log.new_oid.as_bytes());
            write_varint(log.committer_name.len() as u64, &mut log_payload);
            log_payload.extend_from_slice(log.committer_name.as_bytes());
            write_varint(log.committer_email.len() as u64, &mut log_payload);
            log_payload.extend_from_slice(log.committer_email.as_bytes());
            write_varint(log.time_seconds, &mut log_payload);
            log_payload.extend_from_slice(&log.tz_offset_minutes.to_be_bytes());
            write_varint(log.message.len() as u64, &mut log_payload);
            log_payload.extend_from_slice(log.message.as_bytes());
            let _ = key; // ditto: prior_key elided since every log record is a restart.
        }
        if any_log {
            // Restart table at tail.
            for off in &log_restarts {
                write_u24(*off, &mut log_payload);
            }
            write_u16(log_restarts.len() as u16, &mut log_payload);

            // Wrap with 4-byte block header (g + uint24 inflated length).
            // "block_len in the header is the inflated size (including the
            // 4-byte block header)". So inflated total = 4 + log_payload.len().
            let inflated_total = (log_payload.len() + 4) as u32;
            let mut block_header = Vec::with_capacity(4);
            block_header.push(BLOCK_TYPE_LOG);
            write_u24(inflated_total & 0xff_ffff, &mut block_header);

            // Now compress just the payload.
            let compressed = format::zlib_compress(&log_payload).map_err(|e| {
                super::super::RefError::Malformed {
                    name: path.display().to_string(),
                    reason: e.to_string(),
                }
            })?;
            log_pos = file.len() as u64;
            file.extend_from_slice(&block_header);
            file.extend_from_slice(&compressed);
        }
        log_pos
    };

    // Footer.
    let footer = format::Footer {
        header: header.clone(),
        ref_index_position: 0,
        obj_position: 0,
        obj_id_len: 0,
        obj_index_position: 0,
        log_position,
        log_index_position: 0,
    };
    footer.encode(&mut file);

    debug_assert!(
        file.len()
            >= header_len
                + if version == VERSION_V2 {
                    FOOTER_LEN_V2
                } else {
                    FOOTER_LEN_V1
                }
    );

    // Atomic write: write to a temp file in the same dir, then rename.
    let dir = path.parent().ok_or_else(|| super::super::RefError::Io {
        path: path.to_path_buf(),
        source: std::io::Error::new(std::io::ErrorKind::InvalidInput, "no parent dir"),
    })?;
    let tmp_path = dir.join(format!(
        ".tmp_{}_{}",
        update_index,
        path.file_name().unwrap().to_string_lossy()
    ));
    std::fs::write(&tmp_path, &file).map_err(|e| super::super::RefError::Io {
        path: tmp_path.clone(),
        source: e,
    })?;
    std::fs::rename(&tmp_path, path).map_err(|e| super::super::RefError::Io {
        path: path.to_path_buf(),
        source: e,
    })?;
    Ok(())
}

/// Construct the canonical reftable filename:
/// `${min_update_index:012x}-${max_update_index:012x}-${random}.ref`.
/// Matches what real git emits (e.g., `0x000000000001-0x000000000001-abc.ref`).
pub fn make_table_filename(min_index: u64, max_index: u64, suffix: &str) -> String {
    format!("0x{:012x}-0x{:012x}-{}.ref", min_index, max_index, suffix)
}

/// Generate a short random suffix (8 hex chars). Doesn't need crypto strength;
/// uniqueness within a `tables.list` is all we need.
pub fn random_suffix() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let pid = std::process::id();
    // 4 bytes of mix-in.
    let mix = (nanos as u64) ^ ((pid as u64) << 32) ^ 0x9e3779b97f4a7c15u64;
    format!("{:08x}", mix as u32)
}

// Constants kept here for spec-symmetry — referenced by tests and the size
// `debug_assert!` above.
#[allow(dead_code)]
const _SIZES: usize = HEADER_LEN_V1 + FOOTER_LEN_V1 + FOOTER_LEN_V2;
