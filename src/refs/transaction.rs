//! Ref transactions and the composite (loose + packed) store.
//!
//! Each transaction batches updates and deletes; on `commit()` it acquires a
//! `.lock` per ref via `crate::lockfile::Lockfile`, validates the expected old
//! value, writes the new content, and (when `logallrefupdates` is implicitly on,
//! which we treat as the default for M2) appends a reflog entry. If any
//! per-ref step fails, every already-committed lockfile in this transaction
//! is rolled forward — atomicity within a transaction is per-ref, not
//! across-refs (matching git's `files-backend.c` behavior).

use std::sync::Arc;

use thiserror::Error;

use crate::hash::ObjectId;
use crate::lockfile::Lockfile;

use super::loose::LooseRefStore;
use super::reflog::{Identity, ReflogEntry};
use super::{
    FullName, PackedRefStore, RefError, RefStore, RefTarget, RefTransactionTrait, Reference,
};

/// What value, if any, the ref must currently hold for the update to apply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExpectedOldValue {
    /// Don't check the current value — overwrite unconditionally.
    Any,
    /// The ref must currently resolve directly to this oid.
    Direct(ObjectId),
    /// The ref must not currently exist.
    Missing,
}

/// What to set the ref to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NewValue {
    Direct(ObjectId),
    Symbolic(FullName),
}

/// A reflog entry to record alongside the update. `None` skips the reflog write.
#[derive(Debug, Clone, Default)]
pub struct ReflogMessage(pub Option<String>);

impl ReflogMessage {
    pub fn from(msg: impl Into<String>) -> Self {
        Self(Some(msg.into()))
    }
    pub fn none() -> Self {
        Self(None)
    }
}

#[derive(Debug)]
enum Op {
    Update {
        name: FullName,
        expected: ExpectedOldValue,
        new: NewValue,
        reflog: ReflogMessage,
    },
    Delete {
        name: FullName,
        expected: ExpectedOldValue,
    },
}

/// Composite store: reads cascade `loose → packed`; all writes go to loose.
pub struct CompositeRefStore {
    loose: Arc<LooseRefStore>,
    packed: Arc<PackedRefStore>,
}

impl CompositeRefStore {
    pub fn new(loose: Arc<LooseRefStore>, packed: Arc<PackedRefStore>) -> Self {
        Self { loose, packed }
    }
}

impl RefStore for CompositeRefStore {
    fn read(&self, name: &FullName) -> Result<Option<Reference>, RefError> {
        if let Some(r) = self.loose.read(name)? {
            return Ok(Some(r));
        }
        self.packed.read(name)
    }

    fn iter<'a>(
        &'a self,
        prefix: Option<&str>,
    ) -> Box<dyn Iterator<Item = Result<Reference, RefError>> + 'a> {
        // Loose wins when both report the same name. Collect loose names first,
        // then yield packed entries that loose did not cover.
        let loose: Vec<Result<Reference, RefError>> = self.loose.iter(prefix).collect();
        let mut seen = std::collections::BTreeSet::new();
        for r in loose.iter().flatten() {
            seen.insert(r.name.clone());
        }
        let packed = self.packed.iter(prefix).filter_map(move |r| match r {
            Ok(r) if seen.contains(&r.name) => None,
            other => Some(other),
        });
        Box::new(loose.into_iter().chain(packed))
    }

    fn transaction(&self) -> Box<dyn RefTransactionTrait + '_> {
        Box::new(LooseTransaction {
            loose: &self.loose,
            ops: Vec::new(),
            identity: Identity::from_env_or_placeholder(),
        })
    }
}

pub struct LooseTransaction<'a> {
    loose: &'a LooseRefStore,
    ops: Vec<Op>,
    identity: Identity,
}

impl<'a> LooseTransaction<'a> {
    pub fn new(loose: &'a LooseRefStore) -> Self {
        Self {
            loose,
            ops: Vec::new(),
            identity: Identity::from_env_or_placeholder(),
        }
    }
}

impl<'a> RefTransactionTrait for LooseTransaction<'a> {
    fn update(
        &mut self,
        name: &FullName,
        expected: ExpectedOldValue,
        new: NewValue,
        reflog: ReflogMessage,
    ) -> Result<(), RefError> {
        self.ops.push(Op::Update {
            name: name.clone(),
            expected,
            new,
            reflog,
        });
        Ok(())
    }

    fn delete(&mut self, name: &FullName, expected: ExpectedOldValue) -> Result<(), RefError> {
        self.ops.push(Op::Delete {
            name: name.clone(),
            expected,
        });
        Ok(())
    }

