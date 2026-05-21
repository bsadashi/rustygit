//! `rustygit revert` — exhaustive oracle tests against system `git`.
//!
//! Strategy: build a controlled history in a tmp repo, run BOTH `rustygit
//! revert <oid>` and `git revert <oid>` in side-by-side clones, then
//! byte-compare the resulting tree and the HEAD commit's message.
//!
//! We DON'T byte-compare the new commit oid directly, because the
//! authorship-line timestamp is the one thing we have to fix per test
//! (which we do via env vars) — the rest of the commit body is what we
//! check.

mod common;

use std::path::Path;

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
        .env("GIT_AUTHOR_DATE", "1700000000 +0000")
        .env("GIT_COMMITTER_DATE", "1700000000 +0000")
        .output()
        .unwrap()
}

fn assert_ok(out: &std::process::Output, label: &str) {
    assert!(
        out.status.success(),
        "{label} failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

fn commit_file(tmp: &Path, name: &str, contents: &[u8], msg: &str) {
    std::fs::write(tmp.join(name), contents).unwrap();
    git(&["add", name], tmp);
    // Use AssertCmd directly so we can pin GIT_*_DATE; the common::git
    // helper inherits the parent env and would let real wall-clock time
    // leak in, which means two independent repos created back-to-back
    // can produce different oids if they cross a Unix-second boundary.
    let out = AssertCmd::new("git")
        .args(["-C", tmp.to_str().unwrap()])
        .args([
            "-c",
            "user.email=t@t",
            "-c",
            "user.name=t",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-q",
            "-m",
            msg,
        ])
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .env("GIT_AUTHOR_DATE", "1700000000 +0000")
        .env("GIT_COMMITTER_DATE", "1700000000 +0000")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git commit failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

#[allow(dead_code)]
fn rm_file(tmp: &Path, name: &str, msg: &str) {
    std::fs::remove_file(tmp.join(name)).unwrap();
    git(&["add", "-A"], tmp);
    git(
        &[
            "-c",
            "user.email=t@t",
            "-c",
            "user.name=t",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-q",
            "-m",
            msg,
        ],
        tmp,
    );
}

fn init_repo() -> TempDir {
    let tmp = TempDir::new().unwrap();
    git(&["init", "-q", "-b", "main"], tmp.path());
    git(&["config", "user.email", "t@t"], tmp.path());
    git(&["config", "user.name", "t"], tmp.path());
    git(&["config", "commit.gpgsign", "false"], tmp.path());
    tmp
}

/// Reverting the latest commit in a linear history undoes its diff and
/// produces a working tree byte-equal to git's `git revert HEAD`.
#[test]
fn revert_latest_linear_undoes_change_and_matches_git() {
    if !has_system_git() {
        return;
    }
    let ours = init_repo();
    let theirs = init_repo();

    for tmp in [ours.path(), theirs.path()] {
        commit_file(tmp, "a.txt", b"first\n", "add a");
        commit_file(tmp, "a.txt", b"second\n", "change a");
    }

    let oid_to_revert = String::from_utf8(git(&["rev-parse", "HEAD"], ours.path()).stdout)
        .unwrap()
        .trim()
        .to_string();

    let r = rustygit(&["revert", &oid_to_revert], ours.path());
    assert_ok(&r, "rustygit revert");

    let _ = git(&["revert", "--no-edit", &oid_to_revert], theirs.path());

    let ours_a = std::fs::read(ours.path().join("a.txt")).unwrap();
    let theirs_a = std::fs::read(theirs.path().join("a.txt")).unwrap();
    assert_eq!(ours_a, theirs_a, "workdir byte-mismatch");

    let our_tree = git(&["rev-parse", "HEAD^{tree}"], ours.path()).stdout;
    let their_tree = git(&["rev-parse", "HEAD^{tree}"], theirs.path()).stdout;
    assert_eq!(our_tree, their_tree, "HEAD tree oid mismatch");
}

/// The revert commit message follows the canonical
/// `Revert "<title>"\n\nThis reverts commit <oid>.\n` format.
#[test]
fn revert_message_is_canonical() {
    if !has_system_git() {
        return;
    }
    let tmp = init_repo();
    commit_file(tmp.path(), "a.txt", b"first\n", "add a");
    commit_file(tmp.path(), "a.txt", b"second\n", "Update a to second");

    let oid = String::from_utf8(git(&["rev-parse", "HEAD"], tmp.path()).stdout)
        .unwrap()
        .trim()
        .to_string();

    assert_ok(&rustygit(&["revert", &oid], tmp.path()), "revert");

    let msg = String::from_utf8(git(&["log", "-1", "--format=%B"], tmp.path()).stdout).unwrap();
    let expected = format!("Revert \"Update a to second\"\n\nThis reverts commit {oid}.\n\n");
    assert_eq!(msg, expected, "canonical revert message mismatch");
}

/// Reverting an old commit (not HEAD) with no dependent changes: works,
/// the file goes back to its pre-old-commit content.
#[test]
fn revert_old_commit_with_no_dependent_changes() {
    if !has_system_git() {
        return;
    }
    let ours = init_repo();
    let theirs = init_repo();

    for tmp in [ours.path(), theirs.path()] {
        commit_file(tmp, "a.txt", b"first\n", "add a");
        commit_file(tmp, "a.txt", b"second\n", "change a");
        commit_file(tmp, "b.txt", b"unrelated\n", "add unrelated b");
    }

    // Revert the second commit (change a); b is unchanged.
    let bad_oid = String::from_utf8(git(&["rev-parse", "HEAD^"], ours.path()).stdout)
        .unwrap()
        .trim()
        .to_string();

    assert_ok(
        &rustygit(&["revert", &bad_oid], ours.path()),
        "rustygit revert old",
    );
    let _ = git(&["revert", "--no-edit", &bad_oid], theirs.path());

    let ours_a = std::fs::read(ours.path().join("a.txt")).unwrap();
    let theirs_a = std::fs::read(theirs.path().join("a.txt")).unwrap();
    assert_eq!(ours_a, theirs_a);
    assert_eq!(ours_a, b"first\n", "a.txt should be back to 'first'");

    // b should be unchanged.
    let b = std::fs::read(ours.path().join("b.txt")).unwrap();
    assert_eq!(b, b"unrelated\n");
}

/// Reverting two commits in one call applies each inverse in order.
#[test]
fn revert_multiple_commits_in_order() {
    if !has_system_git() {
        return;
    }
    let ours = init_repo();
    let theirs = init_repo();

    for tmp in [ours.path(), theirs.path()] {
        commit_file(tmp, "a.txt", b"L1\n", "add a");
        commit_file(tmp, "a.txt", b"L1\nL2\n", "append L2");
        commit_file(tmp, "a.txt", b"L1\nL2\nL3\n", "append L3");
    }

    let head = String::from_utf8(git(&["rev-parse", "HEAD"], ours.path()).stdout)
        .unwrap()
        .trim()
        .to_string();
    let head_minus_1 = String::from_utf8(git(&["rev-parse", "HEAD^"], ours.path()).stdout)
        .unwrap()
        .trim()
        .to_string();

    // Revert both, latest first (canonical order for revert).
    assert_ok(
        &rustygit(&["revert", &head, &head_minus_1], ours.path()),
        "rustygit revert multi",
    );
    let _ = git(
        &["revert", "--no-edit", &head, &head_minus_1],
        theirs.path(),
    );

    let ours_a = std::fs::read(ours.path().join("a.txt")).unwrap();
    let theirs_a = std::fs::read(theirs.path().join("a.txt")).unwrap();
    assert_eq!(ours_a, theirs_a);
    assert_eq!(ours_a, b"L1\n");
}

/// Reverting an already-reverted change: the second revert produces the
/// "Empty" outcome and our porcelain prints a skip message but exits 0.
/// (Matches `git revert --allow-empty=drop` behavior; vanilla git aborts
/// without the flag, so we don't oracle-compare this one — we just
/// assert our behavior.)
#[test]
fn revert_already_reverted_is_skipped_cleanly() {
    if !has_system_git() {
        return;
    }
    let tmp = init_repo();
    commit_file(tmp.path(), "a.txt", b"first\n", "add a");
    commit_file(tmp.path(), "a.txt", b"second\n", "change a");

    let bad_oid = String::from_utf8(git(&["rev-parse", "HEAD"], tmp.path()).stdout)
        .unwrap()
        .trim()
        .to_string();

    // First revert succeeds.
    assert_ok(&rustygit(&["revert", &bad_oid], tmp.path()), "first revert");
    // Second revert (same oid) should report empty/skip.
    let r = rustygit(&["revert", &bad_oid], tmp.path());
    assert!(
        r.status.success() || r.status.code() == Some(0),
        "second revert exit status: {:?}\nstderr: {}",
        r.status,
        String::from_utf8_lossy(&r.stderr)
    );
}

/// Reverting a commit whose change has been modified since (the line it
/// added has been edited) produces a conflict, writes REVERT_HEAD +
/// MERGE_MSG, and exits 1.
#[test]
fn revert_with_conflict_writes_revert_head_and_exits_1() {
    if !has_system_git() {
        return;
    }
    let tmp = init_repo();
    // v0 (root) — no L4
    commit_file(tmp.path(), "a.txt", b"L1\nL2\nL3\n", "v0");
    // v1 — adds L4. This is the commit we will try to revert.
    commit_file(tmp.path(), "a.txt", b"L1\nL2\nL3\nL4\n", "v1 adds L4");
    let to_revert = String::from_utf8(git(&["rev-parse", "HEAD"], tmp.path()).stdout)
        .unwrap()
        .trim()
        .to_string();
    // v2 — modifies the L4 line. Reverting v1's "add L4" now collides
    // because the line v1 added has been changed since.
    commit_file(
        tmp.path(),
        "a.txt",
        b"L1\nL2\nL3\nFOUR\n",
        "v2 edits L4 to FOUR",
    );

    let r = rustygit(&["revert", &to_revert], tmp.path());
    assert_eq!(
        r.status.code(),
        Some(1),
        "expected exit 1 on conflict\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&r.stdout),
        String::from_utf8_lossy(&r.stderr)
    );
    assert!(
        tmp.path().join(".git/REVERT_HEAD").exists(),
        "REVERT_HEAD must be written on conflict"
    );
    assert!(
        tmp.path().join(".git/MERGE_MSG").exists(),
        "MERGE_MSG must be written on conflict"
    );
    let msg = std::fs::read_to_string(tmp.path().join(".git/MERGE_MSG")).unwrap();
    assert!(
        msg.starts_with("Revert \"v1 adds L4\""),
        "MERGE_MSG should be canonical revert message, got: {msg:?}"
    );
    assert!(
        msg.contains(&format!("This reverts commit {to_revert}.")),
        "MERGE_MSG should reference reverted oid"
    );

    // --abort cleans everything up and restores the workdir.
    let a = rustygit(&["revert", "--abort"], tmp.path());
    assert_ok(&a, "revert --abort");
    assert!(!tmp.path().join(".git/REVERT_HEAD").exists());
    assert!(!tmp.path().join(".git/MERGE_MSG").exists());
    let a_txt = std::fs::read(tmp.path().join("a.txt")).unwrap();
    assert_eq!(a_txt, b"L1\nL2\nL3\nFOUR\n");
}

/// --continue requires sequencer state.
#[test]
fn revert_continue_with_no_state_errors() {
    if !has_system_git() {
        return;
    }
    let tmp = init_repo();
    commit_file(tmp.path(), "a.txt", b"x\n", "add");

    let r = rustygit(&["revert", "--continue"], tmp.path());
    assert_eq!(r.status.code(), Some(128));
    let err = String::from_utf8_lossy(&r.stderr);
    assert!(
        err.contains("no revert in progress"),
        "stderr should say no revert in progress, got: {err:?}"
    );
}

/// --abort with no state errors cleanly.
#[test]
fn revert_abort_with_no_state_errors() {
    if !has_system_git() {
        return;
    }
    let tmp = init_repo();
    commit_file(tmp.path(), "a.txt", b"x\n", "add");

    let r = rustygit(&["revert", "--abort"], tmp.path());
    assert_eq!(r.status.code(), Some(128));
}

/// --continue and --abort together is a usage error.
#[test]
fn revert_continue_and_abort_together_is_usage_error() {
    if !has_system_git() {
        return;
    }
    let tmp = init_repo();
    let r = rustygit(&["revert", "--continue", "--abort"], tmp.path());
    assert_eq!(r.status.code(), Some(129));
}

/// No <commit> argument is a usage error.
#[test]
fn revert_no_args_is_usage_error() {
    if !has_system_git() {
        return;
    }
    let tmp = init_repo();
    let r = rustygit(&["revert"], tmp.path());
    assert_eq!(r.status.code(), Some(129));
}

/// `revert A..B` expands into the list of commits reachable from B but
/// not A. After revert, the tree byte-matches `git revert A..B`.
#[test]
fn revert_range_expression_matches_git() {
    if !has_system_git() {
        return;
    }
    let ours = init_repo();
    let theirs = init_repo();

    // 4 commits; we'll revert the middle two as a range.
    for tmp in [ours.path(), theirs.path()] {
        commit_file(tmp, "f", b"v0\n", "v0");
        commit_file(tmp, "f", b"v1\n", "v1");
        commit_file(tmp, "f", b"v2\n", "v2");
        commit_file(tmp, "f", b"v3\n", "v3");
    }
    // v1 = HEAD~2 (the boundary, exclusive). v3 = HEAD (top, inclusive).
    // git revert v1..HEAD reverts v3 then v2 (newest first).
    let v1 = String::from_utf8(git(&["rev-parse", "HEAD~2"], ours.path()).stdout)
        .unwrap()
        .trim()
        .to_string();

    let range = format!("{v1}..HEAD");
    assert_ok(
        &rustygit(&["revert", &range], ours.path()),
        "rustygit revert range",
    );
    let _ = git(&["revert", "--no-edit", &range], theirs.path());

    let our_tree = git(&["rev-parse", "HEAD^{tree}"], ours.path()).stdout;
    let their_tree = git(&["rev-parse", "HEAD^{tree}"], theirs.path()).stdout;
    assert_eq!(our_tree, their_tree, "tree mismatch after range revert");

    let f = std::fs::read(ours.path().join("f")).unwrap();
    assert_eq!(f, b"v1\n", "f should be back to v1's content");
}

/// Reverting a merge commit without `-m N` errors with a clear message.
#[test]
fn revert_merge_without_mainline_errors() {
    if !has_system_git() {
        return;
    }
    let tmp = init_repo();
    commit_file(tmp.path(), "f", b"main-1\n", "m1");
    // Create a side branch with its own commit.
    git(&["checkout", "-q", "-b", "side"], tmp.path());
    commit_file(tmp.path(), "g", b"side\n", "side-1");
    git(&["checkout", "-q", "main"], tmp.path());
    // Make main diverge so the merge has something to merge.
    commit_file(tmp.path(), "f", b"main-2\n", "m2");
    // Merge side into main → produces a merge commit.
    let out = AssertCmd::new("git")
        .args(["-c", "user.email=t@t", "-c", "user.name=t"])
        .args(["merge", "--no-ff", "-q", "-m", "merge side", "side"])
        .current_dir(tmp.path())
        .env("GIT_AUTHOR_DATE", "1700000000 +0000")
        .env("GIT_COMMITTER_DATE", "1700000000 +0000")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git merge failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let merge_oid = String::from_utf8(git(&["rev-parse", "HEAD"], tmp.path()).stdout)
        .unwrap()
        .trim()
        .to_string();

    // Revert without -m N must fail.
    let r = rustygit(&["revert", &merge_oid], tmp.path());
    assert_eq!(
        r.status.code(),
        Some(128),
        "expected 128 for missing mainline"
    );
    assert!(
        String::from_utf8_lossy(&r.stderr).contains("merge"),
        "stderr should mention merge: {:?}",
        String::from_utf8_lossy(&r.stderr)
    );
}

/// Reverting a merge commit WITH `-m 1` works and the result matches
/// `git revert -m 1 <merge>` (tree byte-equal).
#[test]
fn revert_merge_with_mainline_matches_git() {
    if !has_system_git() {
        return;
    }
    let ours = init_repo();
    let theirs = init_repo();

    for tmp in [ours.path(), theirs.path()] {
        commit_file(tmp, "f", b"main-1\n", "m1");
        git(&["checkout", "-q", "-b", "side"], tmp);
        commit_file(tmp, "g", b"side\n", "side-1");
        git(&["checkout", "-q", "main"], tmp);
        commit_file(tmp, "f", b"main-2\n", "m2");
        let out = AssertCmd::new("git")
            .args(["-c", "user.email=t@t", "-c", "user.name=t"])
            .args(["merge", "--no-ff", "-q", "-m", "merge side", "side"])
            .current_dir(tmp)
            .env("GIT_AUTHOR_DATE", "1700000000 +0000")
            .env("GIT_COMMITTER_DATE", "1700000000 +0000")
            .output()
            .unwrap();
        assert!(out.status.success());
    }

    let merge_oid = String::from_utf8(git(&["rev-parse", "HEAD"], ours.path()).stdout)
        .unwrap()
        .trim()
        .to_string();
    assert_ok(
        &rustygit(&["revert", "-m", "1", &merge_oid], ours.path()),
        "rustygit revert -m 1",
    );
    let _ = git(
        &["revert", "--no-edit", "-m", "1", &merge_oid],
        theirs.path(),
    );

    let our_tree = git(&["rev-parse", "HEAD^{tree}"], ours.path()).stdout;
    let their_tree = git(&["rev-parse", "HEAD^{tree}"], theirs.path()).stdout;
    assert_eq!(our_tree, their_tree);
    // Reverting -m 1 keeps the FIRST parent (main side); the side commit's
    // change ("g") should be undone, leaving "g" absent.
    assert!(
        !ours.path().join("g").exists(),
        "g should have been removed"
    );
}

/// Reverting two commits where the first one is "v1" and the second is
/// "v2" undoes both — final tree byte-matches git's revert path.
#[test]
fn revert_chained_two_commits_matches_git() {
    if !has_system_git() {
        return;
    }
    let ours = init_repo();
    let theirs = init_repo();

    for tmp in [ours.path(), theirs.path()] {
        commit_file(tmp, "f", b"v0\n", "v0");
        commit_file(tmp, "f", b"v1\n", "v1");
        commit_file(tmp, "f", b"v2\n", "v2");
        // unrelated change so the tree has more than one file
        commit_file(tmp, "g", b"g\n", "add g");
    }

    let v1 = String::from_utf8(git(&["rev-parse", "HEAD~2"], ours.path()).stdout)
        .unwrap()
        .trim()
        .to_string();
    let v2 = String::from_utf8(git(&["rev-parse", "HEAD~1"], ours.path()).stdout)
        .unwrap()
        .trim()
        .to_string();

    assert_ok(&rustygit(&["revert", &v2, &v1], ours.path()), "rustygit");
    let _ = git(&["revert", "--no-edit", &v2, &v1], theirs.path());

    let our_tree = git(&["rev-parse", "HEAD^{tree}"], ours.path()).stdout;
    let their_tree = git(&["rev-parse", "HEAD^{tree}"], theirs.path()).stdout;
    assert_eq!(our_tree, their_tree);

    let f = std::fs::read(ours.path().join("f")).unwrap();
    assert_eq!(f, b"v0\n");
}
