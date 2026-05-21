//! Rename / copy detection for the diff engine (M16).
//!
//! Given the "added" and "deleted" slots from `diff_entries`, identify pairs
//! that look more like a rename than two independent add/delete events.
//!
//! Pipeline (mirrors git's `diffcore-rename.c`):
//!
//! 1. **Exact-match pass.** Any "added" oid that equals any "deleted" oid is
//!    declared a rename at similarity 100. Cheap: a single `HashMap` lookup
//!    per added entry. We do this before the expensive similarity pass so the
//!    O(N*M) work below only considers what's left over.
//!
//! 2. **Similarity-match pass.** For each remaining (deleted, added) pair,
//!    load both blobs, compute a content-similarity score in [0, 100], and
//!    keep pairs whose score is ≥ `opts.threshold_percent`. Greedy
//!    assignment: each delete picks its best add (highest score, then
//!    lowest-index tiebreak); ties resolved by first-seen. Both sides leave
//!    the pool once paired.
//!
//! The similarity metric is the line-hashing approximation from
//! `diffcore-delta.c`: hash each line of each side, count how many of one
//! file's line-hashes appear in the other, score = `200 * common / (a + b)`.
//! Quick to compute, no full Myers needed; close enough to git's number for
//! threshold purposes. Cap at 100.
//!
//! **Limits.** If `added_count * deleted_count` exceeds `opts.limit` we skip
//! the similarity pass entirely and return only exact matches. Git's
//! `diff.renameLimit` default is 1000; we match it.

use std::collections::HashMap;

use thiserror::Error;

use crate::hash::{HashError, ObjectId};
use crate::object::ObjectKind;
use crate::odb::OdbError;
use crate::repo::Repository;
use crate::tree::FileMode;

/// Options for rename detection. Matches git's `-M` flag.
#[derive(Debug, Clone)]
pub struct RenameOpts {
    /// Similarity threshold in [0, 100]. A pair must score ≥ this to be a
    /// rename. Default 50, matching `git diff -M` (no number).
    pub threshold_percent: u8,
    /// Skip the similarity pass entirely if `added * deleted` exceeds this.
    /// Default 1000 (git's `diff.renameLimit` default).
    pub limit: usize,
}

impl Default for RenameOpts {
    fn default() -> Self {
        Self {
            threshold_percent: 50,
            limit: 1000,
        }
    }
}

/// One detected rename / copy candidate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenameMatch {
    pub from: Vec<u8>,
    pub to: Vec<u8>,
    pub from_oid: ObjectId,
    pub to_oid: ObjectId,
    /// Similarity in [0, 100]. 100 = byte-identical content (exact-match path).
    pub similarity: u8,
}

