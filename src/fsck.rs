//! `fsck` — verify the integrity of the object database.
//!
//! Walks every loose object (and, for `--full`, every pack entry) and:
//!
//! 1. **Hash check**: re-hash each object's framed bytes and compare to the
//!    filename oid. Mismatch → recorded in `bad_hashes`.
//! 2. **Structural check**: parse commits, trees, tags. Each referenced oid
//!    (commit→tree, commit→parent, tree→entries, tag→object) must exist in the
//!    odb. Missing references are reported as `BrokenLink` plus a `missing`
//!    entry.
//! 3. **Reachability**: walk from every ref tip; objects not reached are
//!    `dangling` (informational, not an error).
//! 4. **Ref tips**: each ref must resolve to a real object.
//!
//! Output format mirrors `git fsck`:
//! ```text
//! Checking object directories: 100% (256/256), done.
//! Checking objects: 100% (N/N), done.
//! dangling commit <oid>
//! missing tree <oid>
//! broken link from commit <oid> to tree <oid>
//! ```
//!
//! Exit code: 0 if no missing/broken/bad-hash issues; non-zero otherwise.

use std::collections::{BTreeSet, HashSet};
use std::path::PathBuf;

use thiserror::Error;

use crate::commit::{Commit, CommitError};
use crate::hash::{HashError, ObjectId};
use crate::object::{ObjectKind, RawObject};
use crate::odb::{ObjectStore, OdbError};
use crate::pack::PackError;
use crate::reachable::ReachableSet;
use crate::refs::{RefError, RefTarget};
use crate::repo::Repository;
use crate::tree::{Tree, TreeError};

/// Options controlling what fsck checks.
#[derive(Debug, Clone, Default)]
pub struct FsckOpts {
    /// Check pack objects in addition to loose. Default behaviour matches
    /// `git fsck --full`. We treat this as enabled by default so the report
    /// is meaningful even when most objects live in packs.
    pub full: bool,
    /// Only check structural connectivity; skip the (potentially expensive)
    /// per-object re-hash. Matches `git fsck --connectivity-only`.
    pub connectivity_only: bool,
    /// Report dangling (unreachable) objects in addition to errors.
    pub dangling: bool,
}

/// A reference from one object to another (commit→tree, tree→blob, etc.)
/// that points at an oid not present in the odb.
#[derive(Debug, Clone)]
pub struct BrokenLink {
    pub from: ObjectId,
    pub from_kind: ObjectKind,
    pub to: ObjectId,
    pub reason: String,
}

/// Summary of an fsck run.
#[derive(Debug, Clone, Default)]
pub struct FsckReport {
    /// References from one object to another that don't resolve.
    pub broken_links: Vec<BrokenLink>,
    /// Object oids referenced but absent from the odb. Each appears at most once.
    pub missing: Vec<ObjectId>,
    /// Objects present in the odb but unreachable from any ref. Informational.
    pub dangling: Vec<ObjectId>,
    /// Objects whose stored bytes don't hash to their filename. Real corruption.
    pub bad_hashes: Vec<ObjectId>,
    /// Total number of objects examined.
    pub object_count: u64,
}

impl FsckReport {
    /// True if fsck found anything that should make us exit non-zero.
    pub fn has_errors(&self) -> bool {
        !self.broken_links.is_empty() || !self.missing.is_empty() || !self.bad_hashes.is_empty()
    }
}

