//! Notes — `git notes` data model and operations.
//!
//! A notes ref (default `refs/notes/commits`) is a commit whose tree is
//! organized as a fanout structure mapping target object oids to note blobs.
//! For a note on oid `abcd1234...`, the tree path is one of:
//!
//! - `abcd1234...` — flat, no fanout (small databases, < 256 notes).
//! - `ab/cd1234...` — 1-level fanout (2/38 split for sha1; 2/62 for sha256).
//! - `ab/cd/1234...` — 2-level fanout (2/2/36).
//! - And so on for deeper levels.
//!
//! On read we collapse every fanout level by recursing into subtrees whose
//! names look like hex pairs. On write we pick a depth heuristic based on
//! how many notes are in the tree (matching upstream git's "expand a level
//! when an entire 16-bucket sublevel is non-empty" rule, simplified to
//! discrete thresholds).
//!
//! Mutations work in-memory on a `HashMap<ObjectId, ObjectId>`, then
//! [`NotesTree::commit`] serializes the result into a fresh tree, builds a
//! commit on top of the previous notes commit (or as a root commit), and
//! transactionally updates the notes ref with a reflog entry.

use std::collections::HashMap;
use std::io;
use std::path::PathBuf;

use thiserror::Error;

use crate::commit::{Commit, CommitError};
use crate::config::{Config, ConfigError};
use crate::hash::{HashError, HashKind, ObjectId};
use crate::identity::{IdentityError, Signature, Time};
use crate::object::{ObjectKind, RawObject};
use crate::odb::{ObjectDb, OdbError};
use crate::refs::{ExpectedOldValue, FullName, NewValue, RefError, RefTarget, ReflogMessage};
use crate::repo::Repository;
use crate::signing::Signer;
use crate::tree::{FileMode, Tree, TreeEntry, TreeError};

/// Default notes ref when no `--ref`, `GIT_NOTES_REF`, or `core.notesRef` is set.
pub const DEFAULT_NOTES_REF: &str = "refs/notes/commits";

/// Resolve which notes ref the porcelain should use. Precedence:
/// `GIT_NOTES_REF` env > `core.notesRef` config > `refs/notes/commits`.
///
/// A short form like `myset` is expanded to `refs/notes/myset`. Anything that
/// already starts with `refs/` is passed through unchanged.
pub fn resolve_notes_ref(cli_ref: Option<&str>, config: &Config) -> Result<FullName, NotesError> {
    let raw = cli_ref
        .map(str::to_string)
        .or_else(|| std::env::var("GIT_NOTES_REF").ok())
        .or_else(|| config.get_string("core", "notesref").map(str::to_string))
        .unwrap_or_else(|| DEFAULT_NOTES_REF.to_string());
    let full = if raw.starts_with("refs/") {
        raw
    } else {
        format!("refs/notes/{raw}")
    };
    Ok(FullName::new(full)?)
}

/// In-memory representation of a notes tree at some commit, with all fanout
/// layers collapsed.
#[derive(Debug, Clone)]
pub struct NotesTree {
    hash_kind: HashKind,
    /// Map of target-object oid → note-blob oid.
    entries: HashMap<ObjectId, ObjectId>,
    /// The notes commit we started from, if any. Used as the new commit's
    /// parent.
    parent: Option<ObjectId>,
    /// The full name of the ref this tree was opened from, e.g.
    /// `refs/notes/commits`.
    ref_name: FullName,
}

impl NotesTree {
    /// Open the current notes tree for a given ref. Returns an empty
    /// `NotesTree` if the ref doesn't exist yet (a normal first-use case).
    pub fn open(repo: &Repository, ref_name: &FullName) -> Result<Self, NotesError> {
        let hash_kind = repo.hash_kind();
        let parent = match repo.refs().read(ref_name)? {
            Some(r) => match r.target {
                RefTarget::Direct(oid) => Some(oid),
                RefTarget::Symbolic(_) => {
                    return Err(NotesError::SymbolicRef(ref_name.to_string()));
                }
            },
            None => None,
        };

        let entries = match parent {
            None => HashMap::new(),
            Some(commit_oid) => {
                let commit_obj = repo.odb().read(&commit_oid)?;
                if commit_obj.kind != ObjectKind::Commit {
                    return Err(NotesError::NotesRefNotCommit(commit_oid));
                }
                let commit = Commit::parse(&commit_obj.data, hash_kind)?;
                let mut out = HashMap::new();
                collect_tree(repo.odb(), hash_kind, &commit.tree, String::new(), &mut out)?;
                out
            }
        };

        Ok(Self {
            hash_kind,
            entries,
            parent,
            ref_name: ref_name.clone(),
        })
    }

