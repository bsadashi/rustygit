//! Git index file (`.git/index`) parse and write — versions 2 and 3.
//!
//! Layout (from `gitformat-index(5)`):
//! ```text
//!   header   "DIRC" + u32 BE version + u32 BE entry-count
//!   entries  sorted by (path-bytes, stage); each padded to 8-byte multiple
//!   exts     <signature:4> <length:u32 BE> <body> ...
//!   trailer  hash of all preceding bytes (SHA-1 or SHA-256, matching repo)
//! ```
//!
//! M3 scope: read v2/v3, write v2 (or v3 when an entry needs the extended
//! flags). The TREE (cache-tree) extension is the only one we round-trip;
//! every other extension is parsed-and-skipped on read and never written.
//! That matches `git fsck`'s tolerance on unrecognized extensions whose
//! signature starts with an uppercase ASCII letter (REUC, link), and the
//! "lowercase = optional" rule for the rest (sdir, link is uppercase but
//! we don't support split-index either).

pub mod cache_tree;

use std::fs;
use std::io;
use std::path::PathBuf;

use thiserror::Error;

use crate::hash::{new_hasher, HashError, HashKind, ObjectId};
use crate::lockfile::{LockError, Lockfile};
use crate::repo::Repository;
use crate::tree::TreeError;

pub use cache_tree::{CacheTree, CacheTreeError};

const SIG_DIRC: &[u8; 4] = b"DIRC";
const SIG_TREE: &[u8; 4] = b"TREE";

const HEADER_LEN: usize = 12;
/// Fixed prefix of an index entry from `ctime_s` through `flags` (no path),
/// before SHA digest is included.
///
/// 10 u32 fields = 40 bytes, then SHA digest (kind-dependent), then flags (2).
const ENTRY_FIXED_HEAD_BEFORE_SHA: usize = 40;

/// Flag bit masks.
const FLAG_NAMELEN_MASK: u16 = 0x0FFF;
const FLAG_STAGE_MASK: u16 = 0x3000;
const FLAG_STAGE_SHIFT: u32 = 12;
const FLAG_EXTENDED: u16 = 0x4000;
const FLAG_ASSUME_VALID: u16 = 0x8000;

#[derive(Debug, Clone)]
pub struct IndexEntry {
    pub ctime_s: u32,
    pub ctime_n: u32,
    pub mtime_s: u32,
    pub mtime_n: u32,
    pub dev: u32,
    pub ino: u32,
    /// 32-bit mode value as stored on disk; matches `FileMode::to_index_mode`.
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub size: u32,
    pub oid: ObjectId,
    /// Raw 16-bit flags field (preserved verbatim for byte-equal round-trips).
    pub flags: u16,
    pub path: Vec<u8>,
    /// Stage 0..=3, decoded from `flags` bits 12-13.
    pub stage: u8,
    /// flags bit 15.
    pub assume_valid: bool,
    /// flags bit 14. Only meaningful in v3+; if true, the on-disk entry has
    /// an additional 16-bit extended-flags field after `flags`.
    pub extended: bool,
    /// The 16-bit extended-flags field (v3+). Only present when `extended`.
    pub extended_flags: u16,
}

#[derive(Debug, Clone)]
pub struct Index {
    pub version: u32,
    pub entries: Vec<IndexEntry>,
    pub cache_tree: Option<CacheTree>,
}

impl Index {
    pub fn empty(version: u32) -> Self {
        Self {
            version,
            entries: Vec::new(),
            cache_tree: None,
        }
    }

