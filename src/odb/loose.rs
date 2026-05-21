//! Loose object store. Each object lives at `objects/aa/bbbbb...` (the first
//! two hex chars of the OID name a directory, the remaining 38 or 62 chars
//! name the file). On-disk format: zlib-compressed `<kind> <size>\0<payload>`.
//!
//! Writes go via temp + atomic rename. Reads are decompressed in full and
//! the framing header is parsed and validated.

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;
use flate2::Compression;

use crate::hash::{HashKind, ObjectId};
use crate::object::{ObjectKind, RawObject};

use super::{ObjectStore, OdbError, PrefixMatch};

pub struct LooseStore {
    objects_dir: PathBuf,
    hash_kind: HashKind,
}

impl LooseStore {
    pub fn new(objects_dir: PathBuf, hash_kind: HashKind) -> Self {
        Self {
            objects_dir,
            hash_kind,
        }
    }

    pub fn hash_kind(&self) -> HashKind {
        self.hash_kind
    }

    fn path_for(&self, id: &ObjectId) -> PathBuf {
        let hex = id.to_string();
        let (dir, file) = hex.split_at(2);
        self.objects_dir.join(dir).join(file)
    }

    fn read_framed(&self, id: &ObjectId) -> Result<Option<Vec<u8>>, OdbError> {
        let path = self.path_for(id);
        let bytes = match fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => {
                return Err(OdbError::Io {
                    path: path.clone(),
                    source: e,
                });
            }
        };
        let mut decoder = ZlibDecoder::new(&bytes[..]);
        let mut framed = Vec::new();
        decoder
            .read_to_end(&mut framed)
            .map_err(|e| OdbError::Corrupt {
                oid: *id,
                reason: format!("zlib decompression failed: {e}"),
            })?;
        Ok(Some(framed))
    }
}

impl ObjectStore for LooseStore {
    fn contains(&self, id: &ObjectId) -> Result<bool, OdbError> {
        Ok(self.path_for(id).is_file())
    }

    fn read(&self, id: &ObjectId) -> Result<Option<RawObject>, OdbError> {
        let Some(framed) = self.read_framed(id)? else {
            return Ok(None);
        };
        let null_pos = framed
            .iter()
            .position(|&b| b == 0)
            .ok_or_else(|| OdbError::Corrupt {
                oid: *id,
                reason: "missing null terminator in header".into(),
            })?;
        let header = std::str::from_utf8(&framed[..null_pos]).map_err(|_| OdbError::Corrupt {
            oid: *id,
            reason: "non-utf8 header".into(),
        })?;
        let (kind_str, len_str) = header.split_once(' ').ok_or_else(|| OdbError::Corrupt {
            oid: *id,
            reason: format!("malformed header: {header:?}"),
        })?;
        let kind = ObjectKind::parse(kind_str)?;
        let claimed_len: usize = len_str.parse().map_err(|_| OdbError::Corrupt {
            oid: *id,
            reason: format!("non-numeric size in header: {len_str:?}"),
        })?;
        let data = framed[null_pos + 1..].to_vec();
        if data.len() != claimed_len {
            return Err(OdbError::Corrupt {
                oid: *id,
                reason: format!(
                    "size mismatch: header says {claimed_len}, payload is {}",
                    data.len()
                ),
            });
        }
        Ok(Some(RawObject::new(kind, data)))
    }

    fn read_header(&self, id: &ObjectId) -> Result<Option<(ObjectKind, u64)>, OdbError> {
        // For loose objects we still have to inflate enough to find the null
        // byte, but we can stop reading the payload after that. For now read
        // the full thing — the optimization matters in pack land.
        let Some(framed) = self.read_framed(id)? else {
            return Ok(None);
        };
        let null_pos = framed
            .iter()
            .position(|&b| b == 0)
            .ok_or_else(|| OdbError::Corrupt {
                oid: *id,
                reason: "missing null terminator in header".into(),
            })?;
        let header = std::str::from_utf8(&framed[..null_pos]).map_err(|_| OdbError::Corrupt {
            oid: *id,
            reason: "non-utf8 header".into(),
        })?;
        let (kind_str, len_str) = header.split_once(' ').ok_or_else(|| OdbError::Corrupt {
            oid: *id,
            reason: format!("malformed header: {header:?}"),
        })?;
        let kind = ObjectKind::parse(kind_str)?;
        let size: u64 = len_str.parse().map_err(|_| OdbError::Corrupt {
            oid: *id,
            reason: "non-numeric size".into(),
        })?;
        Ok(Some((kind, size)))
    }

    fn write(&self, obj: &RawObject) -> Result<ObjectId, OdbError> {
        let oid = obj.oid(self.hash_kind);
        let path = self.path_for(&oid);
        if path.exists() {
            return Ok(oid);
        }
        let parent = path.parent().expect("loose object path has parent");
        fs::create_dir_all(parent).map_err(|e| OdbError::Io {
            path: parent.to_path_buf(),
            source: e,
        })?;

        let framed = obj.framed();
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&framed).map_err(|e| OdbError::Io {
            path: path.clone(),
            source: e,
        })?;
        let compressed = encoder.finish().map_err(|e| OdbError::Io {
            path: path.clone(),
            source: e,
        })?;

