//! Bisect — binary search through commit history to find a regression.
//!
//! ## Algorithm
//!
//! `bisect` maintains three commit sets:
//! - **good**: commits known not to have the bug.
//! - **bad**: a single commit known to have the bug.
//! - **candidates**: commits reachable from `bad` but not from any `good`.
//!
//! Each step picks the candidate that best halves the remaining search space.
//! Following `git`'s `do_find_bisection`/`best_bisection` in `bisect.c`:
//!
//! 1. Mark `good` commits and their ancestors as UNINTERESTING (excluded).
//! 2. Walk from `bad` over the interesting graph to collect the candidate list.
//! 3. For each candidate C, compute `weight(C)` = the number of interesting
//!    commits reachable from C (including C itself).
//! 4. The candidate with the largest min(weight, total - weight) — the one
//!    closest to halfway — is the next commit to test.
//!
//! ## State
//!
//! Bisect is a multi-invocation state machine. We persist between commands:
//! - `.git/BISECT_START`     — branch name (or detached oid) at session start
//! - `.git/BISECT_TERMS`     — `bad`/`good` (or future custom terms)
//! - `.git/BISECT_LOG`       — human-readable log appended on each step
//! - `.git/BISECT_EXPECTED_REV` — the oid last checked out for testing
//! - `.git/refs/bisect/bad`             — current bad oid (one)
//! - `.git/refs/bisect/good-<full-oid>` — one ref per good commit
//!
//! `BISECT_NAMES` (pathspec restriction) is intentionally unsupported.

use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::commit::{Commit, CommitError};
use crate::hash::{HashError, ObjectId};
use crate::object::ObjectKind;
use crate::odb::OdbError;
use crate::refs::{FullName, RefError, RefTarget};
use crate::repo::Repository;

/// In-memory bisect state, parsed from `.git/BISECT_*` files + `refs/bisect/*`.
#[derive(Debug, Clone)]
pub struct State {
    /// The current bad commit (the tip of the suspect range). `None` only when
    /// a session has been started but `bisect bad` hasn't run yet.
    pub bad: Option<ObjectId>,
    /// Every commit explicitly marked good. May be empty before the first
    /// `bisect good` runs.
    pub good: Vec<ObjectId>,
    /// Branch name HEAD pointed to at session start (or `None` for detached).
    /// Used by `bisect reset` to restore HEAD.
    pub start_branch: Option<FullName>,
    /// HEAD's resolved oid at session start. Fallback for `bisect reset` when
    /// `start_branch` is `None` (i.e. HEAD was detached when the session began).
    pub start_oid: ObjectId,
    /// terms.bad / terms.good — defaults to `bad` / `good`. Custom terms
    /// (`new`/`old`) aren't fully supported but the load/save plumbing is here.
    pub term_bad: String,
    pub term_good: String,
}

/// The result of computing the next bisect step.
#[derive(Debug)]
pub enum BisectStep {
    /// More candidates remain; check out `commit` and have the user test it.
    /// `remaining` is the *total* number of suspect commits left (matching git
    /// log output: "X revisions left to test after this").
    Next { commit: ObjectId, remaining: usize },
    /// Converged: every candidate has been bisected; this is the first bad.
    Done { first_bad: ObjectId },
}

// ---------------------------------------------------------------------------
// State persistence
// ---------------------------------------------------------------------------

impl State {
    /// Load an in-progress bisect session, if any. Returns `Ok(None)` if no
    /// session is active (no `BISECT_START` file).
    pub fn load(repo: &Repository) -> Result<Option<Self>, BisectError> {
        let gitdir = repo.gitdir();
        let start_path = gitdir.join("BISECT_START");
        let start_content = match fs::read_to_string(&start_path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(io_err(&start_path, e)),
        };
        let (start_branch, start_oid) = parse_bisect_start(&start_content, repo)?;

        let (term_bad, term_good) = read_terms(gitdir)?;

        // Read the bad / good refs.
        let bad_name = FullName::new("refs/bisect/bad")
            .map_err(|e| BisectError::Malformed(format!("invalid ref name: {e}")))?;
        let bad = match repo.refs().read(&bad_name)? {
            Some(r) => match r.target {
                RefTarget::Direct(o) => Some(o),
                RefTarget::Symbolic(_) => None,
            },
            None => None,
        };

        // Iterate refs/bisect/ collecting the `good-<oid>` ones.
        let mut good = Vec::new();
        for r in repo.refs().iter(Some("refs/bisect/")) {
            let r = r?;
            if let Some(rest) = r.name.as_str().strip_prefix("refs/bisect/good-") {
                if let Ok(oid) = ObjectId::parse_hex(repo.hash_kind(), rest) {
                    good.push(oid);
                }
            }
        }
        good.sort();
        good.dedup();

        Ok(Some(Self {
            bad,
            good,
            start_branch,
            start_oid,
            term_bad,
            term_good,
        }))
    }