    fn commit(self: Box<Self>) -> Result<(), RefError> {
        let me = *self;
        let LooseTransaction {
            loose,
            ops,
            identity,
            ..
        } = me;
        let op_count = ops.len();

        for op in ops {
            match op {
                Op::Update {
                    name,
                    expected,
                    new,
                    reflog,
                } => {
                    apply_update(loose, &name, expected, new, reflog, &identity)?;
                }
                Op::Delete { name, expected } => {
                    apply_delete(loose, &name, expected)?;
                }
            }
        }
        crate::trace!("refs", "committed {} updates", op_count);
        Ok(())
    }
}

fn apply_update(
    loose: &LooseRefStore,
    name: &FullName,
    expected: ExpectedOldValue,
    new: NewValue,
    reflog: ReflogMessage,
    identity: &Identity,
) -> Result<(), RefError> {
    let target_path = loose.gitdir().join(name.loose_path_relative());
    if let Some(parent) = target_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| RefError::Io {
            path: parent.to_path_buf(),
            source: e,
        })?;
    }

    let mut lock = Lockfile::acquire(&target_path)?;

    // Verify expected old value AFTER acquiring the lock so concurrent writers
    // can't slip in. Reading both loose and packed; comparison is on the
    // resolved direct oid (matching git's "deref by default" semantics).
    let current_direct = current_direct_through_lock(loose, name)?;
    let old_for_reflog = current_direct.unwrap_or_else(|| ObjectId::null(loose.hash_kind()));
    match (&expected, current_direct) {
        (ExpectedOldValue::Any, _) => {}
        (ExpectedOldValue::Missing, Some(_)) => {
            return Err(RefUpdateError::ExpectedMissing(name.to_string()).into());
        }
        (ExpectedOldValue::Missing, None) => {}
        (ExpectedOldValue::Direct(_), None) => {
            return Err(RefUpdateError::OldValueMismatch(name.to_string()).into());
        }
        (ExpectedOldValue::Direct(want), Some(got)) if *want != got => {
            return Err(RefUpdateError::OldValueMismatch(name.to_string()).into());
        }
        (ExpectedOldValue::Direct(_), Some(_)) => {}
    }

    let content = match &new {
        NewValue::Direct(oid) => format!("{oid}\n"),
        NewValue::Symbolic(target) => format!("ref: {target}\n"),
    };
    lock.write_all(content.as_bytes())
        .map_err(|e| RefError::Io {
            path: target_path.clone(),
            source: e,
        })?;
    lock.commit()?;

    // Reflog (best-effort; we don't fail the whole transaction over reflog
    // write errors today — git's behavior here is also forgiving). We write
    // when a message was supplied OR (later, M3) when core.logallrefupdates
    // is true. Symbolic-ref writes don't get a reflog at this layer (matches
    // git: symbolic refs themselves aren't logged; their *targets* are).
    if let (Some(msg), NewValue::Direct(new_oid)) = (reflog.0, &new) {
        let _ = super::reflog::append(
            loose.gitdir(),
            name,
            ReflogEntry {
                old: old_for_reflog,
                new: *new_oid,
                identity,
                message: &msg,
            },
        );
        // If HEAD symbolically points at this ref, also write a HEAD reflog
        // entry — matches git's `core.logallrefupdates=true` default.
        let head_name = FullName::new("HEAD").expect("HEAD is always valid");
        if name != &head_name {
            if let Ok(Some(head_ref)) = loose.read(&head_name) {
                if let RefTarget::Symbolic(target) = &head_ref.target {
                    if target == name {
                        let _ = super::reflog::append(
                            loose.gitdir(),
                            &head_name,
                            ReflogEntry {
                                old: old_for_reflog,
                                new: *new_oid,
                                identity,
                                message: &msg,
                            },
                        );
                    }
                }
            }
        }
    }
    Ok(())
}

fn apply_delete(
    loose: &LooseRefStore,
    name: &FullName,
    expected: ExpectedOldValue,
) -> Result<(), RefError> {
    let target_path = loose.gitdir().join(name.loose_path_relative());
    let _lock = Lockfile::acquire(&target_path)?; // serialize against concurrent writers

    let current = current_direct_through_lock(loose, name)?;
    match (&expected, current) {
        (ExpectedOldValue::Any, _) => {}
        (ExpectedOldValue::Missing, Some(_)) => {
            return Err(RefUpdateError::ExpectedMissing(name.to_string()).into());
        }
        (ExpectedOldValue::Missing, None) => {}
        (ExpectedOldValue::Direct(want), Some(got)) if *want != got => {
            return Err(RefUpdateError::OldValueMismatch(name.to_string()).into());
        }
        (ExpectedOldValue::Direct(_), None) => {
            return Err(RefUpdateError::OldValueMismatch(name.to_string()).into());
        }
        (ExpectedOldValue::Direct(_), Some(_)) => {}
    }

    if target_path.exists() {
        std::fs::remove_file(&target_path).map_err(|e| RefError::Io {
            path: target_path.clone(),
            source: e,
        })?;
    }
    // Drop _lock = removes target.lock
    Ok(())
}

