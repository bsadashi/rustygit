//! TREE (cache-tree) extension parse/write.
//!
//! The cache tree is a recursive structure that mirrors the directory tree
//! covered by the index. Each node records:
//!   - the path component name (empty at the root),
//!   - the number of index entries this subtree spans (or `-1` to mean
//!     "invalidated; recompute before use"),
//!   - the OID of the tree object that would result from writing that span
//!     (only present when the entry count is non-negative),
//!   - and the count of immediate child subtrees, listed in order.
//!
//! Wire format (from `gitformat-index(5)`, `=== Cache tree`):
//!   path-component '\0' ASCII-decimal-entry-count ' ' ASCII-decimal-subtree-count '\n'
//!   [raw-OID iff entry-count >= 0]
//!   <child-subtree-count children, recursively in this same format>
//!
//! Entries are written depth-first, top-down.

use thiserror::Error;

use crate::hash::{HashError, HashKind, ObjectId};

/// One node in the cache tree.
#[derive(Debug, Clone)]
pub struct CacheTree {
    /// Path component relative to parent. Empty at the root.
    pub name: Vec<u8>,
    /// Number of index entries this subtree covers. `None` means invalidated
    /// (encoded as `-1` on disk; the tree must be recomputed before use).
    pub entry_count: Option<u32>,
    /// OID of the tree object that materializes this subtree. Present iff
    /// `entry_count.is_some()`.
    pub oid: Option<ObjectId>,
    /// Children, in the order they appear in the on-disk extension.
    pub children: Vec<CacheTree>,
}

impl CacheTree {
    /// An invalid (uncomputed) root with no children.
    pub fn invalid_root() -> Self {
        Self {
            name: Vec::new(),
            entry_count: None,
            oid: None,
            children: Vec::new(),
        }
    }

    /// Parse a TREE extension body into a single root node. The body is
    /// expected to contain exactly one top-level entry (the root) followed
    /// by all of its descendants depth-first.
    pub fn parse(body: &[u8], hash_kind: HashKind) -> Result<Self, CacheTreeError> {
        let mut cur = 0usize;
        let root = parse_one(body, &mut cur, hash_kind)?;
        if cur != body.len() {
            return Err(CacheTreeError::TrailingBytes {
                consumed: cur,
                total: body.len(),
            });
        }
        Ok(root)
    }

    /// Serialize the tree in the on-disk depth-first order.
    pub fn serialize(&self) -> Vec<u8> {
        let mut out = Vec::new();
        write_one(self, &mut out);
        out
    }
}

fn parse_one(
    body: &[u8],
    cur: &mut usize,
    hash_kind: HashKind,
) -> Result<CacheTree, CacheTreeError> {
    // path component, NUL-terminated.
    let nul = body[*cur..]
        .iter()
        .position(|&b| b == 0)
        .ok_or(CacheTreeError::Truncated(
            "missing NUL after path component",
        ))?;
    let name = body[*cur..*cur + nul].to_vec();
    *cur += nul + 1;

    // entry-count (ASCII decimal, possibly negative)
    let space = body[*cur..]
        .iter()
        .position(|&b| b == b' ')
        .ok_or(CacheTreeError::Truncated("missing space after entry-count"))?;
    let ec_str = std::str::from_utf8(&body[*cur..*cur + space])
        .map_err(|_| CacheTreeError::Malformed("non-utf8 entry count"))?;
    let entry_count_signed: i64 = ec_str
        .parse()
        .map_err(|_| CacheTreeError::Malformed("non-numeric entry count"))?;
    *cur += space + 1;

    // subtree-count (ASCII decimal, non-negative), terminated by '\n'.
    let lf = body[*cur..]
        .iter()
        .position(|&b| b == b'\n')
        .ok_or(CacheTreeError::Truncated("missing LF after subtree-count"))?;
    let st_str = std::str::from_utf8(&body[*cur..*cur + lf])
        .map_err(|_| CacheTreeError::Malformed("non-utf8 subtree count"))?;
    let subtree_count: u32 = st_str
        .parse()
        .map_err(|_| CacheTreeError::Malformed("non-numeric subtree count"))?;
    *cur += lf + 1;

    let (entry_count, oid) = if entry_count_signed < 0 {
        (None, None)
    } else {
        let raw_len = hash_kind.raw_len();
        if *cur + raw_len > body.len() {
            return Err(CacheTreeError::Truncated("missing OID after subtree-count"));
        }
        let id = ObjectId::from_bytes(hash_kind, &body[*cur..*cur + raw_len])?;
        *cur += raw_len;
        (Some(entry_count_signed as u32), Some(id))
    };

    let mut children = Vec::with_capacity(subtree_count as usize);
    for _ in 0..subtree_count {
        children.push(parse_one(body, cur, hash_kind)?);
    }

    Ok(CacheTree {
        name,
        entry_count,
        oid,
        children,
    })
}

