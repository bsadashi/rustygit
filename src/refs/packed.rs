//! `packed-refs` parser and read-only ref backend.
//!
//! File format (one ref per line):
//!
//! ```text
//! # pack-refs with: peeled fully-peeled sorted
//! <oid> <ref-name>
//! ^<peeled-oid>      // optional: peeled annotated-tag commit, follows its tag line
//! ```
//!
//! Comments start with `#` and are typically just the header. We tolerate
//! arbitrary leading comment lines and silently drop the peeled-tag info
//! (M2 doesn't need it; tag deref happens via reading the tag object).

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use crate::hash::{HashKind, ObjectId};

use super::{FullName, RefError, RefStore, RefTarget, RefTransactionTrait, Reference};

pub struct PackedRefStore {
    path: PathBuf,
    hash_kind: HashKind,
}

impl PackedRefStore {
    pub fn new(path: PathBuf, hash_kind: HashKind) -> Self {
        Self { path, hash_kind }
    }

    /// Parse the entire packed-refs file into a name → oid map.
    pub fn load(&self) -> Result<BTreeMap<FullName, ObjectId>, RefError> {
        let bytes = match fs::read(&self.path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
            Err(e) => {
                return Err(RefError::Io {
                    path: self.path.clone(),
                    source: e,
                });
            }
        };
        let text = std::str::from_utf8(&bytes).map_err(|_| RefError::Malformed {
            name: self.path.display().to_string(),
            reason: "packed-refs is not valid UTF-8".into(),
        })?;
        let mut out = BTreeMap::new();
        for line in text.lines() {
            if line.is_empty() {
                continue;
            }
            if line.starts_with('#') || line.starts_with('^') {
                // header line, or peeled-tag annotation we don't track yet
                continue;
            }
            let (oid_str, name_str) = line.split_once(' ').ok_or_else(|| RefError::Malformed {
                name: self.path.display().to_string(),
                reason: format!("bad packed-refs line: {line:?}"),
            })?;
            let oid = ObjectId::parse_hex(self.hash_kind, oid_str.trim())?;
            let name = FullName::new(name_str.trim())?;
            out.insert(name, oid);
        }
        Ok(out)
    }
}

impl RefStore for PackedRefStore {
    fn read(&self, name: &FullName) -> Result<Option<Reference>, RefError> {
        let map = self.load()?;
        Ok(map.get(name).map(|o| Reference {
            name: name.clone(),
            target: RefTarget::Direct(*o),
        }))
    }

    fn iter<'a>(
        &'a self,
        prefix: Option<&str>,
    ) -> Box<dyn Iterator<Item = Result<Reference, RefError>> + 'a> {
        let map = match self.load() {
            Ok(m) => m,
            Err(e) => return Box::new(std::iter::once(Err(e))),
        };
        let prefix = prefix.map(|s| s.to_string());
        Box::new(map.into_iter().filter_map(move |(name, oid)| {
            if let Some(p) = &prefix {
                if !name.as_str().starts_with(p) {
                    return None;
                }
            }
            Some(Ok(Reference {
                name,
                target: RefTarget::Direct(oid),
            }))
        }))
    }

    fn transaction(&self) -> Box<dyn RefTransactionTrait + '_> {
        Box::new(NoopTransaction)
    }
}

/// Packed-refs is read-only from the per-ref-update path; rewriting it is the
/// job of the (future) `pack-refs` command. The composite store routes all
/// writes to the loose backend, so this is fine.
struct NoopTransaction;

impl RefTransactionTrait for NoopTransaction {
    fn update(
        &mut self,
        _name: &FullName,
        _expected: super::ExpectedOldValue,
        _new: super::NewValue,
        _reflog: super::ReflogMessage,
    ) -> Result<(), RefError> {
        Err(super::RefUpdateError::ReadOnlyBackend.into())
    }

    fn delete(
        &mut self,
        _name: &FullName,
        _expected: super::ExpectedOldValue,
    ) -> Result<(), RefError> {
        Err(super::RefUpdateError::ReadOnlyBackend.into())
    }

    fn commit(self: Box<Self>) -> Result<(), RefError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn parse_simple_packed_refs() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("packed-refs");
        fs::write(
            &path,
            "# pack-refs with: peeled fully-peeled sorted \n\
             abcdef0123456789abcdef0123456789abcdef01 refs/heads/main\n\
             1234567890abcdef1234567890abcdef12345678 refs/tags/v1\n\
             ^abcdef0123456789abcdef0123456789abcdef01\n",
        )
        .unwrap();
        let store = PackedRefStore::new(path, HashKind::Sha1);
        let map = store.load().unwrap();
        assert_eq!(map.len(), 2);
        let main = FullName::new("refs/heads/main").unwrap();
        let v1 = FullName::new("refs/tags/v1").unwrap();
        assert!(map.contains_key(&main));
        assert!(map.contains_key(&v1));
    }

    #[test]
    fn missing_packed_refs_is_empty_map() {
        let dir = tempdir().unwrap();
        let store = PackedRefStore::new(dir.path().join("packed-refs"), HashKind::Sha1);
        assert!(store.load().unwrap().is_empty());
    }
}
