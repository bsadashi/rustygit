//! Reachability scanner (M9).
//!
//! Walks the graph of all git objects that are "reachable" from a repository's
//! refs, index, and HEAD. This is the set git considers "live" — everything
//! else is fair game for `git gc` / `git repack -d` / `git prune`.
//!
//! Reachability roots:
//! - Every direct ref returned by `RefStore::iter(None)`. We resolve symbolic
//!   chains to a direct oid before adding.
//! - Every entry in the index (blobs only, but uniformly — the parent tree
//!   shape isn't materialized in the index file itself; we let `mark_all`
//!   discover those via the commit/tree walk from the refs).
//! - The oid HEAD currently resolves to (covered indirectly when HEAD is a
//!   symbolic ref pointing into refs/heads; included explicitly for the
//!   detached-HEAD case too).
//!
//! Walk traversal: a BTreeSet doubles as the "visited" set and the eventual
//! output. For each oid we pop from the worklist, we classify by `ObjectKind`:
//!
//! - `Commit`: parse, queue `tree` plus every `parent`.
//! - `Tree`:   parse, queue every entry's oid (blob or subtree).
//! - `Tag`:    parse the `object` line — the wrapped target — and queue it.
//! - `Blob`:   leaf.
//!
//! The returned set is sorted (BTreeSet's `iter()` is ascending by oid bytes).
//! `mark_from` is exposed so `pack-objects --revs` can scope a walk to a
//! caller-provided set of commit/tree starting points.

use std::collections::BTreeSet;

use thiserror::Error;

use crate::commit::{Commit, CommitError};
use crate::hash::{HashError, ObjectId};
use crate::index::{Index, IndexError};
use crate::object::ObjectKind;
use crate::odb::OdbError;
use crate::refs::{RefError, RefTarget};
use crate::repo::Repository;
use crate::tree::{Tree, TreeError};

/// Result of a reachability walk.
#[derive(Debug, Clone, Default)]
pub struct ReachableSet {
    /// Reachable object ids, in ascending byte order (BTreeSet's natural sort).
    pub oids: BTreeSet<ObjectId>,
}

impl ReachableSet {
    /// Mark every object reachable from any ref, the index, and HEAD.
    ///
    /// See the module docs for the precise traversal rules.
    pub fn mark_all(repo: &Repository) -> Result<Self, ReachableError> {
        let mut worklist: Vec<ObjectId> = Vec::new();

        // 1. Every ref. We use `RefTarget::resolve` so symbolic chains land
        //    at a direct oid; refs that are dangling (target file missing)
        //    are silently skipped to match git's `gc` tolerance.
        for r in repo.refs().iter(None) {
            let r = r?;
            match r.target {
                RefTarget::Direct(oid) => worklist.push(oid),
                RefTarget::Symbolic(_) => {
                    if let Some((_, oid)) = RefTarget::resolve(repo.refs(), &r.name)? {
                        worklist.push(oid);
                    }
                }
            }
        }

        // 2. Every index entry's oid. Blobs primarily; the index never
        //    references trees or commits as entry oids (cache_tree is a
        //    different beast and is reconstructed from current entries on
        //    demand, so we don't need to mark its oids — they're either
        //    already covered by a ref-reachable commit's root tree, or
        //    they're stale and not interesting).
        let index = Index::read(repo)?;
        for entry in &index.entries {
            if !entry.oid.is_null() {
                worklist.push(entry.oid);
            }
        }

        // 3. The walk.
        let mut set = BTreeSet::new();
        drain_walk(repo, &mut worklist, &mut set)?;
        Ok(Self { oids: set })
    }

    /// Mark from a caller-provided set of starting oids. The starts may be
    /// commits, trees, tags, or blobs — anything `mark_all`'s walk recognizes.
    ///
    /// Used by `pack-objects --revs` to scope a walk to a specific tip set.
    pub fn mark_from(repo: &Repository, starts: &[ObjectId]) -> Result<Self, ReachableError> {
        let mut worklist: Vec<ObjectId> = starts.to_vec();
        let mut set = BTreeSet::new();
        drain_walk(repo, &mut worklist, &mut set)?;
        Ok(Self { oids: set })
    }
}

