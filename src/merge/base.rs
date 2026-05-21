//! Merge-base computation (the lowest common ancestor(s) of two commits in
//! the parent DAG).
//!
//! This is the foundation of three-way merge: knowing what commit to diff
//! both sides against. The algorithm mirrors git's `paint_down_to_common` in
//! `commit-reach.c` — a "paint down" walk that marks ancestors of A with one
//! flag, ancestors of B with another, and records commits with both flags
//! as candidate bases. Once a candidate is found, every commit on its
//! ancestor side is marked STALE so we don't re-emit a strictly-older base
//! when a newer one would have been the answer.
//!
//! ## Why timestamps drive the walk
//!
//! We pop commits from a max-heap keyed on committer timestamp. The point of
//! the max-heap is *pruning*: once the heap's top is older than every
//! candidate base we still might emit, we're done. The simple rule that
//! makes this work in `paint_down_to_common` is that propagating STALE from
//! a found base happens *via the priority queue*, not via a separate
//! ancestor pass. By the time we'd pop a commit that's an ancestor of an
//! already-found base, it will already carry the STALE bit (pushed by the
//! base's child), so we won't record it again.
//!
//! Note that git timestamps are not perfectly monotonic (rebases reset
//! committer time, clocks differ across machines). For pruning purposes
//! this is fine: a stale-marked commit's children may still get added to
//! the queue and processed correctly. We never "miss" a base; in the worst
//! case we just walk a slightly larger subgraph than the ideal.
//!
//! ## Flag semantics
//!
//! - `PARENT1` (0b001): this commit is an ancestor (reflexively) of `a`.
//! - `PARENT2` (0b010): this commit is an ancestor (reflexively) of `b`.
//! - `STALE`   (0b100): this commit is an ancestor of an already-emitted
//!   merge base, so it must not itself be emitted as a separate base.
//!
//! A commit with `PARENT1 | PARENT2` and not `STALE` is a candidate base.
//! Setting `STALE` for it (and propagating that bit upward) ensures that
//! its ancestors don't fire the same check independently.
//!
//! ## What we deliberately don't implement (yet)
//!
//! - Commit-graph generation numbers (`commit-graph.c`). Would let us prune
//!   more aggressively. The rustygit codebase doesn't have a commit-graph
//!   reader; without it we use the timestamp heap and accept walking more.
//! - Recursive merge-base (when there are multiple LCAs, git's
//!   `merge-recursive` strategy merges them in turn to form a virtual base).
//!   `merge_base` returns the earliest by committer time when there are
//!   multiple; Track B's three-way driver can rev that up later.

use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap};

use thiserror::Error;

use crate::commit::{Commit, CommitError};
use crate::hash::{HashError, ObjectId};
use crate::object::ObjectKind;
use crate::odb::OdbError;
use crate::repo::Repository;

/// Bit set: commit is an ancestor of the first input.
const PARENT1: u8 = 0b0001;
/// Bit set: commit is an ancestor of the second input.
const PARENT2: u8 = 0b0010;
/// Bit set: commit lies on an ancestor chain of a base we've already emitted.
/// Must not be emitted again.
const STALE: u8 = 0b0100;
/// Bit set: commit has already been pushed onto the `result` vector.
/// Prevents double-emission when the commit is popped multiple times from
/// the priority queue (which happens when more than one descendant push
/// caused it to be enqueued).
const RESULT: u8 = 0b1000;

