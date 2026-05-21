//! NON_GOALS.md Batch B — read-only acceptance of git-only format
//! optimizations.
//!
//! The promise: a repo that system git has populated with reachability
//! bitmaps (`.bitmap`), commit-graph Bloom filters (BIDX/BDAT chunks), or
//! a multi-pack-index with attached bitmap (`.midx` + sibling `.bitmap`)
//! must still be openable and queryable by rustygit. Using these for
//! optimization is deferred; not breaking on them is mandatory.
//!
//! Each test sets up a real repo with system `git`, exercises the
//! optimization, then asks rustygit basic questions that exercise the
//! affected subsystem.

mod common;

use std::path::Path;
use std::process::Command;

use assert_cmd::Command as AssertCmd;
use common::{git, has_system_git};
use tempfile::TempDir;

fn rustygit(args: &[&str], cwd: &Path) -> std::process::Output {
    AssertCmd::cargo_bin("rustygit")
        .unwrap()
        .args(args)
        .current_dir(cwd)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .output()
        .unwrap()
}

/// Make a tiny repo, generate some commit history, return the resulting tip oid.
fn build_history(tmp: &Path, commits: usize) -> String {
    let g = git(&["init", "-q", "."], tmp);
    assert!(g.status.success(), "git init failed");
    let _ = git(&["config", "user.name", "T"], tmp);
    let _ = git(&["config", "user.email", "t@e"], tmp);
    for i in 0..commits {
        std::fs::write(tmp.join(format!("file_{i}.txt")), format!("content {i}\n")).unwrap();
        assert!(git(&["add", "."], tmp).status.success());
        let msg = format!("commit {i}");
        assert!(git(&["commit", "-q", "-m", &msg], tmp).status.success());
    }
    let out = git(&["rev-parse", "HEAD"], tmp);
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// `git repack -d -A --write-bitmap-index` produces a `.pack`/`.idx`/`.bitmap`
/// triple. rustygit's pack discovery is `.pack`-suffix only, so the sibling
/// `.bitmap` should be silently ignored — verify the repo still opens and
/// reads work end-to-end.
#[test]
fn pack_bitmap_does_not_break_reads() {
    if !has_system_git() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    let head_oid = build_history(tmp.path(), 5);

    // Produce a single pack with a sibling .bitmap.
    let repack = git(&["repack", "-d", "-a", "--write-bitmap-index"], tmp.path());
    if !repack.status.success() {
        // Some git builds disable bitmap writing for tiny repos; skip cleanly.
        let stderr = String::from_utf8_lossy(&repack.stderr);
        if stderr.contains("disabling bitmap writing") || stderr.contains("No reachable objects") {
            eprintln!("skipping: git declined to write bitmap");
            return;
        }
        panic!("git repack --write-bitmap-index failed: {stderr}");
    }

    // Confirm a .bitmap actually exists.
    let pack_dir = tmp.path().join(".git").join("objects").join("pack");
    let bitmap_present = std::fs::read_dir(&pack_dir)
        .unwrap()
        .flatten()
        .any(|e| e.path().extension().and_then(|s| s.to_str()) == Some("bitmap"));
    if !bitmap_present {
        eprintln!("skipping: no .bitmap produced");
        return;
    }

    // rustygit should: open the repo, resolve HEAD, read the tip commit.
    let r = rustygit(&["rev-parse", "HEAD"], tmp.path());
    assert!(
        r.status.success(),
        "rev-parse failed on bitmap-equipped repo: stderr={}",
        String::from_utf8_lossy(&r.stderr)
    );
    let got = String::from_utf8_lossy(&r.stdout).trim().to_string();
    assert_eq!(got, head_oid, "rev-parse oid mismatch");

    // cat-file -t should also work (exercises the pack reader).
    let t = rustygit(&["cat-file", "-t", &head_oid], tmp.path());
    assert!(t.status.success(), "cat-file -t failed");
    assert_eq!(
        String::from_utf8_lossy(&t.stdout).trim(),
        "commit",
        "cat-file -t output wrong"
    );

    // log should walk the chain (5 commits).
    let l = rustygit(&["log", "--oneline"], tmp.path());
    assert!(l.status.success(), "log failed");
    let lines = String::from_utf8_lossy(&l.stdout).lines().count();
    assert_eq!(lines, 5, "expected 5 commits, got {lines}");
}

/// `git commit-graph write --changed-paths` adds BIDX/BDAT chunks to
/// `.git/objects/info/commit-graph`. The plan documents we don't USE these
/// to filter pathspecs in `log`, but we must read the graph without
/// rejecting the unknown chunks.
#[test]
fn commit_graph_with_bloom_chunks_still_reads() {
    if !has_system_git() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    let head_oid = build_history(tmp.path(), 4);

    // Write a commit-graph with Bloom filters.
    let cg = git(
        &["commit-graph", "write", "--reachable", "--changed-paths"],
        tmp.path(),
    );
    if !cg.status.success() {
        let stderr = String::from_utf8_lossy(&cg.stderr);
        eprintln!("skipping: commit-graph --changed-paths unsupported: {stderr}");
        return;
    }
    let cg_path = tmp.path().join(".git/objects/info/commit-graph");
    if !cg_path.exists() {
        eprintln!("skipping: no commit-graph produced");
        return;
    }

    // The graph must contain BIDX/BDAT — confirm so we know we're actually
    // testing the bloom-filter case.
    let bytes = std::fs::read(&cg_path).unwrap();
    let has_bidx = bytes.windows(4).any(|w| w == b"BIDX");
    let has_bdat = bytes.windows(4).any(|w| w == b"BDAT");
    if !(has_bidx && has_bdat) {
        eprintln!("skipping: commit-graph lacks BIDX/BDAT chunks");
        return;
    }

    // rustygit should still read the graph and walk.
    let l = rustygit(&["log", "--oneline"], tmp.path());
    assert!(
        l.status.success(),
        "log failed on bloom-equipped commit-graph: stderr={}",
        String::from_utf8_lossy(&l.stderr)
    );
    let lines = String::from_utf8_lossy(&l.stdout).lines().count();
    assert_eq!(lines, 4);

    // And confirm rev-parse HEAD still agrees.
    let r = rustygit(&["rev-parse", "HEAD"], tmp.path());
    assert!(r.status.success());
    assert_eq!(
        String::from_utf8_lossy(&r.stdout).trim(),
        head_oid,
        "rev-parse oid mismatch on bloom-equipped repo"
    );
}

/// A multi-pack-index with an attached bitmap is created by
/// `git multi-pack-index write --bitmap`. Our midx reader skips unknown
/// chunks (BTMP among them) and the `.bitmap` file is invisible to the
/// pack discovery. Verify reads still work.
#[test]
fn multi_pack_bitmap_does_not_break_reads() {
    if !has_system_git() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    let head_oid = build_history(tmp.path(), 4);

    // Make two packs so the midx is non-trivial.
    let _ = git(&["repack", "-d", "-a"], tmp.path());
    std::fs::write(tmp.path().join("extra.txt"), b"extra\n").unwrap();
    let _ = git(&["add", "."], tmp.path());
    let _ = git(&["commit", "-q", "-m", "extra"], tmp.path());
    let _ = git(&["repack"], tmp.path());

    // Write the midx with a bitmap.
    let midx = git(&["multi-pack-index", "write", "--bitmap"], tmp.path());
    if !midx.status.success() {
        let stderr = String::from_utf8_lossy(&midx.stderr);
        eprintln!("skipping: multi-pack-index --bitmap unsupported: {stderr}");
        return;
    }
    let midx_path = tmp.path().join(".git/objects/pack/multi-pack-index");
    if !midx_path.exists() {
        eprintln!("skipping: no midx produced");
        return;
    }

    // rustygit should still resolve and walk.
    let r = rustygit(&["rev-parse", "HEAD"], tmp.path());
    assert!(
        r.status.success(),
        "rev-parse failed on midx-with-bitmap repo: stderr={}",
        String::from_utf8_lossy(&r.stderr)
    );
    // HEAD has moved past head_oid because we added "extra"; confirm at least
    // that rev-parse returns something valid.
    let got = String::from_utf8_lossy(&r.stdout).trim().to_string();
    assert!(!got.is_empty(), "rev-parse stdout empty");
    assert_ne!(got, head_oid, "expected HEAD to have advanced past initial");

    // log should walk 5 commits (4 + "extra").
    let l = rustygit(&["log", "--oneline"], tmp.path());
    assert!(l.status.success());
    let lines = String::from_utf8_lossy(&l.stdout).lines().count();
    assert_eq!(lines, 5);
}

/// `git pack-refs --all` plus `git commit-graph write --split` lets git
/// produce a CHAIN of commit-graph files under `.git/objects/info/commit-graphs/`
/// instead of a single `commit-graph`. We don't read split chains; verify
/// the repo still opens (rustygit just falls back to walking commits directly).
#[test]
fn split_commit_graph_chain_falls_back_gracefully() {
    if !has_system_git() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    let head_oid = build_history(tmp.path(), 3);

    // Force a split-chain layout.
    let cg = git(
        &["commit-graph", "write", "--reachable", "--split"],
        tmp.path(),
    );
    if !cg.status.success() {
        eprintln!("skipping: commit-graph --split unsupported");
        return;
    }

    // Check whether the chain layout was actually produced.
    let chain_path = tmp
        .path()
        .join(".git/objects/info/commit-graphs/commit-graph-chain");
    let single_path = tmp.path().join(".git/objects/info/commit-graph");
    if !chain_path.exists() && !single_path.exists() {
        eprintln!("skipping: no commit-graph layout produced");
        return;
    }

    // rustygit must still answer basic questions correctly.
    let r = rustygit(&["rev-parse", "HEAD"], tmp.path());
    assert!(r.status.success());
    assert_eq!(String::from_utf8_lossy(&r.stdout).trim(), head_oid);

    let l = rustygit(&["log", "--oneline"], tmp.path());
    assert!(l.status.success());
    assert_eq!(String::from_utf8_lossy(&l.stdout).lines().count(), 3);
}

/// Last but not least: a repo with ALL the optimizations applied at once
/// must still be readable. This combines `git repack --write-bitmap-index`,
/// `git commit-graph write --changed-paths`, and (optionally) a midx with
/// bitmap.
#[test]
fn repo_with_all_optimizations_applied_still_reads() {
    if !has_system_git() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    let head_oid = build_history(tmp.path(), 6);

    // Compose the optimization stack — each step is best-effort; if any
    // step fails on the host's git build, we still verify rustygit reads
    // what's there.
    let _ = git(&["repack", "-d", "-a", "--write-bitmap-index"], tmp.path());
    let _ = git(
        &["commit-graph", "write", "--reachable", "--changed-paths"],
        tmp.path(),
    );
    let _ = git(&["multi-pack-index", "write", "--bitmap"], tmp.path());

    let r = rustygit(&["rev-parse", "HEAD"], tmp.path());
    assert!(
        r.status.success(),
        "rev-parse failed on fully-optimized repo: stderr={}",
        String::from_utf8_lossy(&r.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&r.stdout).trim(), head_oid);

    let l = rustygit(&["log", "--oneline"], tmp.path());
    assert!(l.status.success());
    let lines = String::from_utf8_lossy(&l.stdout).lines().count();
    assert_eq!(lines, 6);

    // git fsck must agree the repo is intact (our reads can't have
    // disturbed anything).
    let fsck = Command::new("git")
        .args(["fsck", "--full"])
        .current_dir(tmp.path())
        .output()
        .unwrap();
    assert!(
        fsck.status.success(),
        "git fsck failed: {}",
        String::from_utf8_lossy(&fsck.stderr)
    );
}