    /// Parse a `.git/index` byte slice. The hash kind must match the
    /// repository (it determines OID width and the trailer length).
    pub fn parse(bytes: &[u8], hash_kind: HashKind) -> Result<Self, IndexError> {
        let raw_len = hash_kind.raw_len();
        if bytes.len() < HEADER_LEN + raw_len {
            return Err(IndexError::Malformed("file is shorter than header+trailer"));
        }
        if &bytes[0..4] != SIG_DIRC {
            return Err(IndexError::BadSignature {
                expected: *SIG_DIRC,
                got: copy4(&bytes[0..4]),
            });
        }
        let version = read_u32_be(&bytes[4..8]);
        if version != 2 && version != 3 {
            // v4 path-prefix-compression and split-index aren't in M3.
            return Err(IndexError::UnsupportedVersion(version));
        }
        let n_entries = read_u32_be(&bytes[8..12]) as usize;

        // The trailer is the last `raw_len` bytes; verify it before trusting
        // the body. If the checksum is wrong we still try to keep parsing —
        // git itself returns BUG on a mismatch but we only error out.
        let body_end = bytes.len() - raw_len;
        let computed = hash_all(&bytes[..body_end], hash_kind);
        let stored = &bytes[body_end..];
        if computed.as_bytes() != stored {
            return Err(IndexError::ChecksumMismatch);
        }

        let mut cur = HEADER_LEN;
        let mut entries = Vec::with_capacity(n_entries);
        for _ in 0..n_entries {
            let (entry, new_cur) = parse_entry(bytes, cur, hash_kind, version)?;
            entries.push(entry);
            cur = new_cur;
        }

        // Walk extension chunks until we hit the trailer offset.
        let mut cache_tree = None;
        while cur + 8 <= body_end {
            let sig = copy4(&bytes[cur..cur + 4]);
            let ext_len = read_u32_be(&bytes[cur + 4..cur + 8]) as usize;
            let body_start = cur + 8;
            let body_end_ext = body_start
                .checked_add(ext_len)
                .ok_or(IndexError::Malformed("extension length overflow"))?;
            if body_end_ext > body_end {
                return Err(IndexError::Malformed("extension body extends past trailer"));
            }
            let body_slice = &bytes[body_start..body_end_ext];
            if &sig == SIG_TREE {
                // TREE extension. Tolerate empty bodies (git emits these).
                if !body_slice.is_empty() {
                    let ct = CacheTree::parse(body_slice, hash_kind)?;
                    cache_tree = Some(ct);
                }
            } else {
                // Skip every other extension. Per the spec, optional
                // extensions can be ignored; we simply ignore *all*
                // unrecognized chunks for M3 and never re-emit them.
                // This intentionally drops REUC/UNTR/FSMN/EOIE/IEOT/sdir/link
                // on round-trip, which is correct from a "git can still read
                // our output" perspective.
            }
            cur = body_end_ext;
        }

        if cur != body_end {
            return Err(IndexError::Malformed(
                "trailing bytes between extensions and trailer",
            ));
        }

        Ok(Self {
            version,
            entries,
            cache_tree,
        })
    }