#[derive(Error, Debug)]
pub enum MergeBaseError {
    #[error(transparent)]
    Odb(#[from] OdbError),
    #[error(transparent)]
    Commit(#[from] CommitError),
    #[error(transparent)]
    Hash(#[from] HashError),
    #[error("not a commit: {0}")]
    NotACommit(ObjectId),
}

/// An entry in the priority queue. We sort by committer timestamp descending
/// (max-heap) so we always explore the most recent commit next.
///
/// Ties are broken by oid bytes to make the ordering total and deterministic
/// — without a tie-breaker, two commits with identical timestamps would be
/// in implementation-defined order, which would flake tests that compare
/// our "first" of multiple bases against git's.
#[derive(Eq, PartialEq)]
struct QueueEntry {
    time: i64,
    oid: ObjectId,
}

impl Ord for QueueEntry {
    fn cmp(&self, other: &Self) -> Ordering {
        // Max-heap by time. On ties, larger oid first so the order is total
        // and we have a stable, deterministic dequeue order across runs.
        self.time
            .cmp(&other.time)
            .then_with(|| self.oid.cmp(&other.oid))
    }
}

impl PartialOrd for QueueEntry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Read and parse a commit from the odb, returning `(parents, committer_time)`.
fn read_commit(repo: &Repository, oid: ObjectId) -> Result<(Vec<ObjectId>, i64), MergeBaseError> {
    let obj = repo.odb().read(&oid)?;
    if obj.kind != ObjectKind::Commit {
        return Err(MergeBaseError::NotACommit(oid));
    }
    let commit = Commit::parse(&obj.data, repo.hash_kind())?;
    let time = commit.committer.when.seconds;
    Ok((commit.parents, time))
}

/// Just the committer timestamp, for putting a commit onto the priority
/// queue without needing its parents yet.
fn committer_time(repo: &Repository, oid: ObjectId) -> Result<i64, MergeBaseError> {
    let obj = repo.odb().read(&oid)?;
    if obj.kind != ObjectKind::Commit {
        return Err(MergeBaseError::NotACommit(oid));
    }
    let commit = Commit::parse(&obj.data, repo.hash_kind())?;
    Ok(commit.committer.when.seconds)
}

/// Return all merge bases (LCAs) of `a` and `b`.
///
/// For most pairs (linear history, simple branched-and-merged), the returned
/// vector has exactly one element. For true criss-cross merges — diamond
/// patterns where M1 has parents (X, Y) and M2 has parents (Y, X) — the
/// vector may contain both X and Y.
///
/// If `a` and `b` have no common ancestor (e.g. unrelated histories from two
/// independent `git init`s that were later glued together), the vector is
/// empty.
pub fn merge_bases(
    repo: &Repository,
    a: ObjectId,
    b: ObjectId,
) -> Result<Vec<ObjectId>, MergeBaseError> {
    // Reflexive case: a commit is its own merge base with itself.
    if a == b {
        return Ok(vec![a]);
    }

    // Per-oid flag state. Holds PARENT1, PARENT2, and STALE bits.
    let mut state: HashMap<ObjectId, u8> = HashMap::new();
    // Priority queue keyed on committer time, max-heap. We always pop the
    // most recent commit not yet processed.
    let mut queue: BinaryHeap<QueueEntry> = BinaryHeap::new();
    // Candidate bases — commits seen with both PARENT1 and PARENT2 and not
    // yet known to be STALE. We collect all of them during the walk, then
    // filter out the ones whose STALE bit got set later when a more-recent
    // base was found whose ancestor chain covers them.
    let mut result: Vec<ObjectId> = Vec::new();

    let time_a = committer_time(repo, a)?;
    let time_b = committer_time(repo, b)?;
    state.insert(a, PARENT1);
    state.insert(b, PARENT2);
    queue.push(QueueEntry {
        time: time_a,
        oid: a,
    });
    queue.push(QueueEntry {
        time: time_b,
        oid: b,
    });

    while let Some(QueueEntry { oid, .. }) = queue.pop() {
        // The flag set at the moment of dequeue. Subsequent pushes can OR in
        // more bits, but those will be handled when their pushed-again copies
        // are popped — or, more commonly, will already be reflected in `state`
        // because we update `state` BEFORE pushing.
        let flags = *state.get(&oid).unwrap_or(&0);

        // If this commit is already STALE, propagate STALE up to parents
        // (they may not have it yet even though we do) and continue. We
        // don't try to record it as a candidate base — it's stale by
        // definition.
        if flags & STALE != 0 {
            propagate_to_parents(repo, oid, flags, &mut state, &mut queue)?;
            continue;
        }

        // Candidate detection: both colors and not STALE. RESULT prevents
        // double-emission when the same commit is popped multiple times
        // (because separate descendants each enqueued it once).
        if (flags & (PARENT1 | PARENT2)) == (PARENT1 | PARENT2) {
            if flags & RESULT == 0 {
                result.push(oid);
                state.insert(oid, flags | RESULT);
            }
            // Mark this commit's PARENTS (not this commit itself) as STALE
            // so we don't re-emit a less-specific base further up the chain.
            // Matching git's paint_down_to_common: it ORs STALE into the
            // *local* flags variable used for propagation, but never writes
            // STALE onto the candidate commit itself. Setting STALE on the
            // candidate would cause our post-walk filter to drop it.
            let propagate_flags = flags | STALE;
            propagate_to_parents(repo, oid, propagate_flags, &mut state, &mut queue)?;
            continue;
        }

        // Not a candidate yet. Propagate this commit's color bits to its
        // parents so the walk converges on common ancestors.
        propagate_to_parents(repo, oid, flags, &mut state, &mut queue)?;
    }

    // Final filter: a base recorded earlier may have had STALE pushed onto
    // it from a later-discovered closer base. (This happens when commit D
    // is later popped and discovered to be a base, while one of D's
    // descendants C — which was emitted as a base previously — is also an
    // ancestor of the other side; but our STALE propagation goes from
    // children to parents only, so this filtering primarily catches the
    // case where we recorded a base and *later* found the STALE chain via
    // a different path.) The simpler way to look at it: after the walk
    // settles, any oid whose state has STALE set is dominated by another
    // emitted base and must be removed.
    Ok(result
        .into_iter()
        .filter(|oid| (*state.get(oid).unwrap_or(&0)) & STALE == 0)
        .collect())
}

/// Add `flags` (already masking the bits we want to push down) onto every
/// parent of `oid`, queuing parents that learned new bits.
fn propagate_to_parents(
    repo: &Repository,
    oid: ObjectId,
    flags: u8,
    state: &mut HashMap<ObjectId, u8>,
    queue: &mut BinaryHeap<QueueEntry>,
) -> Result<(), MergeBaseError> {
    let (parents, _) = read_commit(repo, oid)?;
    // Only PARENT1, PARENT2, and STALE propagate to parents — RESULT is a
    // per-commit "already emitted" marker, not a transitive property.
    let propagating = flags & (PARENT1 | PARENT2 | STALE);
    for parent in parents {
        let prev = *state.get(&parent).unwrap_or(&0);
        let new = prev | propagating;
        if new != prev {
            state.insert(parent, new);
            let t = committer_time(repo, parent)?;
            queue.push(QueueEntry {
                time: t,
                oid: parent,
            });
        }
    }
    Ok(())
}

/// Return a single "best" merge base, or `None` if `a` and `b` have no
/// common ancestor.
///
/// When `merge_bases` returns multiple LCAs (the criss-cross case), we
/// pick the one with the earliest committer time. The deterministic choice
/// matters: callers that diff against the result need the same answer
/// across processes and platforms. Ties are broken by oid bytes.
pub fn merge_base(
    repo: &Repository,
    a: ObjectId,
    b: ObjectId,
) -> Result<Option<ObjectId>, MergeBaseError> {
    let bases = merge_bases(repo, a, b)?;
    if bases.is_empty() {
        return Ok(None);
    }
    // Pair each base with its committer time, then pick the one with the
    // smallest (time, oid). The lexicographic tie-break by oid bytes is
    // what makes the choice fully deterministic when timestamps collide.
    let mut entries: Vec<(i64, ObjectId)> = Vec::with_capacity(bases.len());
    for oid in bases {
        entries.push((committer_time(repo, oid)?, oid));
    }
    entries.sort_by(|x, y| x.0.cmp(&y.0).then_with(|| x.1.cmp(&y.1)));
    Ok(Some(entries.into_iter().next().unwrap().1))
}

/// True if `ancestor` is reachable from `descendant` via parent links
/// (reflexively — every commit is its own ancestor).
///
/// Implementation: BFS from `descendant`, short-circuiting as soon as
/// `ancestor` is found in the parent set. We avoid building the full
/// reachable set — bailing early when the answer's known is the whole
/// point of the API.
///
/// We use a timestamp-keyed max-heap so we explore "recent" commits first.
/// We prune any commit whose committer time is strictly less than
/// `ancestor`'s own committer time: parents are at least as old (typically
/// older), so a commit older than `ancestor` can't have `ancestor` as a
/// parent on any path.
///
/// Caveat: git committer timestamps aren't perfectly monotonic across
/// rebase/rewrite. The pruning here is a heuristic but matches git's
/// `is_ancestor` style without commit-graph generation numbers. In the
/// non-monotonic edge case the heuristic may yield a false negative — but
/// only if a parent has a *higher* timestamp than its child, which is a
/// rare anomaly that real-world tooling avoids.
pub fn is_ancestor(
    repo: &Repository,
    ancestor: ObjectId,
    descendant: ObjectId,
) -> Result<bool, MergeBaseError> {
    if ancestor == descendant {
        return Ok(true);
    }
    let ancestor_time = committer_time(repo, ancestor)?;

    // BFS from descendant. Visited set prevents revisits.
    let mut visited: std::collections::HashSet<ObjectId> = std::collections::HashSet::new();
    let mut queue: BinaryHeap<QueueEntry> = BinaryHeap::new();
    let descendant_time = committer_time(repo, descendant)?;
    visited.insert(descendant);
    queue.push(QueueEntry {
        time: descendant_time,
        oid: descendant,
    });

    while let Some(QueueEntry { oid, time }) = queue.pop() {
        // Prune: a commit older than `ancestor` can't have `ancestor` on
        // any of its parent paths (parents are at least as old).
        if time < ancestor_time {
            continue;
        }
        let (parents, _) = read_commit(repo, oid)?;
        for parent in parents {
            if parent == ancestor {
                return Ok(true);
            }
            if visited.insert(parent) {
                let t = committer_time(repo, parent)?;
                queue.push(QueueEntry {
                    time: t,
                    oid: parent,
                });
            }
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::process::Command;
    use tempfile::tempdir;

    // ---------- test helpers (mirrors src/reachable.rs's style) ----------

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

    fn git_allow_fail(dir: &Path, args: &[&str]) -> std::process::Output {
        Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .expect("run git")
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
        // Disable signing in case a global config has it turned on for the
        // test user — that would otherwise hang waiting for a passphrase.
        git(dir, &["config", "commit.gpgsign", "false"]);
        git(dir, &["config", "tag.gpgsign", "false"]);
    }

    fn write_and_add(dir: &Path, path: &str, content: &str) {
        let p = dir.join(path);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(p, content).unwrap();
        git(dir, &["add", path]);
    }

    /// Empty commit with a fixed committer/author timestamp.
    fn make_commit_at(dir: &Path, msg: &str, secs: i64) -> String {
        let date = format!("{secs} +0000");
        git_env(
            dir,
            &["commit", "-q", "-m", msg, "--allow-empty"],
            &[("GIT_AUTHOR_DATE", &date), ("GIT_COMMITTER_DATE", &date)],
        );
        let out = git(dir, &["rev-parse", "HEAD"]).stdout;
        String::from_utf8(out).unwrap().trim().to_string()
    }

    /// Commit with a file change at a fixed timestamp.
    fn commit_file_at(dir: &Path, path: &str, content: &str, msg: &str, secs: i64) -> String {
        write_and_add(dir, path, content);
        let date = format!("{secs} +0000");
        git_env(
            dir,
            &["commit", "-q", "-m", msg],
            &[("GIT_AUTHOR_DATE", &date), ("GIT_COMMITTER_DATE", &date)],
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

    /// Shell `git merge-base --all <a> <b>` and return the result as a
    /// sorted list of oids. Used to cross-verify our merge_bases output.
    fn git_merge_base_all(dir: &Path, a: &str, b: &str) -> Vec<ObjectId> {
        let out = git_allow_fail(dir, &["merge-base", "--all", a, b]);
        if !out.status.success() {
            // git exits 1 when there's no merge base — treat as empty set.
            return Vec::new();
        }
        let mut v: Vec<ObjectId> = String::from_utf8(out.stdout)
            .unwrap()
            .lines()
            .filter(|l| !l.is_empty())
            .map(oid)
            .collect();
        v.sort();
        v
    }

    /// Shell `git merge-base <a> <b>` for the single-base form. Returns
    /// None on no-common-ancestor (git exits non-zero).
    fn git_merge_base_one(dir: &Path, a: &str, b: &str) -> Option<ObjectId> {
        let out = git_allow_fail(dir, &["merge-base", a, b]);
        if !out.status.success() {
            return None;
        }
        let s = String::from_utf8(out.stdout).unwrap();
        let line = s.lines().next().unwrap_or("").trim();
        if line.is_empty() {
            None
        } else {
            Some(oid(line))
        }
    }

    /// `git merge-base --is-ancestor <a> <b>`: exit 0 = yes, exit 1 = no.
    fn git_is_ancestor(dir: &Path, a: &str, b: &str) -> bool {
        let status = Command::new("git")
            .args(["merge-base", "--is-ancestor", a, b])
            .current_dir(dir)
            .status()
            .expect("run git");
        // exit 0 = ancestor; 1 = not; >= 128 = error (which we don't expect
        // in test setups).
        status.success()
    }

    fn our_merge_bases_sorted(repo: &Repository, a: ObjectId, b: ObjectId) -> Vec<ObjectId> {
        let mut v = merge_bases(repo, a, b).unwrap();
        v.sort();
        v
    }

    // ============= Test 1: identical commits =============

    #[test]
    fn identical_commits_is_own_base() {
        if !git_available() {
            eprintln!("skipping: no git");
            return;
        }
        let tmp = tempdir().unwrap();
        let dir = tmp.path();
        init_repo(dir);
        let c1 = commit_file_at(dir, "a.txt", "1\n", "c1", 1_700_000_000);

        let repo = Repository::discover(dir).unwrap();
        let c1_oid = oid(&c1);

        // merge_base(X, X) = [X].
        let bases = merge_bases(&repo, c1_oid, c1_oid).unwrap();
        assert_eq!(bases, vec![c1_oid]);

        let one = merge_base(&repo, c1_oid, c1_oid).unwrap();
        assert_eq!(one, Some(c1_oid));

        // is_ancestor(X, X) = true.
        assert!(is_ancestor(&repo, c1_oid, c1_oid).unwrap());

        // Cross-verify with git.
        let git_bases = git_merge_base_all(dir, &c1, &c1);
        assert_eq!(git_bases, vec![c1_oid]);
        assert!(git_is_ancestor(dir, &c1, &c1));
    }

    // ============= Test 2: linear history =============

    #[test]
    fn linear_history() {
        if !git_available() {
            eprintln!("skipping: no git");
            return;
        }
        let tmp = tempdir().unwrap();
        let dir = tmp.path();
        init_repo(dir);
        let a = commit_file_at(dir, "x.txt", "1\n", "A", 1_700_000_000);
        let b = commit_file_at(dir, "x.txt", "2\n", "B", 1_700_000_100);
        let c = commit_file_at(dir, "x.txt", "3\n", "C", 1_700_000_200);

        let repo = Repository::discover(dir).unwrap();
        let (a_o, b_o, c_o) = (oid(&a), oid(&b), oid(&c));

        // merge_base(B, C) = B (B is C's parent).
        assert_eq!(our_merge_bases_sorted(&repo, b_o, c_o), vec![b_o]);
        assert_eq!(merge_base(&repo, b_o, c_o).unwrap(), Some(b_o));
        // merge_base(A, C) = A.
        assert_eq!(our_merge_bases_sorted(&repo, a_o, c_o), vec![a_o]);
        // merge_base(C, A) — symmetric.
        assert_eq!(our_merge_bases_sorted(&repo, c_o, a_o), vec![a_o]);

        // is_ancestor checks.
        assert!(is_ancestor(&repo, a_o, c_o).unwrap());
        assert!(is_ancestor(&repo, b_o, c_o).unwrap());
        assert!(!is_ancestor(&repo, c_o, a_o).unwrap());
        assert!(!is_ancestor(&repo, c_o, b_o).unwrap());

        // Cross-verify with git.
        assert_eq!(git_merge_base_all(dir, &b, &c), vec![b_o]);
        assert_eq!(git_merge_base_all(dir, &a, &c), vec![a_o]);
        assert_eq!(git_merge_base_one(dir, &a, &c), Some(a_o));
        assert!(git_is_ancestor(dir, &a, &c));
        assert!(!git_is_ancestor(dir, &c, &a));
    }

    // ============= Test 3: simple Y-fork =============

    #[test]
    fn y_fork() {
        if !git_available() {
            eprintln!("skipping: no git");
            return;
        }
        // A → B → C  (main)
        //      ↘
        //       D → E  (feature)
        let tmp = tempdir().unwrap();
        let dir = tmp.path();
        init_repo(dir);
        let a = commit_file_at(dir, "f.txt", "a\n", "A", 1_700_000_000);
        let b = commit_file_at(dir, "f.txt", "b\n", "B", 1_700_000_100);
        let c = commit_file_at(dir, "f.txt", "c\n", "C", 1_700_000_200);
        // Branch off B.
        git(dir, &["checkout", "-q", "-b", "feature", &b]);
        let d = commit_file_at(dir, "f.txt", "d\n", "D", 1_700_000_300);
        let e = commit_file_at(dir, "f.txt", "e\n", "E", 1_700_000_400);

        let repo = Repository::discover(dir).unwrap();
        let (b_o, c_o, d_o, e_o, a_o) = (oid(&b), oid(&c), oid(&d), oid(&e), oid(&a));

        // The classic case: merge_base(C, E) = B.
        assert_eq!(our_merge_bases_sorted(&repo, c_o, e_o), vec![b_o]);
        // And symmetric.
        assert_eq!(our_merge_bases_sorted(&repo, e_o, c_o), vec![b_o]);
        // merge_base(D, A) = A.
        assert_eq!(our_merge_bases_sorted(&repo, d_o, a_o), vec![a_o]);
        // merge_base(D, C) = B.
        assert_eq!(our_merge_bases_sorted(&repo, d_o, c_o), vec![b_o]);

        // is_ancestor — none of {C, D, E} are ancestors of each other,
        // but all share B.
        assert!(!is_ancestor(&repo, c_o, e_o).unwrap());
        assert!(!is_ancestor(&repo, e_o, c_o).unwrap());
        assert!(is_ancestor(&repo, b_o, e_o).unwrap());
        assert!(is_ancestor(&repo, b_o, c_o).unwrap());
        assert!(is_ancestor(&repo, a_o, e_o).unwrap());

        // Cross-verify with git.
        assert_eq!(git_merge_base_all(dir, &c, &e), vec![b_o]);
        assert_eq!(git_merge_base_one(dir, &c, &e), Some(b_o));
        assert!(!git_is_ancestor(dir, &c, &e));
        assert!(git_is_ancestor(dir, &b, &e));
    }

    // ============= Test 4: two branches with merge commit =============

    #[test]
    fn two_branches_with_merge_commit() {
        if !git_available() {
            eprintln!("skipping: no git");
            return;
        }
        // A → B → C → M       (main, after merge)
        //      ↘  ↗
        //       D
        // Where M has parents (C, D) and D's parent is B.
        let tmp = tempdir().unwrap();
        let dir = tmp.path();
        init_repo(dir);
        let _a = commit_file_at(dir, "f1.txt", "a\n", "A", 1_700_000_000);
        let b = commit_file_at(dir, "f1.txt", "b\n", "B", 1_700_000_100);
        let c = commit_file_at(dir, "f2.txt", "c\n", "C", 1_700_000_200);
        // Branch off B → D
        git(dir, &["checkout", "-q", "-b", "topic", &b]);
        let d = commit_file_at(dir, "f3.txt", "d\n", "D", 1_700_000_150);
        // Merge D back into main.
        git(dir, &["checkout", "-q", "main"]);
        git_env(
            dir,
            &["merge", "-q", "--no-ff", "-m", "merge topic", "topic"],
            &[
                ("GIT_AUTHOR_DATE", "1700000300 +0000"),
                ("GIT_COMMITTER_DATE", "1700000300 +0000"),
            ],
        );
        let m = rev_parse(dir, "HEAD");

        let repo = Repository::discover(dir).unwrap();
        let (b_o, c_o, d_o, m_o) = (oid(&b), oid(&c), oid(&d), oid(&m));

        // merge_base(C, D) = B (their fork point).
        assert_eq!(our_merge_bases_sorted(&repo, c_o, d_o), vec![b_o]);
        // merge_base(C, M) = C (C is one of M's parents).
        assert_eq!(our_merge_bases_sorted(&repo, c_o, m_o), vec![c_o]);
        // merge_base(D, M) = D (D is the other parent).
        assert_eq!(our_merge_bases_sorted(&repo, d_o, m_o), vec![d_o]);
        // merge_base(B, M) = B.
        assert_eq!(our_merge_bases_sorted(&repo, b_o, m_o), vec![b_o]);

        // is_ancestor:
        assert!(is_ancestor(&repo, c_o, m_o).unwrap());
        assert!(is_ancestor(&repo, d_o, m_o).unwrap());
        assert!(is_ancestor(&repo, b_o, m_o).unwrap());
        assert!(!is_ancestor(&repo, m_o, c_o).unwrap());

        // Cross-verify.
        assert_eq!(git_merge_base_all(dir, &c, &d), vec![b_o]);
        assert_eq!(git_merge_base_all(dir, &c, &m), vec![c_o]);
        assert_eq!(git_merge_base_all(dir, &d, &m), vec![d_o]);
        assert!(git_is_ancestor(dir, &c, &m));
        assert!(git_is_ancestor(dir, &d, &m));
    }

    // ============= Test 5: criss-cross diamond =============

    #[test]
    fn criss_cross_diamond_has_multiple_bases() {
        if !git_available() {
            eprintln!("skipping: no git");
            return;
        }
        // Build a true criss-cross via `git commit-tree` so we can control
        // parent order exactly:
        //
        //       X
        //      / \
        //     Y   Z
        //     |\ /|
        //     | X |
        //     |/ \|
        //     M1  M2
        //
        // M1.parents = [Y, Z], M2.parents = [Z, Y]. The LCAs of M1 and M2
        // are BOTH Y and Z — that's the canonical criss-cross.
        let tmp = tempdir().unwrap();
        let dir = tmp.path();
        init_repo(dir);
        let x = commit_file_at(dir, "f.txt", "x\n", "X", 1_700_000_000);
        git(dir, &["checkout", "-q", "-b", "y", &x]);
        let y = commit_file_at(dir, "f.txt", "y\n", "Y", 1_700_000_100);
        git(dir, &["checkout", "-q", "-b", "z", &x]);
        let z = commit_file_at(dir, "f.txt", "z\n", "Z", 1_700_000_100);

        let y_tree = rev_parse(dir, &format!("{y}^{{tree}}"));
        let m1_out = git_env(
            dir,
            &["commit-tree", &y_tree, "-p", &y, "-p", &z, "-m", "M1"],
            &[
                ("GIT_AUTHOR_DATE", "1700000200 +0000"),
                ("GIT_COMMITTER_DATE", "1700000200 +0000"),
            ],
        );
        let m1 = String::from_utf8(m1_out.stdout).unwrap().trim().to_string();

        let z_tree = rev_parse(dir, &format!("{z}^{{tree}}"));
        let m2_out = git_env(
            dir,
            &["commit-tree", &z_tree, "-p", &z, "-p", &y, "-m", "M2"],
            &[
                ("GIT_AUTHOR_DATE", "1700000200 +0000"),
                ("GIT_COMMITTER_DATE", "1700000200 +0000"),
            ],
        );
        let m2 = String::from_utf8(m2_out.stdout).unwrap().trim().to_string();

        let repo = Repository::discover(dir).unwrap();
        let (m1_o, m2_o, y_o, z_o) = (oid(&m1), oid(&m2), oid(&y), oid(&z));

        // Our result must equal git's --all output as a set.
        let ours = our_merge_bases_sorted(&repo, m1_o, m2_o);
        let theirs = git_merge_base_all(dir, &m1, &m2);
        assert_eq!(ours, theirs, "criss-cross merge bases must match git");
        // Specifically contains both Y and Z (not just X).
        assert!(ours.contains(&y_o), "missing Y in {:?}", ours);
        assert!(ours.contains(&z_o), "missing Z in {:?}", ours);
        assert_eq!(ours.len(), 2);

        // merge_base() picks deterministically; we don't predict which oid
        // wins the tie-break (Y vs. Z share a timestamp), only that one of
        // them is returned.
        let one = merge_base(&repo, m1_o, m2_o).unwrap().unwrap();
        assert!(one == y_o || one == z_o);

        // is_ancestor: Y and Z are both ancestors of both M1 and M2.
        assert!(is_ancestor(&repo, y_o, m1_o).unwrap());
        assert!(is_ancestor(&repo, z_o, m1_o).unwrap());
        assert!(is_ancestor(&repo, y_o, m2_o).unwrap());
        assert!(is_ancestor(&repo, z_o, m2_o).unwrap());
    }

    // ============= Test 6: unrelated histories =============

    #[test]
    fn unrelated_histories_have_no_merge_base() {
        if !git_available() {
            eprintln!("skipping: no git");
            return;
        }
        let tmp = tempdir().unwrap();
        let dir = tmp.path();
        init_repo(dir);
        let a = commit_file_at(dir, "a.txt", "1\n", "A", 1_700_000_000);

        // Make a parallel orphan history.
        git(dir, &["checkout", "-q", "--orphan", "orphan"]);
        // `git rm -rfq .` may fail if the index is already empty; allow.
        git_allow_fail(dir, &["rm", "-rfq", "."]);
        let b = commit_file_at(dir, "b.txt", "2\n", "Z", 1_700_000_500);

        let repo = Repository::discover(dir).unwrap();
        let (a_o, b_o) = (oid(&a), oid(&b));

        let bases = merge_bases(&repo, a_o, b_o).unwrap();
        assert!(
            bases.is_empty(),
            "unrelated histories should have empty merge-base set, got {:?}",
            bases
        );
        let one = merge_base(&repo, a_o, b_o).unwrap();
        assert_eq!(one, None);
        assert!(!is_ancestor(&repo, a_o, b_o).unwrap());
        assert!(!is_ancestor(&repo, b_o, a_o).unwrap());

        // Cross-verify with git.
        assert!(git_merge_base_all(dir, &a, &b).is_empty());
        assert_eq!(git_merge_base_one(dir, &a, &b), None);
        assert!(!git_is_ancestor(dir, &a, &b));
    }

    // ============= Test 7: long chain (perf sanity) =============

    #[test]
    fn long_chain_completes_quickly() {
        if !git_available() {
            eprintln!("skipping: no git");
            return;
        }
        // 100-commit linear chain + 50-commit branch off commit at position 30.
        let tmp = tempdir().unwrap();
        let dir = tmp.path();
        init_repo(dir);

        let mut mains: Vec<String> = Vec::new();
        for i in 0..100 {
            mains.push(make_commit_at(
                dir,
                &format!("main-{i}"),
                1_700_000_000 + i as i64,
            ));
        }
        // Branch off commit at index 30.
        let fork = mains[30].clone();
        git(dir, &["checkout", "-q", "-b", "branch", &fork]);
        let mut branches: Vec<String> = Vec::new();
        for i in 0..50 {
            branches.push(make_commit_at(
                dir,
                &format!("branch-{i}"),
                1_700_000_000 + 100 + i as i64,
            ));
        }

        let repo = Repository::discover(dir).unwrap();
        let main_tip = oid(mains.last().unwrap());
        let branch_tip = oid(branches.last().unwrap());

        let start = std::time::Instant::now();
        let bases = merge_bases(&repo, main_tip, branch_tip).unwrap();
        let elapsed = start.elapsed();

        // The fork point is mains[30].
        assert_eq!(bases, vec![oid(&fork)]);
        // Soft perf guard: 150 commits should be quick. (5s is generous to
        // tolerate slow CI.)
        eprintln!("long_chain elapsed: {elapsed:?}");
        assert!(
            elapsed.as_secs() < 5,
            "merge_bases took too long: {elapsed:?}"
        );

        // is_ancestor: fork point is ancestor of both tips.
        assert!(is_ancestor(&repo, oid(&fork), main_tip).unwrap());
        assert!(is_ancestor(&repo, oid(&fork), branch_tip).unwrap());
        // Tip of main is NOT ancestor of branch tip.
        assert!(!is_ancestor(&repo, main_tip, branch_tip).unwrap());

        // Cross-verify.
        assert_eq!(
            git_merge_base_all(dir, mains.last().unwrap(), branches.last().unwrap()),
            vec![oid(&fork)]
        );
    }

    // ============= Test 8: random DAG cross-check =============

    /// Tiny deterministic PRNG. xorshift32 — we only need a few bits of
    /// "random-looking" output, no need to pull in `rand`.
    struct Rng(u32);
    impl Rng {
        fn new(seed: u32) -> Self {
            Self(seed.max(1))
        }
        fn next_u32(&mut self) -> u32 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            self.0 = x;
            x
        }
        fn range(&mut self, n: u32) -> u32 {
            self.next_u32() % n
        }
    }

    #[test]
    fn random_dag_matches_system_git() {
        if !git_available() {
            eprintln!("skipping: no git");
            return;
        }
        // Build a randomly-shaped DAG of N commits, where each commit has 1
        // or 2 parents picked from earlier commits. The first commit is a
        // root. Some fraction of subsequent commits are 2-parent merge
        // commits, generating a real merge DAG with criss-crosses.
        //
        // For 20 random pairs, compare merge_bases against
        // `git merge-base --all`.
        let tmp = tempdir().unwrap();
        let dir = tmp.path();
        init_repo(dir);

        const N: usize = 30;
        let mut rng = Rng::new(42);
        let mut commits: Vec<String> = Vec::with_capacity(N);

        // Seed commit (root).
        let root = commit_file_at(dir, "z.txt", "c0\n", "c0", 1_700_000_000);
        commits.push(root);

        for i in 1..N {
            // Modify z.txt so the tree changes — otherwise git would
            // collapse identical trees into the same oid.
            std::fs::write(dir.join("z.txt"), format!("c{i}\n")).unwrap();
            git(dir, &["add", "z.txt"]);
            let tree_out = git(dir, &["write-tree"]);
            let tree = String::from_utf8(tree_out.stdout)
                .unwrap()
                .trim()
                .to_string();

            // Choose 1 or 2 parents at random from earlier commits.
            let p1_idx = rng.range(i as u32) as usize;
            let p1 = commits[p1_idx].clone();
            let two_parents = rng.range(100) < 40 && i >= 2;

            let mut args: Vec<String> = vec![
                "commit-tree".to_string(),
                tree,
                "-p".to_string(),
                p1.clone(),
            ];
            if two_parents {
                let mut p2_idx = rng.range(i as u32) as usize;
                if p2_idx == p1_idx && i >= 2 {
                    p2_idx = (p2_idx + 1) % i;
                }
                if p2_idx != p1_idx {
                    args.push("-p".to_string());
                    args.push(commits[p2_idx].clone());
                }
            }
            args.push("-m".to_string());
            args.push(format!("c{i}"));

            let date = format!("{} +0000", 1_700_000_000 + i as i64 * 10);
            let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
            let out = git_env(
                dir,
                &arg_refs,
                &[("GIT_AUTHOR_DATE", &date), ("GIT_COMMITTER_DATE", &date)],
            );
            let oid_str = String::from_utf8(out.stdout).unwrap().trim().to_string();
            // Point HEAD at the new commit so the next `write-tree` builds
            // against an index that's consistent with our new commit.
            git(dir, &["update-ref", "HEAD", &oid_str]);
            commits.push(oid_str);
        }

        // Reopen the repo so the ObjectDb sees all new commits.
        let repo = Repository::discover(dir).unwrap();

        // 20 random pairs cross-verified against git.
        for trial in 0..20 {
            let i = rng.range(N as u32) as usize;
            let j = rng.range(N as u32) as usize;
            let a = &commits[i];
            let b = &commits[j];
            let a_o = oid(a);
            let b_o = oid(b);

            let ours = our_merge_bases_sorted(&repo, a_o, b_o);
            let theirs = git_merge_base_all(dir, a, b);
            assert_eq!(
                ours, theirs,
                "trial {trial}: merge_bases({i}, {j}) mismatch.\n\
                 a={a}\nb={b}\nours={ours:?}\ntheirs={theirs:?}"
            );

            // Spot-check is_ancestor too.
            let our_anc = is_ancestor(&repo, a_o, b_o).unwrap();
            let their_anc = git_is_ancestor(dir, a, b);
            assert_eq!(
                our_anc, their_anc,
                "trial {trial}: is_ancestor({a}, {b}) mismatch: ours={our_anc} theirs={their_anc}"
            );

            // And in the other direction.
            let our_anc_rev = is_ancestor(&repo, b_o, a_o).unwrap();
            let their_anc_rev = git_is_ancestor(dir, b, a);
            assert_eq!(
                our_anc_rev, their_anc_rev,
                "trial {trial}: is_ancestor({b}, {a}) mismatch"
            );
        }
    }

    // ============= Edge case: two unrelated roots =============

    #[test]
    fn two_root_commits_unrelated() {
        if !git_available() {
            eprintln!("skipping: no git");
            return;
        }
        let tmp = tempdir().unwrap();
        let dir = tmp.path();
        init_repo(dir);
        let r1 = commit_file_at(dir, "a.txt", "x\n", "root1", 1_700_000_000);
        // Second root (orphan branch).
        git(dir, &["checkout", "-q", "--orphan", "alt"]);
        git_allow_fail(dir, &["rm", "-rfq", "."]);
        let r2 = commit_file_at(dir, "b.txt", "y\n", "root2", 1_700_000_500);

        let repo = Repository::discover(dir).unwrap();
        let (r1_o, r2_o) = (oid(&r1), oid(&r2));
        assert!(merge_bases(&repo, r1_o, r2_o).unwrap().is_empty());
        assert_eq!(merge_base(&repo, r1_o, r2_o).unwrap(), None);
        assert!(!is_ancestor(&repo, r1_o, r2_o).unwrap());
        assert!(!is_ancestor(&repo, r2_o, r1_o).unwrap());
        assert!(git_merge_base_all(dir, &r1, &r2).is_empty());
    }

    // ============= Edge case: commit + its own parent =============

    #[test]
    fn commit_with_its_parent_returns_parent() {
        if !git_available() {
            eprintln!("skipping: no git");
            return;
        }
        let tmp = tempdir().unwrap();
        let dir = tmp.path();
        init_repo(dir);
        let p = commit_file_at(dir, "f.txt", "1\n", "parent", 1_700_000_000);
        let c = commit_file_at(dir, "f.txt", "2\n", "child", 1_700_000_100);

        let repo = Repository::discover(dir).unwrap();
        let (p_o, c_o) = (oid(&p), oid(&c));
        assert_eq!(our_merge_bases_sorted(&repo, p_o, c_o), vec![p_o]);
        assert_eq!(merge_base(&repo, p_o, c_o).unwrap(), Some(p_o));
        assert!(is_ancestor(&repo, p_o, c_o).unwrap());
        assert!(!is_ancestor(&repo, c_o, p_o).unwrap());
    }

    // ============= Edge case: octopus 3-parent merge =============

    #[test]
    fn octopus_three_parent_merge() {
        if !git_available() {
            eprintln!("skipping: no git");
            return;
        }
        //     A
        //    /|\
        //   B C D
        //    \|/
        //     M    (3 parents)
        let tmp = tempdir().unwrap();
        let dir = tmp.path();
        init_repo(dir);
        let a = commit_file_at(dir, "f.txt", "a\n", "A", 1_700_000_000);
        git(dir, &["checkout", "-q", "-b", "b_branch", &a]);
        let b = commit_file_at(dir, "f.txt", "b\n", "B", 1_700_000_100);
        git(dir, &["checkout", "-q", "-b", "c_branch", &a]);
        let c = commit_file_at(dir, "f.txt", "c\n", "C", 1_700_000_200);
        git(dir, &["checkout", "-q", "-b", "d_branch", &a]);
        let d = commit_file_at(dir, "f.txt", "d\n", "D", 1_700_000_300);

        // M = commit-tree <tree-of-B> -p B -p C -p D
        let b_tree = rev_parse(dir, &format!("{b}^{{tree}}"));
        let m_out = git_env(
            dir,
            &[
                "commit-tree",
                &b_tree,
                "-p",
                &b,
                "-p",
                &c,
                "-p",
                &d,
                "-m",
                "M",
            ],
            &[
                ("GIT_AUTHOR_DATE", "1700000400 +0000"),
                ("GIT_COMMITTER_DATE", "1700000400 +0000"),
            ],
        );
        let m = String::from_utf8(m_out.stdout).unwrap().trim().to_string();

        let repo = Repository::discover(dir).unwrap();
        let (b_o, c_o, d_o, m_o, a_o) = (oid(&b), oid(&c), oid(&d), oid(&m), oid(&a));

        // M's direct parents are their own merge-bases with M.
        assert_eq!(our_merge_bases_sorted(&repo, m_o, b_o), vec![b_o]);
        assert_eq!(our_merge_bases_sorted(&repo, m_o, c_o), vec![c_o]);
        assert_eq!(our_merge_bases_sorted(&repo, m_o, d_o), vec![d_o]);
        assert!(is_ancestor(&repo, c_o, m_o).unwrap());
        assert!(is_ancestor(&repo, d_o, m_o).unwrap());
        // merge_base(B, C) is A — they share only A.
        assert_eq!(our_merge_bases_sorted(&repo, b_o, c_o), vec![a_o]);
        assert_eq!(our_merge_bases_sorted(&repo, b_o, d_o), vec![a_o]);

        // Cross-verify.
        assert_eq!(git_merge_base_all(dir, &m, &b), vec![b_o]);
        assert_eq!(git_merge_base_all(dir, &b, &c), vec![a_o]);
    }

    // ============= Edge case: missing commit in odb =============

    #[test]
    fn missing_commit_surfaces_odb_error() {
        if !git_available() {
            eprintln!("skipping: no git");
            return;
        }
        let tmp = tempdir().unwrap();
        let dir = tmp.path();
        init_repo(dir);
        let _ = commit_file_at(dir, "f.txt", "a\n", "A", 1_700_000_000);

        let repo = Repository::discover(dir).unwrap();

        // Use two distinct fake oids so we don't trigger the a==b early-exit
        // (which returns Ok(vec![a]) without reading anything).
        let fake1 = oid("deadbeefdeadbeefdeadbeefdeadbeefdeadbeef");
        let fake2 = oid("cafef00dcafef00dcafef00dcafef00dcafef00d");
        match merge_bases(&repo, fake1, fake2) {
            Err(MergeBaseError::Odb(OdbError::NotFound(o))) => {
                assert!(o == fake1 || o == fake2, "got NotFound for {o}");
            }
            other => panic!("expected Odb(NotFound), got {other:?}"),
        }

        // is_ancestor with a missing ancestor and a real descendant should
        // also surface an OdbError (we read ancestor's time first).
        let real = rev_parse(dir, "HEAD");
        let r_o = oid(&real);
        match is_ancestor(&repo, fake1, r_o) {
            Err(MergeBaseError::Odb(OdbError::NotFound(o))) => assert_eq!(o, fake1),
            other => panic!("expected Odb(NotFound), got {other:?}"),
        }
    }

    // ============= Edge case: non-commit target =============

    #[test]
    fn non_commit_target_errors() {
        if !git_available() {
            eprintln!("skipping: no git");
            return;
        }
        let tmp = tempdir().unwrap();
        let dir = tmp.path();
        init_repo(dir);
        let c = commit_file_at(dir, "f.txt", "1\n", "A", 1_700_000_000);
        let tree = rev_parse(dir, &format!("{c}^{{tree}}"));

        let repo = Repository::discover(dir).unwrap();
        let (c_o, t_o) = (oid(&c), oid(&tree));
        match merge_bases(&repo, c_o, t_o) {
            Err(MergeBaseError::NotACommit(o)) => assert_eq!(o, t_o),
            other => panic!("expected NotACommit, got {other:?}"),
        }
    }

    // ============= Test: complex DAG with back-merges =============

    #[test]
    fn complex_dag_with_back_merges() {
        if !git_available() {
            eprintln!("skipping: no git");
            return;
        }
        // Construct a back-merge pattern via commit-tree so parent order
        // is explicit, then cross-verify every pair against system git.
        let tmp = tempdir().unwrap();
        let dir = tmp.path();
        init_repo(dir);
        let a = commit_file_at(dir, "f.txt", "a\n", "A", 1_700_000_000);
        let b = commit_file_at(dir, "f.txt", "b\n", "B", 1_700_000_100);

        // X: parents (A, B) — back-merges B into a branch off A.
        let a_tree = rev_parse(dir, &format!("{a}^{{tree}}"));
        let x_out = git_env(
            dir,
            &["commit-tree", &a_tree, "-p", &a, "-p", &b, "-m", "X"],
            &[
                ("GIT_AUTHOR_DATE", "1700000150 +0000"),
                ("GIT_COMMITTER_DATE", "1700000150 +0000"),
            ],
        );
        let x = String::from_utf8(x_out.stdout).unwrap().trim().to_string();

        // C: child of B (linear on the "main" side).
        let c = commit_file_at(dir, "f.txt", "c\n", "C", 1_700_000_200);

        // Y: parents (X, C) — merges C into the X-side.
        let x_tree = rev_parse(dir, &format!("{x}^{{tree}}"));
        let y_out = git_env(
            dir,
            &["commit-tree", &x_tree, "-p", &x, "-p", &c, "-m", "Y"],
            &[
                ("GIT_AUTHOR_DATE", "1700000250 +0000"),
                ("GIT_COMMITTER_DATE", "1700000250 +0000"),
            ],
        );
        let y = String::from_utf8(y_out.stdout).unwrap().trim().to_string();

        let repo = Repository::discover(dir).unwrap();

        let names = ["A", "B", "C", "X", "Y"];
        let oids_str = [&a, &b, &c, &x, &y];
        for i in 0..names.len() {
            for j in 0..names.len() {
                let oa = oid(oids_str[i]);
                let ob = oid(oids_str[j]);
                let ours = our_merge_bases_sorted(&repo, oa, ob);
                let theirs = git_merge_base_all(dir, oids_str[i], oids_str[j]);
                assert_eq!(
                    ours, theirs,
                    "complex_dag: merge_base({}, {}) mismatch: ours={:?} theirs={:?}",
                    names[i], names[j], ours, theirs,
                );

                let our_anc = is_ancestor(&repo, oa, ob).unwrap();
                let their_anc = git_is_ancestor(dir, oids_str[i], oids_str[j]);
                assert_eq!(
                    our_anc, their_anc,
                    "complex_dag: is_ancestor({}, {}) mismatch",
                    names[i], names[j]
                );
            }
        }
    }

    // ============= Test: merge_base picks earliest in criss-cross =============

    #[test]
    fn merge_base_picks_earliest_in_criss_cross() {
        if !git_available() {
            eprintln!("skipping: no git");
            return;
        }
        // Criss-cross with Y earlier than Z so we can predict the tie-break
        // is by time (not oid).
        let tmp = tempdir().unwrap();
        let dir = tmp.path();
        init_repo(dir);
        let x = commit_file_at(dir, "f.txt", "x\n", "X", 1_700_000_000);
        git(dir, &["checkout", "-q", "-b", "y", &x]);
        let y = commit_file_at(dir, "f.txt", "y\n", "Y", 1_700_000_100); // earlier
        git(dir, &["checkout", "-q", "-b", "z", &x]);
        let z = commit_file_at(dir, "f.txt", "z\n", "Z", 1_700_000_200); // later

        let y_tree = rev_parse(dir, &format!("{y}^{{tree}}"));
        let m1_out = git_env(
            dir,
            &["commit-tree", &y_tree, "-p", &y, "-p", &z, "-m", "M1"],
            &[
                ("GIT_AUTHOR_DATE", "1700000300 +0000"),
                ("GIT_COMMITTER_DATE", "1700000300 +0000"),
            ],
        );
        let m1 = String::from_utf8(m1_out.stdout).unwrap().trim().to_string();
        let z_tree = rev_parse(dir, &format!("{z}^{{tree}}"));
        let m2_out = git_env(
            dir,
            &["commit-tree", &z_tree, "-p", &z, "-p", &y, "-m", "M2"],
            &[
                ("GIT_AUTHOR_DATE", "1700000300 +0000"),
                ("GIT_COMMITTER_DATE", "1700000300 +0000"),
            ],
        );
        let m2 = String::from_utf8(m2_out.stdout).unwrap().trim().to_string();

        let repo = Repository::discover(dir).unwrap();
        let bases = our_merge_bases_sorted(&repo, oid(&m1), oid(&m2));
        assert_eq!(bases.len(), 2);
        let one = merge_base(&repo, oid(&m1), oid(&m2)).unwrap().unwrap();
        assert_eq!(one, oid(&y), "merge_base should pick Y (earlier time)");
    }

    // ============= Test: symmetry =============

    #[test]
    fn merge_bases_is_symmetric() {
        if !git_available() {
            eprintln!("skipping: no git");
            return;
        }
        // For any pair, merge_bases(a, b) == merge_bases(b, a) as sets.
        let tmp = tempdir().unwrap();
        let dir = tmp.path();
        init_repo(dir);
        let a = commit_file_at(dir, "f.txt", "a\n", "A", 1_700_000_000);
        let b = commit_file_at(dir, "f.txt", "b\n", "B", 1_700_000_100);
        git(dir, &["checkout", "-q", "-b", "side", &a]);
        let c = commit_file_at(dir, "f.txt", "c\n", "C", 1_700_000_200);

        let repo = Repository::discover(dir).unwrap();
        let (a_o, b_o, c_o) = (oid(&a), oid(&b), oid(&c));

        for &(p1, p2) in &[(a_o, b_o), (b_o, c_o), (a_o, c_o)] {
            let m1 = our_merge_bases_sorted(&repo, p1, p2);
            let m2 = our_merge_bases_sorted(&repo, p2, p1);
            assert_eq!(m1, m2, "merge_bases({p1}, {p2}) != merge_bases({p2}, {p1})");
        }
    }

    // ============= Test: root commit with itself =============

    #[test]
    fn root_commit_with_itself() {
        if !git_available() {
            eprintln!("skipping: no git");
            return;
        }
        let tmp = tempdir().unwrap();
        let dir = tmp.path();
        init_repo(dir);
        let r = commit_file_at(dir, "f.txt", "r\n", "root", 1_700_000_000);

        let repo = Repository::discover(dir).unwrap();
        let r_o = oid(&r);
        assert_eq!(our_merge_bases_sorted(&repo, r_o, r_o), vec![r_o]);
        assert!(is_ancestor(&repo, r_o, r_o).unwrap());
    }

    // ============= Test: far-apart is_ancestor =============

    #[test]
    fn far_apart_ancestor_check_is_correct() {
        if !git_available() {
            eprintln!("skipping: no git");
            return;
        }
        let tmp = tempdir().unwrap();
        let dir = tmp.path();
        init_repo(dir);
        // Linear chain of 50 commits.
        let mut commits: Vec<String> = Vec::new();
        for i in 0..50 {
            commits.push(make_commit_at(
                dir,
                &format!("c{i}"),
                1_700_000_000 + i as i64,
            ));
        }
        let repo = Repository::discover(dir).unwrap();
        assert!(is_ancestor(&repo, oid(&commits[0]), oid(&commits[49])).unwrap());
        assert!(!is_ancestor(&repo, oid(&commits[49]), oid(&commits[0])).unwrap());
        // Random middles.
        assert!(is_ancestor(&repo, oid(&commits[10]), oid(&commits[40])).unwrap());
        assert!(!is_ancestor(&repo, oid(&commits[40]), oid(&commits[10])).unwrap());

        // Cross-verify with git on a few:
        assert!(git_is_ancestor(dir, &commits[0], &commits[49]));
        assert!(!git_is_ancestor(dir, &commits[49], &commits[0]));
    }

    // ============= Test: criss-cross with extra trailing commits =============

    #[test]
    fn criss_cross_with_descendant_commits() {
        if !git_available() {
            eprintln!("skipping: no git");
            return;
        }
        // Build the canonical criss-cross, then extend it with descendant
        // commits on both sides. The criss-cross bases of the new tips must
        // still be Y and Z (not their descendants on one side and not X).
        let tmp = tempdir().unwrap();
        let dir = tmp.path();
        init_repo(dir);
        let x = commit_file_at(dir, "f.txt", "x\n", "X", 1_700_000_000);
        git(dir, &["checkout", "-q", "-b", "y", &x]);
        let y = commit_file_at(dir, "f.txt", "y\n", "Y", 1_700_000_100);
        git(dir, &["checkout", "-q", "-b", "z", &x]);
        let z = commit_file_at(dir, "f.txt", "z\n", "Z", 1_700_000_100);

        let y_tree = rev_parse(dir, &format!("{y}^{{tree}}"));
        let m1_out = git_env(
            dir,
            &["commit-tree", &y_tree, "-p", &y, "-p", &z, "-m", "M1"],
            &[
                ("GIT_AUTHOR_DATE", "1700000200 +0000"),
                ("GIT_COMMITTER_DATE", "1700000200 +0000"),
            ],
        );
        let m1 = String::from_utf8(m1_out.stdout).unwrap().trim().to_string();
        let z_tree = rev_parse(dir, &format!("{z}^{{tree}}"));
        let m2_out = git_env(
            dir,
            &["commit-tree", &z_tree, "-p", &z, "-p", &y, "-m", "M2"],
            &[
                ("GIT_AUTHOR_DATE", "1700000200 +0000"),
                ("GIT_COMMITTER_DATE", "1700000200 +0000"),
            ],
        );
        let m2 = String::from_utf8(m2_out.stdout).unwrap().trim().to_string();

        // Now add a child of M1 and a child of M2 (single-parent extensions).
        let m1_tree = rev_parse(dir, &format!("{m1}^{{tree}}"));
        let n1_out = git_env(
            dir,
            &["commit-tree", &m1_tree, "-p", &m1, "-m", "N1"],
            &[
                ("GIT_AUTHOR_DATE", "1700000300 +0000"),
                ("GIT_COMMITTER_DATE", "1700000300 +0000"),
            ],
        );
        let n1 = String::from_utf8(n1_out.stdout).unwrap().trim().to_string();
        let m2_tree = rev_parse(dir, &format!("{m2}^{{tree}}"));
        let n2_out = git_env(
            dir,
            &["commit-tree", &m2_tree, "-p", &m2, "-m", "N2"],
            &[
                ("GIT_AUTHOR_DATE", "1700000300 +0000"),
                ("GIT_COMMITTER_DATE", "1700000300 +0000"),
            ],
        );
        let n2 = String::from_utf8(n2_out.stdout).unwrap().trim().to_string();

        let repo = Repository::discover(dir).unwrap();
        let ours = our_merge_bases_sorted(&repo, oid(&n1), oid(&n2));
        let theirs = git_merge_base_all(dir, &n1, &n2);
        assert_eq!(ours, theirs, "post-criss-cross extension must match git");
        // Specifically: should be Y and Z, not X, not M1/M2.
        assert!(ours.contains(&oid(&y)));
        assert!(ours.contains(&oid(&z)));
        assert!(!ours.contains(&oid(&x)));
    }

    // ============= Test: merge_base with merge commit as one side =============

    #[test]
    fn one_side_is_a_merge_commit() {
        if !git_available() {
            eprintln!("skipping: no git");
            return;
        }
        // Build a small graph, then ask for the merge-base where one side
        // is a merge commit and the other is a sibling.
        //
        //     A
        //    / \
        //   B   C
        //    \ /
        //     M
        //     |
        //     D
        //
        // merge_base(M, B) should be B (B is one of M's parents).
        // merge_base(D, B) should be B too (D is descendant of M which is
        // descendant of B).
        // merge_base(D, C) should be C.
        let tmp = tempdir().unwrap();
        let dir = tmp.path();
        init_repo(dir);
        let a = commit_file_at(dir, "f.txt", "a\n", "A", 1_700_000_000);
        git(dir, &["checkout", "-q", "-b", "b", &a]);
        let b = commit_file_at(dir, "f.txt", "b\n", "B", 1_700_000_100);
        git(dir, &["checkout", "-q", "-b", "c", &a]);
        let c = commit_file_at(dir, "f.txt", "c\n", "C", 1_700_000_200);
        // M = merge of B and C.
        let b_tree = rev_parse(dir, &format!("{b}^{{tree}}"));
        let m_out = git_env(
            dir,
            &["commit-tree", &b_tree, "-p", &b, "-p", &c, "-m", "M"],
            &[
                ("GIT_AUTHOR_DATE", "1700000300 +0000"),
                ("GIT_COMMITTER_DATE", "1700000300 +0000"),
            ],
        );
        let m = String::from_utf8(m_out.stdout).unwrap().trim().to_string();
        // D = child of M.
        let d_out = git_env(
            dir,
            &["commit-tree", &b_tree, "-p", &m, "-m", "D"],
            &[
                ("GIT_AUTHOR_DATE", "1700000400 +0000"),
                ("GIT_COMMITTER_DATE", "1700000400 +0000"),
            ],
        );
        let d = String::from_utf8(d_out.stdout).unwrap().trim().to_string();

        let repo = Repository::discover(dir).unwrap();
        let (a_o, b_o, c_o, m_o, d_o) = (oid(&a), oid(&b), oid(&c), oid(&m), oid(&d));
        assert_eq!(our_merge_bases_sorted(&repo, m_o, b_o), vec![b_o]);
        assert_eq!(our_merge_bases_sorted(&repo, m_o, c_o), vec![c_o]);
        assert_eq!(our_merge_bases_sorted(&repo, d_o, b_o), vec![b_o]);
        assert_eq!(our_merge_bases_sorted(&repo, d_o, c_o), vec![c_o]);
        assert_eq!(our_merge_bases_sorted(&repo, d_o, a_o), vec![a_o]);

        // Cross-verify.
        assert_eq!(git_merge_base_all(dir, &m, &b), vec![b_o]);
        assert_eq!(git_merge_base_all(dir, &d, &c), vec![c_o]);
    }

    // ============= Test: deep history with sparse merges =============

    #[test]
    fn deep_history_with_periodic_merges() {
        if !git_available() {
            eprintln!("skipping: no git");
            return;
        }
        // Two branches that intermittently merge into each other. Tests
        // that propagation of STALE bits works correctly across many
        // generations.
        let tmp = tempdir().unwrap();
        let dir = tmp.path();
        init_repo(dir);
        let root = commit_file_at(dir, "f.txt", "0\n", "root", 1_700_000_000);

        // Maintain two linear branches. Each iteration grows main by one
        // commit, grows side by one commit, and occasionally cross-merges.
        let mut main_tip = root.clone();
        git(dir, &["checkout", "-q", "-b", "side", &root]);
        // Side gets its first commit:
        let mut side_tip = commit_file_at(dir, "s.txt", "0\n", "s0", 1_700_000_050);
        git(dir, &["checkout", "-q", "main"]);

        let mut t = 1_700_000_100i64;
        for i in 0..10 {
            // main extends
            std::fs::write(dir.join("f.txt"), format!("{}\n", i + 1)).unwrap();
            git(dir, &["add", "f.txt"]);
            let tree = String::from_utf8(git(dir, &["write-tree"]).stdout)
                .unwrap()
                .trim()
                .to_string();
            let date = format!("{t} +0000");
            let out = git_env(
                dir,
                &[
                    "commit-tree",
                    &tree,
                    "-p",
                    &main_tip,
                    "-m",
                    &format!("main-{i}"),
                ],
                &[("GIT_AUTHOR_DATE", &date), ("GIT_COMMITTER_DATE", &date)],
            );
            main_tip = String::from_utf8(out.stdout).unwrap().trim().to_string();
            git(dir, &["update-ref", "refs/heads/main", &main_tip]);
            t += 10;

            // side extends
            std::fs::write(dir.join("s.txt"), format!("{}\n", i + 1)).unwrap();
            git(dir, &["add", "s.txt"]);
            let tree = String::from_utf8(git(dir, &["write-tree"]).stdout)
                .unwrap()
                .trim()
                .to_string();
            let date = format!("{t} +0000");
            let out = git_env(
                dir,
                &[
                    "commit-tree",
                    &tree,
                    "-p",
                    &side_tip,
                    "-m",
                    &format!("side-{i}"),
                ],
                &[("GIT_AUTHOR_DATE", &date), ("GIT_COMMITTER_DATE", &date)],
            );
            side_tip = String::from_utf8(out.stdout).unwrap().trim().to_string();
            t += 10;

            // Every 3 iterations, side merges main into itself.
            if i % 3 == 2 {
                let date = format!("{t} +0000");
                let out = git_env(
                    dir,
                    &[
                        "commit-tree",
                        &tree,
                        "-p",
                        &side_tip,
                        "-p",
                        &main_tip,
                        "-m",
                        &format!("side-merge-{i}"),
                    ],
                    &[("GIT_AUTHOR_DATE", &date), ("GIT_COMMITTER_DATE", &date)],
                );
                side_tip = String::from_utf8(out.stdout).unwrap().trim().to_string();
                t += 10;
            }
        }
        git(dir, &["update-ref", "refs/heads/side", &side_tip]);

        let repo = Repository::discover(dir).unwrap();
        let ours = our_merge_bases_sorted(&repo, oid(&main_tip), oid(&side_tip));
        let theirs = git_merge_base_all(dir, &main_tip, &side_tip);
        assert_eq!(
            ours, theirs,
            "deep history merge bases mismatch.\nmain={main_tip}\nside={side_tip}"
        );

        // is_ancestor: whether main_tip is an ancestor of side_tip
        // depends on whether the last iteration was a "merge" iteration.
        // The final side commit might be a merge that pulled main_tip in,
        // or might be a plain side commit (in which case the last *merged*
        // main commit is some earlier commit on main, not main_tip itself).
        // Whichever it is, our answer must match git's answer.
        let our_anc = is_ancestor(&repo, oid(&main_tip), oid(&side_tip)).unwrap();
        let their_anc = git_is_ancestor(dir, &main_tip, &side_tip);
        assert_eq!(our_anc, their_anc, "deep_history is_ancestor mismatch");
    }

    // ============= Test: queue entry ordering is total =============

    #[test]
    fn queue_entry_ordering_is_total_and_stable() {
        // Synthetic: same timestamp, different oids — verify the ordering
        // is total and decided by oid bytes. This protects the "deterministic
        // pick" guarantee of merge_base() across runs.
        let o1 = oid("0000000000000000000000000000000000000001");
        let o2 = oid("0000000000000000000000000000000000000002");
        let q1 = QueueEntry { time: 100, oid: o1 };
        let q2 = QueueEntry { time: 100, oid: o2 };
        // o2 > o1 by bytes, so q2 > q1 (max-heap pops q2 first).
        assert!(q2 > q1);
        assert!(q1 < q2);
        // And the time field still dominates.
        let q3 = QueueEntry { time: 200, oid: o1 };
        assert!(q3 > q2);
    }
}