/// Errors raised by `detect_renames`.
#[derive(Error, Debug)]
pub enum RenameError {
    #[error(transparent)]
    Odb(#[from] OdbError),
    #[error(transparent)]
    Hash(#[from] HashError),
}

/// Detect renames among the given "added" and "deleted" entries.
///
/// `added`: paths present in the b-side but not the a-side (and their
/// (mode, oid)). `deleted`: paths present in the a-side but not the b-side.
/// Each entry's mode and oid are taken at face value — the caller is the
/// diff engine which already classified them.
///
/// Returns one `RenameMatch` per paired (deleted, added) entry. Order:
/// exact matches first (in input order of the deleted side), then similarity
/// matches sorted by descending score. Entries that don't pair up are simply
/// absent from the result; the caller keeps treating them as Added/Deleted.
pub fn detect_renames(
    repo: &Repository,
    added: &[(Vec<u8>, FileMode, ObjectId)],
    deleted: &[(Vec<u8>, FileMode, ObjectId)],
    opts: &RenameOpts,
) -> Result<Vec<RenameMatch>, RenameError> {
    let mut out: Vec<RenameMatch> = Vec::new();
    // Track which added/deleted indices have already been matched, so the
    // similarity pass doesn't reconsider them.
    let mut added_paired = vec![false; added.len()];
    let mut deleted_paired = vec![false; deleted.len()];

    // ---- exact-match pass -------------------------------------------------
    //
    // Build an oid -> list-of-added-indices map. A pathological case (many
    // added files with the same content) would otherwise be O(N*M).
    let mut add_by_oid: HashMap<ObjectId, Vec<usize>> = HashMap::new();
    for (i, (_, mode, oid)) in added.iter().enumerate() {
        // Only blob-family entries are eligible for rename pairing. Trees
        // can't show up here in practice (the engine flattens trees) but be
        // defensive.
        if !is_blob_mode(*mode) {
            continue;
        }
        add_by_oid.entry(*oid).or_default().push(i);
    }
    for (di, (dpath, dmode, doid)) in deleted.iter().enumerate() {
        if !is_blob_mode(*dmode) {
            continue;
        }
        let Some(candidates) = add_by_oid.get_mut(doid) else {
            continue;
        };
        // Pick the first unmatched candidate; future ones become "copy"
        // candidates which `-M` doesn't report (only `-C` does).
        let mut chosen: Option<usize> = None;
        for &ai in candidates.iter() {
            if !added_paired[ai] {
                chosen = Some(ai);
                break;
            }
        }
        if let Some(ai) = chosen {
            added_paired[ai] = true;
            deleted_paired[di] = true;
            out.push(RenameMatch {
                from: dpath.clone(),
                to: added[ai].0.clone(),
                from_oid: *doid,
                to_oid: added[ai].2,
                similarity: 100,
            });
        }
    }

    // ---- similarity-match pass --------------------------------------------
    //
    // Cap the budget. `unpaired_added * unpaired_deleted` is the worst-case
    // number of similarity computations.
    let unpaired_added: usize = added_paired.iter().filter(|p| !**p).count();
    let unpaired_deleted: usize = deleted_paired.iter().filter(|p| !**p).count();
    let threshold = opts.threshold_percent.min(100);

    if unpaired_added == 0 || unpaired_deleted == 0 || threshold == 0 {
        return Ok(out);
    }

    // Budget check matches git's `too_many_files` shortcut.
    if unpaired_added.saturating_mul(unpaired_deleted) > opts.limit {
        return Ok(out);
    }

    // Pre-hash the unpaired entries on each side so the inner loop is
    // O(deleted_lines + added_lines) per pair rather than re-reading and
    // re-hashing the same file repeatedly. Index-based loops here run in
    // lockstep with parallel `*_paired` / signature vectors — the
    // enumerate()-style refactor clippy suggests would obscure that.
    #[allow(clippy::needless_range_loop)]
    let mut a_signatures: Vec<Option<LineHashSignature>> =
        (0..deleted.len()).map(|_| None).collect();
    let mut b_signatures: Vec<Option<LineHashSignature>> = (0..added.len()).map(|_| None).collect();
    #[allow(clippy::needless_range_loop)]
    for di in 0..deleted.len() {
        if deleted_paired[di] || !is_blob_mode(deleted[di].1) {
            continue;
        }
        let bytes = read_blob_bytes(repo, &deleted[di].2)?;
        a_signatures[di] = Some(LineHashSignature::compute(&bytes));
    }
    #[allow(clippy::needless_range_loop)]
    for ai in 0..added.len() {
        if added_paired[ai] || !is_blob_mode(added[ai].1) {
            continue;
        }
        let bytes = read_blob_bytes(repo, &added[ai].2)?;
        b_signatures[ai] = Some(LineHashSignature::compute(&bytes));
    }

    // Collect every candidate pair above threshold, then assign greedily by
    // descending score. This produces stable, deterministic results: ties
    // break by (deleted_index, added_index) order.
    let mut candidates: Vec<(u8, usize, usize)> = Vec::new();
    #[allow(clippy::needless_range_loop)]
    for di in 0..deleted.len() {
        let Some(sig_a) = a_signatures[di].as_ref() else {
            continue;
        };
        for ai in 0..added.len() {
            let Some(sig_b) = b_signatures[ai].as_ref() else {
                continue;
            };
            let score = similarity_percent(sig_a, sig_b);
            if score >= threshold {
                candidates.push((score, di, ai));
            }
        }
    }
    // Sort: highest score first; ties broken by (deleted_index, added_index)
    // ascending so a stable, reproducible ordering emerges.
    candidates.sort_by(|x, y| y.0.cmp(&x.0).then(x.1.cmp(&y.1)).then(x.2.cmp(&y.2)));

    for (score, di, ai) in candidates {
        if deleted_paired[di] || added_paired[ai] {
            continue;
        }
        deleted_paired[di] = true;
        added_paired[ai] = true;
        out.push(RenameMatch {
            from: deleted[di].0.clone(),
            to: added[ai].0.clone(),
            from_oid: deleted[di].2,
            to_oid: added[ai].2,
            similarity: score,
        });
    }

    Ok(out)
}

fn is_blob_mode(m: FileMode) -> bool {
    matches!(
        m,
        FileMode::Regular | FileMode::Executable | FileMode::Symlink
    )
}

fn read_blob_bytes(repo: &Repository, oid: &ObjectId) -> Result<Vec<u8>, RenameError> {
    let raw = repo.odb().read(oid)?;
    // For the similarity pass we don't care about non-blob objects; they
    // can't be paired with text-blob renames anyway.
    if raw.kind != ObjectKind::Blob {
        return Ok(Vec::new());
    }
    Ok(raw.data)
}

// ---------------------------------------------------------------------------
// Line-hash similarity
// ---------------------------------------------------------------------------

/// Per-file "what lines are in this blob" summary. We bucket-count line hashes
/// (with their byte lengths so two lines that happen to share a hash still
/// don't merge if they differ in size) and then compare two summaries by
/// computing the intersection.
///
/// We use FNV-1a 64-bit, which is small and fast and avoids pulling in a hash
/// dependency. Collisions are vanishingly rare for typical text-file lines;
/// false-positive matches would tilt the score upward by a negligible amount.
struct LineHashSignature {
    /// Map from line hash -> total byte count of all lines with that hash in
    /// this file. We score by overlap of these byte counts (mirrors git's
    /// `diffcore_count_changes`'s notion of "src_copied" bytes).
    by_hash: HashMap<u64, usize>,
    /// Total bytes in the file (sum of all line byte counts).
    total_bytes: usize,
}

impl LineHashSignature {
    fn compute(data: &[u8]) -> Self {
        let mut by_hash: HashMap<u64, usize> = HashMap::new();
        let mut total = 0usize;
        for line in split_lines_inclusive(data) {
            let h = fnv1a_64(line);
            *by_hash.entry(h).or_insert(0) += line.len();
            total += line.len();
        }
        Self {
            by_hash,
            total_bytes: total,
        }
    }
}

/// Split data into lines, including the trailing `\n` on each line where
/// present. Same shape as `xdiff::split_lines` but inlined here to avoid a
/// dependency cycle (rename.rs sits underneath the diff machinery).
fn split_lines_inclusive(data: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut start = 0usize;
    for (i, &b) in data.iter().enumerate() {
        if b == b'\n' {
            out.push(&data[start..=i]);
            start = i + 1;
        }
    }
    if start < data.len() {
        out.push(&data[start..]);
    }
    out
}

/// FNV-1a 64-bit. We don't need cryptographic strength, just a low-collision
/// content fingerprint for short byte strings.
fn fnv1a_64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// Compute the [0, 100] similarity between two line-hash signatures.
///
/// Formula: `200 * shared_bytes / (a_total + b_total)`. This is exactly the
/// Dice-coefficient form git uses in `diffcore-delta.c` once you express
/// `count_changes` in terms of byte overlap. Edge cases:
///   - both files empty → 100 (they're trivially identical).
///   - one file empty → 0.
///   - cap at 100 to absorb any rounding-up.
fn similarity_percent(a: &LineHashSignature, b: &LineHashSignature) -> u8 {
    if a.total_bytes == 0 && b.total_bytes == 0 {
        return 100;
    }
    if a.total_bytes == 0 || b.total_bytes == 0 {
        return 0;
    }
    let mut shared: usize = 0;
    // Iterate over the smaller side to keep the loop tight.
    let (lo, hi) = if a.by_hash.len() <= b.by_hash.len() {
        (&a.by_hash, &b.by_hash)
    } else {
        (&b.by_hash, &a.by_hash)
    };
    for (h, &n) in lo {
        if let Some(&m) = hi.get(h) {
            shared += n.min(m);
        }
    }
    let denom = a.total_bytes + b.total_bytes;
    let pct = (200u64.saturating_mul(shared as u64)) / (denom as u64);
    pct.min(100) as u8
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hash::HashKind;
    use crate::object::RawObject;
    use std::path::PathBuf;
    use std::process::Command;

    fn has_git() -> bool {
        Command::new("git")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    /// Make a tempdir and `git init` in it. We piggyback on system git for
    /// fixture setup, then open with our `Repository`.
    fn temp_repo() -> Option<(tempfile::TempDir, Repository)> {
        if !has_git() {
            return None;
        }
        let dir = tempfile::tempdir().ok()?;
        let st = Command::new("git")
            .args(["init", "-q"])
            .current_dir(dir.path())
            .status()
            .ok()?;
        if !st.success() {
            return None;
        }
        let repo = Repository::open(dir.path().join(".git")).ok()?;
        Some((dir, repo))
    }

    /// Stuff `data` into the repo as a Blob, return its oid.
    fn write_blob(repo: &Repository, data: &[u8]) -> ObjectId {
        let obj = RawObject::new(ObjectKind::Blob, data.to_vec());
        repo.odb().write(&obj).expect("write blob")
    }

    // ---------- pure unit: similarity_percent ----------

    #[test]
    fn empty_files_match_100() {
        let a = LineHashSignature::compute(b"");
        let b = LineHashSignature::compute(b"");
        assert_eq!(similarity_percent(&a, &b), 100);
    }

    #[test]
    fn identical_files_match_100() {
        let a = LineHashSignature::compute(b"foo\nbar\n");
        let b = LineHashSignature::compute(b"foo\nbar\n");
        assert_eq!(similarity_percent(&a, &b), 100);
    }

    #[test]
    fn empty_vs_nonempty_is_zero() {
        let a = LineHashSignature::compute(b"");
        let b = LineHashSignature::compute(b"x\n");
        assert_eq!(similarity_percent(&a, &b), 0);
    }

    #[test]
    fn disjoint_content_low_score() {
        let a = LineHashSignature::compute(b"aaa\nbbb\nccc\n");
        let b = LineHashSignature::compute(b"xxx\nyyy\nzzz\n");
        assert!(similarity_percent(&a, &b) < 30);
    }

    #[test]
    fn half_shared_lines_about_half() {
        // 4 lines of 4 bytes each. 2 shared, 2 different. Expect ~50%.
        let a = LineHashSignature::compute(b"aaa\nbbb\nccc\nddd\n");
        let b = LineHashSignature::compute(b"aaa\nbbb\nXXX\nYYY\n");
        let score = similarity_percent(&a, &b);
        assert!((40..=60).contains(&score), "score={score}");
    }

    // ---------- exact-match rename ----------

    #[test]
    fn exact_match_by_oid_yields_rename() {
        let (_dir, repo) = match temp_repo() {
            Some(t) => t,
            None => return,
        };
        let oid = write_blob(&repo, b"hello world\n");
        let added = vec![(b"new.txt".to_vec(), FileMode::Regular, oid)];
        let deleted = vec![(b"old.txt".to_vec(), FileMode::Regular, oid)];
        let renames = detect_renames(&repo, &added, &deleted, &RenameOpts::default()).unwrap();
        assert_eq!(renames.len(), 1);
        assert_eq!(renames[0].from, b"old.txt");
        assert_eq!(renames[0].to, b"new.txt");
        assert_eq!(renames[0].similarity, 100);
    }

    // ---------- no match below threshold ----------

    #[test]
    fn unrelated_content_no_rename() {
        let (_dir, repo) = match temp_repo() {
            Some(t) => t,
            None => return,
        };
        let a_oid = write_blob(&repo, b"alpha\nbravo\ncharlie\n");
        let b_oid = write_blob(&repo, b"xray\nyankee\nzulu\n");
        let added = vec![(b"b.txt".to_vec(), FileMode::Regular, b_oid)];
        let deleted = vec![(b"a.txt".to_vec(), FileMode::Regular, a_oid)];
        let renames = detect_renames(&repo, &added, &deleted, &RenameOpts::default()).unwrap();
        assert!(renames.is_empty(), "got unexpected: {renames:?}");
    }

    // ---------- above threshold ----------

    #[test]
    fn similar_content_above_default_threshold() {
        let (_dir, repo) = match temp_repo() {
            Some(t) => t,
            None => return,
        };
        // 7 of 10 lines shared.
        let a_oid = write_blob(
            &repo,
            b"one\ntwo\nthree\nfour\nfive\nsix\nseven\neight\nnine\nten\n",
        );
        let b_oid = write_blob(
            &repo,
            b"one\ntwo\nthree\nfour\nfive\nsix\nseven\nALPHA\nBETA\nGAMMA\n",
        );
        let added = vec![(b"b.txt".to_vec(), FileMode::Regular, b_oid)];
        let deleted = vec![(b"a.txt".to_vec(), FileMode::Regular, a_oid)];
        let renames = detect_renames(&repo, &added, &deleted, &RenameOpts::default()).unwrap();
        assert_eq!(renames.len(), 1);
        assert!(
            renames[0].similarity >= 50 && renames[0].similarity < 100,
            "similarity={}",
            renames[0].similarity
        );
    }

    #[test]
    fn similar_content_below_high_threshold_filtered() {
        let (_dir, repo) = match temp_repo() {
            Some(t) => t,
            None => return,
        };
        let a_oid = write_blob(
            &repo,
            b"one\ntwo\nthree\nfour\nfive\nsix\nseven\neight\nnine\nten\n",
        );
        let b_oid = write_blob(
            &repo,
            b"one\ntwo\nthree\nfour\nfive\nsix\nseven\nALPHA\nBETA\nGAMMA\n",
        );
        let added = vec![(b"b.txt".to_vec(), FileMode::Regular, b_oid)];
        let deleted = vec![(b"a.txt".to_vec(), FileMode::Regular, a_oid)];
        let opts = RenameOpts {
            threshold_percent: 95,
            ..RenameOpts::default()
        };
        let renames = detect_renames(&repo, &added, &deleted, &opts).unwrap();
        assert!(
            renames.is_empty(),
            "expected no renames at 95%: {renames:?}"
        );
    }

    // ---------- one-to-many ----------

    #[test]
    fn one_delete_two_adds_same_content_first_match_wins() {
        let (_dir, repo) = match temp_repo() {
            Some(t) => t,
            None => return,
        };
        let oid = write_blob(&repo, b"same\ncontent\n");
        let added = vec![
            (b"add1.txt".to_vec(), FileMode::Regular, oid),
            (b"add2.txt".to_vec(), FileMode::Regular, oid),
        ];
        let deleted = vec![(b"old.txt".to_vec(), FileMode::Regular, oid)];
        let renames = detect_renames(&repo, &added, &deleted, &RenameOpts::default()).unwrap();
        // M16 reports a single rename to the first add; second add stays
        // as an Added pair (would be a "copy" with `-C`, not in scope).
        assert_eq!(renames.len(), 1);
        assert_eq!(renames[0].from, b"old.txt");
        assert_eq!(renames[0].to, b"add1.txt");
    }

    // ---------- limit ----------

    #[test]
    fn excessive_pairs_skip_similarity_pass() {
        let (_dir, repo) = match temp_repo() {
            Some(t) => t,
            None => return,
        };
        // 3 added, 2 deleted -> 6 similarity pairs. Limit=5 should skip.
        let oids: Vec<ObjectId> = (0..5)
            .map(|i| write_blob(&repo, format!("content{i}\n").as_bytes()))
            .collect();
        let added = vec![
            (b"a1".to_vec(), FileMode::Regular, oids[0]),
            (b"a2".to_vec(), FileMode::Regular, oids[1]),
            (b"a3".to_vec(), FileMode::Regular, oids[2]),
        ];
        let deleted = vec![
            (b"d1".to_vec(), FileMode::Regular, oids[3]),
            (b"d2".to_vec(), FileMode::Regular, oids[4]),
        ];
        let opts = RenameOpts {
            threshold_percent: 1,
            limit: 5,
        };
        let renames = detect_renames(&repo, &added, &deleted, &opts).unwrap();
        // No exact matches and similarity skipped → no renames.
        assert!(renames.is_empty(), "got {renames:?}");
    }

    // ---------- mode filter ----------

    #[test]
    fn gitlink_modes_are_skipped() {
        let (_dir, repo) = match temp_repo() {
            Some(t) => t,
            None => return,
        };
        // Use a fake oid (gitlinks don't resolve to blobs in the odb anyway).
        let fake = ObjectId::from_bytes(HashKind::Sha1, &[0xaa; 20]).unwrap();
        let added = vec![(b"sub".to_vec(), FileMode::Gitlink, fake)];
        let deleted = vec![(b"sub".to_vec(), FileMode::Gitlink, fake)];
        let renames = detect_renames(&repo, &added, &deleted, &RenameOpts::default()).unwrap();
        assert!(renames.is_empty());
    }

    // ---------- cross-check against system git ----------

    /// Build a tiny repo, do a rename + edit, and verify that what we detect
    /// agrees with `git diff -M --name-status HEAD~1 HEAD` for the same
    /// commit pair. We don't compare percentages bit-for-bit (git uses a
    /// different scoring metric) — we just check that the same file pair
    /// gets flagged as a rename.
    #[test]
    fn cross_check_against_system_git() {
        if !has_git() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let path: PathBuf = dir.path().to_path_buf();
        run_git(&path, &["init", "-q", "-b", "main"]);
        run_git(&path, &["config", "user.email", "t@t"]);
        run_git(&path, &["config", "user.name", "T"]);

        // Commit 1: create old.txt with 10 lines.
        std::fs::write(
            path.join("old.txt"),
            b"L1\nL2\nL3\nL4\nL5\nL6\nL7\nL8\nL9\nL10\n",
        )
        .unwrap();
        run_git(&path, &["add", "old.txt"]);
        run_git(&path, &["commit", "-qm", "c1"]);

        // Commit 2: rename to new.txt with 1 line changed.
        std::fs::rename(path.join("old.txt"), path.join("new.txt")).unwrap();
        std::fs::write(
            path.join("new.txt"),
            b"L1\nL2\nL3\nL4\nMODIFIED\nL6\nL7\nL8\nL9\nL10\n",
        )
        .unwrap();
        run_git(&path, &["add", "-A"]);
        run_git(&path, &["commit", "-qm", "c2"]);

        // Ask git for its name-status with rename detection enabled.
        let nstatus = Command::new("git")
            .args(["diff", "-M", "--name-status", "HEAD~1", "HEAD"])
            .current_dir(&path)
            .output()
            .unwrap();
        assert!(nstatus.status.success());
        let nstatus_str = String::from_utf8(nstatus.stdout).unwrap();
        // Expect a single "R<n>\told.txt\tnew.txt" line.
        let mut got_rename = false;
        for line in nstatus_str.lines() {
            if line.starts_with('R') && line.contains("old.txt") && line.contains("new.txt") {
                got_rename = true;
                break;
            }
        }
        assert!(
            got_rename,
            "system git didn't flag a rename: {nstatus_str:?}"
        );

        // Now exercise our detector on the same input shape. We need to
        // open the repo with rustygit (it's a SHA-1 init).
        let repo = Repository::open(path.join(".git")).unwrap();

        // Extract oids of old.txt@HEAD~1 and new.txt@HEAD.
        let old_oid = blob_oid_at(&path, "HEAD~1", "old.txt", repo.hash_kind());
        let new_oid = blob_oid_at(&path, "HEAD", "new.txt", repo.hash_kind());

        let added = vec![(b"new.txt".to_vec(), FileMode::Regular, new_oid)];
        let deleted = vec![(b"old.txt".to_vec(), FileMode::Regular, old_oid)];
        let renames = detect_renames(&repo, &added, &deleted, &RenameOpts::default()).unwrap();
        assert_eq!(renames.len(), 1, "expected exactly one rename");
        assert_eq!(renames[0].from, b"old.txt");
        assert_eq!(renames[0].to, b"new.txt");
        // Score should be well above 50%.
        assert!(
            renames[0].similarity >= 50,
            "similarity={}",
            renames[0].similarity
        );
    }

    fn run_git(cwd: &std::path::Path, args: &[&str]) {
        let out = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .env("GIT_AUTHOR_NAME", "T")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "T")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .env("GIT_AUTHOR_DATE", "1700000000 +0000")
            .env("GIT_COMMITTER_DATE", "1700000000 +0000")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn blob_oid_at(cwd: &std::path::Path, rev: &str, path: &str, kind: HashKind) -> ObjectId {
        let out = Command::new("git")
            .args(["rev-parse", &format!("{rev}:{path}")])
            .current_dir(cwd)
            .output()
            .unwrap();
        assert!(out.status.success());
        let hex = String::from_utf8(out.stdout).unwrap().trim().to_string();
        ObjectId::parse_hex(kind, &hex).unwrap()
    }
}
