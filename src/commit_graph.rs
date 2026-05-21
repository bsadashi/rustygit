//! Commit-graph file (M15).
//!
//! A cached, on-disk binary representation of every reachable commit's
//! (parents, root tree, commit time, generation number). Git uses this to
//! accelerate revision walks, merge-base calculations, and ancestry queries
//! without zlib-inflating each commit object from the loose/pack store.
//!
//! The file lives at `<gitdir>/objects/info/commit-graph` and the byte layout
//! is documented in `git/Documentation/technical/commit-graph-format.adoc`
//! (the design doc) and `git/commit-graph.c` (the authoritative writer).
//!
//! Layout (high level):
//!
//! ```text
//! HEADER (8 bytes)
//!   "CGPH"                 4 bytes magic
//!   version                1 byte  (= 1)
//!   hash version           1 byte  (= 1 for sha1, = 2 for sha256)
//!   num chunks             1 byte
//!   num base graphs        1 byte  (0 for non-chained file)
//!
//! CHUNK LOOKUP TABLE
//!   (num_chunks + 1) entries, each 12 bytes: u32 BE id + u64 BE offset.
//!   The terminal entry has id == 0 and offset == total file size.
//!
//! CHUNKS (in lookup order)
//!   OIDF: 256 * u32 BE — fanout (cumulative commit count whose first oid byte <= i)
//!   OIDL: N * raw_oid (sorted ascending)
//!   CDAT: N * (hash_size + 16) bytes:
//!         root tree oid (hash_size bytes)
//!         parent1 edge (u32 BE)
//!         parent2 edge (u32 BE)
//!         packedDate[0] (u32 BE) — (gen << 2) | ((commit_time >> 32) & 0x3)
//!         packedDate[1] (u32 BE) — commit_time & 0xFFFFFFFF
//!   EDGE: optional u32 BE array for parents 3+ of octopus merges; the LAST
//!         entry of each run has the high bit (0x80000000) set.
//!
//! TRAILER
//!   raw hash (sha1 = 20 bytes, sha256 = 32 bytes) over every preceding byte.
//! ```
//!
//! Parent encoding inside CDAT:
//! - `0x70000000` (`GRAPH_PARENT_NONE`) — no parent at this slot.
//! - `0..N`                              — position of the parent in OIDL.
//! - `0x80000000 | edge_offset`          — parent2 slot only. Indicates "the
//!                                          parents starting at this offset into
//!                                          the EDGE chunk represent every
//!                                          parent after the first." The last
//!                                          parent in the EDGE run has bit
//!                                          0x80000000 set on its own index.
//!
//! Generation number: we emit topological-level v1 (1 + max(parent generations);
//! root = 1) and pack it into the high 30 bits of `packedDate[0]`. This is
//! `GENERATION_NUMBER_V1`; the corrected-commit-date v2 (GDA2/GDO2 chunks) is a
//! later optimization we don't need for the "git commit-graph verify" gate.

use std::path::{Path, PathBuf};

use memmap2::Mmap;
use thiserror::Error;

use crate::commit::{Commit, CommitError};
use crate::hash::{new_hasher, HashError, HashKind, Hasher, ObjectId};
use crate::reachable::{ReachableError, ReachableSet};
use crate::repo::Repository;

// -- on-disk constants -----------------------------------------------------

const MAGIC: [u8; 4] = *b"CGPH";
const VERSION: u8 = 1;

const CHUNK_OIDF: [u8; 4] = *b"OIDF";
const CHUNK_OIDL: [u8; 4] = *b"OIDL";
const CHUNK_CDAT: [u8; 4] = *b"CDAT";
const CHUNK_EDGE: [u8; 4] = *b"EDGE";

const GRAPH_HEADER_SIZE: usize = 8;
const CHUNK_TOC_ENTRY_SIZE: usize = 12; // 4 (id) + 8 (offset)
const GRAPH_FANOUT_SIZE: usize = 256 * 4;

/// Sentinel parent edge meaning "no parent in this slot".
const GRAPH_PARENT_NONE: u32 = 0x7000_0000;
/// Bit set in parent2's slot meaning "the second-and-later parents are encoded
/// in the EDGE chunk starting at the offset in the low 31 bits".
const GRAPH_EXTRA_EDGES_NEEDED: u32 = 0x8000_0000;
/// Bit set on the LAST u32 in an EDGE-chunk run, marking the end of that run.
const GRAPH_LAST_EDGE: u32 = 0x8000_0000;
/// Cap for the v1 topological-level generation number (30 bits).
const GENERATION_NUMBER_V1_MAX: u32 = 0x3FFF_FFFF;

// -- errors ----------------------------------------------------------------

