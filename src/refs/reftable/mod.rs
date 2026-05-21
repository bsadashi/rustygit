//! Reftable ref-storage backend (`extensions.refStorage = reftable`).
//!
//! Reftable is git's binary, log-aware ref format described in
//! `Documentation/technical/reftable.adoc`. A `.git/reftable/` directory holds
//! a stack of immutable `.ref` table files plus a plain-text `tables.list`
//! enumerating them oldest-to-newest. Reads cascade newest → oldest; writes
//! append a single new table per transaction.
//!
//! This module currently implements:
//!   * v1 format (24-byte header, 68-byte footer, SHA-1 hash).
//!   * Ref blocks: read + write, with restart points and prefix compression.
//!   * Log blocks: read + write, zlib-compressed payloads.
//!   * Stack iteration with newest-wins merge for `read()` and `iter()`.
//!   * Append-only transactions: one new `.ref` file per `commit()`, stack
//!     update via `tables.list.lock` + atomic rename.
//!
//! Deferred:
//!   * Obj blocks and the ref/obj/log indexes (single-block files only).
//!   * Multi-level indexes for large tables.
//!   * Compaction (always grows the stack).
//!   * v2 SHA-256 (the type plumbing is in place but untested).
//!
//! All spec references in comments cite the section numbering from the
//! upstream `reftable.adoc`.

pub mod format;
pub mod reader;
pub mod transaction;
pub mod writer;

use std::path::PathBuf;
use std::sync::{Mutex, RwLock};

use crate::hash::HashKind;

use super::{FullName, RefError, RefStore, RefTransactionTrait, Reference};
use reader::StackReader;

pub use format::ReftableFormatError;

/// `RefStore` impl backed by `.git/reftable/`.
pub struct ReftableStore {
    reftable_dir: PathBuf,
    hash_kind: HashKind,
    /// Cached open readers, refreshed when tables.list mtime changes.
    stack: RwLock<StackReader>,
    /// Serializes writers — the file `tables.list.lock` is the canonical
    /// inter-process lock, but inside one process we want to avoid contention.
    write_lock: Mutex<()>,
}

impl ReftableStore {
    /// Open an existing reftable directory. Creates `tables.list` if missing.
    pub fn open(reftable_dir: PathBuf, hash_kind: HashKind) -> Result<Self, RefError> {
        if !reftable_dir.is_dir() {
            std::fs::create_dir_all(&reftable_dir).map_err(|e| RefError::Io {
                path: reftable_dir.clone(),
                source: e,
            })?;
        }
        let tables_list = reftable_dir.join("tables.list");
        if !tables_list.exists() {
            // Create empty stack file so other ops can proceed.
            std::fs::write(&tables_list, b"").map_err(|e| RefError::Io {
                path: tables_list.clone(),
                source: e,
            })?;
        }
        let stack = StackReader::load(&reftable_dir, hash_kind)?;
        Ok(Self {
            reftable_dir,
            hash_kind,
            stack: RwLock::new(stack),
            write_lock: Mutex::new(()),
        })
    }

    pub fn reftable_dir(&self) -> &std::path::Path {
        &self.reftable_dir
    }

    pub fn hash_kind(&self) -> HashKind {
        self.hash_kind
    }

    /// Refresh the in-memory stack from `tables.list` on disk. Called after
    /// commit() and before reads that might race with another writer.
    pub fn refresh(&self) -> Result<(), RefError> {
        let new = StackReader::load(&self.reftable_dir, self.hash_kind)?;
        let mut guard = self.stack.write().expect("stack lock poisoned");
        *guard = new;
        Ok(())
    }

    /// Read all reflog entries for a single ref from the stack (newest first).
    /// Used by the `reflog` plumbing — the stack already stores log records in
    /// (refname asc, update_index desc) order.
    pub fn read_reflog(&self, name: &FullName) -> Result<Vec<reader::LogRecordRead>, RefError> {
        let guard = self.stack.read().expect("stack lock poisoned");
        guard.read_reflog(name)
    }

    pub(crate) fn write_guard(&self) -> std::sync::MutexGuard<'_, ()> {
        self.write_lock.lock().expect("write lock poisoned")
    }
}

impl RefStore for ReftableStore {
    fn read(&self, name: &FullName) -> Result<Option<Reference>, RefError> {
        let guard = self.stack.read().expect("stack lock poisoned");
        guard.read(name)
    }

    fn iter<'a>(
        &'a self,
        prefix: Option<&str>,
    ) -> Box<dyn Iterator<Item = Result<Reference, RefError>> + 'a> {
        let guard = self.stack.read().expect("stack lock poisoned");
        // Materialize — the iterator borrows the guard, so we collect to free
        // the lock before returning.
        let items: Vec<_> = guard.iter(prefix).collect();
        Box::new(items.into_iter())
    }

    fn transaction(&self) -> Box<dyn RefTransactionTrait + '_> {
        Box::new(transaction::ReftableTransaction::new(self))
    }
}

// Re-export the reader's TableReader for sibling test crates that want to
// crack open an individual `.ref` file directly.
pub use reader::{
    LogRecordRead, RefRecord, RefValue, StackReader as StackReaderHandle, TableReader,
};