/// Run fsck against `repo`.
pub fn fsck(repo: &Repository, opts: &FsckOpts) -> Result<FsckReport, FsckError> {
    let mut report = FsckReport::default();
    let hash_kind = repo.hash_kind();

    // 1. Enumerate every object in the odb. We use the loose iter directly
    //    plus, for `--full`, every pack's iter. We can't ask ObjectDb for "all
    //    stores", but we can use a fresh LooseStore + walk packs from the
    //    gitdir.
    let mut all_oids: BTreeSet<ObjectId> = BTreeSet::new();

    let loose = crate::odb::LooseStore::new(repo.gitdir().join("objects"), hash_kind);
    for oid in loose.iter() {
        let oid = oid?;
        all_oids.insert(oid);
    }

    if opts.full || !opts.connectivity_only {
        // Walk every pack file under .git/objects/pack.
        let pack_dir = repo.gitdir().join("objects").join("pack");
        if pack_dir.is_dir() {
            let entries = std::fs::read_dir(&pack_dir).map_err(|e| FsckError::Io {
                path: pack_dir.clone(),
                source: e,
            })?;
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|s| s.to_str()) != Some("pack") {
                    continue;
                }
                match crate::pack::PackStore::open_pair(&path, hash_kind) {
                    Ok(store) => {
                        for oid in store.iter() {
                            let oid = oid?;
                            all_oids.insert(oid);
                        }
                    }
                    Err(e) => {
                        return Err(FsckError::Pack {
                            path: path.clone(),
                            source: e,
                        });
                    }
                }
            }
        }
    }

    report.object_count = all_oids.len() as u64;

    // 2. For each object: optionally re-hash; always parse for structural checks.
    //    Missing-link detection accumulates `missing` set as we go.
    let mut missing_set: HashSet<ObjectId> = HashSet::new();
    for oid in &all_oids {
        // Read the object via the full odb so packs are resolved transparently.
        let obj = match repo.odb().read(oid) {
            Ok(o) => o,
            Err(_) => {
                // Should be impossible given we enumerated from the odb, but
                // surface it as bad-hash style corruption rather than panicking.
                report.bad_hashes.push(*oid);
                continue;
            }
        };

        // 2a. Hash check (only for loose objects; pack entries are validated
        //    by their idx CRC + pack checksum, which we don't recompute here).
        if !opts.connectivity_only {
            // Only hash-check objects that exist as loose files — for pack
            // entries, the bytes we read are post-delta-resolution and won't
            // re-hash trivially. Re-resolving the chain is what `git fsck`
            // does, but for M16 we skip pack-hash verification and rely on
            // the pack/idx checksum (done at PackStore::open).
            let loose_path = loose_path_for(repo, oid);
            if loose_path.exists() {
                let computed = obj.oid(hash_kind);
                if computed != *oid {
                    report.bad_hashes.push(*oid);
                }
            }
        }

        // 2b. Structural check.
        match obj.kind {
            ObjectKind::Commit => match Commit::parse(&obj.data, hash_kind) {
                Ok(c) => {
                    check_link(
                        repo,
                        *oid,
                        ObjectKind::Commit,
                        c.tree,
                        "tree",
                        &mut report,
                        &mut missing_set,
                    );
                    for p in c.parents {
                        check_link(
                            repo,
                            *oid,
                            ObjectKind::Commit,
                            p,
                            "parent",
                            &mut report,
                            &mut missing_set,
                        );
                    }
                }
                Err(_e) => {
                    report.bad_hashes.push(*oid);
                }
            },
            ObjectKind::Tree => match Tree::parse(&obj.data, hash_kind) {
                Ok(t) => {
                    for entry in t.entries {
                        check_link(
                            repo,
                            *oid,
                            ObjectKind::Tree,
                            entry.oid,
                            "tree entry",
                            &mut report,
                            &mut missing_set,
                        );
                    }
                }
                Err(_e) => {
                    report.bad_hashes.push(*oid);
                }
            },
            ObjectKind::Tag => {
                if let Some(target) = parse_tag_target(&obj, hash_kind) {
                    check_link(
                        repo,
                        *oid,
                        ObjectKind::Tag,
                        target,
                        "object",
                        &mut report,
                        &mut missing_set,
                    );
                }
            }
            ObjectKind::Blob => {
                // Blobs are leaves; no structural references.
            }
        }
    }

    // 3. Ref-tip checks: each ref must resolve to a real object.
    for r in repo.refs().iter(None) {
        let r = match r {
            Ok(r) => r,
            Err(_) => continue,
        };
        let tip_oid = match r.target {
            RefTarget::Direct(o) => o,
            RefTarget::Symbolic(name) => match RefTarget::resolve(repo.refs(), &name) {
                Ok(Some((_, o))) => o,
                _ => continue, // dangling symbolic ref; not our scope
            },
        };
        if !all_oids.contains(&tip_oid) {
            // The ref points at an oid the odb doesn't have.
            if missing_set.insert(tip_oid) {
                report.missing.push(tip_oid);
            }
        }
    }

    // 4. Dangling: every object not reached from any ref/index is "dangling".
    if opts.dangling {
        // mark_all walks refs+index+HEAD; that matches `git fsck`'s notion of
        // "live" objects.
        let reachable = match ReachableSet::mark_all(repo) {
            Ok(r) => r.oids,
            Err(e) => return Err(FsckError::Reachable(e.to_string())),
        };
        for oid in &all_oids {
            if !reachable.contains(oid) {
                report.dangling.push(*oid);
            }
        }
    }

    // De-dup and sort for stable output.
    report.missing.sort();
    report.missing.dedup();
    report.bad_hashes.sort();
    report.bad_hashes.dedup();
    report.dangling.sort();

    Ok(report)
}

