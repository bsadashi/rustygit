//! Refs (ADR A3).
//!
//! All ref reads and writes go through `RefStore`. Writes are always
//! transactional — a `RefTransaction` accumulates updates/deletes, optionally
//! verifies expected old values, and commits atomically (within the limits of
//! whatever lockfile + rename guarantees the underlying filesystem provides).
//!
//! Backends: `LooseRefStore` reads/writes individual files under `.git/refs/`
//! and `.git/HEAD`. `PackedRefStore` reads `.git/packed-refs`. `CompositeRefStore`
//! merges both — loose first, packed as fallback. `ReftableStore` is the v1
//! reftable backend selected via `extensions.refStorage = reftable` (POLISH §8).

pub mod loose;
pub mod name;
pub mod packed;
pub mod reflog;
pub mod reftable;
pub mod transaction;

pub use loose::LooseRefStore;
pub use name::{FullName, RefNameError};
pub use packed::PackedRefStore;
pub use reftable::ReftableStore;
pub use transaction::{
    CompositeRefStore, ExpectedOldValue, NewValue, RefUpdateError, ReflogMessage,
};

use thiserror::Error;

use crate::hash::ObjectId;

/// What a ref points at: another oid or another ref name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefTarget {
    Direct(ObjectId),
    Symbolic(FullName),
}

impl RefTarget {
    /// Walk symbolic-ref chains until a direct oid is found, or the depth cap
    /// is reached. Mirrors git's `MAXDEPTH = 5`.
    pub fn resolve(
        store: &dyn RefStore,
        start: &FullName,
    ) -> Result<Option<(FullName, ObjectId)>, RefError> {
        const MAX_DEPTH: usize = 5;
        let mut name = start.clone();
        for _ in 0..MAX_DEPTH {
            match store.read(&name)? {
                None => return Ok(None),
                Some(r) => match r.target {
                    RefTarget::Direct(o) => return Ok(Some((r.name, o))),
                    RefTarget::Symbolic(next) => name = next,
                },
            }
        }
        Err(RefError::SymbolicCycle(name.into_string()))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reference {
    pub name: FullName,
    pub target: RefTarget,
}

/// Backend-agnostic interface to a refs store.
pub trait RefStore: Send + Sync {
    /// Look up a single ref by exact full name.
    fn read(&self, name: &FullName) -> Result<Option<Reference>, RefError>;

    /// Iterate all refs, optionally filtered by full-name prefix
    /// (e.g. `Some("refs/heads/")`).
    fn iter<'a>(
        &'a self,
        prefix: Option<&str>,
    ) -> Box<dyn Iterator<Item = Result<Reference, RefError>> + 'a>;

    /// Begin a transaction that batches updates and commits atomically.
    fn transaction(&self) -> Box<dyn RefTransactionTrait + '_>;
}

/// Object-safe trait for transactions. The concrete impls do most of the work
/// in `commit`.
pub trait RefTransactionTrait {
    fn update(
        &mut self,
        name: &FullName,
        expected: ExpectedOldValue,
        new: NewValue,
        reflog: ReflogMessage,
    ) -> Result<(), RefError>;

    fn delete(&mut self, name: &FullName, expected: ExpectedOldValue) -> Result<(), RefError>;

    fn commit(self: Box<Self>) -> Result<(), RefError>;
}

#[derive(Error, Debug)]
pub enum RefError {
    #[error("invalid ref name: {0}")]
    Name(#[from] RefNameError),

    #[error("symbolic ref cycle starting at {0}")]
    SymbolicCycle(String),

    #[error("malformed ref content for {name}: {reason}")]
    Malformed { name: String, reason: String },

    #[error("io error on {path}: {source}")]
    Io {
        path: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("ref update failed: {0}")]
    Update(#[from] RefUpdateError),

    #[error(transparent)]
    Hash(#[from] crate::hash::HashError),

    #[error("lockfile error: {0}")]
    Lock(#[from] crate::lockfile::LockError),
}
