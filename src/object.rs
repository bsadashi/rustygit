//! Git object model: `blob`, `tree`, `commit`, `tag`.
//!
//! On-disk format (for loose objects, identical for all four types):
//! `<type> <decimal-size>\0<payload>`, zlib-compressed, stored at
//! `objects/aa/bbbbbbb...` where `aabbb...` is the hex of the SHA-1 (or SHA-256)
//! of the *uncompressed* framed bytes.
//!
//! This module defines the in-memory representation only. Reads/writes against
//! the filesystem live in `crate::odb` (M1).

use std::fmt;

use thiserror::Error;

use crate::hash::{new_hasher, HashKind, ObjectId};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ObjectKind {
    Blob,
    Tree,
    Commit,
    Tag,
}

impl ObjectKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            ObjectKind::Blob => "blob",
            ObjectKind::Tree => "tree",
            ObjectKind::Commit => "commit",
            ObjectKind::Tag => "tag",
        }
    }

    pub fn parse(s: &str) -> Result<Self, ObjectError> {
        Ok(match s {
            "blob" => ObjectKind::Blob,
            "tree" => ObjectKind::Tree,
            "commit" => ObjectKind::Commit,
            "tag" => ObjectKind::Tag,
            other => return Err(ObjectError::UnknownKind(other.to_string())),
        })
    }
}

impl fmt::Display for ObjectKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// An in-memory git object: type tag + raw payload.
///
/// The framed-and-hashed bytes are `"<kind> <len>\0<payload>"`; the OID is the
/// hash of those bytes under whichever algorithm the repository uses.
#[derive(Debug, Clone)]
pub struct RawObject {
    pub kind: ObjectKind,
    pub data: Vec<u8>,
}

impl RawObject {
    pub fn new(kind: ObjectKind, data: Vec<u8>) -> Self {
        Self { kind, data }
    }

    /// Build the framed bytes that would be hashed and zlib-compressed on disk.
    pub fn framed(&self) -> Vec<u8> {
        let header = format!("{} {}\0", self.kind.as_str(), self.data.len());
        let mut out = Vec::with_capacity(header.len() + self.data.len());
        out.extend_from_slice(header.as_bytes());
        out.extend_from_slice(&self.data);
        out
    }

    /// Compute the OID under the given hash algorithm.
    pub fn oid(&self, kind: HashKind) -> ObjectId {
        let mut hasher = new_hasher(kind);
        let header = format!("{} {}\0", self.kind.as_str(), self.data.len());
        hasher.update(header.as_bytes());
        hasher.update(&self.data);
        hasher.finalize()
    }
}

#[derive(Error, Debug)]
pub enum ObjectError {
    #[error("unknown object type: {0}")]
    UnknownKind(String),
    #[error("malformed object header: {0}")]
    MalformedHeader(String),
    #[error("object size mismatch: header says {header}, payload is {actual}")]
    SizeMismatch { header: usize, actual: usize },
    #[error(transparent)]
    Hash(#[from] crate::hash::HashError),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_blob_oid_sha1_matches_git() {
        let blob = RawObject::new(ObjectKind::Blob, Vec::new());
        let oid = blob.oid(HashKind::Sha1);
        assert_eq!(oid.to_string(), "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391");
    }

    #[test]
    fn hello_world_blob_oid_sha1_matches_git() {
        // `printf 'hello world' | git hash-object --stdin`
        let blob = RawObject::new(ObjectKind::Blob, b"hello world".to_vec());
        let oid = blob.oid(HashKind::Sha1);
        assert_eq!(oid.to_string(), "95d09f2b10159347eece71399a7e2e907ea3df4f");
    }

    #[test]
    fn framed_format() {
        let blob = RawObject::new(ObjectKind::Blob, b"hi".to_vec());
        assert_eq!(blob.framed(), b"blob 2\0hi");
    }

    #[test]
    fn kind_round_trip() {
        for k in [
            ObjectKind::Blob,
            ObjectKind::Tree,
            ObjectKind::Commit,
            ObjectKind::Tag,
        ] {
            assert_eq!(ObjectKind::parse(k.as_str()).unwrap(), k);
        }
        assert!(matches!(
            ObjectKind::parse("widget"),
            Err(ObjectError::UnknownKind(_))
        ));
    }
}