fn write_one(node: &CacheTree, out: &mut Vec<u8>) {
    out.extend_from_slice(&node.name);
    out.push(0);
    match node.entry_count {
        Some(n) => {
            // ASCII decimal, no sign.
            out.extend_from_slice(n.to_string().as_bytes());
        }
        None => {
            out.extend_from_slice(b"-1");
        }
    }
    out.push(b' ');
    out.extend_from_slice(node.children.len().to_string().as_bytes());
    out.push(b'\n');
    if let Some(oid) = node.oid {
        out.extend_from_slice(oid.as_bytes());
    }
    for child in &node.children {
        write_one(child, out);
    }
}

#[derive(Error, Debug)]
pub enum CacheTreeError {
    #[error("truncated cache tree: {0}")]
    Truncated(&'static str),
    #[error("malformed cache tree: {0}")]
    Malformed(&'static str),
    #[error("trailing bytes in cache tree extension: consumed {consumed} of {total}")]
    TrailingBytes { consumed: usize, total: usize },
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
    fn round_trip_single_root_no_children() {
        let tree = CacheTree {
            name: Vec::new(),
            entry_count: Some(0),
            oid: Some(fake_oid(0xab)),
            children: Vec::new(),
        };
        let bytes = tree.serialize();
        let parsed = CacheTree::parse(&bytes, HashKind::Sha1).unwrap();
        assert_eq!(parsed.name, b"");
        assert_eq!(parsed.entry_count, Some(0));
        assert_eq!(parsed.oid, Some(fake_oid(0xab)));
        assert!(parsed.children.is_empty());
    }

    #[test]
    fn round_trip_with_children() {
        let tree = CacheTree {
            name: Vec::new(),
            entry_count: Some(3),
            oid: Some(fake_oid(0x11)),
            children: vec![
                CacheTree {
                    name: b"src".to_vec(),
                    entry_count: Some(2),
                    oid: Some(fake_oid(0x22)),
                    children: vec![CacheTree {
                        name: b"sub".to_vec(),
                        entry_count: Some(1),
                        oid: Some(fake_oid(0x33)),
                        children: Vec::new(),
                    }],
                },
                CacheTree {
                    name: b"docs".to_vec(),
                    entry_count: Some(1),
                    oid: Some(fake_oid(0x44)),
                    children: Vec::new(),
                },
            ],
        };
        let bytes = tree.serialize();
        let parsed = CacheTree::parse(&bytes, HashKind::Sha1).unwrap();
        assert_eq!(parsed.entry_count, Some(3));
        assert_eq!(parsed.children.len(), 2);
        assert_eq!(parsed.children[0].name, b"src");
        assert_eq!(parsed.children[0].children.len(), 1);
        assert_eq!(parsed.children[0].children[0].name, b"sub");
        assert_eq!(parsed.children[1].name, b"docs");
    }

    #[test]
    fn invalidated_node_has_no_oid() {
        let tree = CacheTree {
            name: Vec::new(),
            entry_count: None,
            oid: None,
            children: Vec::new(),
        };
        let bytes = tree.serialize();
        // expected wire bytes: "" '\0' "-1" ' ' "0" '\n'
        assert_eq!(bytes, b"\0-1 0\n");
        let parsed = CacheTree::parse(&bytes, HashKind::Sha1).unwrap();
        assert_eq!(parsed.entry_count, None);
        assert!(parsed.oid.is_none());
    }

    #[test]
    fn rejects_trailing_garbage() {
        let mut bytes = CacheTree {
            name: Vec::new(),
            entry_count: Some(0),
            oid: Some(fake_oid(0xff)),
            children: Vec::new(),
        }
        .serialize();
        bytes.push(0xaa);
        let err = CacheTree::parse(&bytes, HashKind::Sha1).unwrap_err();
        assert!(matches!(err, CacheTreeError::TrailingBytes { .. }));
    }

    #[test]
    fn rejects_truncated_oid() {
        // Header: empty path (NUL), entry_count=5, subtree_count=0 — claims 5
        // entries which would need a 20-byte OID, but the input ends here.
        // (The `\x00` is the empty-name terminator, then ASCII "5 0\n" forms
        // the rest of the header.)
        let bytes = b"\x005 0\n".to_vec();
        let err = CacheTree::parse(&bytes, HashKind::Sha1).unwrap_err();
        assert!(matches!(err, CacheTreeError::Truncated(_)));
    }
}