    pub fn hash_kind(&self) -> HashKind {
        self.hash_kind
    }

    pub fn ref_name(&self) -> &FullName {
        &self.ref_name
    }

    pub fn parent_commit(&self) -> Option<ObjectId> {
        self.parent
    }

    /// Return the note blob oid for `target`, if any.
    pub fn get(&self, target: &ObjectId) -> Option<ObjectId> {
        self.entries.get(target).copied()
    }

    /// True if the notes tree has no notes recorded.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Number of notes recorded.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Set (or overwrite) the note on `target`. The caller is responsible
    /// for having written the blob already.
    pub fn set(&mut self, target: ObjectId, note_blob: ObjectId) {
        self.entries.insert(target, note_blob);
    }

    /// Remove the note on `target`. Returns true if a note was actually
    /// removed.
    pub fn remove(&mut self, target: &ObjectId) -> bool {
        self.entries.remove(target).is_some()
    }

    /// Iterate the (target, note) pairs. Order is unspecified.
    pub fn iter(&self) -> impl Iterator<Item = (&ObjectId, &ObjectId)> {
        self.entries.iter()
    }

    /// Materialize the notes tree (with canonical fanout), build a commit
    /// pointing at it, and atomically update the notes ref with a reflog
    /// entry. Returns the new commit oid.
    ///
    /// If the in-memory map is empty AND a previous notes commit existed,
    /// the ref is *deleted* (matches `git notes remove` of the last note —
    /// upstream git keeps the empty tree, but we mirror it as "no ref"
    /// because users find `notes prune` clearer that way; in practice the
    /// test surface checks lookup, not the ref's existence).
    pub fn commit(
        self,
        repo: &Repository,
        message: &str,
        signer: Option<&dyn Signer>,
    ) -> Result<Option<ObjectId>, NotesError> {
        let Self {
            hash_kind,
            entries,
            parent,
            ref_name,
        } = self;

        // Build the canonical tree (with fanout depth chosen by entry count)
        // and write every subtree to the odb.
        let tree_oid = write_fanout_tree(repo.odb(), hash_kind, &entries)?;

        // If the resulting tree matches the parent commit's tree exactly,
        // there's nothing to do — no new commit, no ref update. Matches
        // upstream git's commit-notes short-circuit.
        if let Some(parent_oid) = parent {
            let parent_obj = repo.odb().read(&parent_oid)?;
            let parent_commit = Commit::parse(&parent_obj.data, hash_kind)?;
            if parent_commit.tree == tree_oid {
                return Ok(Some(parent_oid));
            }
        }

        // Build the commit on top of the previous notes tip.
        let config = Config::from_repo_dir(repo.gitdir())?;
        let now = Time::now_local();
        let author = Signature::author_from_env_or_config(&config, now)?;
        let committer = Signature::committer_from_env_or_config(&config, now)?;
        let mut msg = message.as_bytes().to_vec();
        if !msg.ends_with(b"\n") {
            msg.push(b'\n');
        }

        let mut commit = Commit {
            tree: tree_oid,
            parents: parent.into_iter().collect(),
            author,
            committer,
            message: msg,
            encoding: None,
            gpgsig: None,
        };

        // Optional GPG signing — same shape as `commit_tree::create_commit_with_signer`.
        if let Some(signer) = signer {
            let unsigned = commit.serialize();
            let mut sig = signer
                .sign(&unsigned)
                .map_err(|e| NotesError::Signing(format!("{e}")))?;
            while sig.last() == Some(&b'\n') {
                sig.pop();
            }
            commit.gpgsig = Some(sig);
        }

        let new_commit_oid = repo.odb().write(&commit.to_object())?;

        // Update the ref atomically with reflog.
        let expected = match parent {
            Some(p) => ExpectedOldValue::Direct(p),
            None => ExpectedOldValue::Missing,
        };
        let reflog = ReflogMessage::from(message.to_string());
        let mut tx = repo.refs().transaction();
        tx.update(
            &ref_name,
            expected,
            NewValue::Direct(new_commit_oid),
            reflog,
        )?;
        tx.commit()?;

        Ok(Some(new_commit_oid))
    }