/// Append a broken-link record if `target` isn't in the odb.
fn check_link(
    repo: &Repository,
    from: ObjectId,
    from_kind: ObjectKind,
    target: ObjectId,
    reason: &str,
    report: &mut FsckReport,
    missing_set: &mut HashSet<ObjectId>,
) {
    let exists = repo.odb().contains(&target).unwrap_or(false);
    if !exists {
        report.broken_links.push(BrokenLink {
            from,
            from_kind,
            to: target,
            reason: reason.to_string(),
        });
        if missing_set.insert(target) {
            report.missing.push(target);
        }
    }
}

fn parse_tag_target(obj: &RawObject, hash_kind: crate::hash::HashKind) -> Option<ObjectId> {
    let body = std::str::from_utf8(&obj.data).ok()?;
    for line in body.lines() {
        if line.is_empty() {
            break;
        }
        if let Some(rest) = line.strip_prefix("object ") {
            return ObjectId::parse_hex(hash_kind, rest.trim()).ok();
        }
    }
    None
}

/// Construct the on-disk loose path for `oid` under `repo`'s gitdir.
fn loose_path_for(repo: &Repository, oid: &ObjectId) -> PathBuf {
    let hex = oid.to_string();
    let (dir, file) = hex.split_at(2);
    repo.gitdir().join("objects").join(dir).join(file)
}

