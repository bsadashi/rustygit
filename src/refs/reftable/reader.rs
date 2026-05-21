//! Reftable reader: parse `.git/reftable/*.ref` files and stack them.
//!
//! Spec references in this file:
//!   * §4.3 — ref block format.
//!   * §4.5 — log block format (zlib-deflated payload).
//!   * §4.6 — log record key encoding (ref_name '\0' reverse_int64(update_index)).
//!   * §4.7 — file footer.
//!   * §5    — `tables.list` stack file.
//!
//! Read path:
//!   1. Parse `tables.list` into an ordered list of filenames.
//!   2. For each file, mmap (we just `read_to_end` for simplicity) the file.
//!   3. Read the header + footer to learn the block layout.
//!   4. Iterate ref blocks linearly to satisfy `iter()`; for `read(name)` we
//!      scan each table newest → oldest and return the first hit.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::sync::Arc;

use crate::hash::{HashKind, ObjectId};

use super::super::{FullName, RefError, RefTarget, Reference};
use super::format::{
    self, BlockHeader, FileHeader, Footer, ReftableFormatError, BLOCK_TYPE_LOG, BLOCK_TYPE_REF,
    FOOTER_LEN_V1, FOOTER_LEN_V2, VERSION_V2,
};

/// Per-record ref value (spec §4.3.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefValue {
    /// `value_type = 0`: explicit deletion (tombstone).
    Deletion,
    /// `value_type = 1`: single oid.
    Direct(ObjectId),
    /// `value_type = 2`: oid + peeled oid (annotated tag).
    Peeled { oid: ObjectId, peeled: ObjectId },
    /// `value_type = 3`: symbolic ref target.
    Symbolic(String),
}

#[derive(Debug, Clone)]
pub struct RefRecord {
    pub name: String,
    pub update_index: u64,
    pub value: RefValue,
}

/// Decoded log record (spec §4.6).
#[derive(Debug, Clone)]
pub struct LogRecordRead {
    pub name: String,
    pub update_index: u64,
    pub old_oid: ObjectId,
    pub new_oid: ObjectId,
    pub committer_name: String,
    pub committer_email: String,
    pub time_seconds: u64,
    pub tz_offset_minutes: i16,
    pub message: String,
    /// `log_type = 0` is a tombstone (deletion in the reflog).
    pub deleted: bool,
}

/// Owns the raw bytes of one reftable file plus the parsed footer.
pub struct TableReader {
    bytes: Vec<u8>,
    footer: Footer,
    hash_kind: HashKind,
    name: String,
}

impl TableReader {
    pub fn open(path: &Path, hash_kind: HashKind) -> Result<Self, RefError> {
        let bytes = fs::read(path).map_err(|e| RefError::Io {
            path: path.to_path_buf(),
            source: e,
        })?;
        let footer = Footer::parse(&bytes).map_err(|e| RefError::Malformed {
            name: path.display().to_string(),
            reason: e.to_string(),
        })?;
        let name = path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        Ok(Self {
            bytes,
            footer,
            hash_kind,
            name,
        })
    }

