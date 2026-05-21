//! `PackStore` — an `ObjectStore` implementation backed by a single pack and its
//! companion idx file.
//!
//! A pack stores objects in three flavours:
//!   - `Direct(kind)` — the raw zlib-compressed object body.
//!   - `OfsDelta { base_offset }` — a delta against an earlier object in the
//!     same pack, keyed by absolute pack offset.
//!   - `RefDelta { base_oid }` — a delta against any object in the wider odb,
//!     keyed by oid. If the base lives in another pack, this `PackStore`
//!     returns "not found"; the wider `ObjectDb` is responsible for cross-pack
//!     resolution.
//!
//! Resolution walks back along the delta chain to a non-delta base, then
//! replays each delta's instruction stream forwards. We cache decoded bases in
//! a tiny hand-rolled LRU keyed by `(pack_offset)` so chains sharing a base
//! don't redo the work.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::hash::{HashKind, ObjectId};
use crate::object::{ObjectKind, RawObject};
use crate::odb::{ObjectStore, OdbError, PrefixMatch};

use super::delta::apply_delta;
use super::{IdxFile, PackEntryKind, PackError, PackFile};

/// Cache size — small enough to be cheap, large enough to absorb the bursty
/// "this delta chain reuses the same base" pattern most packs exhibit.
const CACHE_CAPACITY: usize = 64;

/// Tiny ad-hoc LRU. Backed by a BTreeMap of (key, age) and a counter that
/// monotonically increases on every access. Eviction looks for the smallest age.
///
/// Capacity is fixed at construction. We do *not* pull in the `lru` crate.
struct Lru<K: Ord + Clone, V> {
    capacity: usize,
    counter: u64,
    /// key -> (value, last-access age)
    map: BTreeMap<K, (V, u64)>,
}

impl<K: Ord + Clone, V> Lru<K, V> {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            counter: 0,
            map: BTreeMap::new(),
        }
    }

    fn get(&mut self, key: &K) -> Option<&V> {
        self.counter += 1;
        let age = self.counter;
        let entry = self.map.get_mut(key)?;
        entry.1 = age;
        Some(&entry.0)
    }

    fn insert(&mut self, key: K, value: V) {
        self.counter += 1;
        let age = self.counter;
        if !self.map.contains_key(&key) && self.map.len() >= self.capacity {
            // Evict the entry with the smallest age.
            if let Some(oldest_key) = self
                .map
                .iter()
                .min_by_key(|(_, (_, a))| *a)
                .map(|(k, _)| k.clone())
            {
                self.map.remove(&oldest_key);
            }
        }
        self.map.insert(key, (value, age));
    }
}

/// Cached value: a fully-resolved object kind + payload, indexed by pack offset.
type CacheEntry = (ObjectKind, Arc<Vec<u8>>);

pub struct PackStore {
    pack: Arc<PackFile>,
    idx: Arc<IdxFile>,
    /// LRU keyed by pack offset → resolved (kind, bytes).
    cache: Mutex<Lru<u64, CacheEntry>>,
}

impl PackStore {
    pub fn open(pack_path: &Path, idx_path: &Path, hash_kind: HashKind) -> Result<Self, PackError> {
        let pack = PackFile::open(pack_path, hash_kind)?;
        let idx = IdxFile::open(idx_path, hash_kind)?;
        if pack.object_count() != idx.object_count() {
            return Err(PackError::Malformed(
                "pack and idx disagree on object count",
            ));
        }
        Ok(Self {
            pack: Arc::new(pack),
            idx: Arc::new(idx),
            cache: Mutex::new(Lru::new(CACHE_CAPACITY)),
        })
    }

    /// Convenience: open `<base>.pack` + `<base>.idx`, given either path.
    pub fn open_pair(pack_path: &Path, hash_kind: HashKind) -> Result<Self, PackError> {
        let (pack, idx) = pair_paths(pack_path);
        Self::open(&pack, &idx, hash_kind)
    }

    pub fn pack(&self) -> &PackFile {
        &self.pack
    }

    pub fn idx(&self) -> &IdxFile {
        &self.idx
    }

    /// Resolve the object stored at the given pack offset, walking delta chains
    /// as needed. Returns the final (kind, bytes).
    fn resolve_at_offset(&self, offset: u64) -> Result<(ObjectKind, Arc<Vec<u8>>), OdbError> {
        // Cache hit?
        if let Some(hit) = self.cache_get(offset) {
            return Ok(hit);
        }

        let entry = self.pack.read_entry_at(offset).map_err(packerr_to_odb)?;
        let result = match entry.kind {
            PackEntryKind::Direct(kind) => (kind, Arc::new(entry.data)),
            PackEntryKind::OfsDelta { base_offset } => {
                let (base_kind, base_bytes) = self.resolve_at_offset(base_offset)?;
                let patched =
                    apply_delta(&base_bytes, &entry.data).map_err(|e| OdbError::Corrupt {
                        oid: ObjectId::null(self.pack.hash_kind()),
                        reason: format!("delta at {offset}: {e}"),
                    })?;
                (base_kind, Arc::new(patched))
            }
            PackEntryKind::RefDelta { base_oid } => {
                // Base must live in this pack. Cross-pack base resolution is
                // the ObjectDb's job — return NotFound here.
                let Some(base_offset) = self.idx.lookup(&base_oid) else {
                    return Err(OdbError::NotFound(base_oid));
                };
                let (base_kind, base_bytes) = self.resolve_at_offset(base_offset)?;
                let patched =
                    apply_delta(&base_bytes, &entry.data).map_err(|e| OdbError::Corrupt {
                        oid: ObjectId::null(self.pack.hash_kind()),
                        reason: format!("delta at {offset}: {e}"),
                    })?;
                (base_kind, Arc::new(patched))
            }
        };
        self.cache_insert(offset, result.clone());
        Ok(result)
    }

