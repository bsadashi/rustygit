//! Local push — `git push <path>` or `git push file://<path>`.
//!
//! Reads the destination as a bare-style repository (the path may be the
//! `.git/` directory directly, or a working tree whose `.git/` we use), and
//! for each refspec:
//!
//!   1. Resolves the local source ref to an oid.
//!   2. Reads the destination's current value of the destination ref.
//!   3. Verifies that the new oid is a descendant of the old (fast-forward)
//!      unless `--force` was passed or the refspec was prefixed with `+`.
//!   4. Collects the objects reachable from the new tips but not from the
//!      destination's current tips, and writes a pack of them into the
//!      destination's `objects/pack/` directory.
//!   5. Updates the destination's refs atomically via `RefStore::transaction`.
//!
//! M11 simplifications: no thin-pack assembly (we pack all new-side reachables
//! and let the destination's pack reader handle the union); no `--mirror`
//! semantics; no remote-tracking-ref update on the SOURCE side (the network
//! variant handles that — local pushes are usually between sibling worktrees
//! where there's no meaningful "tracking" side).

use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::hash::{HashKind, ObjectId};
use crate::object::ObjectKind;
use crate::pack;
use crate::reachable::{ReachableError, ReachableSet};
use crate::refs::{ExpectedOldValue, FullName, NewValue, RefError, ReflogMessage};
use crate::repo::{RepoError, Repository};

use super::{PushError, PushOpts, Refspec};

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