        // Atomic temp + rename. Loose objects are single-writer-per-oid by
        // design (the OID itself names the file), so we don't need a .lock.
        let tmp = path.with_extension("tmp");
        fs::write(&tmp, &compressed).map_err(|e| OdbError::Io {
            path: tmp.clone(),
            source: e,
        })?;
        fs::rename(&tmp, &path).map_err(|e| OdbError::Io {
            path: path.clone(),
            source: e,
        })?;
        crate::trace!("odb", "wrote loose {} ({} bytes)", oid, compressed.len());
        Ok(oid)
    }

    fn iter(&self) -> Box<dyn Iterator<Item = Result<ObjectId, OdbError>> + '_> {
        Box::new(LooseIter::new(&self.objects_dir, self.hash_kind))
    }

    fn resolve_prefix(&self, prefix: &str) -> Result<PrefixMatch, OdbError> {
        if prefix.len() < 2 {
            return Ok(PrefixMatch::None);
        }
        let prefix = prefix.to_lowercase();
        let (dir_part, rest) = prefix.split_at(2);
        let dir = self.objects_dir.join(dir_part);
        if !dir.is_dir() {
            return Ok(PrefixMatch::None);
        }
        let mut matches = Vec::new();
        for entry in fs::read_dir(&dir).map_err(|e| OdbError::Io {
            path: dir.clone(),
            source: e,
        })? {
            let entry = entry.map_err(|e| OdbError::Io {
                path: dir.clone(),
                source: e,
            })?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with(rest) {
                let full = format!("{dir_part}{name}");
                if let Ok(oid) = ObjectId::parse_hex(self.hash_kind, &full) {
                    matches.push(oid);
                }
            }
        }
        matches.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
        matches.dedup();
        Ok(match matches.len() {
            0 => PrefixMatch::None,
            1 => PrefixMatch::Found(matches.into_iter().next().unwrap()),
            _ => PrefixMatch::Ambiguous(matches),
        })
    }
}

struct LooseIter {
    hash_kind: HashKind,
    /// Iterator over the `aa/` shard directories, plus the current shard's
    /// inner iterator if any.
    shards: Option<fs::ReadDir>,
    current: Option<(String, fs::ReadDir)>,
}

impl LooseIter {
    fn new(objects_dir: &Path, hash_kind: HashKind) -> Self {
        let shards = fs::read_dir(objects_dir).ok();
        Self {
            hash_kind,
            shards,
            current: None,
        }
    }
}

impl Iterator for LooseIter {
    type Item = Result<ObjectId, OdbError>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some((shard_name, inner)) = self.current.as_mut() {
                match inner.next() {
                    Some(Ok(entry)) => {
                        let fname = entry.file_name();
                        let fname = fname.to_string_lossy().to_string();
                        let hex = format!("{shard_name}{fname}");
                        if hex.len() == self.hash_kind.hex_len() {
                            return Some(
                                ObjectId::parse_hex(self.hash_kind, &hex).map_err(Into::into),
                            );
                        }
                        continue;
                    }
                    Some(Err(e)) => {
                        let path = PathBuf::from(shard_name.clone());
                        return Some(Err(OdbError::Io { path, source: e }));
                    }
                    None => {
                        self.current = None;
                        continue;
                    }
                }
            }
            // Pull the next shard.
            let shards = self.shards.as_mut()?;
            match shards.next()? {
                Ok(entry) => {
                    let name = entry.file_name();
                    let name = name.to_string_lossy().to_string();
                    // Only shard dirs are 2 hex chars.
                    if name.len() == 2 && name.chars().all(|c| c.is_ascii_hexdigit()) {
                        if let Ok(rd) = fs::read_dir(entry.path()) {
                            self.current = Some((name, rd));
                        }
                    }
                }
                Err(e) => {
                    return Some(Err(OdbError::Io {
                        path: PathBuf::new(),
                        source: e,
                    }));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn write_then_read_round_trip() {
        let dir = tempdir().unwrap();
        let store = LooseStore::new(dir.path().to_path_buf(), HashKind::Sha1);
        let blob = RawObject::new(ObjectKind::Blob, b"hello world".to_vec());
        let oid = store.write(&blob).unwrap();
        assert_eq!(oid.to_string(), "95d09f2b10159347eece71399a7e2e907ea3df4f");
        let read_back = store.read(&oid).unwrap().unwrap();
        assert_eq!(read_back.kind, ObjectKind::Blob);
        assert_eq!(read_back.data, b"hello world");
    }

    #[test]
    fn read_missing_object_returns_none() {
        let dir = tempdir().unwrap();
        let store = LooseStore::new(dir.path().to_path_buf(), HashKind::Sha1);
        let bogus = ObjectId::parse_hex(HashKind::Sha1, &"a".repeat(40)).unwrap();
        assert!(store.read(&bogus).unwrap().is_none());
        assert!(!store.contains(&bogus).unwrap());
    }

    #[test]
    fn resolve_prefix_unique_match() {
        let dir = tempdir().unwrap();
        let store = LooseStore::new(dir.path().to_path_buf(), HashKind::Sha1);
        let oid = store
            .write(&RawObject::new(ObjectKind::Blob, b"x".to_vec()))
            .unwrap();
        let hex = oid.to_string();
        let m = store.resolve_prefix(&hex[..7]).unwrap();
        assert!(matches!(m, PrefixMatch::Found(o) if o == oid));
    }

    #[test]
    fn iter_yields_written_objects() {
        let dir = tempdir().unwrap();
        let store = LooseStore::new(dir.path().to_path_buf(), HashKind::Sha1);
        let a = store
            .write(&RawObject::new(ObjectKind::Blob, b"a".to_vec()))
            .unwrap();
        let b = store
            .write(&RawObject::new(ObjectKind::Blob, b"b".to_vec()))
            .unwrap();
        let mut got: Vec<_> = store.iter().filter_map(Result::ok).collect();
        got.sort_by(|x, y| x.as_bytes().cmp(y.as_bytes()));
        let mut want = vec![a, b];
        want.sort_by(|x, y| x.as_bytes().cmp(y.as_bytes()));
        assert_eq!(got, want);
    }
}
