//! Comparison engine driving `diff`, `diff-tree`, `diff-index`, `diff-files`.
//!
//! The "what to compare" logic and the "how to format" logic are split. This
//! module owns the former: given two flat lists of `DiffEntry` (each one a
//! `path → (mode, oid)` triple), produce a `Vec<DiffPair>` describing the
//! per-path delta.
//!
//! Each list is sorted by path bytes ascending; we two-finger-walk both lists
//! simultaneously. That lets the engine stay O(N+M) and oblivious to whether
//! the entries originated in a tree, an index, or the working tree.
//!
//! The four public entry points (`diff_two_trees`, `diff_tree_index`,
//! `diff_index_workdir`, `diff_tree_workdir`) build the input lists from the
//! repository and pipe formatted hunks through `crate::diff::format`.

pub mod format;
pub mod rename;

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;

use thiserror::Error;

use crate::commit::{Commit, CommitError};
use crate::hash::{HashError, ObjectId};
use crate::index::{Index, IndexEntry, IndexError};
use crate::object::{ObjectKind, RawObject};
use crate::odb::OdbError;
use crate::repo::Repository;
use crate::tree::{FileMode, Tree, TreeError};

/// One side of a diff: the (mode, oid) pair tied to a path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffEntry {
    pub path: Vec<u8>,
    pub mode: FileMode,
    pub oid: ObjectId,
}

/// What kind of change happened on a single path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffStatus {
    /// Path is on the b-side only.
    Added,
    /// Path is on the a-side only.
    Deleted,
    /// Same path, same broad type (blob↔blob), but oid differs.
    Modified,
    /// Same path, different broad type (e.g. blob↔symlink).
    TypeChanged,
    /// Same path, same oid, only the file mode changed (e.g. 100644→100755).
    ModeChanged,
}

/// One row in the comparison output.
#[derive(Debug, Clone)]
pub struct DiffPair {
    pub status: DiffStatus,
    /// `None` means the path didn't exist on the a-side (i.e. Added).
    pub a: Option<DiffEntry>,
    /// `None` means the path doesn't exist on the b-side (i.e. Deleted).
    pub b: Option<DiffEntry>,
}

