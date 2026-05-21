//! `rustygit write-tree` — write the current index to a tree object.
//!
//! Algorithm: group index entries by their path's directory components,
//! build a `Tree` for each subdirectory, recurse bottom-up. Each subtree's
//! oid is hashed and written to the loose store, then referenced from its
//! parent's `TreeEntry`. We do NOT yet honor the index's `CacheTree`
//! extension as a fast path — full rebuild every time. M3+ optimization.

use std::collections::BTreeMap;
use std::io;

use clap::Args;

use crate::hash::ObjectId;
use crate::index::{Index, IndexEntry};
use crate::repo::Repository;
use crate::tree::{FileMode, Tree, TreeEntry};

#[derive(Debug, Args)]
pub struct WriteTreeArgs {
    /// Optional reserved flag — git accepts `--missing-ok` and `--prefix=<path>`,
    /// neither of which we implement in M3.
    #[arg(long)]
    pub missing_ok: bool,
}

pub fn run(_args: WriteTreeArgs) -> io::Result<i32> {
    let repo = Repository::discover_from_cwd().map_err(io_err)?;
    let oid = build_tree_from_index(&repo).map_err(io_err)?;
    println!("{oid}");
    Ok(0)
}

/// Public entry point for porcelain use: builds a tree from the current
/// index and returns its oid (writing all needed tree objects).
pub fn build_tree_from_index(repo: &Repository) -> Result<ObjectId, WriteTreeError> {
    let index = Index::read(repo).map_err(WriteTreeError::Index)?;
    build_tree_from_index_ref(repo, &index)
}

/// Same as [`build_tree_from_index`], but builds from an in-memory
/// `&Index` (does not read or write the on-disk index file). Used by
/// `stash` to build a workdir-snapshot tree without touching the
/// caller's actual index.
pub fn build_tree_from_index_ref(
    repo: &Repository,
    index: &Index,
) -> Result<ObjectId, WriteTreeError> {
    if index.entries.is_empty() {
        return Err(WriteTreeError::EmptyIndex);
    }
    for e in &index.entries {
        if e.stage != 0 {
            return Err(WriteTreeError::UnmergedEntries);
        }
    }
    let root = build_node(&index.entries);
    write_node(repo, &root)
}

#[derive(Debug)]
struct Node {
    /// Direct file entries at this directory level.
    files: Vec<IndexEntry>,
    /// Subdirectories: name → child node.
    subdirs: BTreeMap<Vec<u8>, Node>,
}

impl Node {
    fn new() -> Self {
        Self {
            files: Vec::new(),
            subdirs: BTreeMap::new(),
        }
    }
}

fn build_node(entries: &[IndexEntry]) -> Node {
    let mut root = Node::new();
    for entry in entries {
        insert_entry(&mut root, &entry.path, entry);
    }
    root
}

fn insert_entry(node: &mut Node, path: &[u8], entry: &IndexEntry) {
    match path.iter().position(|&b| b == b'/') {
        None => {
            let mut e = entry.clone();
            e.path = path.to_vec();
            node.files.push(e);
        }
        Some(slash) => {
            let dir = path[..slash].to_vec();
            let rest = path[slash + 1..].to_vec();
            let child = node.subdirs.entry(dir).or_insert_with(Node::new);
            insert_entry(child, &rest, entry);
        }
    }
}

fn write_node(repo: &Repository, node: &Node) -> Result<ObjectId, WriteTreeError> {
    let mut entries: Vec<TreeEntry> = Vec::with_capacity(node.files.len() + node.subdirs.len());

    // Files at this level.
    for f in &node.files {
        let mode = FileMode::from_index_mode(f.mode).map_err(|e| {
            WriteTreeError::Other(format!(
                "bad mode {:o} for {}: {e}",
                f.mode,
                String::from_utf8_lossy(&f.path)
            ))
        })?;
        entries.push(TreeEntry {
            mode,
            name: f.path.clone(),
            oid: f.oid,
        });
    }

    // Subdirectories — recurse, then add as Tree entries.
    for (name, child) in &node.subdirs {
        let child_oid = write_node(repo, child)?;
        entries.push(TreeEntry {
            mode: FileMode::Tree,
            name: name.clone(),
            oid: child_oid,
        });
    }

    let tree = Tree::new(entries);
    let obj = tree.to_object();
    repo.odb()
        .write(&obj)
        .map_err(|e| WriteTreeError::Odb(format!("{e}")))
}

#[derive(thiserror::Error, Debug)]
pub enum WriteTreeError {
    #[error("empty index -- nothing to write")]
    EmptyIndex,
    #[error("unmerged entries in index")]
    UnmergedEntries,
    #[error("index error: {0}")]
    Index(#[source] crate::index::IndexError),
    #[error("object database error: {0}")]
    Odb(String),
    #[error("{0}")]
    Other(String),
}

fn io_err<E: std::fmt::Display>(e: E) -> io::Error {
    io::Error::other(format!("{e}"))
}
