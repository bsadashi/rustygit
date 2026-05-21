//! Tree object format.
//!
//! Wire format: a sequence of `<mode-octal-ascii> <name>\0<raw-digest>` entries,
//! sorted by a slightly weird key — names are compared as bytes, but tree
//! entries sort *as if* their name had a trailing `/`. We replicate that here.
//!
//! The raw-digest length depends on the repository's hash algorithm (20 bytes
//! for SHA-1, 32 for SHA-256).

use std::cmp::Ordering;

use thiserror::Error;

use crate::hash::{HashError, HashKind, ObjectId};
use crate::object::{ObjectKind, RawObject};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileMode {
    /// Regular non-executable file.
    Regular,
    /// Regular executable file.
    Executable,
    /// Symbolic link.
    Symlink,
    /// Subdirectory (subtree).
    Tree,
    /// Gitlink (submodule). Stored but not actively supported.
    Gitlink,
}

impl FileMode {
    pub const fn as_octal(self) -> &'static str {
        match self {
            FileMode::Regular => "100644",
            FileMode::Executable => "100755",
            FileMode::Symlink => "120000",
            FileMode::Tree => "40000",
            FileMode::Gitlink => "160000",
        }
    }

    pub fn parse(s: &str) -> Result<Self, TreeError> {
        Ok(match s {
            "100644" => FileMode::Regular,
            "100755" => FileMode::Executable,
            "120000" => FileMode::Symlink,
            "40000" | "040000" => FileMode::Tree,
            "160000" => FileMode::Gitlink,
            other => return Err(TreeError::UnknownMode(other.to_string())),
        })
    }

    pub const fn is_tree(self) -> bool {
        matches!(self, FileMode::Tree)
    }

    pub const fn object_kind(self) -> ObjectKind {
        if self.is_tree() {
            ObjectKind::Tree
        } else {
            ObjectKind::Blob
        }
    }

    /// Encode as the 32-bit mode value used in the index.
    pub const fn to_index_mode(self) -> u32 {
        match self {
            FileMode::Regular => 0o100644,
            FileMode::Executable => 0o100755,
            FileMode::Symlink => 0o120000,
            FileMode::Tree => 0o040000,
            FileMode::Gitlink => 0o160000,
        }
    }

    pub fn from_index_mode(mode: u32) -> Result<Self, TreeError> {
        // The index mode upper bits encode file type; lower bits are POSIX perm.
        // Git only really cares about 0644 vs 0755 for blobs.
        let kind = mode & 0o170000;
        Ok(match kind {
            0o100000 => {
                if mode & 0o111 != 0 {
                    FileMode::Executable
                } else {
                    FileMode::Regular
                }
            }
            0o120000 => FileMode::Symlink,
            0o040000 => FileMode::Tree,
            0o160000 => FileMode::Gitlink,
            _ => return Err(TreeError::UnknownIndexMode(mode)),
        })
    }
}

#[derive(Debug, Clone)]
pub struct TreeEntry {
    pub mode: FileMode,
    pub name: Vec<u8>,
    pub oid: ObjectId,
}

#[derive(Debug, Clone)]
pub struct Tree {
    pub entries: Vec<TreeEntry>,
}

impl Tree {
    pub fn new(mut entries: Vec<TreeEntry>) -> Self {
        Self::sort_entries(&mut entries);
        Self { entries }
    }

    /// Git's tree sort order: byte-lexicographic on names, but tree entries
    /// sort as if their name had a trailing `/`.
    pub fn sort_entries(entries: &mut [TreeEntry]) {
        entries.sort_by(compare_entries);
    }

    pub fn serialize(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for e in &self.entries {
            out.extend_from_slice(e.mode.as_octal().as_bytes());
            out.push(b' ');
            out.extend_from_slice(&e.name);
            out.push(0);
            out.extend_from_slice(e.oid.as_bytes());
        }
        out
    }