/// Errors raised by the diff engine and entry points.
#[derive(Error, Debug)]
pub enum DiffError {
    #[error("io error on {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error(transparent)]
    Odb(#[from] OdbError),
    #[error(transparent)]
    Tree(#[from] TreeError),
    #[error(transparent)]
    Commit(#[from] CommitError),
    #[error(transparent)]
    Index(#[from] IndexError),
    #[error(transparent)]
    Hash(#[from] HashError),
    #[error("expected commit-ish, got {0}")]
    NotCommitish(ObjectId),
}

impl From<DiffError> for io::Error {
    fn from(e: DiffError) -> io::Error {
        io::Error::other(format!("{e}"))
    }
}

/// Compare two sorted-by-path lists of entries; produce per-path `DiffPair`s.
///
/// Both inputs must be sorted ascending by path bytes; the result is in the
/// same order. Paths that are identical between the two lists with matching
/// mode and oid are dropped (no `DiffPair` is emitted).
pub fn diff_entries(a: &[DiffEntry], b: &[DiffEntry]) -> Vec<DiffPair> {
    let mut out = Vec::new();
    let mut i = 0usize;
    let mut j = 0usize;
    while i < a.len() && j < b.len() {
        match a[i].path.cmp(&b[j].path) {
            Ordering::Less => {
                out.push(DiffPair {
                    status: DiffStatus::Deleted,
                    a: Some(a[i].clone()),
                    b: None,
                });
                i += 1;
            }
            Ordering::Greater => {
                out.push(DiffPair {
                    status: DiffStatus::Added,
                    a: None,
                    b: Some(b[j].clone()),
                });
                j += 1;
            }
            Ordering::Equal => {
                let pair = classify_pair(&a[i], &b[j]);
                if let Some(p) = pair {
                    out.push(p);
                }
                i += 1;
                j += 1;
            }
        }
    }
    while i < a.len() {
        out.push(DiffPair {
            status: DiffStatus::Deleted,
            a: Some(a[i].clone()),
            b: None,
        });
        i += 1;
    }
    while j < b.len() {
        out.push(DiffPair {
            status: DiffStatus::Added,
            a: None,
            b: Some(b[j].clone()),
        });
        j += 1;
    }
    out
}

fn classify_pair(a: &DiffEntry, b: &DiffEntry) -> Option<DiffPair> {
    let same_oid = a.oid == b.oid;
    let same_mode = a.mode == b.mode;
    if same_oid && same_mode {
        return None;
    }
    let status = if same_oid && !same_mode {
        DiffStatus::ModeChanged
    } else {
        // Same path, oids differ. Check whether the broad type changed
        // (blob ↔ symlink ↔ gitlink ↔ tree). Within the "blob" family
        // (Regular vs Executable) git reports Modified, not TypeChanged.
        let a_blob = matches!(a.mode, FileMode::Regular | FileMode::Executable);
        let b_blob = matches!(b.mode, FileMode::Regular | FileMode::Executable);
        if (a_blob && b_blob) || a.mode == b.mode {
            DiffStatus::Modified
        } else {
            DiffStatus::TypeChanged
        }
    };
    Some(DiffPair {
        status,
        a: Some(a.clone()),
        b: Some(b.clone()),
    })
}

// ---------------------------------------------------------------------------
// Tree flattening
// ---------------------------------------------------------------------------

/// Resolve a commit-ish or tree-ish OID to a tree OID.
pub fn peel_to_tree(repo: &Repository, oid: ObjectId) -> Result<ObjectId, DiffError> {
    let obj = repo.odb().read(&oid)?;
    match obj.kind {
        ObjectKind::Tree => Ok(oid),
        ObjectKind::Commit => {
            let commit = Commit::parse(&obj.data, repo.hash_kind())?;
            Ok(commit.tree)
        }
        ObjectKind::Tag => {
            // Walk the tag's `object` line.
            let body = std::str::from_utf8(&obj.data).map_err(|_| DiffError::NotCommitish(oid))?;
            for line in body.lines() {
                if let Some(rest) = line.strip_prefix("object ") {
                    let next = ObjectId::parse_hex(repo.hash_kind(), rest.trim())?;
                    return peel_to_tree(repo, next);
                }
                if line.is_empty() {
                    break;
                }
            }
            Err(DiffError::NotCommitish(oid))
        }
        _ => Err(DiffError::NotCommitish(oid)),
    }
}

/// Recursively walk a tree object and emit one `DiffEntry` per non-tree leaf.
/// The result is sorted by path bytes ascending (matching git's diff order
/// regardless of tree-sort quirks like trailing-slash on subtrees).
pub fn flatten_tree(repo: &Repository, tree_oid: &ObjectId) -> Result<Vec<DiffEntry>, DiffError> {
    let mut out: BTreeMap<Vec<u8>, DiffEntry> = BTreeMap::new();
    let mut prefix: Vec<u8> = Vec::new();
    flatten_tree_inner(repo, tree_oid, &mut prefix, &mut out)?;
    Ok(out.into_values().collect())
}

fn flatten_tree_inner(
    repo: &Repository,
    tree_oid: &ObjectId,
    prefix: &mut Vec<u8>,
    out: &mut BTreeMap<Vec<u8>, DiffEntry>,
) -> Result<(), DiffError> {
    let raw = repo.odb().read(tree_oid)?;
    if raw.kind != ObjectKind::Tree {
        return Ok(());
    }
    let tree = Tree::parse(&raw.data, repo.hash_kind())?;
    for entry in &tree.entries {
        let saved = prefix.len();
        if !prefix.is_empty() {
            prefix.push(b'/');
        }
        prefix.extend_from_slice(&entry.name);
        if entry.mode.is_tree() {
            flatten_tree_inner(repo, &entry.oid, prefix, out)?;
        } else {
            out.insert(
                prefix.clone(),
                DiffEntry {
                    path: prefix.clone(),
                    mode: entry.mode,
                    oid: entry.oid,
                },
            );
        }
        prefix.truncate(saved);
    }
    Ok(())
}

/// Build a list of `DiffEntry` from the index's stage-0 entries, sorted by path.
pub fn flatten_index(index: &Index) -> Vec<DiffEntry> {
    let mut out = Vec::new();
    for ent in &index.entries {
        if ent.stage != 0 {
            continue;
        }
        let mode = match FileMode::from_index_mode(ent.mode) {
            Ok(m) => m,
            Err(_) => continue,
        };
        out.push(DiffEntry {
            path: ent.path.clone(),
            mode,
            oid: ent.oid,
        });
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

/// For each indexed path, build a `DiffEntry` whose oid reflects the
/// working-tree content.
///
/// If the on-disk stat matches the index entry we trust it and reuse the
/// indexed oid; otherwise we hash the workfile and use its real digest. Files
/// that have been removed from the workdir become `DiffEntry`s with the index
/// oid suppressed by returning `None` for that slot — which the caller turns
/// into a Deleted pair.
pub fn flatten_workdir_against_index(
    repo: &Repository,
    index: &Index,
) -> Result<Vec<DiffEntry>, DiffError> {
    let mut out = Vec::new();
    for ent in &index.entries {
        if ent.stage != 0 {
            continue;
        }
        let abs = repo.workdir().join(bytes_to_relpath(&ent.path));
        let metadata = match fs::symlink_metadata(&abs) {
            Ok(m) => m,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                // Workdir is missing the path → Deleted in the second source.
                // We model this by simply not emitting the path on the b-side;
                // the merge in `diff_entries` will recognize it as deleted.
                continue;
            }
            Err(source) => return Err(DiffError::Io { path: abs, source }),
        };

        let ft = metadata.file_type();
        let on_disk_mode = if ft.is_symlink() {
            FileMode::Symlink
        } else if ft.is_file() {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if metadata.permissions().mode() & 0o111 != 0 {
                    FileMode::Executable
                } else {
                    FileMode::Regular
                }
            }
            #[cfg(not(unix))]
            {
                FileMode::Regular
            }
        } else {
            // Directory or other → no comparable mode. Skip.
            continue;
        };

        // Stat shortcut: if the stat looks unchanged, reuse the indexed oid.
        if stat_matches_index(&metadata, ent) {
            out.push(DiffEntry {
                path: ent.path.clone(),
                mode: on_disk_mode,
                oid: ent.oid,
            });
            continue;
        }

        // Re-hash content. We also write the blob into the loose store so the
        // formatter can read it back for the diff body — git's `diff` likewise
        // materializes workfile blobs into the odb on demand.
        let oid = blob_write_for_workfile(repo, &abs, &ft).map_err(|source| DiffError::Io {
            path: abs.clone(),
            source,
        })?;
        out.push(DiffEntry {
            path: ent.path.clone(),
            mode: on_disk_mode,
            oid,
        });
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

#[cfg(unix)]
fn stat_matches_index(meta: &fs::Metadata, ent: &IndexEntry) -> bool {
    use std::os::unix::fs::MetadataExt;
    let size_ok = (meta.size().min(u32::MAX as u64) as u32) == ent.size;
    let mtime_ok =
        (meta.mtime() as u32) == ent.mtime_s && (meta.mtime_nsec() as u32) == ent.mtime_n;
    size_ok && mtime_ok
}

#[cfg(not(unix))]
fn stat_matches_index(_meta: &fs::Metadata, _ent: &IndexEntry) -> bool {
    false
}

fn blob_write_for_workfile(
    repo: &Repository,
    abs: &std::path::Path,
    ft: &fs::FileType,
) -> Result<ObjectId, io::Error> {
    let payload = if ft.is_symlink() {
        fs::read_link(abs)?
            .as_os_str()
            .to_string_lossy()
            .into_owned()
            .into_bytes()
    } else {
        fs::read(abs)?
    };
    let blob = RawObject::new(ObjectKind::Blob, payload);
    repo.odb()
        .write(&blob)
        .map_err(|e| io::Error::other(format!("{e}")))
}

fn bytes_to_relpath(b: &[u8]) -> PathBuf {
    #[cfg(unix)]
    {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;
        PathBuf::from(OsStr::from_bytes(b))
    }
    #[cfg(not(unix))]
    {
        PathBuf::from(String::from_utf8_lossy(b).into_owned())
    }
}

// ---------------------------------------------------------------------------
// Public entry points used by the CLI commands.
// ---------------------------------------------------------------------------

/// Diff two trees (both inputs are commit-ish or tree-ish OIDs).
pub fn diff_two_trees<W: Write>(
    repo: &Repository,
    a: ObjectId,
    b: ObjectId,
    out: &mut W,
) -> io::Result<()> {
    let a_tree = peel_to_tree(repo, a)?;
    let b_tree = peel_to_tree(repo, b)?;
    let a_entries = flatten_tree(repo, &a_tree)?;
    let b_entries = flatten_tree(repo, &b_tree)?;
    let pairs = diff_entries(&a_entries, &b_entries);
    for pair in &pairs {
        format::format_pair(repo, pair, out)?;
    }
    Ok(())
}

/// Diff a tree against the index (a-side: tree, b-side: index).
pub fn diff_tree_index<W: Write>(repo: &Repository, tree: ObjectId, out: &mut W) -> io::Result<()> {
    let tree_oid = peel_to_tree(repo, tree)?;
    let a_entries = flatten_tree(repo, &tree_oid)?;
    let index = Index::read(repo).map_err(DiffError::from)?;
    let b_entries = flatten_index(&index);
    let pairs = diff_entries(&a_entries, &b_entries);
    for pair in &pairs {
        format::format_pair(repo, pair, out)?;
    }
    Ok(())
}

/// Diff the index against the working tree (a-side: index, b-side: workdir).
pub fn diff_index_workdir<W: Write>(repo: &Repository, out: &mut W) -> io::Result<()> {
    let index = Index::read(repo).map_err(DiffError::from)?;
    let a_entries = flatten_index(&index);
    let b_entries = flatten_workdir_against_index(repo, &index)?;
    let pairs = diff_entries(&a_entries, &b_entries);
    for pair in &pairs {
        format::format_pair(repo, pair, out)?;
    }
    Ok(())
}

/// Diff a tree against the working tree (no index in the middle).
///
/// We model the workdir as "every indexed path's on-disk content" — paths that
/// are tracked but missing on disk become Deleted; paths that exist in the
/// workdir but aren't tracked are NOT included (matching `git diff <tree>`'s
/// behavior of not showing untracked files).
pub fn diff_tree_workdir<W: Write>(
    repo: &Repository,
    tree: ObjectId,
    out: &mut W,
) -> io::Result<()> {
    let tree_oid = peel_to_tree(repo, tree)?;
    let a_entries = flatten_tree(repo, &tree_oid)?;
    let index = Index::read(repo).map_err(DiffError::from)?;
    let b_entries = flatten_workdir_against_index(repo, &index)?;
    let pairs = diff_entries(&a_entries, &b_entries);
    for pair in &pairs {
        format::format_pair(repo, pair, out)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hash::HashKind;

    fn fake_oid(byte: u8) -> ObjectId {
        ObjectId::from_bytes(HashKind::Sha1, &[byte; 20]).unwrap()
    }

    fn entry(path: &[u8], mode: FileMode, oid_byte: u8) -> DiffEntry {
        DiffEntry {
            path: path.to_vec(),
            mode,
            oid: fake_oid(oid_byte),
        }
    }

    #[test]
    fn identical_trees_produce_empty_diff() {
        let a = vec![
            entry(b"a.txt", FileMode::Regular, 0x01),
            entry(b"b.txt", FileMode::Regular, 0x02),
        ];
        let b = a.clone();
        let pairs = diff_entries(&a, &b);
        assert!(pairs.is_empty(), "{:?}", pairs);
    }

    #[test]
    fn one_added_file_yields_one_added_pair() {
        let a = vec![entry(b"keep.txt", FileMode::Regular, 0x10)];
        let b = vec![
            entry(b"keep.txt", FileMode::Regular, 0x10),
            entry(b"new.txt", FileMode::Regular, 0x11),
        ];
        let pairs = diff_entries(&a, &b);
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0].status, DiffStatus::Added);
        assert!(pairs[0].a.is_none());
        assert_eq!(pairs[0].b.as_ref().unwrap().path, b"new.txt");
    }

    #[test]
    fn one_removed_file_yields_one_deleted_pair() {
        let a = vec![
            entry(b"keep.txt", FileMode::Regular, 0x10),
            entry(b"old.txt", FileMode::Regular, 0x11),
        ];
        let b = vec![entry(b"keep.txt", FileMode::Regular, 0x10)];
        let pairs = diff_entries(&a, &b);
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0].status, DiffStatus::Deleted);
        assert!(pairs[0].b.is_none());
        assert_eq!(pairs[0].a.as_ref().unwrap().path, b"old.txt");
    }

    #[test]
    fn different_oid_same_mode_yields_modified() {
        let a = vec![entry(b"f.txt", FileMode::Regular, 0x10)];
        let b = vec![entry(b"f.txt", FileMode::Regular, 0x20)];
        let pairs = diff_entries(&a, &b);
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0].status, DiffStatus::Modified);
    }

    #[test]
    fn same_oid_different_mode_yields_modechanged() {
        let a = vec![entry(b"f.txt", FileMode::Regular, 0x10)];
        let b = vec![entry(b"f.txt", FileMode::Executable, 0x10)];
        let pairs = diff_entries(&a, &b);
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0].status, DiffStatus::ModeChanged);
    }