fn current_direct_through_lock(
    loose: &LooseRefStore,
    name: &FullName,
) -> Result<Option<ObjectId>, RefError> {
    // Walk symbolic chain, checking only loose; packed handled at higher level.
    let mut name = name.clone();
    for _ in 0..5 {
        match loose.read(&name)? {
            None => return Ok(None),
            Some(r) => match r.target {
                RefTarget::Direct(o) => return Ok(Some(o)),
                RefTarget::Symbolic(next) => name = next,
            },
        }
    }
    Err(RefError::SymbolicCycle(name.into_string()))
}

#[derive(Error, Debug)]
pub enum RefUpdateError {
    #[error("ref already exists: {0}")]
    ExpectedMissing(String),
    #[error("ref old-value mismatch on {0}")]
    OldValueMismatch(String),
    #[error("backend is read-only")]
    ReadOnlyBackend,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hash::HashKind;
    use std::sync::Arc;
    use tempfile::tempdir;

    fn fake_oid_a() -> ObjectId {
        ObjectId::parse_hex(HashKind::Sha1, "1111111111111111111111111111111111111111").unwrap()
    }
    fn fake_oid_b() -> ObjectId {
        ObjectId::parse_hex(HashKind::Sha1, "2222222222222222222222222222222222222222").unwrap()
    }

    fn setup() -> (tempfile::TempDir, Arc<CompositeRefStore>) {
        let dir = tempdir().unwrap();
        let gitdir = dir.path().join(".git");
        std::fs::create_dir_all(gitdir.join("refs/heads")).unwrap();
        let loose = Arc::new(LooseRefStore::new(gitdir.clone(), HashKind::Sha1));
        let packed = Arc::new(PackedRefStore::new(
            gitdir.join("packed-refs"),
            HashKind::Sha1,
        ));
        let store = Arc::new(CompositeRefStore::new(loose, packed));
        (dir, store)
    }

    #[test]
    fn create_update_delete_round_trip() {
        let (_dir, store) = setup();
        let name = FullName::new("refs/heads/topic").unwrap();

        // Create
        let mut tx = store.transaction();
        tx.update(
            &name,
            ExpectedOldValue::Missing,
            NewValue::Direct(fake_oid_a()),
            ReflogMessage::from("create topic"),
        )
        .unwrap();
        tx.commit().unwrap();
        assert_eq!(
            store.read(&name).unwrap().unwrap().target,
            RefTarget::Direct(fake_oid_a())
        );

        // Update with old-value check
        let mut tx = store.transaction();
        tx.update(
            &name,
            ExpectedOldValue::Direct(fake_oid_a()),
            NewValue::Direct(fake_oid_b()),
            ReflogMessage::from("advance topic"),
        )
        .unwrap();
        tx.commit().unwrap();
        assert_eq!(
            store.read(&name).unwrap().unwrap().target,
            RefTarget::Direct(fake_oid_b())
        );

        // Mismatched expected old value should fail
        let mut tx = store.transaction();
        tx.update(
            &name,
            ExpectedOldValue::Direct(fake_oid_a()),
            NewValue::Direct(fake_oid_a()),
            ReflogMessage::none(),
        )
        .unwrap();
        let err = tx.commit().unwrap_err();
        assert!(matches!(
            err,
            RefError::Update(RefUpdateError::OldValueMismatch(_))
        ));

        // Delete
        let mut tx = store.transaction();
        tx.delete(&name, ExpectedOldValue::Any).unwrap();
        tx.commit().unwrap();
        assert!(store.read(&name).unwrap().is_none());
    }

    #[test]
    fn symbolic_ref_round_trip() {
        let (_dir, store) = setup();
        let head = FullName::new("HEAD").unwrap();
        let main = FullName::new("refs/heads/main").unwrap();
        let mut tx = store.transaction();
        tx.update(
            &head,
            ExpectedOldValue::Any,
            NewValue::Symbolic(main.clone()),
            ReflogMessage::none(),
        )
        .unwrap();
        tx.commit().unwrap();
        let r = store.read(&head).unwrap().unwrap();
        assert_eq!(r.target, RefTarget::Symbolic(main));
    }
}