    /// Persist `BISECT_START` + `BISECT_TERMS`. Refs for bad/good are managed
    /// separately by callers via the ref transaction layer.
    pub fn save(&self, repo: &Repository) -> Result<(), BisectError> {
        let gitdir = repo.gitdir();
        let start = match &self.start_branch {
            Some(name) => format!(
                "{}\n",
                name.as_str()
                    .strip_prefix("refs/heads/")
                    .unwrap_or(name.as_str())
            ),
            None => format!("{}\n", self.start_oid),
        };
        let start_path = gitdir.join("BISECT_START");
        fs::write(&start_path, start).map_err(|e| io_err(&start_path, e))?;
        let terms_path = gitdir.join("BISECT_TERMS");
        let terms = format!("{}\n{}\n", self.term_bad, self.term_good);
        fs::write(&terms_path, terms).map_err(|e| io_err(&terms_path, e))?;
        Ok(())
    }

    /// Tear down a bisect session: delete all `BISECT_*` files and every ref
    /// under `refs/bisect/`. Used by `bisect reset`.
    pub fn cleanup(repo: &Repository) -> Result<(), BisectError> {
        let gitdir = repo.gitdir();
        for name in [
            "BISECT_START",
            "BISECT_TERMS",
            "BISECT_LOG",
            "BISECT_EXPECTED_REV",
            "BISECT_NAMES",
            "BISECT_RUN",
            "BISECT_ANCESTORS_OK",
        ] {
            let p = gitdir.join(name);
            if p.exists() {
                fs::remove_file(&p).map_err(|e| io_err(&p, e))?;
            }
        }
        // Delete every ref under refs/bisect/ via the ref transaction layer.
        let names: Vec<FullName> = repo
            .refs()
            .iter(Some("refs/bisect/"))
            .filter_map(Result::ok)
            .map(|r| r.name)
            .collect();
        if !names.is_empty() {
            let mut tx = repo.refs().transaction();
            for n in names {
                tx.delete(&n, crate::refs::ExpectedOldValue::Any)?;
            }
            tx.commit()?;
        }
        // Some refs/bisect/ files might remain as empty directories — clean up.
        let bisect_dir = gitdir.join("refs").join("bisect");
        if bisect_dir.is_dir() {
            // Remove any leftover files (e.g. if the ref-store dropped any).
            if let Ok(entries) = fs::read_dir(&bisect_dir) {
                for e in entries.flatten() {
                    let _ = fs::remove_file(e.path());
                }
            }
            let _ = fs::remove_dir(&bisect_dir);
        }
        Ok(())
    }
}

fn parse_bisect_start(
    content: &str,
    repo: &Repository,
) -> Result<(Option<FullName>, ObjectId), BisectError> {
    let line = content.trim();
    if line.is_empty() {
        return Err(BisectError::Malformed("BISECT_START is empty".into()));
    }
    // If it parses as a full hex oid, treat as detached. Otherwise it's a
    // branch short name; resolve refs/heads/<name> to get the tip oid.
    if line.len() == repo.hash_kind().hex_len() {
        if let Ok(oid) = ObjectId::parse_hex(repo.hash_kind(), line) {
            return Ok((None, oid));
        }
    }
    let full = FullName::new(format!("refs/heads/{line}"))
        .map_err(|e| BisectError::Malformed(format!("invalid branch in BISECT_START: {e}")))?;
    let oid = match RefTarget::resolve(repo.refs(), &full)? {
        Some((_, o)) => o,
        None => ObjectId::null(repo.hash_kind()),
    };
    Ok((Some(full), oid))
}

fn read_terms(gitdir: &Path) -> Result<(String, String), BisectError> {
    let path = gitdir.join("BISECT_TERMS");
    let content = match fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(("bad".into(), "good".into()));
        }
        Err(e) => return Err(io_err(&path, e)),
    };
    let mut lines = content.lines();
    let bad = lines.next().unwrap_or("bad").trim().to_string();
    let good = lines.next().unwrap_or("good").trim().to_string();
    Ok((bad, good))
}