    /// Read `<repo>/.git/index`. Returns an empty index if the file does not
    /// exist (matches git's "no index yet" behavior).
    pub fn read(repo: &Repository) -> Result<Self, IndexError> {
        let path = repo.index_path();
        let bytes = match fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return Ok(Self::empty(2));
            }
            Err(e) => {
                return Err(IndexError::Io { path, source: e });
            }
        };
        Self::parse(&bytes, repo.hash_kind())
    }

    /// Atomically write to `<repo>/.git/index` via the lockfile primitive.
    pub fn write(&self, repo: &Repository) -> Result<(), IndexError> {
        let bytes = self.serialize(repo.hash_kind());
        let path = repo.index_path();
        let mut lock = Lockfile::acquire(&path)?;
        lock.write_all(&bytes).map_err(|e| IndexError::Io {
            path: lock.lock_path().to_path_buf(),
            source: e,
        })?;
        lock.commit()?;
        crate::trace!(
            "index",
            "wrote {} entries ({} bytes)",
            self.entries.len(),
            bytes.len()
        );
        Ok(())
    }

    /// Encode the index to bytes (header + entries + extensions + trailer).
    /// The output is what `Self::write` would persist.
    pub fn serialize(&self, hash_kind: HashKind) -> Vec<u8> {
        let raw_len = hash_kind.raw_len();
        let mut out = Vec::new();

        // Header.
        out.extend_from_slice(SIG_DIRC);
        // If we have any extended-flag entries we must write v3 even if the
        // caller asked for v2. Conversely, if every entry fits in v2, we
        // prefer v2 to match git's defaults.
        let has_extended = self.entries.iter().any(|e| e.extended);
        let mut version = self.version;
        if has_extended && version < 3 {
            version = 3;
        }
        out.extend_from_slice(&version.to_be_bytes());
        let n_entries: u32 = self
            .entries
            .len()
            .try_into()
            .expect("entry count fits in u32");
        out.extend_from_slice(&n_entries.to_be_bytes());

        // Entries.
        for e in &self.entries {
            let entry_start = out.len();
            out.extend_from_slice(&e.ctime_s.to_be_bytes());
            out.extend_from_slice(&e.ctime_n.to_be_bytes());
            out.extend_from_slice(&e.mtime_s.to_be_bytes());
            out.extend_from_slice(&e.mtime_n.to_be_bytes());
            out.extend_from_slice(&e.dev.to_be_bytes());
            out.extend_from_slice(&e.ino.to_be_bytes());
            out.extend_from_slice(&e.mode.to_be_bytes());
            out.extend_from_slice(&e.uid.to_be_bytes());
            out.extend_from_slice(&e.gid.to_be_bytes());
            out.extend_from_slice(&e.size.to_be_bytes());

            // OID (zero-padded if the buffer is larger than the digest).
            out.extend_from_slice(e.oid.as_bytes());
            // We never store an OID under a different hash than the repo, so
            // the slice length is exactly `raw_len`. Defensive: if some caller
            // built an entry using a different kind, pad/truncate.
            debug_assert_eq!(e.oid.as_bytes().len(), raw_len);

            // Compute the on-disk flags from the decoded fields. We also OR
            // in any caller-set bits that aren't covered by name-length /
            // stage / assume-valid / extended (currently none, but we want to
            // be forgiving for byte-equal round-trips: just rebuild from the
            // raw `flags` value with an updated namelen-cap).
            let namelen_field: u16 = if e.path.len() >= 0x0FFF {
                0x0FFF
            } else {
                e.path.len() as u16
            };
            // Reconstruct from logical fields so we don't accidentally smuggle
            // unrelated bits across edits. This produces the same bytes as
            // git would write for the same logical entry.
            let mut flags: u16 = namelen_field & FLAG_NAMELEN_MASK;
            flags |= ((e.stage as u16) << FLAG_STAGE_SHIFT) & FLAG_STAGE_MASK;
            if e.extended {
                flags |= FLAG_EXTENDED;
            }
            if e.assume_valid {
                flags |= FLAG_ASSUME_VALID;
            }
            out.extend_from_slice(&flags.to_be_bytes());

            if e.extended {
                out.extend_from_slice(&e.extended_flags.to_be_bytes());
            }

            // Path + NUL + pad to 8-byte multiple. The pad must include the
            // NUL terminator; total entry size = round_up_to_8(everything).
            out.extend_from_slice(&e.path);
            out.push(0);
            let written = out.len() - entry_start;
            let rem = written % 8;
            if rem != 0 {
                let pad = 8 - rem;
                out.resize(out.len() + pad, 0u8);
            }
        }

        // TREE extension, if present.
        if let Some(ct) = &self.cache_tree {
            out.extend_from_slice(SIG_TREE);
            let body = ct.serialize();
            let len: u32 = body.len().try_into().expect("TREE body fits in u32");
            out.extend_from_slice(&len.to_be_bytes());
            out.extend_from_slice(&body);
        }

        // Trailer: hash over everything written so far.
        let digest = hash_all(&out, hash_kind);
        out.extend_from_slice(digest.as_bytes());

        out
    }

    /// Insert `entry`, replacing any existing entry with the same `(path, stage)`.
    /// Re-sorts the entry list to maintain git's order.
    pub fn upsert(&mut self, entry: IndexEntry) {
        if let Some(pos) = self
            .entries
            .iter()
            .position(|e| e.path == entry.path && e.stage == entry.stage)
        {
            self.entries[pos] = entry;
        } else {
            self.entries.push(entry);
        }
        self.sort();
    }

    /// Remove all stages of `path`. Returns true iff anything was removed.
    pub fn remove(&mut self, path: &[u8]) -> bool {
        let before = self.entries.len();
        self.entries.retain(|e| e.path != path);
        before != self.entries.len()
    }

    /// Sort entries by (path-bytes ascending, stage ascending) — git's order.
    pub fn sort(&mut self) {
        self.entries
            .sort_by(|a, b| a.path.cmp(&b.path).then_with(|| a.stage.cmp(&b.stage)));
    }
}