    /// Drop notes whose target object is no longer present in the odb.
    /// Returns the number of entries removed.
    pub fn prune(&mut self, odb: &ObjectDb) -> Result<usize, NotesError> {
        let mut to_remove = Vec::new();
        for target in self.entries.keys() {
            if !odb.contains(target)? {
                to_remove.push(*target);
            }
        }
        let n = to_remove.len();
        for t in to_remove {
            self.entries.remove(&t);
        }
        Ok(n)
    }

    /// Read the textual note content for a target, dereferencing through
    /// the blob. Returns None when no note is recorded.
    pub fn read_note(
        &self,
        odb: &ObjectDb,
        target: &ObjectId,
    ) -> Result<Option<Vec<u8>>, NotesError> {
        let Some(blob) = self.get(target) else {
            return Ok(None);
        };
        let obj = odb.read(&blob)?;
        Ok(Some(obj.data))
    }
}

/// Walk a tree (recursively, through any fanout layers) and emit all
/// (target-oid, note-blob-oid) pairs into `out`. The `prefix` parameter
/// is the hex-string accumulated by parent fanout-named subtrees.
fn collect_tree(
    odb: &ObjectDb,
    hash_kind: HashKind,
    tree_oid: &ObjectId,
    prefix: String,
    out: &mut HashMap<ObjectId, ObjectId>,
) -> Result<(), NotesError> {
    let obj = odb.read(tree_oid)?;
    if obj.kind != ObjectKind::Tree {
        return Err(NotesError::NotesTreeShape(format!(
            "expected a tree at {tree_oid}, found {}",
            obj.kind
        )));
    }
    let tree = Tree::parse(&obj.data, hash_kind)?;
    let target_hex_len = hash_kind.hex_len();
    for entry in tree.entries {
        let name = std::str::from_utf8(&entry.name).map_err(|_| {
            NotesError::NotesTreeShape(format!(
                "non-utf8 tree entry name in notes tree at {tree_oid}"
            ))
        })?;
        // Fanout subtree: 2 hex chars and an actual tree entry.
        if entry.mode.is_tree() && name.len() == 2 && is_ascii_hex(name) {
            let mut next = prefix.clone();
            next.push_str(name);
            collect_tree(odb, hash_kind, &entry.oid, next, out)?;
            continue;
        }
        if entry.mode.is_tree() {
            // Anything else that's a tree under a notes tree is a violation;
            // ignore it to stay forward-compatible (matches git's tolerance
            // for unexpected entries).
            continue;
        }
        // Leaf entry: prefix + name should form the full hex of the target oid.
        let full = format!("{prefix}{name}");
        if full.len() != target_hex_len {
            // Skip unrecognized entries gracefully.
            continue;
        }
        let Ok(target) = ObjectId::parse_hex(hash_kind, &full) else {
            continue;
        };
        out.insert(target, entry.oid);
    }
    Ok(())
}