    fn cache_get(&self, offset: u64) -> Option<(ObjectKind, Arc<Vec<u8>>)> {
        let mut g = self.cache.lock().ok()?;
        g.get(&offset).cloned()
    }

    fn cache_insert(&self, offset: u64, value: (ObjectKind, Arc<Vec<u8>>)) {
        if let Ok(mut g) = self.cache.lock() {
            g.insert(offset, value);
        }
    }
}

/// Map a pack-level error to OdbError. We translate "object not found" into
/// `OdbError::Unsupported`-style cases; most pack errors become `Corrupt`.
fn packerr_to_odb(e: PackError) -> OdbError {
    match e {
        PackError::Io { path, source } => OdbError::Io { path, source },
        other => OdbError::Corrupt {
            oid: ObjectId::null(HashKind::Sha1),
            reason: format!("{other}"),
        },
    }
}

/// Given a path that may end in `.pack` or `.idx` (or neither), return the
/// `(pack, idx)` sibling pair.
fn pair_paths(input: &Path) -> (PathBuf, PathBuf) {
    let s = input.to_string_lossy();
    let stripped: &str = if let Some(rest) = s.strip_suffix(".pack") {
        rest
    } else if let Some(rest) = s.strip_suffix(".idx") {
        rest
    } else {
        s.as_ref()
    };
    let pack = PathBuf::from(format!("{stripped}.pack"));
    let idx = PathBuf::from(format!("{stripped}.idx"));
    (pack, idx)
}

impl ObjectStore for PackStore {
    fn contains(&self, id: &ObjectId) -> Result<bool, OdbError> {
        Ok(self.idx.lookup(id).is_some())
    }

    fn read(&self, id: &ObjectId) -> Result<Option<RawObject>, OdbError> {
        let Some(offset) = self.idx.lookup(id) else {
            return Ok(None);
        };
        let (kind, bytes) = self.resolve_at_offset(offset)?;
        // Defensive copy out of the Arc — RawObject owns its data.
        Ok(Some(RawObject::new(kind, (*bytes).clone())))
    }

    fn read_header(&self, id: &ObjectId) -> Result<Option<(ObjectKind, u64)>, OdbError> {
        // For non-delta entries we could read just the header; for deltas we
        // currently have to resolve the chain to know the final kind+size.
        // That's fine for M7 — this matches loose store's first-cut behaviour.
        let Some(offset) = self.idx.lookup(id) else {
            return Ok(None);
        };
        let (kind, bytes) = self.resolve_at_offset(offset)?;
        Ok(Some((kind, bytes.len() as u64)))
    }

    fn write(&self, _obj: &RawObject) -> Result<ObjectId, OdbError> {
        Err(OdbError::Unsupported)
    }

    fn iter(&self) -> Box<dyn Iterator<Item = Result<ObjectId, OdbError>> + '_> {
        Box::new(self.idx.iter().map(|(oid, _off)| Ok(oid)))
    }

    fn resolve_prefix(&self, prefix: &str) -> Result<PrefixMatch, OdbError> {
        let matches = self.idx.resolve_prefix(prefix);
        Ok(match matches.len() {
            0 => PrefixMatch::None,
            1 => PrefixMatch::Found(matches.into_iter().next().unwrap()),
            _ => PrefixMatch::Ambiguous(matches),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lru_evicts_least_recently_used() {
        let mut lru: Lru<u64, u32> = Lru::new(3);
        lru.insert(1, 100);
        lru.insert(2, 200);
        lru.insert(3, 300);
        // Touch 1 so 2 is now LRU.
        let _ = lru.get(&1);
        // Insert a fourth; 2 should be evicted.
        lru.insert(4, 400);
        assert!(lru.map.contains_key(&1));
        assert!(!lru.map.contains_key(&2));
        assert!(lru.map.contains_key(&3));
        assert!(lru.map.contains_key(&4));
    }

    #[test]
    fn lru_overwrite_existing() {
        let mut lru: Lru<u64, u32> = Lru::new(2);
        lru.insert(1, 100);
        lru.insert(1, 101); // same key
        assert_eq!(lru.map.len(), 1);
        assert_eq!(lru.get(&1), Some(&101));
    }

    #[test]
    fn pair_paths_handles_pack_extension() {
        let (p, i) = pair_paths(Path::new("/x/foo.pack"));
        assert_eq!(p, PathBuf::from("/x/foo.pack"));
        assert_eq!(i, PathBuf::from("/x/foo.idx"));
    }

    #[test]
    fn pair_paths_handles_idx_extension() {
        let (p, i) = pair_paths(Path::new("/x/foo.idx"));
        assert_eq!(p, PathBuf::from("/x/foo.pack"));
        assert_eq!(i, PathBuf::from("/x/foo.idx"));
    }

    #[test]
    fn pair_paths_handles_basename_only() {
        let (p, i) = pair_paths(Path::new("/x/foo"));
        assert_eq!(p, PathBuf::from("/x/foo.pack"));
        assert_eq!(i, PathBuf::from("/x/foo.idx"));
    }

    // Integration tests against a real pack require Track A's PackFile/IdxFile
    // to be implemented. They live in `tests/m7_compat.rs` so they can be gated
    // on `git` being available on PATH.
}