// ---------------------------------------------------------------------------
// Bisection algorithm
// ---------------------------------------------------------------------------

/// Compute the next bisect step given the current state. Reads the commit
/// graph from `repo.odb()`.
///
/// Preconditions: `state.bad` is `Some(_)` and `state.good` is non-empty. The
/// caller is expected to check this before invoking (matching git's "we need
/// both a good and a bad" requirement).
pub fn next_step(repo: &Repository, state: &State) -> Result<BisectStep, BisectError> {
    let bad = state
        .bad
        .ok_or_else(|| BisectError::Incomplete("no bad commit recorded".into()))?;
    if state.good.is_empty() {
        return Err(BisectError::Incomplete("no good commit recorded".into()));
    }

    // 1. Collect every ancestor of every good commit. These are UNINTERESTING.
    let mut uninteresting: HashSet<ObjectId> = HashSet::new();
    for g in &state.good {
        collect_ancestors(repo, *g, &mut uninteresting)?;
    }

    // 2. From bad, collect every commit reachable that is NOT in uninteresting.
    //    These are the candidates. Also record each commit's parent oids so we
    //    can walk forward to compute weights.
    let mut candidates: Vec<ObjectId> = Vec::new();
    let mut parents_of: HashMap<ObjectId, Vec<ObjectId>> = HashMap::new();
    {
        let mut seen: HashSet<ObjectId> = HashSet::new();
        let mut stack = vec![bad];
        while let Some(oid) = stack.pop() {
            if uninteresting.contains(&oid) {
                continue;
            }
            if !seen.insert(oid) {
                continue;
            }
            let commit = read_commit(repo, oid)?;
            // Only commits that *aren't* one of the goods themselves are
            // candidates. Goods are ancestors that mark the boundary.
            candidates.push(oid);
            // Record parents (filtered to interesting ones for weight calc).
            let filtered: Vec<ObjectId> = commit
                .parents
                .iter()
                .copied()
                .filter(|p| !uninteresting.contains(p))
                .collect();
            for p in &filtered {
                stack.push(*p);
            }
            parents_of.insert(oid, filtered);
        }
    }

    if candidates.is_empty() {
        // Empty candidate set means there's nothing between bad and any good
        // — the bad commit IS the first bad (typically: bad's parent is good).
        return Ok(BisectStep::Done { first_bad: bad });
    }

    // 3. If only `bad` itself remains as a candidate, we're done — but git
    //    actually keeps stepping until the candidate is bad's nearest
    //    ancestor. The convention is: when the only candidate is `bad`, that
    //    IS the first-bad.
    if candidates.len() == 1 && candidates[0] == bad {
        return Ok(BisectStep::Done { first_bad: bad });
    }

    // 4. Compute weights. weight(C) = how many candidates are reachable from C
    //    (including C). We do this with a reverse-topological iteration so we
    //    can add child weights into parents. For correctness without a true
    //    topo sort, we just BFS from each candidate counting; small total cost
    //    in a typical bisect (the candidate set has ~log2 of repo size after
    //    a few steps, and the test repos here are tiny). For larger repos
    //    a memoized post-order would matter; M16 keeps it simple.
    let total = candidates.len();
    let mut best_oid = candidates[0];
    let mut best_distance = -1i64;
    let mut weights: HashMap<ObjectId, usize> = HashMap::with_capacity(total);

    // Pre-compute weights via memoized DFS over `parents_of`. Weight = 1 + sum
    // of distinct ancestors via that subtree, but the standard bisect weight is
    // "commits reachable from C in the candidate set". We compute it directly
    // with a BFS per candidate. Cheap enough for our scale.
    for c in &candidates {
        let w = count_reachable_in_set(*c, &parents_of, &candidates_set(&candidates));
        weights.insert(*c, w);
    }

    for c in &candidates {
        let w = *weights.get(c).unwrap_or(&1);
        let other = total - w;
        let distance = w.min(other) as i64;
        let better = distance > best_distance
            || (distance == best_distance && c.as_bytes() < best_oid.as_bytes());
        if better {
            best_distance = distance;
            best_oid = *c;
        }
    }

    Ok(BisectStep::Next {
        commit: best_oid,
        remaining: total.saturating_sub(1),
    })
}

fn candidates_set(c: &[ObjectId]) -> BTreeSet<ObjectId> {
    c.iter().copied().collect()
}