#[derive(Error, Debug)]
pub enum CommitGraphError {
    #[error(transparent)]
    Odb(#[from] crate::odb::OdbError),
    #[error(transparent)]
    Commit(#[from] CommitError),
    #[error(transparent)]
    Reachable(#[from] ReachableError),
    #[error(transparent)]
    Hash(#[from] HashError),
    #[error("io on {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("malformed commit-graph: {0}")]
    Malformed(&'static str),
    #[error("bad signature: expected 'CGPH', got {0:?}")]
    BadSignature([u8; 4]),
    #[error("unsupported version: {0}")]
    UnsupportedVersion(u8),
    #[error("checksum mismatch")]
    ChecksumMismatch,
}

fn io_err(path: &Path, source: std::io::Error) -> CommitGraphError {
    CommitGraphError::Io {
        path: path.to_path_buf(),
        source,
    }
}

// -- public WriteResult ----------------------------------------------------

#[derive(Debug)]
pub struct WriteResult {
    pub commit_count: usize,
    pub path: PathBuf,
    pub bytes_written: u64,
}

// -- write entry point -----------------------------------------------------

/// Walk every commit reachable from refs / HEAD / index, sort by oid, and emit
/// `<gitdir>/objects/info/commit-graph`. Overwrites any existing file.
///
/// A no-commit reachable set still produces a valid (zero-commit) file, mirroring
/// `git commit-graph write`'s behavior when run in a fresh repo.
pub fn write(repo: &Repository) -> Result<WriteResult, CommitGraphError> {
    let hash_kind = repo.hash_kind();
    let hash_size = hash_kind.raw_len();

    // 1. Find every reachable commit, sorted by raw oid bytes (ascending).
    //    `ReachableSet::mark_all` yields *all* reachable objects; we filter to
    //    commits. The resulting BTreeSet iteration order is already ascending
    //    by raw bytes, which is what OIDF/OIDL require.
    let reach = ReachableSet::mark_all(repo)?;
    let mut commits: Vec<(ObjectId, Commit)> = Vec::new();
    for oid in &reach.oids {
        let raw = repo.odb().read(oid)?;
        if matches!(raw.kind, crate::object::ObjectKind::Commit) {
            let c = Commit::parse(&raw.data, hash_kind)?;
            commits.push((*oid, c));
        }
    }
    // BTreeSet iteration is by ObjectId's Ord (kind tag, then bytes). Since
    // every oid in this repo shares the same kind, the order matches raw-byte
    // ascending — exactly what OIDF/OIDL want.
    commits.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));

    let n = commits.len();

    // 2. Build an oid → index map for parent encoding.
    let mut pos_of: std::collections::HashMap<ObjectId, u32> =
        std::collections::HashMap::with_capacity(n);
    for (i, (oid, _)) in commits.iter().enumerate() {
        pos_of.insert(*oid, i as u32);
    }

    // 3. Compute generation numbers (topological levels). A commit's generation
    //    is `1 + max(parent.gen)` — capped at GENERATION_NUMBER_V1_MAX. We have
    //    to walk in a topological order. Easiest: iterate repeatedly until no
    //    change. With small N this is fine; for large N a Kahn-style ordering
    //    would be faster, but we're not optimizing this path yet.
    //
    //    A safer single-pass approach: depth-first compute with memoization.
    let mut generation: Vec<u32> = vec![0; n];
    {
        // For each commit, compute generation via DFS over the parent index map.
        // We must handle commits whose parents aren't in the graph (shouldn't
        // happen on a reachable-closure walk, but defensively treat as gen 0
        // and thus give the commit gen 1).
        fn compute_gen(
            i: u32,
            commits: &[(ObjectId, Commit)],
            pos_of: &std::collections::HashMap<ObjectId, u32>,
            gen: &mut [u32],
            in_progress: &mut [bool],
        ) -> u32 {
            let i_us = i as usize;
            if gen[i_us] != 0 {
                return gen[i_us];
            }
            // Cycle guard (commit graphs are DAGs in practice).
            if in_progress[i_us] {
                return 1;
            }
            in_progress[i_us] = true;
            let mut max_parent_gen: u32 = 0;
            for p in &commits[i_us].1.parents {
                if let Some(&pi) = pos_of.get(p) {
                    let g = compute_gen(pi, commits, pos_of, gen, in_progress);
                    if g > max_parent_gen {
                        max_parent_gen = g;
                    }
                }
            }
            in_progress[i_us] = false;
            let mut g = max_parent_gen.saturating_add(1);
            if g > GENERATION_NUMBER_V1_MAX {
                g = GENERATION_NUMBER_V1_MAX;
            }
            gen[i_us] = g;
            g
        }
        let mut in_progress = vec![false; n];
        for i in 0..(n as u32) {
            compute_gen(i, &commits, &pos_of, &mut generation, &mut in_progress);
        }
    }

    // 4. Pre-compute the EDGE chunk contents. For every commit with > 2 parents,
    //    we emit (parents[1..]) as u32 BE indices into OIDL, with the last one
    //    OR'd with 0x80000000. Then in CDAT, that commit's parent2 slot becomes
    //    0x80000000 | (this commit's offset into EDGE in u32 units).
    let mut edge_chunk: Vec<u32> = Vec::new();
    let mut edge_offset_for_commit: Vec<Option<u32>> = vec![None; n];
    for (i, (_oid, c)) in commits.iter().enumerate() {
        if c.parents.len() > 2 {
            edge_offset_for_commit[i] = Some(edge_chunk.len() as u32);
            // Write parents[1..] (i.e. every parent after the first).
            let mut iter = c.parents.iter().skip(1).peekable();
            while let Some(p) = iter.next() {
                let pos = pos_of.get(p).copied().ok_or(CommitGraphError::Malformed(
                    "commit has parent not in reachable set",
                ))?;
                let is_last = iter.peek().is_none();
                let value = if is_last { pos | GRAPH_LAST_EDGE } else { pos };
                edge_chunk.push(value);
            }
        }
    }
    let has_edge = !edge_chunk.is_empty();

    // 5. Lay out the chunk table-of-contents. Always emit OIDF, OIDL, CDAT;
    //    optionally EDGE. Trailing zero entry for total file size.
    let mut chunks: Vec<(&[u8; 4], u64)> = Vec::new(); // (id, size)
    let oidf_size = GRAPH_FANOUT_SIZE as u64;
    let oidl_size = (n * hash_size) as u64;
    let cdat_size = (n * (hash_size + 16)) as u64;
    let edge_size = (edge_chunk.len() * 4) as u64;
    chunks.push((&CHUNK_OIDF, oidf_size));
    chunks.push((&CHUNK_OIDL, oidl_size));
    chunks.push((&CHUNK_CDAT, cdat_size));
    if has_edge {
        chunks.push((&CHUNK_EDGE, edge_size));
    }
    let num_chunks = chunks.len();

    // Absolute offsets of each chunk: header + (num_chunks + 1) TOC entries.
    let toc_size = (num_chunks + 1) * CHUNK_TOC_ENTRY_SIZE;
    let first_chunk_offset = GRAPH_HEADER_SIZE + toc_size;
    let mut chunk_offsets: Vec<u64> = Vec::with_capacity(num_chunks);
    let mut cursor = first_chunk_offset as u64;
    for (_, sz) in &chunks {
        chunk_offsets.push(cursor);
        cursor += *sz;
    }
    let post_chunks_offset = cursor;

    // 6. Build the file body in memory, then write atomically and hash.
    let mut body: Vec<u8> = Vec::with_capacity(post_chunks_offset as usize);

    // ---- HEADER -----
    body.extend_from_slice(&MAGIC);
    body.push(VERSION);
    body.push(oid_version(hash_kind));
    body.push(num_chunks as u8);
    body.push(0u8); // no base graphs (non-split file)

    // ---- TOC -----
    for (i, (id, _sz)) in chunks.iter().enumerate() {
        body.extend_from_slice(*id);
        body.extend_from_slice(&chunk_offsets[i].to_be_bytes());
    }
    // Terminal zero entry; its offset is the total chunks-end offset (used by
    // readers to compute the LAST chunk's length).
    body.extend_from_slice(&[0u8; 4]);
    body.extend_from_slice(&post_chunks_offset.to_be_bytes());

    // ---- OIDF (256 * u32 BE fanout) -----
    let mut fanout = [0u32; 256];
    for (oid, _) in &commits {
        let b0 = oid.as_bytes()[0] as usize;
        for bucket in fanout.iter_mut().skip(b0) {
            *bucket += 1;
        }
    }
    for v in &fanout {
        body.extend_from_slice(&v.to_be_bytes());
    }

    // ---- OIDL (sorted oids) -----
    for (oid, _) in &commits {
        body.extend_from_slice(oid.as_bytes());
    }

    // ---- CDAT -----
    for (i, (_oid, c)) in commits.iter().enumerate() {
        // Root tree oid.
        body.extend_from_slice(c.tree.as_bytes());

        // Parent edge 1.
        let edge1 = if c.parents.is_empty() {
            GRAPH_PARENT_NONE
        } else {
            *pos_of
                .get(&c.parents[0])
                .ok_or(CommitGraphError::Malformed(
                    "commit's first parent not in reachable set",
                ))?
        };
        body.extend_from_slice(&edge1.to_be_bytes());

        // Parent edge 2.
        let edge2 = match c.parents.len() {
            0 | 1 => GRAPH_PARENT_NONE,
            2 => *pos_of
                .get(&c.parents[1])
                .ok_or(CommitGraphError::Malformed(
                    "commit's second parent not in reachable set",
                ))?,
            _ => {
                let off = edge_offset_for_commit[i].expect("octopus has edge slot");
                GRAPH_EXTRA_EDGES_NEEDED | off
            }
        };
        body.extend_from_slice(&edge2.to_be_bytes());

        // Packed gen + commit_time.
        // packedDate[0] high 32 bits: (gen << 2) | ((time >> 32) & 0x3)
        // packedDate[1] low 32 bits:  time & 0xFFFFFFFF
        // time is i64 in our `Time`; mask to 34 bits per git's convention.
        let commit_time_u64 = (c.committer.when.seconds as u64) & ((1u64 << 34) - 1);
        let gen = generation[i];
        let packed_hi = (gen << 2) | (((commit_time_u64 >> 32) & 0x3) as u32);
        let packed_lo = (commit_time_u64 & 0xFFFF_FFFF) as u32;
        body.extend_from_slice(&packed_hi.to_be_bytes());
        body.extend_from_slice(&packed_lo.to_be_bytes());
    }

    // ---- EDGE -----
    if has_edge {
        for v in &edge_chunk {
            body.extend_from_slice(&v.to_be_bytes());
        }
    }

    // 7. Compute the trailer hash over everything written so far.
    let mut hasher: Box<dyn Hasher> = new_hasher(hash_kind);
    hasher.update(&body);
    let trailer = hasher.finalize();
    body.extend_from_slice(trailer.as_bytes());

    // 8. Write atomically: write to a temp sibling, then rename.
    let out_dir = repo.objects_dir().join("info");
    std::fs::create_dir_all(&out_dir).map_err(|e| io_err(&out_dir, e))?;
    let final_path = out_dir.join("commit-graph");
    let tmp_path = out_dir.join("commit-graph.tmp");

    {
        use std::io::Write as _;
        let mut f = std::fs::File::create(&tmp_path).map_err(|e| io_err(&tmp_path, e))?;
        f.write_all(&body).map_err(|e| io_err(&tmp_path, e))?;
        f.sync_all().map_err(|e| io_err(&tmp_path, e))?;
    }
    std::fs::rename(&tmp_path, &final_path).map_err(|e| io_err(&final_path, e))?;

    Ok(WriteResult {
        commit_count: n,
        path: final_path,
        bytes_written: body.len() as u64,
    })
}

fn oid_version(k: HashKind) -> u8 {
    match k {
        HashKind::Sha1 => 1,
        HashKind::Sha256 => 2,
    }
}

// -- read / verify ---------------------------------------------------------

/// Memory-mapped reader for a commit-graph file.
pub struct CommitGraph {
    bytes: Mmap,
    hash_kind: HashKind,
    oidf_off: usize,
    oidl_off: usize,
    cdat_off: usize,
    edge_off: Option<usize>,
    count: u32,
    path: PathBuf,
}

impl CommitGraph {
    /// Open and validate (header + chunk TOC + presence of required chunks).
    pub fn open(path: impl AsRef<Path>, hash_kind: HashKind) -> Result<Self, CommitGraphError> {
        let path = path.as_ref().to_path_buf();
        let file = std::fs::File::open(&path).map_err(|e| io_err(&path, e))?;
        let bytes = unsafe { Mmap::map(&file) }.map_err(|e| io_err(&path, e))?;

        let hash_size = hash_kind.raw_len();
        let min_len = GRAPH_HEADER_SIZE + CHUNK_TOC_ENTRY_SIZE + hash_size;
        if bytes.len() < min_len {
            return Err(CommitGraphError::Malformed(
                "file shorter than minimum header",
            ));
        }

        // -- header --
        let sig: [u8; 4] = bytes[0..4].try_into().unwrap();
        if sig != MAGIC {
            return Err(CommitGraphError::BadSignature(sig));
        }
        let version = bytes[4];
        if version != VERSION {
            return Err(CommitGraphError::UnsupportedVersion(version));
        }
        let hash_ver = bytes[5];
        let expected_hash_ver = oid_version(hash_kind);
        if hash_ver != expected_hash_ver {
            return Err(CommitGraphError::Malformed(
                "hash version in header doesn't match repository",
            ));
        }
        let num_chunks = bytes[6] as usize;
        // bytes[7] = num_base_graphs (we don't read split chains yet).

        // -- chunk lookup table --
        let toc_total = (num_chunks + 1) * CHUNK_TOC_ENTRY_SIZE;
        if bytes.len() < GRAPH_HEADER_SIZE + toc_total + hash_size {
            return Err(CommitGraphError::Malformed("toc extends past file"));
        }
        let mut oidf_off: Option<usize> = None;
        let mut oidl_off: Option<usize> = None;
        let mut cdat_off: Option<usize> = None;
        let mut edge_off: Option<usize> = None;
        // Track each chunk's (id, offset) so we can compute sizes via the next
        // entry's offset.
        let mut toc: Vec<(u32, u64)> = Vec::with_capacity(num_chunks + 1);
        for i in 0..=num_chunks {
            let entry_off = GRAPH_HEADER_SIZE + i * CHUNK_TOC_ENTRY_SIZE;
            let id = u32::from_be_bytes(bytes[entry_off..entry_off + 4].try_into().unwrap());
            let off = u64::from_be_bytes(bytes[entry_off + 4..entry_off + 12].try_into().unwrap());
            toc.push((id, off));
        }
        let file_end = bytes.len() as u64;
        let trailer_start = file_end - hash_size as u64;
        // Each chunk_offset must point within [GRAPH_HEADER_SIZE+toc_total, trailer_start].
        for w in toc.windows(2) {
            let (id, off) = w[0];
            let (_, next_off) = w[1];
            if next_off < off || next_off > trailer_start {
                return Err(CommitGraphError::Malformed("chunk offsets out of range"));
            }
            // Map the chunk to our typed fields.
            let id_bytes = id.to_be_bytes();
            let off_usize = off as usize;
            if id_bytes == *b"OIDF" {
                oidf_off = Some(off_usize);
            } else if id_bytes == *b"OIDL" {
                oidl_off = Some(off_usize);
            } else if id_bytes == *b"CDAT" {
                cdat_off = Some(off_usize);
            } else if id_bytes == *b"EDGE" {
                edge_off = Some(off_usize);
            }
            // Other chunks (GDA2, BIDX, BDAT, BASE...) are ignored for read.
        }
        let terminal = toc.last().expect("toc has terminal");
        if terminal.0 != 0 {
            return Err(CommitGraphError::Malformed(
                "terminal toc entry has non-zero id",
            ));
        }

        let oidf_off = oidf_off.ok_or(CommitGraphError::Malformed("missing OIDF chunk"))?;
        let oidl_off = oidl_off.ok_or(CommitGraphError::Malformed("missing OIDL chunk"))?;
        let cdat_off = cdat_off.ok_or(CommitGraphError::Malformed("missing CDAT chunk"))?;

        // The total commit count is the last entry in the OIDF table.
        let last_fanout_off = oidf_off + 255 * 4;
        if last_fanout_off + 4 > bytes.len() {
            return Err(CommitGraphError::Malformed("OIDF chunk truncated"));
        }
        let count = u32::from_be_bytes(
            bytes[last_fanout_off..last_fanout_off + 4]
                .try_into()
                .unwrap(),
        );

        // Sanity: OIDL and CDAT must be big enough for `count` entries.
        let oidl_needed = (count as usize) * hash_size;
        let cdat_needed = (count as usize) * (hash_size + 16);
        if oidl_off + oidl_needed > trailer_start as usize {
            return Err(CommitGraphError::Malformed(
                "OIDL chunk too short for fanout count",
            ));
        }
        if cdat_off + cdat_needed > trailer_start as usize {
            return Err(CommitGraphError::Malformed(
                "CDAT chunk too short for fanout count",
            ));
        }

        Ok(Self {
            bytes,
            hash_kind,
            oidf_off,
            oidl_off,
            cdat_off,
            edge_off,
            count,
            path,
        })
    }

    pub fn count(&self) -> u32 {
        self.count
    }

    /// Look up a commit oid via fanout + binary search.
    pub fn position_of(&self, oid: &ObjectId) -> Option<u32> {
        if oid.kind() != self.hash_kind {
            return None;
        }
        let raw = oid.as_bytes();
        let b0 = raw[0] as usize;
        let lo_bound = if b0 == 0 {
            0
        } else {
            let off = self.oidf_off + (b0 - 1) * 4;
            u32::from_be_bytes(self.bytes[off..off + 4].try_into().unwrap())
        };
        let hi_bound = {
            let off = self.oidf_off + b0 * 4;
            u32::from_be_bytes(self.bytes[off..off + 4].try_into().unwrap())
        };
        let hash_size = self.hash_kind.raw_len();
        let mut lo = lo_bound as usize;
        let mut hi = hi_bound as usize; // exclusive
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let entry_off = self.oidl_off + mid * hash_size;
            let entry = &self.bytes[entry_off..entry_off + hash_size];
            match raw.cmp(entry) {
                std::cmp::Ordering::Equal => return Some(mid as u32),
                std::cmp::Ordering::Less => hi = mid,
                std::cmp::Ordering::Greater => lo = mid + 1,
            }
        }
        None
    }

    /// Parse CDAT entry `idx` and resolve all parents (via OIDL + EDGE).
    pub fn commit_at(&self, idx: u32) -> Result<CommitGraphEntry, CommitGraphError> {
        if idx >= self.count {
            return Err(CommitGraphError::Malformed("commit index out of range"));
        }
        let hash_size = self.hash_kind.raw_len();
        let entry_off = self.cdat_off + (idx as usize) * (hash_size + 16);

        let oid = self.oid_at(idx)?;
        let tree = ObjectId::from_bytes(
            self.hash_kind,
            &self.bytes[entry_off..entry_off + hash_size],
        )?;
        let p1 = u32::from_be_bytes(
            self.bytes[entry_off + hash_size..entry_off + hash_size + 4]
                .try_into()
                .unwrap(),
        );
        let p2 = u32::from_be_bytes(
            self.bytes[entry_off + hash_size + 4..entry_off + hash_size + 8]
                .try_into()
                .unwrap(),
        );
        let packed_hi = u32::from_be_bytes(
            self.bytes[entry_off + hash_size + 8..entry_off + hash_size + 12]
                .try_into()
                .unwrap(),
        );
        let packed_lo = u32::from_be_bytes(
            self.bytes[entry_off + hash_size + 12..entry_off + hash_size + 16]
                .try_into()
                .unwrap(),
        );

        let generation = packed_hi >> 2;
        let time_hi = (packed_hi & 0x3) as u64;
        let commit_time = (time_hi << 32) | (packed_lo as u64);

        let mut parents: Vec<ObjectId> = Vec::new();
        if p1 != GRAPH_PARENT_NONE {
            parents.push(self.oid_at(p1)?);
        }
        if p2 != GRAPH_PARENT_NONE {
            if p2 & GRAPH_EXTRA_EDGES_NEEDED != 0 {
                // p2 is "look in EDGE starting at offset (p2 & 0x7fffffff)".
                let edge_off = self.edge_off.ok_or(CommitGraphError::Malformed(
                    "CDAT references EDGE but EDGE missing",
                ))?;
                let start = (p2 & 0x7FFF_FFFF) as usize;
                let mut k = start;
                loop {
                    let off = edge_off + k * 4;
                    if off + 4 > self.bytes.len() {
                        return Err(CommitGraphError::Malformed("EDGE chunk truncated"));
                    }
                    let v = u32::from_be_bytes(self.bytes[off..off + 4].try_into().unwrap());
                    let last = (v & GRAPH_LAST_EDGE) != 0;
                    let pos = v & 0x7FFF_FFFF;
                    parents.push(self.oid_at(pos)?);
                    if last {
                        break;
                    }
                    k += 1;
                }
            } else {
                parents.push(self.oid_at(p2)?);
            }
        }

        Ok(CommitGraphEntry {
            oid,
            tree,
            parents,
            generation,
            commit_time,
        })
    }

    /// Verify trailer hash + invariants. Returns Ok(()) if every check passes.
    pub fn verify(&self) -> Result<(), CommitGraphError> {
        let hash_size = self.hash_kind.raw_len();
        let body_end = self.bytes.len() - hash_size;
        let mut hasher = new_hasher(self.hash_kind);
        hasher.update(&self.bytes[..body_end]);
        let expect = hasher.finalize();
        let stored = ObjectId::from_bytes(self.hash_kind, &self.bytes[body_end..])?;
        if expect.as_bytes() != stored.as_bytes() {
            return Err(CommitGraphError::ChecksumMismatch);
        }
        // Fanout monotonicity + final entry equals total count.
        let mut prev: u32 = 0;
        for i in 0..256 {
            let off = self.oidf_off + i * 4;
            let v = u32::from_be_bytes(self.bytes[off..off + 4].try_into().unwrap());
            if v < prev {
                return Err(CommitGraphError::Malformed("fanout not monotonic"));
            }
            prev = v;
        }
        if prev != self.count {
            return Err(CommitGraphError::Malformed(
                "fanout final entry mismatches count",
            ));
        }
        // OIDL sorted ascending (strictly — no duplicates).
        for i in 1..(self.count as usize) {
            let a_off = self.oidl_off + (i - 1) * hash_size;
            let b_off = self.oidl_off + i * hash_size;
            if self.bytes[a_off..a_off + hash_size] >= self.bytes[b_off..b_off + hash_size] {
                return Err(CommitGraphError::Malformed("OIDL not strictly sorted"));
            }
        }
        // Each commit's first-byte bucket must match what fanout claims.
        for i in 0..(self.count as usize) {
            let off = self.oidl_off + i * hash_size;
            let b0 = self.bytes[off] as usize;
            let fan_off = self.oidf_off + b0 * 4;
            let high = u32::from_be_bytes(self.bytes[fan_off..fan_off + 4].try_into().unwrap());
            if (i as u32) >= high {
                return Err(CommitGraphError::Malformed("oid out of fanout bucket"));
            }
        }
        Ok(())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Helper: read the `i`-th oid from OIDL.
    fn oid_at(&self, i: u32) -> Result<ObjectId, CommitGraphError> {
        if i >= self.count {
            return Err(CommitGraphError::Malformed("oid index out of range"));
        }
        let hash_size = self.hash_kind.raw_len();
        let off = self.oidl_off + (i as usize) * hash_size;
        Ok(ObjectId::from_bytes(
            self.hash_kind,
            &self.bytes[off..off + hash_size],
        )?)
    }
}

#[derive(Debug, Clone)]
pub struct CommitGraphEntry {
    pub oid: ObjectId,
    pub tree: ObjectId,
    pub parents: Vec<ObjectId>,
    pub generation: u32,
    pub commit_time: u64,
}

// -- tests -----------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::process::Command;
    use tempfile::tempdir;

    // ---- git CLI helpers -----------------------------------------------

    fn has_git() -> bool {
        Command::new("git")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    fn git(dir: &Path, args: &[&str]) -> std::process::Output {
        let out = Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .expect("run git");
        if !out.status.success() {
            panic!(
                "git {:?} failed: stdout={:?} stderr={:?}",
                args,
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr),
            );
        }
        out
    }

    fn git_env(dir: &Path, args: &[&str], envs: &[(&str, &str)]) -> std::process::Output {
        let mut cmd = Command::new("git");
        cmd.args(args).current_dir(dir);
        for (k, v) in envs {
            cmd.env(k, v);
        }
        let out = cmd.output().expect("run git");
        if !out.status.success() {
            panic!(
                "git {:?} failed: stdout={:?} stderr={:?}",
                args,
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr),
            );
        }
        out
    }

    fn init_repo(dir: &Path) {
        git(dir, &["init", "-q", "-b", "main", "."]);
        git(dir, &["config", "user.name", "Test"]);
        git(dir, &["config", "user.email", "test@example.com"]);
    }

    fn init_repo_sha256(dir: &Path) -> bool {
        // Older gits may not support --object-format=sha256. Return false if init fails.
        let out = Command::new("git")
            .args(["init", "-q", "-b", "main", "--object-format=sha256", "."])
            .current_dir(dir)
            .output()
            .expect("run git init");
        if !out.status.success() {
            return false;
        }
        git(dir, &["config", "user.name", "Test"]);
        git(dir, &["config", "user.email", "test@example.com"]);
        true
    }

    fn write_and_add(dir: &Path, path: &str, content: &str) {
        let p = dir.join(path);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(p, content).unwrap();
        git(dir, &["add", path]);
    }

    fn make_commit(dir: &Path, msg: &str) -> String {
        git_env(
            dir,
            &["commit", "-q", "-m", msg, "--allow-empty"],
            &[
                ("GIT_AUTHOR_DATE", "1700000000 +0000"),
                ("GIT_COMMITTER_DATE", "1700000000 +0000"),
            ],
        );
        let out = git(dir, &["rev-parse", "HEAD"]).stdout;
        String::from_utf8(out).unwrap().trim().to_string()
    }

    fn rev_parse(dir: &Path, expr: &str) -> String {
        let out = git(dir, &["rev-parse", expr]).stdout;
        String::from_utf8(out).unwrap().trim().to_string()
    }

    fn make_octopus_commit(dir: &Path, parents: &[&str], msg: &str) -> String {
        // Compose a `commit-tree` command with multiple `-p` flags.
        let tree = rev_parse(dir, "HEAD^{tree}");
        let mut args: Vec<String> = vec!["commit-tree".into(), tree, "-m".into(), msg.into()];
        for p in parents {
            args.push("-p".into());
            args.push((*p).to_string());
        }
        let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        let envs = &[
            ("GIT_AUTHOR_NAME", "Test"),
            ("GIT_AUTHOR_EMAIL", "t@e.x"),
            ("GIT_AUTHOR_DATE", "1700000000 +0000"),
            ("GIT_COMMITTER_NAME", "Test"),
            ("GIT_COMMITTER_EMAIL", "t@e.x"),
            ("GIT_COMMITTER_DATE", "1700000000 +0000"),
        ];
        let out = git_env(dir, &arg_refs, envs);
        let oid_str = String::from_utf8(out.stdout).unwrap().trim().to_string();
        // Point main at it so `mark_all` picks it up.
        git(dir, &["update-ref", "refs/heads/main", &oid_str]);
        oid_str
    }

    // ---- the tests -----------------------------------------------------

    #[test]
    fn empty_repo_writes_zero_commit_file() {
        if !has_git() {
            return;
        }
        let tmp = tempdir().unwrap();
        let dir = tmp.path();
        git(dir, &["init", "-q", "-b", "main", "."]);
        let repo = Repository::discover(dir).unwrap();

        let result = write(&repo).unwrap();
        assert_eq!(result.commit_count, 0);
        // We should still be able to open + verify the empty file.
        let cg = CommitGraph::open(&result.path, HashKind::Sha1).unwrap();
        assert_eq!(cg.count(), 0);
        cg.verify().unwrap();
    }

    #[test]
    fn single_commit_round_trips_with_generation_one() {
        if !has_git() {
            return;
        }
        let tmp = tempdir().unwrap();
        let dir = tmp.path();
        init_repo(dir);
        write_and_add(dir, "a.txt", "alpha\n");
        let c1 = make_commit(dir, "c1");

        let repo = Repository::discover(dir).unwrap();
        let result = write(&repo).unwrap();
        assert_eq!(result.commit_count, 1);

        let cg = CommitGraph::open(&result.path, HashKind::Sha1).unwrap();
        let oid = ObjectId::parse_hex(HashKind::Sha1, &c1).unwrap();
        let pos = cg.position_of(&oid).expect("commit must be present");
        let entry = cg.commit_at(pos).unwrap();
        assert_eq!(entry.generation, 1);
        assert_eq!(entry.commit_time, 1_700_000_000);
        assert!(entry.parents.is_empty());
        cg.verify().unwrap();
    }

    #[test]
    fn linear_chain_of_five_generations_one_through_five() {
        if !has_git() {
            return;
        }
        let tmp = tempdir().unwrap();
        let dir = tmp.path();
        init_repo(dir);
        let mut oids: Vec<String> = Vec::new();
        for i in 1..=5 {
            write_and_add(dir, "a.txt", &format!("v{i}\n"));
            oids.push(make_commit(dir, &format!("c{i}")));
        }
        let repo = Repository::discover(dir).unwrap();
        let result = write(&repo).unwrap();
        assert_eq!(result.commit_count, 5);

        let cg = CommitGraph::open(&result.path, HashKind::Sha1).unwrap();
        for (i, hex) in oids.iter().enumerate() {
            let oid = ObjectId::parse_hex(HashKind::Sha1, hex).unwrap();
            let pos = cg.position_of(&oid).expect("commit must be present");
            let entry = cg.commit_at(pos).unwrap();
            assert_eq!(entry.generation, (i as u32) + 1, "commit {hex} (i={i})");
        }
        cg.verify().unwrap();
    }

    #[test]
    fn y_fork_generations() {
        // base <- A <- C
        //     \- B -/  (C has two parents, A and B)
        if !has_git() {
            return;
        }
        let tmp = tempdir().unwrap();
        let dir = tmp.path();
        init_repo(dir);
        write_and_add(dir, "f.txt", "base\n");
        let base = make_commit(dir, "base");

        // A on top of base
        write_and_add(dir, "f.txt", "a\n");
        let a = make_commit(dir, "a");
        // B branches from base
        git(dir, &["checkout", "-q", "-b", "topic", &base]);
        write_and_add(dir, "f.txt", "b\n");
        let b = make_commit(dir, "b");
        // Merge: create C with parents A, B.
        let tree = rev_parse(dir, "HEAD^{tree}");
        let envs = &[
            ("GIT_AUTHOR_NAME", "Test"),
            ("GIT_AUTHOR_EMAIL", "t@e.x"),
            ("GIT_AUTHOR_DATE", "1700000000 +0000"),
            ("GIT_COMMITTER_NAME", "Test"),
            ("GIT_COMMITTER_EMAIL", "t@e.x"),
            ("GIT_COMMITTER_DATE", "1700000000 +0000"),
        ];
        let out = git_env(
            dir,
            &["commit-tree", &tree, "-p", &a, "-p", &b, "-m", "merge"],
            envs,
        );
        let c = String::from_utf8(out.stdout).unwrap().trim().to_string();
        git(dir, &["update-ref", "refs/heads/main", &c]);

        let repo = Repository::discover(dir).unwrap();
        let result = write(&repo).unwrap();
        assert_eq!(result.commit_count, 4); // base, a, b, c

        let cg = CommitGraph::open(&result.path, HashKind::Sha1).unwrap();
        let base_pos = cg
            .position_of(&ObjectId::parse_hex(HashKind::Sha1, &base).unwrap())
            .unwrap();
        let a_pos = cg
            .position_of(&ObjectId::parse_hex(HashKind::Sha1, &a).unwrap())
            .unwrap();
        let b_pos = cg
            .position_of(&ObjectId::parse_hex(HashKind::Sha1, &b).unwrap())
            .unwrap();
        let c_pos = cg
            .position_of(&ObjectId::parse_hex(HashKind::Sha1, &c).unwrap())
            .unwrap();

        assert_eq!(cg.commit_at(base_pos).unwrap().generation, 1);
        assert_eq!(cg.commit_at(a_pos).unwrap().generation, 2);
        assert_eq!(cg.commit_at(b_pos).unwrap().generation, 2);
        let c_entry = cg.commit_at(c_pos).unwrap();
        assert_eq!(c_entry.generation, 3);
        assert_eq!(c_entry.parents.len(), 2);
        cg.verify().unwrap();
    }

    #[test]
    fn octopus_merge_uses_edge_chunk() {
        if !has_git() {
            return;
        }
        let tmp = tempdir().unwrap();
        let dir = tmp.path();
        init_repo(dir);
        write_and_add(dir, "f.txt", "base\n");
        let base = make_commit(dir, "base");
        // Three siblings off `base`.
        git(dir, &["checkout", "-q", "-b", "t1", &base]);
        write_and_add(dir, "f.txt", "t1\n");
        let t1 = make_commit(dir, "t1");
        git(dir, &["checkout", "-q", "-b", "t2", &base]);
        write_and_add(dir, "f.txt", "t2\n");
        let t2 = make_commit(dir, "t2");
        git(dir, &["checkout", "-q", "-b", "t3", &base]);
        write_and_add(dir, "f.txt", "t3\n");
        let t3 = make_commit(dir, "t3");
        // Octopus merge: parents = [t1, t2, t3].
        let octo = make_octopus_commit(dir, &[&t1, &t2, &t3], "octo");

        let repo = Repository::discover(dir).unwrap();
        let result = write(&repo).unwrap();
        // base + t1 + t2 + t3 + octo = 5
        assert_eq!(result.commit_count, 5);

        let cg = CommitGraph::open(&result.path, HashKind::Sha1).unwrap();
        assert!(
            cg.edge_off.is_some(),
            "octopus should produce an EDGE chunk"
        );
        let octo_pos = cg
            .position_of(&ObjectId::parse_hex(HashKind::Sha1, &octo).unwrap())
            .unwrap();
        let entry = cg.commit_at(octo_pos).unwrap();
        assert_eq!(entry.parents.len(), 3, "octopus must reconstruct 3 parents");
        // Generation = 1 + max(parent_gens) = 1 + 2 = 3.
        assert_eq!(entry.generation, 3);
        cg.verify().unwrap();
    }

    #[test]
    fn git_commit_graph_verify_accepts_our_output() {
        // The load-bearing acceptance test. Build a small repo, write our
        // commit-graph, then run `git commit-graph verify` and require exit 0.
        if !has_git() {
            return;
        }
        let tmp = tempdir().unwrap();
        let dir = tmp.path();
        init_repo(dir);
        write_and_add(dir, "a.txt", "v1\n");
        let _c1 = make_commit(dir, "c1");
        write_and_add(dir, "a.txt", "v2\n");
        let _c2 = make_commit(dir, "c2");
        write_and_add(dir, "a.txt", "v3\n");
        let _c3 = make_commit(dir, "c3");
        // Branch off to give us a merge too.
        git(dir, &["checkout", "-q", "-b", "topic", "HEAD~1"]);
        write_and_add(dir, "b.txt", "side\n");
        let _t = make_commit(dir, "t");
        // Back to main and merge `topic`.
        git(dir, &["checkout", "-q", "main"]);
        git_env(
            dir,
            &["merge", "-q", "--no-ff", "topic", "-m", "merge topic"],
            &[
                ("GIT_AUTHOR_DATE", "1700000000 +0000"),
                ("GIT_COMMITTER_DATE", "1700000000 +0000"),
            ],
        );

        let repo = Repository::discover(dir).unwrap();
        let _ = write(&repo).unwrap();

        // Now invoke real git.
        let out = Command::new("git")
            .args(["commit-graph", "verify"])
            .current_dir(dir)
            .output()
            .expect("run git commit-graph verify");
        if !out.status.success() {
            panic!(
                "git commit-graph verify rejected our file:\nstdout={}\nstderr={}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr),
            );
        }
    }

    #[test]
    fn git_commit_graph_verify_accepts_octopus() {
        if !has_git() {
            return;
        }
        let tmp = tempdir().unwrap();
        let dir = tmp.path();
        init_repo(dir);
        write_and_add(dir, "f.txt", "base\n");
        let base = make_commit(dir, "base");
        git(dir, &["checkout", "-q", "-b", "t1", &base]);
        write_and_add(dir, "f.txt", "t1\n");
        let t1 = make_commit(dir, "t1");
        git(dir, &["checkout", "-q", "-b", "t2", &base]);
        write_and_add(dir, "f.txt", "t2\n");
        let t2 = make_commit(dir, "t2");
        git(dir, &["checkout", "-q", "-b", "t3", &base]);
        write_and_add(dir, "f.txt", "t3\n");
        let t3 = make_commit(dir, "t3");
        let _octo = make_octopus_commit(dir, &[&t1, &t2, &t3], "octo");

        let repo = Repository::discover(dir).unwrap();
        let _ = write(&repo).unwrap();

        let out = Command::new("git")
            .args(["commit-graph", "verify"])
            .current_dir(dir)
            .output()
            .expect("run git commit-graph verify");
        if !out.status.success() {
            panic!(
                "git commit-graph verify (octopus) rejected our file:\nstdout={}\nstderr={}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr),
            );
        }
    }

    #[test]
    fn tampering_trailer_triggers_checksum_mismatch() {
        if !has_git() {
            return;
        }
        let tmp = tempdir().unwrap();
        let dir = tmp.path();
        init_repo(dir);
        write_and_add(dir, "a.txt", "x\n");
        let _c = make_commit(dir, "c");

        let repo = Repository::discover(dir).unwrap();
        let result = write(&repo).unwrap();
        // Flip the last byte of the trailer.
        let mut bytes = std::fs::read(&result.path).unwrap();
        let n = bytes.len();
        bytes[n - 1] ^= 0xff;
        std::fs::write(&result.path, &bytes).unwrap();

        let cg = CommitGraph::open(&result.path, HashKind::Sha1).unwrap();
        let err = cg.verify().expect_err("must reject tampered trailer");
        assert!(matches!(err, CommitGraphError::ChecksumMismatch));
    }

    #[test]
    fn sha256_round_trips_and_git_verify_accepts() {
        if !has_git() {
            return;
        }
        let tmp = tempdir().unwrap();
        let dir = tmp.path();
        if !init_repo_sha256(dir) {
            // Older git: silently skip.
            return;
        }
        write_and_add(dir, "a.txt", "v1\n");
        let c1 = make_commit(dir, "c1");
        write_and_add(dir, "a.txt", "v2\n");
        let _c2 = make_commit(dir, "c2");
        write_and_add(dir, "a.txt", "v3\n");
        let _c3 = make_commit(dir, "c3");

        let repo = Repository::discover(dir).unwrap();
        assert_eq!(repo.hash_kind(), HashKind::Sha256);
        let result = write(&repo).unwrap();
        let cg = CommitGraph::open(&result.path, HashKind::Sha256).unwrap();
        let oid1 = ObjectId::parse_hex(HashKind::Sha256, &c1).unwrap();
        let pos = cg.position_of(&oid1).expect("commit must be present");
        let entry = cg.commit_at(pos).unwrap();
        assert_eq!(entry.generation, 1);
        cg.verify().unwrap();

        // Real `git commit-graph verify` for sha256 mode.
        let out = Command::new("git")
            .args(["commit-graph", "verify"])
            .current_dir(dir)
            .output()
            .expect("run git commit-graph verify");
        if !out.status.success() {
            panic!(
                "git commit-graph verify (sha256) rejected our file:\nstdout={}\nstderr={}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr),
            );
        }
    }
}