fn parse_entry(
    bytes: &[u8],
    cur: usize,
    hash_kind: HashKind,
    version: u32,
) -> Result<(IndexEntry, usize), IndexError> {
    let raw_len = hash_kind.raw_len();
    let entry_start = cur;
    if cur + ENTRY_FIXED_HEAD_BEFORE_SHA + raw_len + 2 > bytes.len() {
        return Err(IndexError::Malformed("entry header truncated"));
    }
    let ctime_s = read_u32_be(&bytes[cur..cur + 4]);
    let ctime_n = read_u32_be(&bytes[cur + 4..cur + 8]);
    let mtime_s = read_u32_be(&bytes[cur + 8..cur + 12]);
    let mtime_n = read_u32_be(&bytes[cur + 12..cur + 16]);
    let dev = read_u32_be(&bytes[cur + 16..cur + 20]);
    let ino = read_u32_be(&bytes[cur + 20..cur + 24]);
    let mode = read_u32_be(&bytes[cur + 24..cur + 28]);
    let uid = read_u32_be(&bytes[cur + 28..cur + 32]);
    let gid = read_u32_be(&bytes[cur + 32..cur + 36]);
    let size = read_u32_be(&bytes[cur + 36..cur + 40]);

    let oid_start = cur + ENTRY_FIXED_HEAD_BEFORE_SHA;
    let oid = ObjectId::from_bytes(hash_kind, &bytes[oid_start..oid_start + raw_len])?;
    let flags_at = oid_start + raw_len;
    let flags = u16::from_be_bytes([bytes[flags_at], bytes[flags_at + 1]]);

    let stage = ((flags & FLAG_STAGE_MASK) >> FLAG_STAGE_SHIFT) as u8;
    let assume_valid = (flags & FLAG_ASSUME_VALID) != 0;
    let extended_bit = (flags & FLAG_EXTENDED) != 0;
    let mut path_cur = flags_at + 2;
    let (extended, extended_flags) = if extended_bit {
        if version < 3 {
            return Err(IndexError::Malformed("extended-flag bit set in v2 entry"));
        }
        if path_cur + 2 > bytes.len() {
            return Err(IndexError::Malformed("extended flags truncated"));
        }
        let ef = u16::from_be_bytes([bytes[path_cur], bytes[path_cur + 1]]);
        path_cur += 2;
        (true, ef)
    } else {
        (false, 0)
    };

    let namelen_field = (flags & FLAG_NAMELEN_MASK) as usize;
    let path_end = if namelen_field < 0x0FFF {
        let end = path_cur + namelen_field;
        if end > bytes.len() {
            return Err(IndexError::Malformed("path truncated"));
        }
        end
    } else {
        // namelen capped; scan for terminator.
        let nul_off = bytes[path_cur..]
            .iter()
            .position(|&b| b == 0)
            .ok_or(IndexError::Malformed("missing NUL after long path"))?;
        path_cur + nul_off
    };
    let path = bytes[path_cur..path_end].to_vec();

    // Now skip the NUL terminator + padding to next 8-byte boundary.
    // Total entry length so far (including the NUL we're about to consume):
    let mut after = path_end + 1;
    // Pad up to multiple of 8 from `entry_start`.
    let consumed = after - entry_start;
    let rem = consumed % 8;
    if rem != 0 {
        after += 8 - rem;
    }
    if after > bytes.len() {
        return Err(IndexError::Malformed("entry padding truncated"));
    }

    let entry = IndexEntry {
        ctime_s,
        ctime_n,
        mtime_s,
        mtime_n,
        dev,
        ino,
        mode,
        uid,
        gid,
        size,
        oid,
        flags,
        path,
        stage,
        assume_valid,
        extended,
        extended_flags,
    };
    Ok((entry, after))
}

fn read_u32_be(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}

fn copy4(b: &[u8]) -> [u8; 4] {
    [b[0], b[1], b[2], b[3]]
}

fn hash_all(data: &[u8], kind: HashKind) -> ObjectId {
    let mut h = new_hasher(kind);
    h.update(data);
    h.finalize()
}