/// Count commits reachable from `start` whose oid is in `members`, including
/// `start` itself if it's a member. Edges follow `parents_of`.
fn count_reachable_in_set(
    start: ObjectId,
    parents_of: &HashMap<ObjectId, Vec<ObjectId>>,
    members: &BTreeSet<ObjectId>,
) -> usize {
    let mut seen: HashSet<ObjectId> = HashSet::new();
    let mut q: VecDeque<ObjectId> = VecDeque::new();
    q.push_back(start);
    let mut count = 0usize;
    while let Some(o) = q.pop_front() {
        if !seen.insert(o) {
            continue;
        }
        if members.contains(&o) {
            count += 1;
        }
        if let Some(ps) = parents_of.get(&o) {
            for p in ps {
                q.push_back(*p);
            }
        }
    }
    count
}

/// Walk the ancestry of `start` and insert every commit into `out`.
fn collect_ancestors(
    repo: &Repository,
    start: ObjectId,
    out: &mut HashSet<ObjectId>,
) -> Result<(), BisectError> {
    let mut stack = vec![start];
    while let Some(oid) = stack.pop() {
        if !out.insert(oid) {
            continue;
        }
        // Tolerate missing commits silently — bisect against partial history
        // shouldn't crash. (`git bisect` falls back to an error message; we
        // can do that at the CLI layer.)
        let commit = match read_commit(repo, oid) {
            Ok(c) => c,
            Err(_) => continue,
        };
        for p in commit.parents {
            stack.push(p);
        }
    }
    Ok(())
}

fn read_commit(repo: &Repository, oid: ObjectId) -> Result<Commit, BisectError> {
    let obj = repo.odb().read(&oid)?;
    if obj.kind != ObjectKind::Commit {
        return Err(BisectError::Malformed(format!(
            "{oid} is a {}, not a commit",
            obj.kind
        )));
    }
    Ok(Commit::parse(&obj.data, repo.hash_kind())?)
}

// ---------------------------------------------------------------------------
// Errors and helpers
// ---------------------------------------------------------------------------

fn io_err(path: &Path, e: std::io::Error) -> BisectError {
    BisectError::Io {
        path: path.to_path_buf(),
        source: e,
    }
}