    /// Parse a tree object body. The hash kind must match the repository's;
    /// we read the corresponding number of digest bytes per entry.
    pub fn parse(data: &[u8], hash_kind: HashKind) -> Result<Self, TreeError> {
        let raw_len = hash_kind.raw_len();
        let mut entries = Vec::new();
        let mut i = 0;
        while i < data.len() {
            let space = data[i..]
                .iter()
                .position(|&b| b == b' ')
                .ok_or(TreeError::Truncated("missing space"))?
                + i;
            let null = data[space + 1..]
                .iter()
                .position(|&b| b == 0)
                .ok_or(TreeError::Truncated("missing null"))?
                + space
                + 1;
            let mode_str = std::str::from_utf8(&data[i..space])
                .map_err(|_| TreeError::Truncated("non-utf8 mode"))?;
            let mode = FileMode::parse(mode_str)?;
            let name = data[space + 1..null].to_vec();
            if null + 1 + raw_len > data.len() {
                return Err(TreeError::Truncated("entry truncated before sha"));
            }
            let oid = ObjectId::from_bytes(hash_kind, &data[null + 1..null + 1 + raw_len])?;
            entries.push(TreeEntry { mode, name, oid });
            i = null + 1 + raw_len;
        }
        Ok(Tree { entries })
    }

    pub fn to_object(&self) -> RawObject {
        RawObject::new(ObjectKind::Tree, self.serialize())
    }
}

fn compare_entries(a: &TreeEntry, b: &TreeEntry) -> Ordering {
    let a_bytes = entry_sort_bytes(a);
    let b_bytes = entry_sort_bytes(b);
    a_bytes.cmp(&b_bytes)
}

fn entry_sort_bytes(e: &TreeEntry) -> Vec<u8> {
    let mut v = e.name.clone();
    if e.mode.is_tree() {
        v.push(b'/');
    }
    v
}

#[derive(Error, Debug)]
pub enum TreeError {
    #[error("unknown file mode in tree entry: {0}")]
    UnknownMode(String),
    #[error("unrecognized index mode: {0:o}")]
    UnknownIndexMode(u32),
    #[error("truncated tree object: {0}")]
    Truncated(&'static str),
    #[error(transparent)]
    Hash(#[from] HashError),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_oid(byte: u8) -> ObjectId {
        ObjectId::from_bytes(HashKind::Sha1, &[byte; 20]).unwrap()
    }

    #[test]
    fn round_trip_basic() {
        let entries = vec![
            TreeEntry {
                mode: FileMode::Regular,
                name: b"README".to_vec(),
                oid: fake_oid(0xaa),
            },
            TreeEntry {
                mode: FileMode::Tree,
                name: b"src".to_vec(),
                oid: fake_oid(0xbb),
            },
        ];
        let tree = Tree::new(entries);
        let bytes = tree.serialize();
        let parsed = Tree::parse(&bytes, HashKind::Sha1).unwrap();
        assert_eq!(parsed.entries.len(), 2);
        assert_eq!(parsed.entries[0].name, b"README");
        assert_eq!(parsed.entries[1].mode, FileMode::Tree);
    }

    #[test]
    fn tree_sort_treats_subtrees_as_having_trailing_slash() {
        // "lib" as a tree should sort after "lib.rs" as a file because
        // "lib/" > "lib.rs" byte-wise.
        let mut entries = vec![
            TreeEntry {
                mode: FileMode::Tree,
                name: b"lib".to_vec(),
                oid: fake_oid(1),
            },
            TreeEntry {
                mode: FileMode::Regular,
                name: b"lib.rs".to_vec(),
                oid: fake_oid(2),
            },
        ];
        Tree::sort_entries(&mut entries);
        assert_eq!(entries[0].name, b"lib.rs");
        assert_eq!(entries[1].name, b"lib");
    }

    #[test]
    fn index_mode_round_trip() {
        let cases = [
            (0o100644, FileMode::Regular),
            (0o100755, FileMode::Executable),
            (0o120000, FileMode::Symlink),
            (0o040000, FileMode::Tree),
            (0o160000, FileMode::Gitlink),
        ];
        for (raw, expected) in cases {
            assert_eq!(FileMode::from_index_mode(raw).unwrap(), expected);
            assert_eq!(expected.to_index_mode(), raw);
        }
    }
}