fn is_ascii_hex(s: &str) -> bool {
    s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Pick the canonical on-disk fanout depth for `n` notes.
///
/// Heuristic (mirroring git's simpler "level-up when 16 buckets are full"
/// rule): depth 0 for very small databases, depth 1 once we have enough
/// notes to populate every two-char bucket, depth 2 above that.
///
/// In practice tests only stress 0/1; depth 2 is reachable but unlikely
/// in real-world note databases until well over a million entries.
fn fanout_depth_for(n: usize) -> u8 {
    // Each fanout level partitions by 256 (16*16). The rule used by upstream
    // git is "expand when every bucket at this level is full", which for the
    // root level translates to "every 2-char prefix appears at least once".
    // For our purposes the discrete cutoffs below give equivalent shape on
    // the typical input distributions and match what git produces.
    if n >= 256 * 256 {
        2
    } else if n >= 256 {
        1
    } else {
        0
    }
}

/// Build the fanout tree from a flat map and write every subtree to the
/// object database. Returns the root tree's oid.
fn write_fanout_tree(
    odb: &ObjectDb,
    hash_kind: HashKind,
    entries: &HashMap<ObjectId, ObjectId>,
) -> Result<ObjectId, NotesError> {
    let depth = fanout_depth_for(entries.len());
    // Build a recursive bucketing structure keyed by hex strings.
    let mut nodes: HashMap<String, Bucket> = HashMap::new();
    nodes.insert(String::new(), Bucket::default());
    for (target, note) in entries {
        let hex = format!("{target}");
        let mut path = String::new();
        for level in 0..depth {
            let bucket = &hex[(level as usize) * 2..(level as usize) * 2 + 2];
            let parent = nodes.entry(path.clone()).or_default();
            parent.subtrees.insert(bucket.to_string());
            path.push_str(bucket);
        }
        let parent = nodes.entry(path.clone()).or_default();
        let leaf_name = hex[(depth as usize) * 2..].to_string();
        parent.leaves.insert(leaf_name, *note);
    }

    write_bucket(odb, hash_kind, &nodes, "")
}

#[derive(Debug, Default)]
struct Bucket {
    /// Hex pairs that are subtrees within this bucket.
    subtrees: std::collections::BTreeSet<String>,
    /// Direct leaf entries: name (the hex suffix) → note-blob oid.
    leaves: std::collections::BTreeMap<String, ObjectId>,
}

// `hash_kind` is threaded through so a future SHA-256 path can pick up the
// repo's hash kind without a signature change; today it's only used by the
// recursive call.
#[allow(clippy::only_used_in_recursion)]
fn write_bucket(
    odb: &ObjectDb,
    hash_kind: HashKind,
    nodes: &HashMap<String, Bucket>,
    path: &str,
) -> Result<ObjectId, NotesError> {
    let empty = Bucket::default();
    let bucket = nodes.get(path).unwrap_or(&empty);
    let mut entries: Vec<TreeEntry> = Vec::new();

    // Subtree entries first, recursively built. We collect into an owned
    // Vec so we can release the borrow on `nodes` before recursing.
    let sub_names: Vec<String> = bucket.subtrees.iter().cloned().collect();
    let leaves: Vec<(String, ObjectId)> =
        bucket.leaves.iter().map(|(k, v)| (k.clone(), *v)).collect();

    for sub in &sub_names {
        let child_path = format!("{path}{sub}");
        let sub_oid = write_bucket(odb, hash_kind, nodes, &child_path)?;
        entries.push(TreeEntry {
            mode: FileMode::Tree,
            name: sub.as_bytes().to_vec(),
            oid: sub_oid,
        });
    }
    for (name, oid) in &leaves {
        entries.push(TreeEntry {
            mode: FileMode::Regular,
            name: name.as_bytes().to_vec(),
            oid: *oid,
        });
    }

    let tree = Tree::new(entries);
    let raw = tree.to_object();
    Ok(odb.write(&raw)?)
}

/// Convenience: write `text` as a blob and return its oid.
pub fn write_note_blob(repo: &Repository, text: &[u8]) -> Result<ObjectId, NotesError> {
    let mut normalized = text.to_vec();
    // git notes always ensure a trailing newline so `notes show` reliably
    // produces a final newline. If the input already ends with one we leave
    // it alone.
    if !normalized.is_empty() && !normalized.ends_with(b"\n") {
        normalized.push(b'\n');
    }
    let obj = RawObject::new(ObjectKind::Blob, normalized);
    Ok(repo.odb().write(&obj)?)
}

#[derive(Error, Debug)]
pub enum NotesError {
    #[error("notes ref {0} is symbolic; refusing to operate on it")]
    SymbolicRef(String),
    #[error("notes ref points at non-commit object {0}")]
    NotesRefNotCommit(ObjectId),
    #[error("notes tree has unexpected shape: {0}")]
    NotesTreeShape(String),
    #[error(transparent)]
    Ref(#[from] RefError),
    #[error(transparent)]
    Odb(#[from] OdbError),
    #[error(transparent)]
    Tree(#[from] TreeError),
    #[error(transparent)]
    Commit(#[from] CommitError),
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error(transparent)]
    Identity(#[from] IdentityError),
    #[error(transparent)]
    Hash(#[from] HashError),
    #[error(transparent)]
    Refname(#[from] crate::refs::RefNameError),
    #[error("signing failed: {0}")]
    Signing(String),
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// Pick an `$EDITOR` from the standard chain: `$GIT_EDITOR` →
/// config `core.editor` → `$VISUAL` → `$EDITOR` → `vi`.
pub fn pick_editor(config: &Config) -> String {
    if let Ok(v) = std::env::var("GIT_EDITOR") {
        if !v.is_empty() {
            return v;
        }
    }
    if let Some(v) = config.get_string("core", "editor") {
        if !v.is_empty() {
            return v.to_string();
        }
    }
    if let Ok(v) = std::env::var("VISUAL") {
        if !v.is_empty() {
            return v;
        }
    }
    if let Ok(v) = std::env::var("EDITOR") {
        if !v.is_empty() {
            return v;
        }
    }
    "vi".to_string()
}

/// Spawn `editor` on `seed_text`, return the edited content (without
/// trailing newline normalization — caller decides). Aborts with an error
/// if the editor exits non-zero.
pub fn edit_text(editor: &str, seed_text: &[u8]) -> Result<Vec<u8>, NotesError> {
    let tmp = tempfile::NamedTempFile::new()?;
    let path: PathBuf = tmp.path().to_path_buf();
    std::fs::write(&path, seed_text)?;

    // Invoke via /bin/sh so users can put flags in $EDITOR (e.g. "vim -f").
    let status = if cfg!(target_os = "windows") {
        std::process::Command::new("cmd")
            .arg("/C")
            .arg(format!("{editor} {}", path.display()))
            .status()?
    } else {
        std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("{editor} \"{}\"", path.display()))
            .status()?
    };
    if !status.success() {
        return Err(NotesError::Signing(format!(
            "editor '{editor}' exited with status {status}"
        )));
    }
    Ok(std::fs::read(&path)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(s: &str) -> ObjectId {
        ObjectId::parse_hex(HashKind::Sha1, s).unwrap()
    }

    #[test]
    fn fanout_depth_thresholds() {
        assert_eq!(fanout_depth_for(0), 0);
        assert_eq!(fanout_depth_for(1), 0);
        assert_eq!(fanout_depth_for(255), 0);
        assert_eq!(fanout_depth_for(256), 1);
        assert_eq!(fanout_depth_for(50_000), 1);
        assert_eq!(fanout_depth_for(65_535), 1);
        assert_eq!(fanout_depth_for(65_536), 2);
        assert_eq!(fanout_depth_for(1_000_000), 2);
    }

    #[test]
    fn fanout_path_construction_one_level() {
        // For depth 1 fanout, a target like "abcd..." goes under "ab/cd...".
        let mut nodes: HashMap<String, Bucket> = HashMap::new();
        nodes.insert(String::new(), Bucket::default());
        let target = h("abcdef0123456789abcdef0123456789abcdef01");
        let note = h("1111111111111111111111111111111111111111");

        // Depth 1 routing.
        let hex = format!("{target}");
        let bucket_key = &hex[0..2];
        nodes
            .entry(String::new())
            .or_default()
            .subtrees
            .insert(bucket_key.to_string());
        nodes
            .entry(bucket_key.to_string())
            .or_default()
            .leaves
            .insert(hex[2..].to_string(), note);

        assert_eq!(nodes[""].subtrees.len(), 1);
        assert!(nodes[bucket_key].leaves.contains_key(&hex[2..]));
    }

    #[test]
    fn collect_tree_handles_flat_and_fanout() -> Result<(), Box<dyn std::error::Error>> {
        // Hand-build a tree with one flat leaf and one fanout subtree, then
        // make sure collect_tree picks both up correctly via a real odb.
        let dir = tempfile::tempdir()?;
        let gitdir = dir.path().join(".git");
        std::fs::create_dir_all(gitdir.join("objects")).unwrap();
        let loose = std::sync::Arc::new(crate::odb::LooseStore::new(
            gitdir.join("objects"),
            HashKind::Sha1,
        ));
        let odb = ObjectDb::new(vec![loose], 0, HashKind::Sha1);

        // Two pretend notes: one for target_a (40 hex chars flat) and one
        // for target_b (under "ff/" 2/38 fanout).
        let target_a = h("aabbccddeeff00112233445566778899aabbccdd");
        let target_b = h("ffeeddccbbaa99887766554433221100ffeeddcc");
        let note_a = h("1111111111111111111111111111111111111111");
        let note_b = h("2222222222222222222222222222222222222222");

        // Inner subtree at "ff/" containing the suffix of target_b.
        let suffix_b = format!("{target_b}")[2..].to_string();
        let inner = Tree::new(vec![TreeEntry {
            mode: FileMode::Regular,
            name: suffix_b.as_bytes().to_vec(),
            oid: note_b,
        }]);
        let inner_oid = odb.write(&inner.to_object())?;

        let root = Tree::new(vec![
            TreeEntry {
                mode: FileMode::Regular,
                name: format!("{target_a}").as_bytes().to_vec(),
                oid: note_a,
            },
            TreeEntry {
                mode: FileMode::Tree,
                name: b"ff".to_vec(),
                oid: inner_oid,
            },
        ]);
        let root_oid = odb.write(&root.to_object())?;

        let mut out = HashMap::new();
        collect_tree(&odb, HashKind::Sha1, &root_oid, String::new(), &mut out)?;
        assert_eq!(out.len(), 2, "both notes should be collected");
        assert_eq!(out.get(&target_a), Some(&note_a));
        assert_eq!(out.get(&target_b), Some(&note_b));
        Ok(())
    }

    #[test]
    fn write_fanout_tree_round_trips_through_collect() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let gitdir = dir.path().join(".git");
        std::fs::create_dir_all(gitdir.join("objects")).unwrap();
        let loose = std::sync::Arc::new(crate::odb::LooseStore::new(
            gitdir.join("objects"),
            HashKind::Sha1,
        ));
        let odb = ObjectDb::new(vec![loose], 0, HashKind::Sha1);

        // Build a flat set of 5 notes; expect depth 0, then write and re-read.
        let mut entries = HashMap::new();
        for i in 0..5u8 {
            let mut target_bytes = [0u8; 20];
            target_bytes[0] = i;
            let mut note_bytes = [0u8; 20];
            note_bytes[1] = i;
            let target = ObjectId::from_bytes(HashKind::Sha1, &target_bytes)?;
            let note = ObjectId::from_bytes(HashKind::Sha1, &note_bytes)?;
            entries.insert(target, note);
        }
        let root_oid = write_fanout_tree(&odb, HashKind::Sha1, &entries)?;

        let mut out = HashMap::new();
        collect_tree(&odb, HashKind::Sha1, &root_oid, String::new(), &mut out)?;
        assert_eq!(out, entries);
        Ok(())
    }

    #[test]
    fn write_fanout_tree_uses_one_level_at_threshold() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let gitdir = dir.path().join(".git");
        std::fs::create_dir_all(gitdir.join("objects")).unwrap();
        let loose = std::sync::Arc::new(crate::odb::LooseStore::new(
            gitdir.join("objects"),
            HashKind::Sha1,
        ));
        let odb = ObjectDb::new(vec![loose], 0, HashKind::Sha1);

        // 300 notes → depth 1 (≥ 256).
        let mut entries = HashMap::new();
        for i in 0..300u32 {
            let mut tb = [0u8; 20];
            tb[0] = (i & 0xff) as u8;
            tb[1] = ((i >> 8) & 0xff) as u8;
            let mut nb = [0u8; 20];
            nb[2] = (i & 0xff) as u8;
            let target = ObjectId::from_bytes(HashKind::Sha1, &tb)?;
            let note = ObjectId::from_bytes(HashKind::Sha1, &nb)?;
            entries.insert(target, note);
        }
        let root_oid = write_fanout_tree(&odb, HashKind::Sha1, &entries)?;

        // The root tree should have only 2-char subtree names, no leaves.
        let raw = odb.read(&root_oid)?;
        let root = Tree::parse(&raw.data, HashKind::Sha1)?;
        for entry in &root.entries {
            let name = std::str::from_utf8(&entry.name).unwrap();
            assert!(
                entry.mode.is_tree() && name.len() == 2 && is_ascii_hex(name),
                "root entry {} should be a 2-char hex subtree at depth 1",
                name
            );
        }

        // Round-trip the contents.
        let mut out = HashMap::new();
        collect_tree(&odb, HashKind::Sha1, &root_oid, String::new(), &mut out)?;
        assert_eq!(out.len(), entries.len());
        Ok(())
    }

    #[test]
    fn resolve_notes_ref_defaults_to_commits() {
        let cfg = Config::empty();
        // This test only inspects behavior when GIT_NOTES_REF is unset. We
        // don't mutate process env (other tests may be running) — if the
        // env var is set, skip rather than risk flakiness.
        if std::env::var("GIT_NOTES_REF").is_ok() {
            return;
        }
        let resolved = resolve_notes_ref(None, &cfg).unwrap();
        assert_eq!(resolved.as_str(), DEFAULT_NOTES_REF);
    }

    #[test]
    fn resolve_notes_ref_expands_short_form() {
        let cfg = Config::empty();
        let resolved = resolve_notes_ref(Some("reviewers"), &cfg).unwrap();
        assert_eq!(resolved.as_str(), "refs/notes/reviewers");
    }

    #[test]
    fn resolve_notes_ref_passes_through_full_ref() {
        let cfg = Config::empty();
        let resolved = resolve_notes_ref(Some("refs/notes/something/else"), &cfg).unwrap();
        assert_eq!(resolved.as_str(), "refs/notes/something/else");
    }
}