#[derive(Error, Debug)]
pub enum BisectError {
    #[error("io error on {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("malformed bisect state: {0}")]
    Malformed(String),
    #[error("bisect is incomplete: {0}")]
    Incomplete(String),
    #[error(transparent)]
    Refs(#[from] RefError),
    #[error(transparent)]
    Odb(#[from] OdbError),
    #[error(transparent)]
    Commit(#[from] CommitError),
    #[error(transparent)]
    Hash(#[from] HashError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hash::HashKind;
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

    fn make_commit(dir: &Path, file: &str, content: &str, msg: &str) -> String {
        std::fs::write(dir.join(file), content).unwrap();
        git(dir, &["add", file]);
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

    fn oid(hex: &str) -> ObjectId {
        ObjectId::parse_hex(HashKind::Sha1, hex).expect("hex oid")
    }

    /// Build a 10-commit linear history; return the oids in order (commit 0
    /// is the root, commit 9 is the latest).
    fn ten_commit_history(dir: &Path) -> Vec<String> {
        init_repo(dir);
        let mut oids = Vec::new();
        for i in 0..10 {
            let oid = make_commit(
                dir,
                "file.txt",
                &format!("content v{i}\n"),
                &format!("commit {i}"),
            );
            oids.push(oid);
        }
        oids
    }

    #[test]
    fn next_step_picks_middle_of_linear_history() {
        if !git_available() {
            eprintln!("skipping: no git");
            return;
        }
        let tmp = tempdir().unwrap();
        let dir = tmp.path();
        let history = ten_commit_history(dir);
        let repo = Repository::discover(dir).unwrap();

        // bad = commit 9, good = commit 0. Candidate set should be commits 1..=9.
        let state = State {
            bad: Some(oid(&history[9])),
            good: vec![oid(&history[0])],
            start_branch: None,
            start_oid: oid(&history[9]),
            term_bad: "bad".into(),
            term_good: "good".into(),
        };
        let step = next_step(&repo, &state).unwrap();
        match step {
            BisectStep::Next { commit, remaining } => {
                // The midpoint of [1, 2, .., 9] is commit 5 (4 above, 4 below)
                // by weight; git may pick commit 4 or 5 depending on tie-break.
                // We accept any of 4, 5 — both are equally good halvings.
                let hex = commit.to_string();
                let idx = history.iter().position(|h| h == &hex).unwrap();
                assert!(
                    (3..=5).contains(&idx),
                    "expected midpoint near commit 5, got {idx}"
                );
                assert_eq!(remaining, 8, "9 candidates, minus the one being tested");
            }
            BisectStep::Done { .. } => panic!("should not be done yet"),
        }
    }

    #[test]
    fn bisect_converges_on_introducer() {
        // Simulate full bisection: bad=9, good=0. We pretend commit 7 is the
        // first-bad introducer. Loop: test the midpoint; if it's <7, mark
        // good and continue; if it's >=7, mark bad and continue.
        if !git_available() {
            eprintln!("skipping: no git");
            return;
        }
        let tmp = tempdir().unwrap();
        let dir = tmp.path();
        let history = ten_commit_history(dir);
        let repo = Repository::discover(dir).unwrap();

        let mut state = State {
            bad: Some(oid(&history[9])),
            good: vec![oid(&history[0])],
            start_branch: None,
            start_oid: oid(&history[9]),
            term_bad: "bad".into(),
            term_good: "good".into(),
        };

        let buggy_index = 7usize;

        for _ in 0..20 {
            let step = next_step(&repo, &state).unwrap();
            match step {
                BisectStep::Done { first_bad } => {
                    let hex = first_bad.to_string();
                    let found = history.iter().position(|h| h == &hex).unwrap();
                    assert_eq!(found, buggy_index, "bisect should converge on commit 7");
                    return;
                }
                BisectStep::Next { commit, .. } => {
                    let hex = commit.to_string();
                    let mid_idx = history.iter().position(|h| h == &hex).unwrap();
                    if mid_idx >= buggy_index {
                        // The bug is here too → mark this commit (and the
                        // range above) as bad. New bad = midpoint.
                        state.bad = Some(commit);
                    } else {
                        // The bug isn't here → midpoint is good.
                        state.good.push(commit);
                    }
                }
            }
        }
        panic!("bisect did not converge within 20 iterations");
    }

    #[test]
    fn load_save_round_trip() {
        if !git_available() {
            eprintln!("skipping: no git");
            return;
        }
        let tmp = tempdir().unwrap();
        let dir = tmp.path();
        let history = ten_commit_history(dir);
        let repo = Repository::discover(dir).unwrap();

        let s = State {
            bad: None, // start hasn't recorded the bad ref yet at this level
            good: Vec::new(),
            start_branch: Some(FullName::new("refs/heads/main").unwrap()),
            start_oid: oid(&history[9]),
            term_bad: "bad".into(),
            term_good: "good".into(),
        };
        s.save(&repo).unwrap();
        assert!(repo.gitdir().join("BISECT_START").exists());
        assert!(repo.gitdir().join("BISECT_TERMS").exists());

        let loaded = State::load(&repo).unwrap().expect("state present");
        assert_eq!(
            loaded.start_branch.as_ref().map(|n| n.as_str().to_string()),
            Some("refs/heads/main".to_string())
        );
        assert_eq!(loaded.term_bad, "bad");
        assert_eq!(loaded.term_good, "good");
    }

    #[test]
    fn cleanup_removes_all_state() {
        if !git_available() {
            eprintln!("skipping: no git");
            return;
        }
        let tmp = tempdir().unwrap();
        let dir = tmp.path();
        let history = ten_commit_history(dir);
        let repo = Repository::discover(dir).unwrap();

        let s = State {
            bad: None,
            good: Vec::new(),
            start_branch: Some(FullName::new("refs/heads/main").unwrap()),
            start_oid: oid(&history[9]),
            term_bad: "bad".into(),
            term_good: "good".into(),
        };
        s.save(&repo).unwrap();
        // Also lay down a refs/bisect/bad to be sure cleanup nukes refs too.
        let bad_name = FullName::new("refs/bisect/bad").unwrap();
        let mut tx = repo.refs().transaction();
        tx.update(
            &bad_name,
            crate::refs::ExpectedOldValue::Any,
            crate::refs::NewValue::Direct(oid(&history[9])),
            crate::refs::ReflogMessage::none(),
        )
        .unwrap();
        tx.commit().unwrap();

        State::cleanup(&repo).unwrap();
        assert!(!repo.gitdir().join("BISECT_START").exists());
        assert!(!repo.gitdir().join("BISECT_TERMS").exists());
        assert!(repo.refs().read(&bad_name).unwrap().is_none());
    }
}