    pub fn header(&self) -> &FileHeader {
        &self.footer.header
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// Iterate every ref record in this table, in the order they appear.
    /// `read()` callers can stop early; full enumeration is used by `iter()`.
    pub fn iter_ref_records(&self) -> Result<Vec<RefRecord>, RefError> {
        let mut out = Vec::new();
        for block in self.ref_blocks()? {
            decode_ref_block(
                &block,
                self.hash_kind,
                self.header().min_update_index,
                &mut out,
            )
            .map_err(|e| RefError::Malformed {
                name: self.name.clone(),
                reason: e.to_string(),
            })?;
        }
        Ok(out)
    }

    /// Iterate every log record. Spec §4.5: log blocks are zlib-deflated.
    pub fn iter_log_records(&self) -> Result<Vec<LogRecordRead>, RefError> {
        let mut out = Vec::new();
        for block in self.log_blocks()? {
            decode_log_block(&block, self.hash_kind, &mut out).map_err(|e| {
                RefError::Malformed {
                    name: self.name.clone(),
                    reason: e.to_string(),
                }
            })?;
        }
        Ok(out)
    }

    /// Slice the file into raw ref-block payloads (each starts at a block
    /// boundary except the first, which shares the file header).
    fn ref_blocks(&self) -> Result<Vec<Vec<u8>>, RefError> {
        let header = self.header();
        let mut blocks = Vec::new();

        // Where does the first block live? Always immediately after the
        // header. The first block's `block_len` includes the header bytes
        // (spec §4.3: "In the first ref block, block_len includes 24 bytes for
        // the file header").
        let first_block_start = header.header_len();
        if self.bytes.len() <= first_block_start {
            return Ok(blocks);
        }

        // Decide whether the FIRST block is a ref block. If not, we have a
        // log-only file or empty file.
        let first_type = self.bytes[first_block_start];
        if first_type != BLOCK_TYPE_REF {
            return Ok(blocks);
        }

        // Walk ref blocks starting at first_block_start until we hit a non-ref
        // block, the log section, or the footer.
        let stop_at = self.body_end();
        let block_size = if header.block_size == 0 {
            // Unaligned file: blocks aren't padded; advance by exact block_len.
            0u32
        } else {
            header.block_size
        };
        let mut pos = first_block_start;
        let mut is_first = true;
        while pos < stop_at {
            if self.bytes[pos] != BLOCK_TYPE_REF {
                break;
            }
            let header_bh =
                BlockHeader::parse(&self.bytes[pos..]).map_err(|e| RefError::Malformed {
                    name: self.name.clone(),
                    reason: e.to_string(),
                })?;
            let block_payload_end = if is_first {
                // block_len was computed including the file header bytes.
                pos + header_bh.block_len as usize - header.header_len()
            } else {
                pos + header_bh.block_len as usize
            };
            if block_payload_end > self.bytes.len() {
                return Err(RefError::Malformed {
                    name: self.name.clone(),
                    reason: "ref block extends past EOF".into(),
                });
            }
            blocks.push(self.bytes[pos..block_payload_end].to_vec());
            // Advance: aligned files round up to block_size from the start of
            // the file; unaligned files advance by block_payload_end.
            pos = if block_size > 0 {
                if is_first {
                    block_size as usize
                } else {
                    // Round (pos + block_size) down to a multiple of block_size.
                    let cur_block = pos / block_size as usize;
                    (cur_block + 1) * block_size as usize
                }
            } else {
                block_payload_end
            };
            is_first = false;
        }
        Ok(blocks)
    }

    fn body_end(&self) -> usize {
        // The footer eats the last FOOTER_LEN_V1 / V2 bytes.
        let flen = if self.footer.header.version == VERSION_V2 {
            FOOTER_LEN_V2
        } else {
            FOOTER_LEN_V1
        };
        self.bytes.len().saturating_sub(flen)
    }

    fn log_blocks(&self) -> Result<Vec<Vec<u8>>, RefError> {
        let mut blocks = Vec::new();
        if self.footer.log_position == 0 {
            return Ok(blocks);
        }
        let stop_at = self.body_end();
        // Log section can also be bounded by the log_index_position when
        // present.
        let stop_at = if self.footer.log_index_position != 0 {
            (self.footer.log_index_position as usize).min(stop_at)
        } else {
            stop_at
        };
        let mut pos = self.footer.log_position as usize;
        while pos < stop_at {
            if self.bytes[pos] != BLOCK_TYPE_LOG {
                break;
            }
            let header_bh =
                BlockHeader::parse(&self.bytes[pos..]).map_err(|e| RefError::Malformed {
                    name: self.name.clone(),
                    reason: e.to_string(),
                })?;
            // Header bytes 0..4 are the block_type + block_len; bytes 4.. are
            // zlib-compressed. The on-disk size is variable so we just inflate
            // until end-of-stream — but we DO need to know how many input
            // bytes the inflater consumed to find the next block. Easiest:
            // try decompressing slices of growing size. Simpler: spec says
            // block_len is the INFLATED size including the 4-byte header, so
            // for layout we rely on... actually no: the on-disk position must
            // advance by the compressed-byte count. Use ZlibDecoder which
            // tracks consumed-input via Read::read.
            // We feed bytes[pos+4..stop_at] to a ZlibDecoder and read until it
            // signals end-of-stream, then use `total_in()` to advance.
            let compressed_start = pos + 4;
            let max_remaining = stop_at - compressed_start;
            let compressed_slice = &self.bytes[compressed_start..compressed_start + max_remaining];
            let mut decoder = flate2::read::ZlibDecoder::new(compressed_slice);
            let mut inflated = Vec::with_capacity(header_bh.block_len as usize);
            use std::io::Read;
            decoder
                .read_to_end(&mut inflated)
                .map_err(|e| RefError::Malformed {
                    name: self.name.clone(),
                    reason: format!("log inflate: {e}"),
                })?;
            // total_in() returns the number of compressed bytes consumed.
            let consumed = decoder.total_in() as usize;
            // Prepend the 4-byte block header to the inflated payload so
            // restart_offset values (which include the header) line up.
            let mut full = Vec::with_capacity(4 + inflated.len());
            full.extend_from_slice(&self.bytes[pos..pos + 4]);
            full.extend_from_slice(&inflated);
            blocks.push(full);
            pos = compressed_start + consumed;
        }
        Ok(blocks)
    }
}

/// Decode a single ref block (already sliced from the file). Appends each
/// `RefRecord` to `out`.
fn decode_ref_block(
    block: &[u8],
    hash_kind: HashKind,
    min_update_index: u64,
    out: &mut Vec<RefRecord>,
) -> Result<(), ReftableFormatError> {
    let header = BlockHeader::parse(block)?;
    if header.block_type != BLOCK_TYPE_REF {
        return Err(ReftableFormatError::InvalidBlockType(header.block_type));
    }
    // Locate the restart_count footer at the tail. The block ends with:
    //   <restart_offsets (3 bytes each)> <restart_count (2 bytes)>
    let restart_count = format::read_u16(&block[block.len() - 2..])? as usize;
    if restart_count == 0 {
        // Empty block (spec §4.3: "must not be empty" — but be defensive).
        return Ok(());
    }
    let restart_table_size = restart_count * 3 + 2;
    if block.len() < restart_table_size + 4 {
        return Err(ReftableFormatError::Malformed(
            "ref block too small for restart table".into(),
        ));
    }
    let records_end = block.len() - restart_table_size;

    // Records start right after the 4-byte block header.
    let mut pos = 4;
    let mut prior_name = String::new();
    while pos < records_end {
        // varint prefix_length
        let (prefix_length, n) = format::read_varint(&block[pos..])?;
        pos += n;
        // varint (suffix_length << 3) | value_type
        let (combo, n) = format::read_varint(&block[pos..])?;
        pos += n;
        let suffix_length = (combo >> 3) as usize;
        let value_type = (combo & 0x07) as u8;
        if pos + suffix_length > block.len() {
            return Err(ReftableFormatError::Malformed(
                "ref record suffix runs past block".into(),
            ));
        }
        let suffix = std::str::from_utf8(&block[pos..pos + suffix_length])
            .map_err(|_| ReftableFormatError::Malformed("non-utf8 ref name".into()))?;
        pos += suffix_length;
        // Recompose the name.
        let prefix_length = prefix_length as usize;
        if prefix_length > prior_name.len() {
            return Err(ReftableFormatError::Malformed(
                "ref record prefix_length exceeds prior name".into(),
            ));
        }
        // We must split at a char boundary; ref names are validated to be
        // 7-bit ASCII so any byte boundary is also a char boundary, but
        // be safe.
        let mut name = String::with_capacity(prefix_length + suffix.len());
        name.push_str(&prior_name[..prefix_length]);
        name.push_str(suffix);
        // varint update_index_delta
        let (delta, n) = format::read_varint(&block[pos..])?;
        pos += n;
        let update_index = min_update_index + delta;
        // value payload
        let value = match value_type {
            0 => RefValue::Deletion,
            1 => {
                let raw_len = hash_kind.raw_len();
                if pos + raw_len > block.len() {
                    return Err(ReftableFormatError::Malformed("value-1 truncated".into()));
                }
                let oid = ObjectId::from_bytes(hash_kind, &block[pos..pos + raw_len])
                    .map_err(|e| ReftableFormatError::Malformed(e.to_string()))?;
                pos += raw_len;
                RefValue::Direct(oid)
            }
            2 => {
                let raw_len = hash_kind.raw_len();
                if pos + 2 * raw_len > block.len() {
                    return Err(ReftableFormatError::Malformed("value-2 truncated".into()));
                }
                let oid = ObjectId::from_bytes(hash_kind, &block[pos..pos + raw_len])
                    .map_err(|e| ReftableFormatError::Malformed(e.to_string()))?;
                pos += raw_len;
                let peeled = ObjectId::from_bytes(hash_kind, &block[pos..pos + raw_len])
                    .map_err(|e| ReftableFormatError::Malformed(e.to_string()))?;
                pos += raw_len;
                RefValue::Peeled { oid, peeled }
            }
            3 => {
                let (target_len, n) = format::read_varint(&block[pos..])?;
                pos += n;
                let target_len = target_len as usize;
                if pos + target_len > block.len() {
                    return Err(ReftableFormatError::Malformed(
                        "symref target truncated".into(),
                    ));
                }
                let target = std::str::from_utf8(&block[pos..pos + target_len])
                    .map_err(|_| ReftableFormatError::Malformed("non-utf8 symref target".into()))?
                    .to_string();
                pos += target_len;
                RefValue::Symbolic(target)
            }
            other => {
                return Err(ReftableFormatError::Malformed(format!(
                    "reserved ref value_type {other}"
                )))
            }
        };
        prior_name = name.clone();
        out.push(RefRecord {
            name,
            update_index,
            value,
        });
    }
    Ok(())
}

/// Decode a single (already-inflated) log block. Spec §4.5/§4.6.
fn decode_log_block(
    block: &[u8],
    hash_kind: HashKind,
    out: &mut Vec<LogRecordRead>,
) -> Result<(), ReftableFormatError> {
    let header = BlockHeader::parse(block)?;
    if header.block_type != BLOCK_TYPE_LOG {
        return Err(ReftableFormatError::InvalidBlockType(header.block_type));
    }
    let restart_count = format::read_u16(&block[block.len() - 2..])? as usize;
    if restart_count == 0 {
        return Ok(());
    }
    let restart_table_size = restart_count * 3 + 2;
    if block.len() < 4 + restart_table_size {
        return Err(ReftableFormatError::Malformed("log block too small".into()));
    }
    let records_end = block.len() - restart_table_size;

    let mut pos = 4;
    let mut prior_key = Vec::<u8>::new();
    while pos < records_end {
        let (prefix_length, n) = format::read_varint(&block[pos..])?;
        pos += n;
        let (combo, n) = format::read_varint(&block[pos..])?;
        pos += n;
        let suffix_length = (combo >> 3) as usize;
        let log_type = (combo & 0x07) as u8;
        if pos + suffix_length > block.len() {
            return Err(ReftableFormatError::Malformed(
                "log key suffix truncated".into(),
            ));
        }
        let suffix = &block[pos..pos + suffix_length];
        pos += suffix_length;
        let prefix_length = prefix_length as usize;
        if prefix_length > prior_key.len() {
            return Err(ReftableFormatError::Malformed(
                "log prefix_length exceeds prior key".into(),
            ));
        }
        let mut key = Vec::with_capacity(prefix_length + suffix.len());
        key.extend_from_slice(&prior_key[..prefix_length]);
        key.extend_from_slice(suffix);
        prior_key = key.clone();
        // Key format: refname '\0' reverse_int64(update_index) [9 bytes after \0... wait,
        // actually 8 bytes].
        let nul_pos = key
            .iter()
            .position(|&b| b == 0)
            .ok_or_else(|| ReftableFormatError::Malformed("log key missing NUL".into()))?;
        if key.len() < nul_pos + 1 + 8 {
            return Err(ReftableFormatError::Malformed(
                "log key missing update_index".into(),
            ));
        }
        let name_bytes = &key[..nul_pos];
        let rev_idx_bytes = &key[nul_pos + 1..nul_pos + 1 + 8];
        let rev_idx = u64::from_be_bytes([
            rev_idx_bytes[0],
            rev_idx_bytes[1],
            rev_idx_bytes[2],
            rev_idx_bytes[3],
            rev_idx_bytes[4],
            rev_idx_bytes[5],
            rev_idx_bytes[6],
            rev_idx_bytes[7],
        ]);
        let update_index = format::reverse_u64(rev_idx);
        let name = std::str::from_utf8(name_bytes)
            .map_err(|_| ReftableFormatError::Malformed("non-utf8 log ref name".into()))?
            .to_string();

        let (record, new_pos) = if log_type == 0 {
            (
                LogRecordRead {
                    name,
                    update_index,
                    old_oid: ObjectId::null(hash_kind),
                    new_oid: ObjectId::null(hash_kind),
                    committer_name: String::new(),
                    committer_email: String::new(),
                    time_seconds: 0,
                    tz_offset_minutes: 0,
                    message: String::new(),
                    deleted: true,
                },
                pos,
            )
        } else if log_type == 1 {
            let raw_len = hash_kind.raw_len();
            if pos + 2 * raw_len > block.len() {
                return Err(ReftableFormatError::Malformed("log oids truncated".into()));
            }
            let old_oid = ObjectId::from_bytes(hash_kind, &block[pos..pos + raw_len])
                .map_err(|e| ReftableFormatError::Malformed(e.to_string()))?;
            let new_oid = ObjectId::from_bytes(hash_kind, &block[pos + raw_len..pos + 2 * raw_len])
                .map_err(|e| ReftableFormatError::Malformed(e.to_string()))?;
            let mut p = pos + 2 * raw_len;
            let (name_len, n) = format::read_varint(&block[p..])?;
            p += n;
            let name_len = name_len as usize;
            let committer_name = std::str::from_utf8(&block[p..p + name_len])
                .map_err(|_| ReftableFormatError::Malformed("non-utf8 committer name".into()))?
                .to_string();
            p += name_len;
            let (email_len, n) = format::read_varint(&block[p..])?;
            p += n;
            let email_len = email_len as usize;
            let committer_email = std::str::from_utf8(&block[p..p + email_len])
                .map_err(|_| ReftableFormatError::Malformed("non-utf8 committer email".into()))?
                .to_string();
            p += email_len;
            let (time_seconds, n) = format::read_varint(&block[p..])?;
            p += n;
            // sint16 tz_offset — big-endian signed.
            if p + 2 > block.len() {
                return Err(ReftableFormatError::Malformed("log tz truncated".into()));
            }
            let tz_offset_minutes = i16::from_be_bytes([block[p], block[p + 1]]);
            p += 2;
            let (msg_len, n) = format::read_varint(&block[p..])?;
            p += n;
            let msg_len = msg_len as usize;
            let message = std::str::from_utf8(&block[p..p + msg_len])
                .map_err(|_| ReftableFormatError::Malformed("non-utf8 log message".into()))?
                .to_string();
            p += msg_len;
            (
                LogRecordRead {
                    name,
                    update_index,
                    old_oid,
                    new_oid,
                    committer_name,
                    committer_email,
                    time_seconds,
                    tz_offset_minutes,
                    message,
                    deleted: false,
                },
                p,
            )
        } else {
            return Err(ReftableFormatError::Malformed(format!(
                "unknown log_type {log_type}"
            )));
        };
        pos = new_pos;
        out.push(record);
    }
    Ok(())
}

/// A loaded stack of `TableReader`s — index 0 is the OLDEST (base) table.
pub struct StackReader {
    tables: Vec<Arc<TableReader>>,
    hash_kind: HashKind,
}

impl StackReader {
    pub fn load(reftable_dir: &Path, hash_kind: HashKind) -> Result<Self, RefError> {
        let list_path = reftable_dir.join("tables.list");
        let bytes = match fs::read(&list_path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => {
                return Err(RefError::Io {
                    path: list_path,
                    source: e,
                })
            }
        };
        let text = std::str::from_utf8(&bytes).map_err(|_| RefError::Malformed {
            name: list_path.display().to_string(),
            reason: "tables.list is not valid UTF-8".into(),
        })?;
        let mut tables = Vec::new();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let file_path = reftable_dir.join(line);
            let reader = TableReader::open(&file_path, hash_kind)?;
            tables.push(Arc::new(reader));
        }
        Ok(Self { tables, hash_kind })
    }