    #[test]
    fn blob_to_symlink_yields_typechanged() {
        let a = vec![entry(b"f.txt", FileMode::Regular, 0x10)];
        let b = vec![entry(b"f.txt", FileMode::Symlink, 0x20)];
        let pairs = diff_entries(&a, &b);
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0].status, DiffStatus::TypeChanged);
    }

    #[test]
    fn regular_to_executable_with_different_oid_is_modified_not_typechanged() {
        // Within the blob family, mode flip + content change is just Modified.
        let a = vec![entry(b"f.txt", FileMode::Regular, 0x10)];
        let b = vec![entry(b"f.txt", FileMode::Executable, 0x20)];
        let pairs = diff_entries(&a, &b);
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0].status, DiffStatus::Modified);
    }

    #[test]
    fn interleaved_paths_produce_correct_order() {
        let a = vec![
            entry(b"a", FileMode::Regular, 0x01),
            entry(b"c", FileMode::Regular, 0x03),
            entry(b"e", FileMode::Regular, 0x05),
        ];
        let b = vec![
            entry(b"b", FileMode::Regular, 0x02),
            entry(b"c", FileMode::Regular, 0x33), // modified
            entry(b"d", FileMode::Regular, 0x04),
        ];
        let pairs = diff_entries(&a, &b);
        // Expected: D a, A b, M c, A d, D e
        assert_eq!(pairs.len(), 5);
        assert_eq!(pairs[0].status, DiffStatus::Deleted);
        assert_eq!(pairs[0].a.as_ref().unwrap().path, b"a");
        assert_eq!(pairs[1].status, DiffStatus::Added);
        assert_eq!(pairs[1].b.as_ref().unwrap().path, b"b");
        assert_eq!(pairs[2].status, DiffStatus::Modified);
        assert_eq!(pairs[2].a.as_ref().unwrap().path, b"c");
        assert_eq!(pairs[3].status, DiffStatus::Added);
        assert_eq!(pairs[3].b.as_ref().unwrap().path, b"d");
        assert_eq!(pairs[4].status, DiffStatus::Deleted);
        assert_eq!(pairs[4].a.as_ref().unwrap().path, b"e");
    }

    #[test]
    fn empty_inputs_yield_empty_diff() {
        let a: Vec<DiffEntry> = Vec::new();
        let b: Vec<DiffEntry> = Vec::new();
        assert!(diff_entries(&a, &b).is_empty());
    }

    #[test]
    fn empty_a_treats_all_b_as_added() {
        let a: Vec<DiffEntry> = Vec::new();
        let b = vec![
            entry(b"x", FileMode::Regular, 0x01),
            entry(b"y", FileMode::Regular, 0x02),
        ];
        let pairs = diff_entries(&a, &b);
        assert_eq!(pairs.len(), 2);
        assert!(pairs.iter().all(|p| p.status == DiffStatus::Added));
    }

    #[test]
    fn empty_b_treats_all_a_as_deleted() {
        let a = vec![
            entry(b"x", FileMode::Regular, 0x01),
            entry(b"y", FileMode::Regular, 0x02),
        ];
        let b: Vec<DiffEntry> = Vec::new();
        let pairs = diff_entries(&a, &b);
        assert_eq!(pairs.len(), 2);
        assert!(pairs.iter().all(|p| p.status == DiffStatus::Deleted));
    }
}