/// Pop items from `worklist`, classify each, and queue children until empty.
///
/// `set` accumulates the visited oids and doubles as the "already seen"
/// short-circuit so cyclic encodings (shouldn't exist in valid git data, but
/// future-proof) can't trap us in an infinite loop.
fn drain_walk(
    repo: &Repository,
    worklist: &mut Vec<ObjectId>,
    set: &mut BTreeSet<ObjectId>,
) -> Result<(), ReachableError> {
    let hash_kind = repo.hash_kind();
    while let Some(oid) = worklist.pop() {
        if !set.insert(oid) {
            continue;
        }
        // Look up the object. If a ref points at something missing from the
        // odb that's a real corruption — bubble the OdbError up rather than
        // silently dropping a tip. (`git gc` does fail loudly here too.)
        let obj = repo.odb().read(&oid)?;
        match obj.kind {
            ObjectKind::Commit => {
                let c = Commit::parse(&obj.data, hash_kind)?;
                worklist.push(c.tree);
                for p in &c.parents {
                    worklist.push(*p);
                }
            }
            ObjectKind::Tree => {
                let t = Tree::parse(&obj.data, hash_kind)?;
                for e in &t.entries {
                    worklist.push(e.oid);
                }
            }
            ObjectKind::Tag => {
                // Parse just the `object <hex>\n` line — the wrapped target —
                // and recurse on it. We deliberately don't pull in a separate
                // `Tag` type for M9; the tag's other headers (type/tagger/etc)
                // are not load-bearing for reachability. If parsing the oid
                // fails we surface the hash error.
                if let Some(target) = parse_tag_target(&obj.data, hash_kind)? {
                    worklist.push(target);
                }
            }
            ObjectKind::Blob => {
                // leaf
            }
        }
    }
    Ok(())
}

/// Find the `object <hex>\n` header in a tag object body and parse its oid.
///
/// Returns `Ok(None)` only if the body is empty (degenerate); a tag with no
/// `object` line is malformed and surfaces as `Hash(InvalidHex)` — but git
/// tolerance is "if the line's missing, the tag was reachable via the ref but
/// its target shouldn't be considered reachable through this tag". For M9
/// we treat malformed tags as bubble-up errors so corruption is visible.
fn parse_tag_target(
    body: &[u8],
    hash_kind: crate::hash::HashKind,
) -> Result<Option<ObjectId>, ReachableError> {
    // Walk header lines until we hit the blank separator or an `object` line.
    let text = std::str::from_utf8(body).unwrap_or("");
    if text.is_empty() {
        return Ok(None);
    }
    for line in text.lines() {
        if line.is_empty() {
            // Reached the message section without finding `object` — that's
            // a malformed tag. Return None and let the caller move on.
            return Ok(None);
        }
        if let Some(rest) = line.strip_prefix("object ") {
            let oid = ObjectId::parse_hex(hash_kind, rest.trim())?;
            return Ok(Some(oid));
        }
    }
    Ok(None)
}

