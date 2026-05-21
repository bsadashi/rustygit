//! Object database (ADR A2).
//!
//! `ObjectDb` is a stack of `dyn ObjectStore` implementations. Reads cascade
//! in order (loose → pack → midx → alternates); writes always go to the
//! `writer_index` store (the loose store). Today we have only `LooseStore`;
//! `PackStore` arrives in M7.

pub mod loose;

use std::sync::Arc;

use thiserror::Error;

use crate::hash::{HashError, HashKind, ObjectId};
use crate::object::{ObjectError, ObjectKind, RawObject};

pub use loose::LooseStore;

/// Either an unambiguous match for a hex prefix, or context for an
/// ambiguity / not-found error.
#[derive(Debug, Clone)]
pub enum PrefixMatch {
    Found(ObjectId),
    None,
    Ambiguous(Vec<ObjectId>),
}

/// Backend-agnostic interface to a store of git objects.
pub trait ObjectStore: Send + Sync {
    fn contains(&self, id: &ObjectId) -> Result<bool, OdbError>;

    fn read(&self, id: &ObjectId) -> Result<Option<RawObject>, OdbError>;

    /// Just the type and uncompressed payload size — without inflating the
    /// full object, when the backend can serve that cheaply (packs in M7).
    fn read_header(&self, id: &ObjectId) -> Result<Option<(ObjectKind, u64)>, OdbError>;

    /// Write an object. Stores that are read-only (e.g. packs) return
    /// `OdbError::Unsupported`.
    fn write(&self, obj: &RawObject) -> Result<ObjectId, OdbError>;

    fn iter(&self) -> Box<dyn Iterator<Item = Result<ObjectId, OdbError>> + '_>;

    /// Return all object ids whose hex starts with `prefix`. The caller passes
    /// the lower-case hex prefix as bytes; `hex_len` is the prefix length.
    fn resolve_prefix(&self, prefix: &str) -> Result<PrefixMatch, OdbError>;
}

/// A stack of object stores. Reads cascade through `stores` in order; writes
/// always go to `stores[writer_index]`.
pub struct ObjectDb {
    stores: Vec<Arc<dyn ObjectStore>>,
    writer_index: usize,
    hash_kind: HashKind,
}

impl ObjectDb {
    pub fn new(
        stores: Vec<Arc<dyn ObjectStore>>,
        writer_index: usize,
        hash_kind: HashKind,
    ) -> Self {
        assert!(!stores.is_empty(), "ObjectDb must have at least one store");
        assert!(writer_index < stores.len(), "writer_index out of range");
        Self {
            stores,
            writer_index,
            hash_kind,
        }
    }

    pub fn hash_kind(&self) -> HashKind {
        self.hash_kind
    }

    pub fn contains(&self, id: &ObjectId) -> Result<bool, OdbError> {
        for s in &self.stores {
            if s.contains(id)? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    pub fn read(&self, id: &ObjectId) -> Result<RawObject, OdbError> {
        for s in &self.stores {
            if let Some(obj) = s.read(id)? {
                return Ok(obj);
            }
        }
        Err(OdbError::NotFound(*id))
    }

    pub fn read_header(&self, id: &ObjectId) -> Result<(ObjectKind, u64), OdbError> {
        for s in &self.stores {
            if let Some(h) = s.read_header(id)? {
                return Ok(h);
            }
        }
        Err(OdbError::NotFound(*id))
    }

    pub fn write(&self, obj: &RawObject) -> Result<ObjectId, OdbError> {
        self.stores[self.writer_index].write(obj)
    }

    /// Resolve a hex prefix across every store. Combines matches; returns
    /// `Found` only if exactly one unique oid matches across all stores.
    pub fn resolve_prefix(&self, prefix: &str) -> Result<PrefixMatch, OdbError> {
        let mut all = Vec::new();
        for s in &self.stores {
            match s.resolve_prefix(prefix)? {
                PrefixMatch::Found(o) => all.push(o),
                PrefixMatch::Ambiguous(v) => all.extend(v),
                PrefixMatch::None => {}
            }
        }
        all.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
        all.dedup();
        Ok(match all.len() {
            0 => PrefixMatch::None,
            1 => PrefixMatch::Found(all.into_iter().next().unwrap()),
            _ => PrefixMatch::Ambiguous(all),
        })
    }
}

#[derive(Error, Debug)]
pub enum OdbError {
    #[error("object not found: {0}")]
    NotFound(ObjectId),

    #[error("ambiguous object prefix '{prefix}': {} candidates", .candidates.len())]
    AmbiguousPrefix {
        prefix: String,
        candidates: Vec<ObjectId>,
    },

    #[error("malformed object {oid}: {reason}")]
    Corrupt { oid: ObjectId, reason: String },

    #[error("unsupported operation on this backend")]
    Unsupported,

    #[error("io error on {path}: {source}")]
    Io {
        path: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error(transparent)]
    Hash(#[from] HashError),

    #[error(transparent)]
    Object(#[from] ObjectError),
}
