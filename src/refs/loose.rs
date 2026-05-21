//! Loose ref backend. Each ref is a single file:
//!
//! - Pseudo-refs (`HEAD`, `FETCH_HEAD`, ...) live directly under `.git/`.
//! - Normal refs (e.g. `refs/heads/main`) live at the same path under `.git/`.
//!
//! File contents are either `"<hex-oid>\n"` for direct refs or
//! `"ref: <full-name>\n"` for symbolic refs. Trailing whitespace is tolerated
//! on read but never produced on write.

use std::fs;
use std::path::{Path, PathBuf};

use crate::hash::{HashKind, ObjectId};

use super::{FullName, RefError, RefStore, RefTarget, RefTransactionTrait, Reference};

pub struct LooseRefStore {
    gitdir: PathBuf,
    hash_kind: HashKind,
}

impl LooseRefStore {
    pub fn new(gitdir: PathBuf, hash_kind: HashKind) -> Self {
        Self { gitdir, hash_kind }
    }

    pub fn gitdir(&self) -> &Path {
        &self.gitdir
    }

    pub fn hash_kind(&self) -> HashKind {
        self.hash_kind
    }

    fn path_for(&self, name: &FullName) -> PathBuf {
        self.gitdir.join(name.loose_path_relative())
    }

    /// Parse the contents of a loose ref file.
    pub(crate) fn parse_content(
        name: &FullName,
        content: &[u8],
        hash_kind: HashKind,
    ) -> Result<RefTarget, RefError> {
        let trimmed = trim_trailing_ws(content);
        if let Some(rest) = trimmed.strip_prefix(b"ref: ") {
            let s = std::str::from_utf8(rest).map_err(|_| RefError::Malformed {
                name: name.to_string(),
                reason: "non-utf8 symbolic ref target".into(),
            })?;
            let target = FullName::new(s.trim())?;
            return Ok(RefTarget::Symbolic(target));
        }
        let hex = std::str::from_utf8(trimmed).map_err(|_| RefError::Malformed {
            name: name.to_string(),
            reason: "non-utf8 ref content".into(),
        })?;
        let oid = ObjectId::parse_hex(hash_kind, hex.trim())?;
        Ok(RefTarget::Direct(oid))
    }
}

fn trim_trailing_ws(b: &[u8]) -> &[u8] {
    let mut end = b.len();
    while end > 0 && matches!(b[end - 1], b'\n' | b'\r' | b' ' | b'\t') {
        end -= 1;
    }
    &b[..end]
}

impl RefStore for LooseRefStore {
    fn read(&self, name: &FullName) -> Result<Option<Reference>, RefError> {
        let path = self.path_for(name);
        let bytes = match fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(RefError::Io { path, source: e }),
        };
        let target = Self::parse_content(name, &bytes, self.hash_kind)?;
        Ok(Some(Reference {
            name: name.clone(),
            target,
        }))
    }

    fn iter<'a>(
        &'a self,
        prefix: Option<&str>,
    ) -> Box<dyn Iterator<Item = Result<Reference, RefError>> + 'a> {
        // Walk .git/refs/** plus the well-known pseudo-refs at the top level.
        let pseudo: Vec<FullName> = ["HEAD", "FETCH_HEAD", "ORIG_HEAD", "MERGE_HEAD"]
            .into_iter()
            .filter_map(|n| FullName::new(n).ok())
            .collect();

        let prefix_owned = prefix.map(|s| s.to_string());

        let pseudo_iter = pseudo.into_iter().filter_map(move |n| {
            if let Some(p) = &prefix_owned {
                if !n.as_str().starts_with(p) {
                    return None;
                }
            }
            match self.read(&n) {
                Ok(Some(r)) => Some(Ok(r)),
                Ok(None) => None,
                Err(e) => Some(Err(e)),
            }
        });

        let prefix_owned2 = prefix.map(|s| s.to_string());
        let refs_root = self.gitdir.join("refs");
        let mut entries = Vec::new();
        walk_refs(&refs_root, &refs_root, &mut entries);
        let walked = entries.into_iter().filter_map(move |path| {
            let rel = path.strip_prefix(&self.gitdir).ok()?;
            let rel_str = rel
                .to_string_lossy()
                .replace(std::path::MAIN_SEPARATOR, "/");
            if let Some(p) = &prefix_owned2 {
                if !rel_str.starts_with(p) {
                    return None;
                }
            }
            let name = match FullName::new(rel_str) {
                Ok(n) => n,
                Err(_) => return None,
            };
            match self.read(&name) {
                Ok(Some(r)) => Some(Ok(r)),
                Ok(None) => None,
                Err(e) => Some(Err(e)),
            }
        });

        Box::new(pseudo_iter.chain(walked))
    }

    fn transaction(&self) -> Box<dyn RefTransactionTrait + '_> {
        Box::new(super::transaction::LooseTransaction::new(self))
    }
}

// `root` is threaded through for symmetry with callers that compute a
// relative-to-root path; today only the recursive call uses it.
#[allow(clippy::only_used_in_recursion)]
fn walk_refs(root: &Path, cur: &Path, out: &mut Vec<PathBuf>) {
    let entries = match fs::read_dir(cur) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let ft = match entry.file_type() {
            Ok(t) => t,
            Err(_) => continue,
        };
        if ft.is_dir() {
            walk_refs(root, &path, out);
        } else if ft.is_file() {
            out.push(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn fake_oid_sha1() -> ObjectId {
        ObjectId::parse_hex(HashKind::Sha1, "abcdef0123456789abcdef0123456789abcdef01").unwrap()
    }

    #[test]
    fn parse_direct_ref_content() {
        let n = FullName::new("refs/heads/main").unwrap();
        let target = LooseRefStore::parse_content(
            &n,
            b"abcdef0123456789abcdef0123456789abcdef01\n",
            HashKind::Sha1,
        )
        .unwrap();
        assert_eq!(target, RefTarget::Direct(fake_oid_sha1()));
    }

    #[test]
    fn parse_symbolic_ref_content() {
        let n = FullName::new("HEAD").unwrap();
        let target =
            LooseRefStore::parse_content(&n, b"ref: refs/heads/main\n", HashKind::Sha1).unwrap();
        assert_eq!(
            target,
            RefTarget::Symbolic(FullName::new("refs/heads/main").unwrap())
        );
    }

    #[test]
    fn read_missing_returns_none() {
        let dir = tempdir().unwrap();
        let store = LooseRefStore::new(dir.path().to_path_buf(), HashKind::Sha1);
        let name = FullName::new("refs/heads/missing").unwrap();
        assert!(store.read(&name).unwrap().is_none());
    }
}