    pub fn tables(&self) -> &[Arc<TableReader>] {
        &self.tables
    }

    /// Newest-wins lookup for a single ref name. Walks the stack from end to
    /// start; returns the first non-tombstone hit. Returns `Ok(None)` if not
    /// present anywhere.
    pub fn read(&self, name: &FullName) -> Result<Option<Reference>, RefError> {
        // Walk newest → oldest.
        for table in self.tables.iter().rev() {
            let records = table.iter_ref_records()?;
            // Find the last record matching the name within this table
            // (records inside a table are sorted by name; one table can
            // have only one final value per name per spec §3.3 key unicity).
            for rec in &records {
                if rec.name == name.as_str() {
                    match &rec.value {
                        RefValue::Deletion => return Ok(None),
                        RefValue::Direct(oid) => {
                            return Ok(Some(Reference {
                                name: name.clone(),
                                target: RefTarget::Direct(*oid),
                            }))
                        }
                        RefValue::Peeled { oid, .. } => {
                            return Ok(Some(Reference {
                                name: name.clone(),
                                target: RefTarget::Direct(*oid),
                            }))
                        }
                        RefValue::Symbolic(target) => {
                            let target_name = FullName::new(target.clone())?;
                            return Ok(Some(Reference {
                                name: name.clone(),
                                target: RefTarget::Symbolic(target_name),
                            }));
                        }
                    }
                }
            }
        }
        Ok(None)
    }