#[derive(thiserror::Error, Debug)]
pub enum LocalPushError {
    #[error(transparent)]
    Push(#[from] PushError),
    #[error(transparent)]
    Repo(#[from] RepoError),
    #[error(transparent)]
    Refs(#[from] RefError),
    #[error(transparent)]
    Reachable(#[from] ReachableError),
    #[error(transparent)]
    Odb(#[from] crate::odb::OdbError),
    #[error(transparent)]
    PackBuild(#[from] pack::PackBuildError),
    #[error("destination is not a repository: {0}")]
    NotARepo(PathBuf),
    #[error("hash algorithm mismatch: local is {local}, destination is {dst}")]
    HashMismatch { local: HashKind, dst: HashKind },
    #[error("io on {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

// ---------------------------------------------------------------------------
// Per-ref result
// ---------------------------------------------------------------------------

/// What happened to one ref in a push.
#[derive(Debug, Clone)]
pub enum RefOutcome {
    /// New ref created with `new` oid.
    Created { dst: String, new: ObjectId },
    /// Existing ref advanced from `old` to `new`.
    Updated {
        dst: String,
        old: ObjectId,
        new: ObjectId,
    },
    /// Existing ref forcibly replaced (non-fast-forward).
    Forced {
        dst: String,
        old: ObjectId,
        new: ObjectId,
    },
    /// Existing ref deleted.
    Deleted { dst: String, old: ObjectId },
    /// Push was a no-op for this ref (e.g. dst already at new).
    UpToDate { dst: String, oid: ObjectId },
}

/// Summary of one `push_local` invocation.
#[derive(Debug, Default)]
pub struct LocalPushReport {
    /// Per-ref outcomes, one entry per refspec.
    pub outcomes: Vec<RefOutcome>,
    /// Filesystem path of the destination, for the "To <dst>" header line.
    pub dst_display: String,
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Push `refspecs` from `src_repo` into the repository at `dst_gitdir`.
///
/// `dst_gitdir` should be a path that either points at a bare repo's gitdir
/// directly, or at a working tree whose `.git/` we'll use. `file://` URLs
/// should be stripped by the caller before calling here.
pub fn push_local(
    src_repo: &Repository,
    dst_gitdir: &Path,
    refspecs: &[Refspec],
    opts: &PushOpts,
) -> Result<LocalPushReport, LocalPushError> {
    let dst_gitdir = resolve_dst_gitdir(dst_gitdir)?;
    let dst_repo = Repository::open(dst_gitdir.clone())?;

    if dst_repo.hash_kind() != src_repo.hash_kind() {
        return Err(LocalPushError::HashMismatch {
            local: src_repo.hash_kind(),
            dst: dst_repo.hash_kind(),
        });
    }

    // ---- Plan each refspec --------------------------------------------------

    let mut plans: Vec<RefPlan> = Vec::with_capacity(refspecs.len());
    for rs in refspecs {
        let plan = plan_refspec(src_repo, &dst_repo, rs, opts)?;
        plans.push(plan);
    }

    // ---- Collect new objects across all non-delete plans --------------------
    //
    // For each non-delete plan we want the set of objects reachable from the
    // new tip but NOT from the destination's old tip. We compute the union of
    // such sets across all plans and write a single pack of the result.

    let new_oids: Vec<ObjectId> = plans
        .iter()
        .filter_map(|p| match &p.action {
            PlanAction::Create { new } => Some(*new),
            PlanAction::Update { new, .. } => Some(*new),
            PlanAction::Force { new, .. } => Some(*new),
            _ => None,
        })
        .collect();
    let old_oids: Vec<ObjectId> = plans
        .iter()
        .filter_map(|p| match &p.action {
            PlanAction::Update { old, .. } => Some(*old),
            PlanAction::Force { old, .. } => Some(*old),
            _ => None,
        })
        .collect();

    if !new_oids.is_empty() {
        let need = compute_objects_to_send(src_repo, &new_oids, &old_oids, &dst_repo)?;
        if !need.is_empty() {
            write_objects_to_dst(src_repo, &dst_repo, &need)?;
        }
    }

    // ---- Apply ref updates atomically through a single transaction ----------

    apply_ref_updates(&dst_repo, &plans)?;

    // ---- Build report -------------------------------------------------------

    let outcomes = plans.into_iter().map(|p| p.outcome).collect();
    Ok(LocalPushReport {
        outcomes,
        dst_display: dst_gitdir.display().to_string(),
    })
}

// ---------------------------------------------------------------------------
// Destination resolution
// ---------------------------------------------------------------------------

/// Accept either a working-tree path (use its `.git/`) or a gitdir directly.
fn resolve_dst_gitdir(p: &Path) -> Result<PathBuf, LocalPushError> {
    // Strip `file://` if it slipped through.
    let s = p.to_string_lossy();
    let stripped: PathBuf = if let Some(rest) = s.strip_prefix("file://") {
        PathBuf::from(rest)
    } else {
        p.to_path_buf()
    };

    // Best-effort canonicalize; if the path doesn't exist that's a clear
    // error, but we want to surface it via the "not a repo" message rather
    // than a generic IO failure for a missing dir.
    let canonical = match stripped.canonicalize() {
        Ok(c) => c,
        Err(_) => return Err(LocalPushError::NotARepo(stripped)),
    };

    // Case 1: <p>/.git exists.
    let dot_git = canonical.join(".git");
    if dot_git.is_dir() {
        return Ok(dot_git);
    }
    // Case 2: <p> itself looks like a gitdir.
    if canonical.join("HEAD").is_file() && canonical.join("objects").is_dir() {
        return Ok(canonical);
    }
    Err(LocalPushError::NotARepo(canonical))
}

// ---------------------------------------------------------------------------
// Planning
// ---------------------------------------------------------------------------

#[derive(Debug)]
enum PlanAction {
    Create { new: ObjectId },
    Update { old: ObjectId, new: ObjectId },
    Force { old: ObjectId, new: ObjectId },
    Delete { old: ObjectId },
    UpToDate,
}

#[derive(Debug)]
struct RefPlan {
    dst_name: FullName,
    action: PlanAction,
    outcome: RefOutcome,
}

fn plan_refspec(
    src_repo: &Repository,
    dst_repo: &Repository,
    rs: &Refspec,
    opts: &PushOpts,
) -> Result<RefPlan, LocalPushError> {
    let dst_full = FullName::new(rs.dst.clone()).map_err(PushError::from)?;

    if rs.is_delete() {
        // Delete on the remote.
        let old = read_ref_direct(dst_repo, &dst_full)?;
        match old {
            Some(old) => Ok(RefPlan {
                dst_name: dst_full,
                action: PlanAction::Delete { old },
                outcome: RefOutcome::Deleted {
                    dst: rs.dst.clone(),
                    old,
                },
            }),
            None => Err(LocalPushError::Push(PushError::DeleteMissing {
                dst: rs.dst.clone(),
            })),
        }
    } else {
        // Resolve src on local.
        let src_full = FullName::new(rs.src.clone()).map_err(PushError::from)?;
        let new = match read_ref_direct(src_repo, &src_full)? {
            Some(o) => o,
            None => {
                return Err(LocalPushError::Push(PushError::SourceMissing(
                    rs.src.clone(),
                )))
            }
        };

        let old = read_ref_direct(dst_repo, &dst_full)?;

        match old {
            None => Ok(RefPlan {
                dst_name: dst_full,
                action: PlanAction::Create { new },
                outcome: RefOutcome::Created {
                    dst: rs.dst.clone(),
                    new,
                },
            }),
            Some(old) if old == new => Ok(RefPlan {
                dst_name: dst_full,
                action: PlanAction::UpToDate,
                outcome: RefOutcome::UpToDate {
                    dst: rs.dst.clone(),
                    oid: new,
                },
            }),
            Some(old) => {
                let force = rs.force || opts.force;
                if !force && !is_ancestor(src_repo, old, new)? {
                    return Err(LocalPushError::Push(PushError::NonFastForward {
                        dst: rs.dst.clone(),
                        old: old.to_string(),
                        new: new.to_string(),
                    }));
                }
                let outcome = if force && !is_ancestor(src_repo, old, new)? {
                    RefOutcome::Forced {
                        dst: rs.dst.clone(),
                        old,
                        new,
                    }
                } else {
                    RefOutcome::Updated {
                        dst: rs.dst.clone(),
                        old,
                        new,
                    }
                };
                let action = if force && !is_ancestor(src_repo, old, new)? {
                    PlanAction::Force { old, new }
                } else {
                    PlanAction::Update { old, new }
                };
                Ok(RefPlan {
                    dst_name: dst_full,
                    action,
                    outcome,
                })
            }
        }
    }
}

/// Read a ref through symbolic chains, returning the final direct oid.
fn read_ref_direct(repo: &Repository, name: &FullName) -> Result<Option<ObjectId>, LocalPushError> {
    match crate::refs::RefTarget::resolve(repo.refs(), name)? {
        Some((_, oid)) => Ok(Some(oid)),
        None => Ok(None),
    }
}

/// Is `ancestor` reachable from `descendant`? Walks the commit ancestry of
/// `descendant` looking for `ancestor`. Returns true also when the two are
/// equal (a no-op update is a degenerate fast-forward).
fn is_ancestor(
    repo: &Repository,
    ancestor: ObjectId,
    descendant: ObjectId,
) -> Result<bool, LocalPushError> {
    if ancestor == descendant {
        return Ok(true);
    }
    // Plain commit-only walk: read each commit, queue its parents, stop when
    // we either find `ancestor` or exhaust the queue. We don't use
    // ReachableSet here because we'd be including trees+blobs needlessly.
    let mut seen: BTreeSet<ObjectId> = BTreeSet::new();
    let mut queue: Vec<ObjectId> = vec![descendant];
    let hash_kind = repo.hash_kind();
    while let Some(oid) = queue.pop() {
        if !seen.insert(oid) {
            continue;
        }
        if oid == ancestor {
            return Ok(true);
        }
        let obj = match repo.odb().read(&oid) {
            Ok(o) => o,
            Err(crate::odb::OdbError::NotFound(_)) => continue,
            Err(e) => return Err(LocalPushError::Odb(e)),
        };
        if obj.kind != ObjectKind::Commit {
            // Tag → peel to its target; anything else is a leaf for ancestry.
            if obj.kind == ObjectKind::Tag {
                if let Some(target) = parse_tag_object_oid(&obj.data, hash_kind) {
                    queue.push(target);
                }
            }
            continue;
        }
        let commit =
            crate::commit::Commit::parse(&obj.data, hash_kind).map_err(|e| LocalPushError::Io {
                path: repo.gitdir().to_path_buf(),
                source: io::Error::new(io::ErrorKind::InvalidData, format!("{e}")),
            })?;
        for parent in &commit.parents {
            queue.push(*parent);
        }
    }
    Ok(false)
}

/// Parse `object <hex>` from a tag body. Returns None for malformed input.
fn parse_tag_object_oid(body: &[u8], hash_kind: HashKind) -> Option<ObjectId> {
    let text = std::str::from_utf8(body).ok()?;
    for line in text.lines() {
        if line.is_empty() {
            return None;
        }
        if let Some(rest) = line.strip_prefix("object ") {
            return ObjectId::parse_hex(hash_kind, rest.trim()).ok();
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Object selection + transfer
// ---------------------------------------------------------------------------

/// Compute the set of objects to send: reachable from `news` minus reachable
/// from `olds` minus objects that already exist in `dst_repo`'s odb.
///
/// We use `ReachableSet::mark_from` for both directions. For correctness the
/// destination side is the union of every old-tip's walk AND any objects the
/// dst already happens to contain (which we check via `odb.contains`). M11
/// keeps this conservative — better to over-send by a small margin than to
/// risk a missing-base error on the receive side.
fn compute_objects_to_send(
    src_repo: &Repository,
    news: &[ObjectId],
    olds: &[ObjectId],
    dst_repo: &Repository,
) -> Result<Vec<ObjectId>, LocalPushError> {
    let news_set = ReachableSet::mark_from(src_repo, news)?;
    let olds_set = if olds.is_empty() {
        BTreeSet::new()
    } else {
        // The olds are oids that live in the destination's odb. Walk them in
        // the SOURCE if we have them too — if a parent commit isn't in the
        // source's odb (an unusual case for local push), we treat that
        // ancestor as "not present" and let the destination dedup at receive
        // time. Read errors short-circuit the walk and surface the underlying
        // OdbError.
        let owned_olds: Vec<ObjectId> = olds
            .iter()
            .copied()
            .filter(|o| src_repo.odb().contains(o).unwrap_or(false))
            .collect();
        if owned_olds.is_empty() {
            BTreeSet::new()
        } else {
            ReachableSet::mark_from(src_repo, &owned_olds)?.oids
        }
    };

    let mut out: Vec<ObjectId> = Vec::new();
    for oid in &news_set.oids {
        if olds_set.contains(oid) {
            continue;
        }
        // If the destination already has it (e.g. shared ancestor not on a
        // ref we asked about), skip it.
        if dst_repo.odb().contains(oid).unwrap_or(false) {
            continue;
        }
        out.push(*oid);
    }
    Ok(out)
}

/// Write `oids` as a single pack under `<dst_gitdir>/objects/pack/`.
fn write_objects_to_dst(
    src_repo: &Repository,
    dst_repo: &Repository,
    oids: &[ObjectId],
) -> Result<(), LocalPushError> {
    let pack_dir = dst_repo.gitdir().join("objects").join("pack");
    fs::create_dir_all(&pack_dir).map_err(|e| LocalPushError::Io {
        path: pack_dir.clone(),
        source: e,
    })?;
    pack::build::write_pack(oids, src_repo.odb(), &pack_dir, src_repo.hash_kind())?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Ref updates
// ---------------------------------------------------------------------------

fn apply_ref_updates(dst_repo: &Repository, plans: &[RefPlan]) -> Result<(), LocalPushError> {
    let mut tx = dst_repo.refs().transaction();
    for plan in plans {
        match &plan.action {
            PlanAction::UpToDate => continue,
            PlanAction::Create { new } => {
                tx.update(
                    &plan.dst_name,
                    ExpectedOldValue::Missing,
                    NewValue::Direct(*new),
                    ReflogMessage::from(format!("push: create {new}")),
                )?;
            }
            PlanAction::Update { old, new } => {
                tx.update(
                    &plan.dst_name,
                    ExpectedOldValue::Direct(*old),
                    NewValue::Direct(*new),
                    ReflogMessage::from(format!("push: update {old}..{new}")),
                )?;
            }
            PlanAction::Force { old, new } => {
                tx.update(
                    &plan.dst_name,
                    ExpectedOldValue::Direct(*old),
                    NewValue::Direct(*new),
                    ReflogMessage::from(format!("push: forced-update {old}..{new}")),
                )?;
            }
            PlanAction::Delete { old } => {
                tx.delete(&plan.dst_name, ExpectedOldValue::Direct(*old))?;
            }
        }
    }
    tx.commit()?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::push::parse_refspec;
    use std::process::Command;
    use tempfile::TempDir;

    fn has_system_git() -> bool {
        Command::new("git")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    fn run_git(cwd: &Path, args: &[&str]) {
        let out = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .expect("spawn git");
        assert!(
            out.status.success(),
            "git {args:?} in {} failed: {}",
            cwd.display(),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn run_git_env(cwd: &Path, args: &[&str], envs: &[(&str, &str)]) {
        let mut cmd = Command::new("git");
        cmd.args(args).current_dir(cwd);
        for (k, v) in envs {
            cmd.env(k, v);
        }
        let out = cmd.output().expect("spawn git");
        assert!(
            out.status.success(),
            "git {args:?} in {} failed: {}",
            cwd.display(),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// Stand up a working source repo with a configurable number of commits
    /// on `main`. Returns the repo path.
    fn make_source(tmp: &Path, commits: u32) -> PathBuf {
        let src = tmp.join("src");
        fs::create_dir_all(&src).unwrap();
        run_git(&src, &["init", "-q", "-b", "main", "."]);
        run_git(&src, &["config", "user.email", "test@example.com"]);
        run_git(&src, &["config", "user.name", "Test User"]);
        for i in 0..commits {
            fs::write(src.join(format!("f{i}.txt")), format!("content {i}\n")).unwrap();
            run_git(&src, &["add", "."]);
            run_git_env(
                &src,
                &["commit", "-q", "-m", &format!("c{i}")],
                &[
                    ("GIT_AUTHOR_DATE", "1700000000 +0000"),
                    ("GIT_COMMITTER_DATE", "1700000000 +0000"),
                ],
            );
        }
        src
    }

    /// Create an empty bare repo destination.
    fn make_bare_dst(tmp: &Path, name: &str) -> PathBuf {
        let dst = tmp.join(name);
        run_git(tmp, &["init", "-q", "--bare", "-b", "main", name]);
        dst
    }

    fn rev_parse(dir: &Path, expr: &str) -> String {
        let out = Command::new("git")
            .args(["rev-parse", expr])
            .current_dir(dir)
            .output()
            .expect("spawn git rev-parse");
        assert!(out.status.success());
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    }

    // --- 1. First push (server's ref doesn't exist) ------------------------

    #[test]
    fn first_push_creates_ref_and_objects() {
        if !has_system_git() {
            eprintln!("skipping: no system git");
            return;
        }
        let tmp = TempDir::new().unwrap();
        let src = make_source(tmp.path(), 2);
        let dst = make_bare_dst(tmp.path(), "dst.git");

        let src_repo = Repository::discover(&src).unwrap();
        let rs = parse_refspec("main").unwrap();
        let report = push_local(
            &src_repo,
            &dst,
            std::slice::from_ref(&rs),
            &PushOpts::default(),
        )
        .unwrap();

        // One Created entry.
        assert_eq!(report.outcomes.len(), 1);
        match &report.outcomes[0] {
            RefOutcome::Created { dst, .. } => assert_eq!(dst, "refs/heads/main"),
            other => panic!("expected Created, got {other:?}"),
        }

        // dst ref matches src.
        let src_oid = rev_parse(&src, "refs/heads/main");
        let dst_oid = rev_parse(&dst, "refs/heads/main");
        assert_eq!(src_oid, dst_oid);

        // git fsck on the destination.
        let out = Command::new("git")
            .args(["fsck", "--full"])
            .current_dir(&dst)
            .output()
            .expect("spawn git fsck");
        assert!(
            out.status.success(),
            "fsck failed: stderr={}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    // --- 2. Fast-forward update -------------------------------------------

    #[test]
    fn fast_forward_update_advances_dst() {
        if !has_system_git() {
            eprintln!("skipping: no system git");
            return;
        }
        let tmp = TempDir::new().unwrap();
        let src = make_source(tmp.path(), 1);
        let dst = make_bare_dst(tmp.path(), "dst.git");

        // First push moves main to commit 1.
        let src_repo = Repository::discover(&src).unwrap();
        push_local(
            &src_repo,
            &dst,
            std::slice::from_ref(&parse_refspec("main").unwrap()),
            &PushOpts::default(),
        )
        .unwrap();
        let old_dst = rev_parse(&dst, "refs/heads/main");

        // Add another commit and push.
        fs::write(src.join("more.txt"), b"more\n").unwrap();
        run_git(&src, &["add", "more.txt"]);
        run_git_env(
            &src,
            &["commit", "-q", "-m", "c2"],
            &[
                ("GIT_AUTHOR_DATE", "1700000001 +0000"),
                ("GIT_COMMITTER_DATE", "1700000001 +0000"),
            ],
        );

        let src_repo = Repository::discover(&src).unwrap();
        let report = push_local(
            &src_repo,
            &dst,
            std::slice::from_ref(&parse_refspec("main").unwrap()),
            &PushOpts::default(),
        )
        .unwrap();

        assert_eq!(report.outcomes.len(), 1);
        match &report.outcomes[0] {
            RefOutcome::Updated { old, new, .. } => {
                assert_eq!(old.to_string(), old_dst);
                assert_eq!(new.to_string(), rev_parse(&src, "refs/heads/main"));
            }
            other => panic!("expected Updated, got {other:?}"),
        }
    }

    // --- 3. Refuses non-fast-forward --------------------------------------

    #[test]
    fn refuses_non_fast_forward() {
        if !has_system_git() {
            eprintln!("skipping: no system git");
            return;
        }
        let tmp = TempDir::new().unwrap();
        // Make src and dst share an initial commit.
        let src = make_source(tmp.path(), 1);
        let dst = make_bare_dst(tmp.path(), "dst.git");
        let src_repo = Repository::discover(&src).unwrap();
        push_local(
            &src_repo,
            &dst,
            std::slice::from_ref(&parse_refspec("main").unwrap()),
            &PushOpts::default(),
        )
        .unwrap();

        // Now advance dst by cloning to a temporary worktree, making a new
        // commit there, and pushing it back to the bare dst — so the bare
        // dst has a commit src doesn't know about.
        let other = tmp.path().join("other");
        run_git(
            tmp.path(),
            &[
                "clone",
                "-q",
                dst.to_str().unwrap(),
                other.to_str().unwrap(),
            ],
        );
        run_git(&other, &["config", "user.email", "o@example.com"]);
        run_git(&other, &["config", "user.name", "Other"]);
        fs::write(other.join("other.txt"), b"divergent\n").unwrap();
        run_git(&other, &["add", "."]);
        run_git_env(
            &other,
            &["commit", "-q", "-m", "divergent"],
            &[
                ("GIT_AUTHOR_DATE", "1700000010 +0000"),
                ("GIT_COMMITTER_DATE", "1700000010 +0000"),
            ],
        );
        run_git(&other, &["push", "-q", "origin", "main"]);

        // Now make a different commit in src.
        fs::write(src.join("local.txt"), b"local\n").unwrap();
        run_git(&src, &["add", "."]);
        run_git_env(
            &src,
            &["commit", "-q", "-m", "local"],
            &[
                ("GIT_AUTHOR_DATE", "1700000020 +0000"),
                ("GIT_COMMITTER_DATE", "1700000020 +0000"),
            ],
        );

        // Push without --force should fail with NonFastForward.
        let src_repo = Repository::discover(&src).unwrap();
        let err = push_local(
            &src_repo,
            &dst,
            std::slice::from_ref(&parse_refspec("main").unwrap()),
            &PushOpts::default(),
        )
        .unwrap_err();
        match err {
            LocalPushError::Push(PushError::NonFastForward { .. }) => {}
            other => panic!("expected NonFastForward, got {other:?}"),
        }
    }

    // --- 4. Force push ----------------------------------------------------

    #[test]
    fn force_push_overrides_non_fast_forward() {
        if !has_system_git() {
            eprintln!("skipping: no system git");
            return;
        }
        let tmp = TempDir::new().unwrap();
        let src = make_source(tmp.path(), 1);
        let dst = make_bare_dst(tmp.path(), "dst.git");
        let src_repo = Repository::discover(&src).unwrap();
        push_local(
            &src_repo,
            &dst,
            std::slice::from_ref(&parse_refspec("main").unwrap()),
            &PushOpts::default(),
        )
        .unwrap();

        // Advance dst via a clone (same setup as the non-ff test).
        let other = tmp.path().join("other");
        run_git(
            tmp.path(),
            &[
                "clone",
                "-q",
                dst.to_str().unwrap(),
                other.to_str().unwrap(),
            ],
        );
        run_git(&other, &["config", "user.email", "o@example.com"]);
        run_git(&other, &["config", "user.name", "Other"]);
        fs::write(other.join("other.txt"), b"divergent\n").unwrap();
        run_git(&other, &["add", "."]);
        run_git_env(
            &other,
            &["commit", "-q", "-m", "divergent"],
            &[
                ("GIT_AUTHOR_DATE", "1700000010 +0000"),
                ("GIT_COMMITTER_DATE", "1700000010 +0000"),
            ],
        );
        run_git(&other, &["push", "-q", "origin", "main"]);

        // Different commit in src.
        fs::write(src.join("local.txt"), b"local\n").unwrap();
        run_git(&src, &["add", "."]);
        run_git_env(
            &src,
            &["commit", "-q", "-m", "local"],
            &[
                ("GIT_AUTHOR_DATE", "1700000020 +0000"),
                ("GIT_COMMITTER_DATE", "1700000020 +0000"),
            ],
        );

        let src_repo = Repository::discover(&src).unwrap();
        let new_local = rev_parse(&src, "refs/heads/main");

        // Force push via the +<src>:<dst> refspec form succeeds.
        let report = push_local(
            &src_repo,
            &dst,
            std::slice::from_ref(&parse_refspec("+main:main").unwrap()),
            &PushOpts::default(),
        )
        .unwrap();

        match &report.outcomes[0] {
            RefOutcome::Forced { new, .. } => assert_eq!(new.to_string(), new_local),
            other => panic!("expected Forced, got {other:?}"),
        }
        // Also via opts.force.
        // (Don't re-run — the dst is already up-to-date and the next push
        // would be a no-op.)
        assert_eq!(rev_parse(&dst, "refs/heads/main"), new_local);
    }

    // --- 5. Delete ref ----------------------------------------------------

    #[test]
    fn delete_removes_ref_from_dst() {
        if !has_system_git() {
            eprintln!("skipping: no system git");
            return;
        }
        let tmp = TempDir::new().unwrap();
        let src = make_source(tmp.path(), 1);
        let dst = make_bare_dst(tmp.path(), "dst.git");
        // Create a topic ref to delete.
        run_git(&src, &["branch", "topic"]);

        let src_repo = Repository::discover(&src).unwrap();
        push_local(
            &src_repo,
            &dst,
            &[
                parse_refspec("main").unwrap(),
                parse_refspec("topic").unwrap(),
            ],
            &PushOpts::default(),
        )
        .unwrap();
        // Sanity: topic exists.
        let _ = rev_parse(&dst, "refs/heads/topic");

        // Delete it.
        let src_repo = Repository::discover(&src).unwrap();
        let report = push_local(
            &src_repo,
            &dst,
            std::slice::from_ref(&parse_refspec(":refs/heads/topic").unwrap()),
            &PushOpts::default(),
        )
        .unwrap();
        match &report.outcomes[0] {
            RefOutcome::Deleted { dst: d, .. } => assert_eq!(d, "refs/heads/topic"),
            other => panic!("expected Deleted, got {other:?}"),
        }
        // And it's gone.
        let out = Command::new("git")
            .args(["rev-parse", "--verify", "refs/heads/topic"])
            .current_dir(&dst)
            .output()
            .unwrap();
        assert!(!out.status.success());
    }

    // --- 6. Multiple refs in one push -------------------------------------

    #[test]
    fn multiple_refs_in_one_push() {
        if !has_system_git() {
            eprintln!("skipping: no system git");
            return;
        }
        let tmp = TempDir::new().unwrap();
        let src = make_source(tmp.path(), 1);
        let dst = make_bare_dst(tmp.path(), "dst.git");
        // Make a second branch in src.
        run_git(&src, &["branch", "topic"]);
        fs::write(src.join("topic.txt"), b"topic\n").unwrap();
        run_git(&src, &["checkout", "-q", "topic"]);
        run_git(&src, &["add", "."]);
        run_git_env(
            &src,
            &["commit", "-q", "-m", "topic content"],
            &[
                ("GIT_AUTHOR_DATE", "1700000100 +0000"),
                ("GIT_COMMITTER_DATE", "1700000100 +0000"),
            ],
        );

        let src_repo = Repository::discover(&src).unwrap();
        let report = push_local(
            &src_repo,
            &dst,
            &[
                parse_refspec("main").unwrap(),
                parse_refspec("topic").unwrap(),
            ],
            &PushOpts::default(),
        )
        .unwrap();
        assert_eq!(report.outcomes.len(), 2);

        // Both refs are present.
        assert_eq!(
            rev_parse(&dst, "refs/heads/main"),
            rev_parse(&src, "refs/heads/main")
        );
        assert_eq!(
            rev_parse(&dst, "refs/heads/topic"),
            rev_parse(&src, "refs/heads/topic")
        );
        // fsck passes.
        let out = Command::new("git")
            .args(["fsck", "--full"])
            .current_dir(&dst)
            .output()
            .expect("spawn git fsck");
        assert!(
            out.status.success(),
            "fsck failed: stderr={}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}