#[derive(Error, Debug)]
pub enum FsckError {
    #[error("io error on {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("pack error at {path}: {source}")]
    Pack {
        path: PathBuf,
        #[source]
        source: PackError,
    },
    #[error(transparent)]
    Odb(#[from] OdbError),
    #[error(transparent)]
    Refs(#[from] RefError),
    #[error(transparent)]
    Tree(#[from] TreeError),
    #[error(transparent)]
    Commit(#[from] CommitError),
    #[error(transparent)]
    Hash(#[from] HashError),
    #[error("reachable walk failed: {0}")]
    Reachable(String),
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

    fn make_commit(dir: &Path, file: &str, content: &str, msg: &str) {
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
    }

    #[test]
    fn fsck_clean_repo_has_no_errors() {
        if !git_available() {
            eprintln!("skipping: no git");
            return;
        }
        let tmp = tempdir().unwrap();
        let dir = tmp.path();
        init_repo(dir);
        make_commit(dir, "a.txt", "alpha\n", "c1");
        make_commit(dir, "b.txt", "beta\n", "c2");

        let repo = Repository::discover(dir).unwrap();
        let report = fsck(
            &repo,
            &FsckOpts {
                full: true,
                connectivity_only: false,
                dangling: true,
            },
        )
        .unwrap();
        assert!(
            report.broken_links.is_empty(),
            "broken_links: {:?}",
            report.broken_links
        );
        assert!(report.missing.is_empty(), "missing: {:?}", report.missing);
        assert!(
            report.bad_hashes.is_empty(),
            "bad_hashes: {:?}",
            report.bad_hashes
        );
        // Dangling: every object is reachable from refs/heads/main.
        assert!(
            report.dangling.is_empty(),
            "dangling: {:?}",
            report.dangling
        );
        assert!(report.object_count > 0);
        assert!(!report.has_errors());
    }

    #[test]
    fn fsck_reports_dangling_commit() {
        if !git_available() {
            eprintln!("skipping: no git");
            return;
        }
        let tmp = tempdir().unwrap();
        let dir = tmp.path();
        init_repo(dir);
        make_commit(dir, "a.txt", "alpha\n", "c1");

        // Create an unreferenced commit by committing on a branch then
        // deleting the branch. git keeps the commit's objects loose but
        // unreachable.
        git(dir, &["checkout", "-q", "-b", "stray"]);
        make_commit(dir, "stray.txt", "stray\n", "stray commit");
        let stray_oid = String::from_utf8(git(dir, &["rev-parse", "HEAD"]).stdout)
            .unwrap()
            .trim()
            .to_string();
        git(dir, &["checkout", "-q", "main"]);
        git(dir, &["branch", "-D", "stray"]);

        let repo = Repository::discover(dir).unwrap();
        let report = fsck(
            &repo,
            &FsckOpts {
                full: true,
                connectivity_only: false,
                dangling: true,
            },
        )
        .unwrap();
        let stray = ObjectId::parse_hex(repo.hash_kind(), &stray_oid).unwrap();
        assert!(
            report.dangling.contains(&stray),
            "stray commit {stray_oid} should be reported as dangling; got {:?}",
            report.dangling
        );
        // Connectivity errors should still be zero.
        assert!(!report.has_errors());
    }

    #[test]
    fn fsck_detects_missing_tree() {
        if !git_available() {
            eprintln!("skipping: no git");
            return;
        }
        let tmp = tempdir().unwrap();
        let dir = tmp.path();
        init_repo(dir);
        make_commit(dir, "a.txt", "alpha\n", "c1");

        // Find the tree oid for HEAD and delete its loose object file.
        let tree_hex = String::from_utf8(git(dir, &["rev-parse", "HEAD^{tree}"]).stdout)
            .unwrap()
            .trim()
            .to_string();
        let path = dir
            .join(".git")
            .join("objects")
            .join(&tree_hex[..2])
            .join(&tree_hex[2..]);
        if path.exists() {
            std::fs::remove_file(&path).unwrap();
        } else {
            // The tree was packed; for this test, repack and explode first.
            // Simplest path: re-init objects as loose by running with -d. If
            // the file still isn't loose, skip — we can't reliably break it.
            eprintln!("skipping: tree was packed and we can't selectively delete a pack entry");
            return;
        }

        let repo = Repository::discover(dir).unwrap();
        let report = fsck(
            &repo,
            &FsckOpts {
                full: true,
                connectivity_only: false,
                dangling: false,
            },
        )
        .unwrap();
        let tree = ObjectId::parse_hex(repo.hash_kind(), &tree_hex).unwrap();
        assert!(
            report.missing.contains(&tree),
            "tree {tree_hex} should be reported as missing; got {:?}",
            report.missing
        );
        // We expect at least one broken-link record naming this tree.
        assert!(
            report.broken_links.iter().any(|b| b.to == tree),
            "broken-link record should mention {tree_hex}; got {:?}",
            report.broken_links
        );
        assert!(report.has_errors());
    }

    #[test]
    fn fsck_connectivity_only_skips_hash_check() {
        if !git_available() {
            eprintln!("skipping: no git");
            return;
        }
        let tmp = tempdir().unwrap();
        let dir = tmp.path();
        init_repo(dir);
        make_commit(dir, "a.txt", "alpha\n", "c1");

        let repo = Repository::discover(dir).unwrap();
        let report = fsck(
            &repo,
            &FsckOpts {
                full: true,
                connectivity_only: true,
                dangling: false,
            },
        )
        .unwrap();
        // No errors expected on a clean repo with connectivity-only mode either.
        assert!(!report.has_errors(), "{:?}", report);
    }
}