    /// Merged ref iteration across the stack. Newest value wins per name;
    /// tombstones suppress the name entirely.
    pub fn iter<'a>(
        &'a self,
        prefix: Option<&'a str>,
    ) -> impl Iterator<Item = Result<Reference, RefError>> + 'a {
        // Merge across tables: walk oldest → newest, overwrite a BTreeMap so
        // newer wins.
        let mut merged: BTreeMap<String, RefRecord> = BTreeMap::new();
        let mut err: Option<RefError> = None;
        for table in &self.tables {
            match table.iter_ref_records() {
                Ok(records) => {
                    for r in records {
                        merged.insert(r.name.clone(), r);
                    }
                }
                Err(e) => {
                    err = Some(e);
                    break;
                }
            }
        }
        let prefix_owned = prefix.map(|s| s.to_string());
        let head: Box<dyn Iterator<Item = Result<Reference, RefError>>> = if let Some(e) = err {
            Box::new(std::iter::once(Err(e)))
        } else {
            Box::new(std::iter::empty())
        };
        head.chain(merged.into_iter().filter_map(move |(name, rec)| {
            if let Some(p) = &prefix_owned {
                if !name.starts_with(p) {
                    return None;
                }
            }
            let target = match rec.value {
                RefValue::Deletion => return None,
                RefValue::Direct(oid) | RefValue::Peeled { oid, .. } => RefTarget::Direct(oid),
                RefValue::Symbolic(t) => match FullName::new(t) {
                    Ok(n) => RefTarget::Symbolic(n),
                    Err(e) => return Some(Err(e.into())),
                },
            };
            let full = match FullName::new(name) {
                Ok(n) => n,
                Err(e) => return Some(Err(e.into())),
            };
            Some(Ok(Reference { name: full, target }))
        }))
    }

    /// Collect all log records for a single ref name across the stack,
    /// newest-first. Tombstone records short-circuit (matches spec §4.6).
    pub fn read_reflog(&self, name: &FullName) -> Result<Vec<LogRecordRead>, RefError> {
        // Walk newest → oldest, collect entries until a tombstone.
        let mut out: Vec<LogRecordRead> = Vec::new();
        let mut hit_tombstone = false;
        for table in self.tables.iter().rev() {
            if hit_tombstone {
                break;
            }
            let records = table.iter_log_records()?;
            let mut matches: Vec<LogRecordRead> = records
                .into_iter()
                .filter(|r| r.name == name.as_str())
                .collect();
            // Records within a table are already sorted newest-first
            // (reverse_int64 keeps the order).
            for r in matches.drain(..) {
                if r.deleted {
                    hit_tombstone = true;
                    break;
                }
                out.push(r);
            }
        }
        Ok(out)
    }

    pub fn hash_kind(&self) -> HashKind {
        self.hash_kind
    }
}