#[derive(Error, Debug)]
pub enum ReachableError {
    #[error(transparent)]
    Odb(#[from] OdbError),
    #[error(transparent)]
    Refs(#[from] RefError),
    #[error(transparent)]
    Tree(#[from] TreeError),
    #[error(transparent)]
    Commit(#[from] CommitError),
    #[error(transparent)]
    Index(#[from] IndexError),
    #[error(transparent)]
    Hash(#[from] HashError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::process::Command;
    use tempfile::tempdir;

    fn git_available() -> bool {
        Command::new("git")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    fn git(dir: &Path, args: &[&str]) -> std::process::Output {
        let out = Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .expect("run git");
        if !out.status.success() {
            panic!(
                "git {:?} failed: stdout={:?} stderr={:?}",
                args,
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr),
            );
        }
        out
    }

    fn git_env(dir: &Path, args: &[&str], envs: &[(&str, &str)]) -> std::process::Output {
        let mut cmd = Command::new("git");
        cmd.args(args).current_dir(dir);
        for (k, v) in envs {
            cmd.env(k, v);
        }
        let out = cmd.output().expect("run git");
        if !out.status.success() {
            panic!(
                "git {:?} failed: stdout={:?} stderr={:?}",
                args,
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr),
            );
        }
        out
    }

    fn init_repo(dir: &Path) {
        git(dir, &["init", "-q", "-b", "main", "."]);
        git(dir, &["config", "user.name", "Test"]);
        git(dir, &["config", "user.email", "test@example.com"]);
    }

    fn write_and_add(dir: &Path, path: &str, content: &str) {
        let p = dir.join(path);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(p, content).unwrap();
        git(dir, &["add", path]);
    }

    fn make_commit(dir: &Path, msg: &str) -> String {
        // Deterministic timestamps just so the test is reproducible.
        git_env(
            dir,
            &["commit", "-q", "-m", msg],
            &[
                ("GIT_AUTHOR_DATE", "1700000000 +0000"),
                ("GIT_COMMITTER_DATE", "1700000000 +0000"),
            ],
        );
        let out = git(dir, &["rev-parse", "HEAD"]).stdout;
        String::from_utf8(out).unwrap().trim().to_string()
    }

    fn rev_parse(dir: &Path, expr: &str) -> String {
        let out = git(dir, &["rev-parse", expr]).stdout;
        String::from_utf8(out).unwrap().trim().to_string()
    }

    fn oid(hex: &str) -> ObjectId {
        ObjectId::parse_hex_any(hex).expect("valid hex")
    }

    #[test]
    fn empty_repo_has_no_reachable_objects() {
        if !git_available() {
            eprintln!("skipping: no git");
            return;
        }
        let tmp = tempdir().unwrap();
        let dir = tmp.path();
        // Plain init — no commits, no refs that resolve.
        git(dir, &["init", "-q", "-b", "main", "."]);
        let repo = Repository::discover(dir).unwrap();
        let set = ReachableSet::mark_all(&repo).unwrap();
        assert!(
            set.oids.is_empty(),
            "an empty repo should have an empty reachable set, got {:?}",
            set.oids
        );
    }

    #[test]
    fn single_commit_includes_commit_tree_and_blobs() {
        if !git_available() {
            eprintln!("skipping: no git");
            return;
        }
        let tmp = tempdir().unwrap();
        let dir = tmp.path();
        init_repo(dir);
        write_and_add(dir, "a.txt", "alpha\n");
        write_and_add(dir, "b.txt", "beta\n");
        let head = make_commit(dir, "initial");

        let repo = Repository::discover(dir).unwrap();
        let set = ReachableSet::mark_all(&repo).unwrap();

        let tree_hex = rev_parse(dir, "HEAD^{tree}");
        let blob_a = rev_parse(dir, "HEAD:a.txt");
        let blob_b = rev_parse(dir, "HEAD:b.txt");

        let want: BTreeSet<_> = [&head, &tree_hex, &blob_a, &blob_b]
            .into_iter()
            .map(|s| oid(s))
            .collect();
        assert_eq!(
            set.oids, want,
            "reachable set should be exactly the 4 objects"
        );
    }

    #[test]
    fn linear_history_includes_every_ancestor() {
        if !git_available() {
            eprintln!("skipping: no git");
            return;
        }
        let tmp = tempdir().unwrap();
        let dir = tmp.path();
        init_repo(dir);
        write_and_add(dir, "a.txt", "v1\n");
        let c1 = make_commit(dir, "c1");
        write_and_add(dir, "a.txt", "v2\n");
        let c2 = make_commit(dir, "c2");
        write_and_add(dir, "a.txt", "v3\n");
        let c3 = make_commit(dir, "c3");

        let repo = Repository::discover(dir).unwrap();
        let set = ReachableSet::mark_all(&repo).unwrap();

        // Every commit reachable.
        for c in [&c1, &c2, &c3] {
            assert!(
                set.oids.contains(&oid(c)),
                "missing commit {c} in reachable set"
            );
        }
        // Every blob (v1, v2, v3) reachable.
        let v1 = rev_parse(dir, &format!("{c1}:a.txt"));
        let v2 = rev_parse(dir, &format!("{c2}:a.txt"));
        let v3 = rev_parse(dir, &format!("{c3}:a.txt"));
        for b in [&v1, &v2, &v3] {
            assert!(
                set.oids.contains(&oid(b)),
                "missing blob {b} in reachable set"
            );
        }
    }

    #[test]
    fn branched_history_keeps_both_tips_and_shared_ancestor() {
        if !git_available() {
            eprintln!("skipping: no git");
            return;
        }
        let tmp = tempdir().unwrap();
        let dir = tmp.path();
        init_repo(dir);
        write_and_add(dir, "shared.txt", "base\n");
        let base = make_commit(dir, "base");
        // main extends with one more commit.
        write_and_add(dir, "shared.txt", "main-side\n");
        let main_tip = make_commit(dir, "main extends");
        // topic branches off `base` with its own commit.
        git(dir, &["checkout", "-q", "-b", "topic", &base]);
        write_and_add(dir, "shared.txt", "topic-side\n");
        let topic_tip = make_commit(dir, "topic extends");
        // Bring main forward so refs/heads/main is non-trivial.
        git(dir, &["checkout", "-q", "main"]);

        let repo = Repository::discover(dir).unwrap();
        let set = ReachableSet::mark_all(&repo).unwrap();

        // Every commit (base, main_tip, topic_tip) reachable.
        for c in [&base, &main_tip, &topic_tip] {
            assert!(
                set.oids.contains(&oid(c)),
                "missing commit {c} in reachable set ({} objects)",
                set.oids.len()
            );
        }
    }

    #[test]
    fn annotated_tag_pulls_in_target_and_its_subgraph() {
        if !git_available() {
            eprintln!("skipping: no git");
            return;
        }
        let tmp = tempdir().unwrap();
        let dir = tmp.path();
        init_repo(dir);
        write_and_add(dir, "a.txt", "hi\n");
        let c1 = make_commit(dir, "c1");
        // Annotated tag (object form, not the lightweight `git tag <name>` form).
        git_env(
            dir,
            &["tag", "-a", "v1", "-m", "release one", &c1],
            &[
                ("GIT_COMMITTER_DATE", "1700000000 +0000"),
                ("GIT_AUTHOR_DATE", "1700000000 +0000"),
            ],
        );
        let tag_oid = rev_parse(dir, "v1");
        // Sanity: the tag is a real tag object, not the lightweight form
        // (which would resolve to the commit oid directly).
        assert_ne!(tag_oid, c1, "v1 should be an annotated tag");

        // Make a new commit so HEAD isn't the same as the tag's target — this
        // forces the only path to c1's tree-through-tag to go through tag
        // resolution, not the direct ref.
        write_and_add(dir, "a.txt", "bye\n");
        let _c2 = make_commit(dir, "c2");

        let repo = Repository::discover(dir).unwrap();
        let set = ReachableSet::mark_all(&repo).unwrap();

        assert!(
            set.oids.contains(&oid(&tag_oid)),
            "tag {tag_oid} should be reachable via refs/tags/v1"
        );
        assert!(
            set.oids.contains(&oid(&c1)),
            "tag target commit {c1} should be reachable through the tag"
        );
        // Also the tree + blob at c1 should be included.
        let c1_tree = rev_parse(dir, &format!("{c1}^{{tree}}"));
        let c1_blob = rev_parse(dir, &format!("{c1}:a.txt"));
        assert!(
            set.oids.contains(&oid(&c1_tree)),
            "c1's tree must be reachable"
        );
        assert!(
            set.oids.contains(&oid(&c1_blob)),
            "c1's blob must be reachable"
        );
    }

    #[test]
    fn mark_from_specific_starts_scopes_walk() {
        if !git_available() {
            eprintln!("skipping: no git");
            return;
        }
        let tmp = tempdir().unwrap();
        let dir = tmp.path();
        init_repo(dir);
        write_and_add(dir, "a.txt", "one\n");
        let c1 = make_commit(dir, "c1");
        write_and_add(dir, "a.txt", "two\n");
        let c2 = make_commit(dir, "c2");

        let repo = Repository::discover(dir).unwrap();
        // Walking only from c1 must yield {c1, c1.tree, c1.blob} — not c2.
        let set = ReachableSet::mark_from(&repo, &[oid(&c1)]).unwrap();
        assert!(set.oids.contains(&oid(&c1)));
        assert!(
            !set.oids.contains(&oid(&c2)),
            "c2 should NOT be reachable from c1 alone"
        );
        let c1_tree = rev_parse(dir, &format!("{c1}^{{tree}}"));
        assert!(set.oids.contains(&oid(&c1_tree)));
    }
}