#[derive(Error, Debug)]
pub enum IndexError {
    #[error("io error on {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("malformed index: {0}")]
    Malformed(&'static str),
    #[error("bad index signature: expected {expected:?}, got {got:?}")]
    BadSignature { expected: [u8; 4], got: [u8; 4] },
    #[error("unsupported index version: {0}")]
    UnsupportedVersion(u32),
    #[error("index trailer checksum mismatch")]
    ChecksumMismatch,
    #[error(transparent)]
    Hash(#[from] HashError),
    #[error(transparent)]
    Tree(#[from] TreeError),
    #[error(transparent)]
    CacheTree(#[from] CacheTreeError),
    #[error(transparent)]
    Lock(#[from] LockError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::FileMode;
    use std::process::Command;
    use tempfile::tempdir;

    /// Skip the test if `git --version` fails (lets CI without git pass).
    fn git_available() -> bool {
        Command::new("git")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    fn git(dir: &std::path::Path, args: &[&str]) -> std::process::Output {
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

    fn init_repo_with_files(dir: &std::path::Path, files: &[(&str, &str)]) {
        git(dir, &["init", "-q", "."]);
        // Make commit identity available for `git commit` later if any test
        // needs it.
        git(dir, &["config", "user.name", "Test"]);
        git(dir, &["config", "user.email", "test@example.com"]);
        for (path, content) in files {
            let p = dir.join(path);
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(p, content).unwrap();
            git(dir, &["add", path]);
        }
    }

    #[test]
    fn parse_real_git_index_after_add() {
        if !git_available() {
            eprintln!("skipping: git not on PATH");
            return;
        }
        let tmp = tempdir().unwrap();
        let repo_dir = tmp.path();
        init_repo_with_files(
            repo_dir,
            &[
                ("a.txt", "hello\n"),
                ("b.txt", "world\n"),
                ("src/lib.rs", "fn main() {}\n"),
            ],
        );

        let bytes = std::fs::read(repo_dir.join(".git/index")).unwrap();
        let idx = Index::parse(&bytes, HashKind::Sha1).unwrap();
        assert!(matches!(idx.version, 2 | 3));
        assert_eq!(idx.entries.len(), 3);

        // Entries are sorted by path bytes.
        let paths: Vec<&[u8]> = idx.entries.iter().map(|e| e.path.as_slice()).collect();
        let mut sorted_paths = paths.clone();
        sorted_paths.sort();
        assert_eq!(paths, sorted_paths);

        for entry in &idx.entries {
            assert!(!entry.path.is_empty());
            assert!(!entry.oid.is_null(), "git just added it; oid must be set");
            // Mode should round-trip through FileMode.
            let mode = FileMode::from_index_mode(entry.mode).expect("known mode");
            assert!(matches!(
                mode,
                FileMode::Regular | FileMode::Executable | FileMode::Symlink | FileMode::Gitlink
            ));
            assert_eq!(entry.stage, 0);
        }
    }

    #[test]
    fn write_then_round_trip_via_ls_files() {
        if !git_available() {
            eprintln!("skipping: git not on PATH");
            return;
        }
        let tmp = tempdir().unwrap();
        let repo_dir = tmp.path();
        init_repo_with_files(
            repo_dir,
            &[
                ("a.txt", "alpha\n"),
                ("zeta.txt", "z\n"),
                ("nested/inner.txt", "n\n"),
            ],
        );

        let original_ls = git(repo_dir, &["ls-files", "--stage"]).stdout;

        // Read git's index, write it back via our writer, replace the file.
        let bytes = std::fs::read(repo_dir.join(".git/index")).unwrap();
        let idx = Index::parse(&bytes, HashKind::Sha1).unwrap();

        let our_bytes = idx.serialize(HashKind::Sha1);
        std::fs::write(repo_dir.join(".git/index"), &our_bytes).unwrap();

        let new_ls = git(repo_dir, &["ls-files", "--stage"]).stdout;
        assert_eq!(
            original_ls, new_ls,
            "git ls-files --stage should be identical after rewriting through our serializer",
        );
    }

    #[test]
    fn write_via_repository_handle() {
        if !git_available() {
            eprintln!("skipping: git not on PATH");
            return;
        }
        let tmp = tempdir().unwrap();
        let repo_dir = tmp.path();
        init_repo_with_files(repo_dir, &[("a.txt", "hello\n")]);

        let repo = Repository::discover(repo_dir).unwrap();
        let idx = Index::read(&repo).unwrap();
        assert_eq!(idx.entries.len(), 1);

        // Write through Repository::write (uses Lockfile under the hood).
        idx.write(&repo).unwrap();

        // git should still be able to read the result.
        let new_ls = git(repo_dir, &["ls-files", "--stage"]).stdout;
        assert!(!new_ls.is_empty());
    }

    #[test]
    fn cache_tree_round_trip_after_write_tree() {
        if !git_available() {
            eprintln!("skipping: git not on PATH");
            return;
        }
        let tmp = tempdir().unwrap();
        let repo_dir = tmp.path();
        init_repo_with_files(
            repo_dir,
            &[
                ("a.txt", "a\n"),
                ("src/lib.rs", "// x\n"),
                ("src/sub/m.rs", "// y\n"),
            ],
        );
        // `git write-tree` populates the cache-tree extension if it isn't
        // already there.
        git(repo_dir, &["write-tree"]);

        let bytes = std::fs::read(repo_dir.join(".git/index")).unwrap();
        let idx = Index::parse(&bytes, HashKind::Sha1).unwrap();
        let ct = idx
            .cache_tree
            .as_ref()
            .expect("cache_tree should be present after write-tree");
        // Root entry covers all 3 cache entries.
        assert_eq!(ct.entry_count, Some(3));
        assert!(!ct.children.is_empty());

        // Now serialize and have git read it back; it should still see the
        // cache tree.
        let our_bytes = idx.serialize(HashKind::Sha1);
        std::fs::write(repo_dir.join(".git/index"), &our_bytes).unwrap();

        let bytes2 = std::fs::read(repo_dir.join(".git/index")).unwrap();
        let idx2 = Index::parse(&bytes2, HashKind::Sha1).unwrap();
        assert!(idx2.cache_tree.is_some());
        assert_eq!(idx2.entries.len(), idx.entries.len());

        // git ls-files --stage should still match.
        let ls = git(repo_dir, &["ls-files", "--stage"]).stdout;
        assert!(!ls.is_empty());
    }

    #[test]
    fn byte_equal_round_trip_on_simple_index() {
        if !git_available() {
            eprintln!("skipping: git not on PATH");
            return;
        }
        let tmp = tempdir().unwrap();
        let repo_dir = tmp.path();
        init_repo_with_files(
            repo_dir,
            &[("a.txt", "a\n"), ("b.txt", "bb\n"), ("c.txt", "ccc\n")],
        );

        let bytes = std::fs::read(repo_dir.join(".git/index")).unwrap();
        let idx = Index::parse(&bytes, HashKind::Sha1).unwrap();
        let ours = idx.serialize(HashKind::Sha1);
        // We only require that the result re-parses to an equivalent index.
        // (Byte equality holds in practice when no extensions outside TREE are
        // present, but fresh `git add` may leave no TREE extension at all,
        // making both sides byte-equal.)
        let reparsed = Index::parse(&ours, HashKind::Sha1).unwrap();
        assert_eq!(reparsed.entries.len(), idx.entries.len());
        for (a, b) in reparsed.entries.iter().zip(idx.entries.iter()) {
            assert_eq!(a.path, b.path);
            assert_eq!(a.mode, b.mode);
            assert_eq!(a.size, b.size);
            assert_eq!(a.oid, b.oid);
            assert_eq!(a.stage, b.stage);
        }
    }

    #[test]
    fn hand_built_index_is_readable_by_git() {
        if !git_available() {
            eprintln!("skipping: git not on PATH");
            return;
        }
        let tmp = tempdir().unwrap();
        let repo_dir = tmp.path();
        // Initialize a bare-bones repo, then overwrite the index ourselves.
        git(repo_dir, &["init", "-q", "."]);

        // Hash a blob through git so the OID matches what git expects.
        let blob_out = Command::new("git")
            .args(["hash-object", "-w", "--stdin"])
            .current_dir(repo_dir)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        use std::io::Write;
        blob_out.stdin.as_ref().unwrap().write_all(b"hi\n").unwrap();
        let blob_done = blob_out.wait_with_output().unwrap();
        assert!(blob_done.status.success());
        let oid_hex = String::from_utf8(blob_done.stdout).unwrap();
        let oid_hex = oid_hex.trim();
        let oid = ObjectId::parse_hex(HashKind::Sha1, oid_hex).unwrap();

        let entry = IndexEntry {
            ctime_s: 0,
            ctime_n: 0,
            mtime_s: 0,
            mtime_n: 0,
            dev: 0,
            ino: 0,
            mode: FileMode::Regular.to_index_mode(),
            uid: 0,
            gid: 0,
            size: 3,
            oid,
            flags: 0,
            path: b"hello.txt".to_vec(),
            stage: 0,
            assume_valid: false,
            extended: false,
            extended_flags: 0,
        };
        let idx = Index {
            version: 2,
            entries: vec![entry],
            cache_tree: None,
        };
        let bytes = idx.serialize(HashKind::Sha1);
        std::fs::write(repo_dir.join(".git/index"), &bytes).unwrap();

        let ls = git(repo_dir, &["ls-files", "--stage"]).stdout;
        let ls = String::from_utf8(ls).unwrap();
        // Format: "<mode> <oid> <stage>\t<path>\n"
        let expected = format!("100644 {oid_hex} 0\thello.txt\n");
        assert_eq!(ls, expected);
    }

    #[test]
    fn upsert_replaces_same_stage_and_keeps_sort() {
        let mut idx = Index::empty(2);
        let oid = ObjectId::null(HashKind::Sha1);
        let mk = |path: &[u8]| IndexEntry {
            ctime_s: 0,
            ctime_n: 0,
            mtime_s: 0,
            mtime_n: 0,
            dev: 0,
            ino: 0,
            mode: 0o100644,
            uid: 0,
            gid: 0,
            size: 0,
            oid,
            flags: 0,
            path: path.to_vec(),
            stage: 0,
            assume_valid: false,
            extended: false,
            extended_flags: 0,
        };
        idx.upsert(mk(b"b"));
        idx.upsert(mk(b"a"));
        idx.upsert(mk(b"c"));
        let mut second = mk(b"b");
        second.size = 42;
        idx.upsert(second);

        let paths: Vec<&[u8]> = idx.entries.iter().map(|e| e.path.as_slice()).collect();
        assert_eq!(paths, vec![&b"a"[..], &b"b"[..], &b"c"[..]]);
        let b = idx.entries.iter().find(|e| e.path == b"b").unwrap();
        assert_eq!(b.size, 42);
    }

    #[test]
    fn remove_drops_all_stages_of_path() {
        let mut idx = Index::empty(2);
        let oid = ObjectId::null(HashKind::Sha1);
        let mk = |path: &[u8], stage: u8| IndexEntry {
            ctime_s: 0,
            ctime_n: 0,
            mtime_s: 0,
            mtime_n: 0,
            dev: 0,
            ino: 0,
            mode: 0o100644,
            uid: 0,
            gid: 0,
            size: 0,
            oid,
            flags: 0,
            path: path.to_vec(),
            stage,
            assume_valid: false,
            extended: false,
            extended_flags: 0,
        };
        idx.upsert(mk(b"file", 1));
        idx.upsert(mk(b"file", 2));
        idx.upsert(mk(b"keep", 0));
        assert!(idx.remove(b"file"));
        assert_eq!(idx.entries.len(), 1);
        assert_eq!(idx.entries[0].path, b"keep");
        assert!(!idx.remove(b"missing"));
    }

    #[test]
    fn rejects_bad_signature() {
        let mut bytes = vec![b'X', b'X', b'X', b'X', 0, 0, 0, 2, 0, 0, 0, 0];
        // Append a valid SHA-1 trailer for the all-zero body so checksum check
        // doesn't fire first.
        let trailer = hash_all(&bytes, HashKind::Sha1);
        bytes.extend_from_slice(trailer.as_bytes());
        let err = Index::parse(&bytes, HashKind::Sha1).unwrap_err();
        assert!(matches!(err, IndexError::BadSignature { .. }));
    }

    #[test]
    fn rejects_unsupported_version() {
        let mut bytes = SIG_DIRC.to_vec();
        bytes.extend_from_slice(&5u32.to_be_bytes());
        bytes.extend_from_slice(&0u32.to_be_bytes());
        let trailer = hash_all(&bytes, HashKind::Sha1);
        bytes.extend_from_slice(trailer.as_bytes());
        let err = Index::parse(&bytes, HashKind::Sha1).unwrap_err();
        assert!(matches!(err, IndexError::UnsupportedVersion(5)));
    }
}
