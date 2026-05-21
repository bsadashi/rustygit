//! M14: cherry-pick, rebase, reflog.

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
        .env("GIT_AUTHOR_DATE", "1700000000 +0000")
        .env("GIT_COMMITTER_DATE", "1700000000 +0000")
        .output()
        .unwrap()
}

fn assert_success(out: &std::process::Output, label: &str) {
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
    git(
        &[
            "-c",
            "user.email=t@t",
            "-c",
            "user.name=t",
            "commit",
            "-q",
            "-m",
            msg,
        ],
        tmp,
    );
}

// ----------------------------------------------------------------------------
// cherry-pick
// ----------------------------------------------------------------------------

#[test]
fn cherry_pick_clean_apply() {
    if !has_system_git() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    git(&["init", "-q", "--initial-branch=master", "."], tmp.path());
    commit_file(tmp.path(), "f.txt", b"base\n", "base");
    git(&["checkout", "-q", "-b", "feature"], tmp.path());
    commit_file(tmp.path(), "g.txt", b"feature-content\n", "feat-c");
    let feat_oid = String::from_utf8(git(&["rev-parse", "HEAD"], tmp.path()).stdout)
        .unwrap()
        .trim()
        .to_string();
    git(&["checkout", "-q", "master"], tmp.path());

    assert_success(
        &rustygit(&["cherry-pick", &feat_oid], tmp.path()),
        "cherry-pick",
    );
    assert!(tmp.path().join("g.txt").exists());
    // git fsck clean.
    let fsck = Command::new("git")
        .args(["fsck", "--full"])
        .current_dir(tmp.path())
        .output()
        .unwrap();
    assert!(fsck.status.success());
}

#[test]
fn cherry_pick_conflict_writes_state() {
    if !has_system_git() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    git(&["init", "-q", "--initial-branch=master", "."], tmp.path());
    commit_file(tmp.path(), "f.txt", b"base\n", "base");
    git(&["checkout", "-q", "-b", "feature"], tmp.path());
    commit_file(tmp.path(), "f.txt", b"feature-version\n", "feat-mod");
    let feat_oid = String::from_utf8(git(&["rev-parse", "HEAD"], tmp.path()).stdout)
        .unwrap()
        .trim()
        .to_string();
    git(&["checkout", "-q", "master"], tmp.path());
    commit_file(tmp.path(), "f.txt", b"master-version\n", "mast-mod");

    let r = rustygit(&["cherry-pick", &feat_oid], tmp.path());
    assert!(!r.status.success(), "cherry-pick should conflict");
    assert!(tmp.path().join(".git/CHERRY_PICK_HEAD").exists());
    let body = std::fs::read_to_string(tmp.path().join("f.txt")).unwrap();
    assert!(body.contains("<<<<<<<"));
    assert!(body.contains(">>>>>>>"));
}

#[test]
fn cherry_pick_abort_restores_head() {
    if !has_system_git() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    git(&["init", "-q", "--initial-branch=master", "."], tmp.path());
    commit_file(tmp.path(), "f.txt", b"base\n", "base");
    git(&["checkout", "-q", "-b", "feature"], tmp.path());
    commit_file(tmp.path(), "f.txt", b"feature-version\n", "feat");
    let feat_oid = String::from_utf8(git(&["rev-parse", "HEAD"], tmp.path()).stdout)
        .unwrap()
        .trim()
        .to_string();
    git(&["checkout", "-q", "master"], tmp.path());
    commit_file(tmp.path(), "f.txt", b"master-version\n", "mast");
    let master_before = String::from_utf8(git(&["rev-parse", "HEAD"], tmp.path()).stdout)
        .unwrap()
        .trim()
        .to_string();

    // Cherry-pick that conflicts.
    let _ = rustygit(&["cherry-pick", &feat_oid], tmp.path());
    // Abort.
    assert_success(&rustygit(&["cherry-pick", "--abort"], tmp.path()), "abort");
    let master_after = String::from_utf8(git(&["rev-parse", "HEAD"], tmp.path()).stdout)
        .unwrap()
        .trim()
        .to_string();
    assert_eq!(master_before, master_after, "HEAD must be restored");
    assert!(!tmp.path().join(".git/CHERRY_PICK_HEAD").exists());
}

// ----------------------------------------------------------------------------
// rebase
// ----------------------------------------------------------------------------

#[test]
fn rebase_already_up_to_date() {
    if !has_system_git() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    git(&["init", "-q", "--initial-branch=master", "."], tmp.path());
    commit_file(tmp.path(), "f.txt", b"v1\n", "c1");
    git(&["checkout", "-q", "-b", "feature"], tmp.path());
    let r = rustygit(&["rebase", "master"], tmp.path());
    assert!(r.status.success());
}

#[test]
fn rebase_replays_commits_onto_new_base() {
    if !has_system_git() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    git(&["init", "-q", "--initial-branch=master", "."], tmp.path());
    commit_file(tmp.path(), "base.txt", b"base\n", "c0");
    git(&["checkout", "-q", "-b", "feature"], tmp.path());
    commit_file(tmp.path(), "a.txt", b"feat-a\n", "feat-a");
    commit_file(tmp.path(), "b.txt", b"feat-b\n", "feat-b");
    git(&["checkout", "-q", "master"], tmp.path());
    commit_file(tmp.path(), "m.txt", b"mast\n", "mast");
    git(&["checkout", "-q", "feature"], tmp.path());

    assert_success(&rustygit(&["rebase", "master"], tmp.path()), "rebase");

    // After rebase, feature's first-parent walk should reach master's tip.
    let log = git(&["log", "--oneline", "--first-parent"], tmp.path());
    let log_str = String::from_utf8(log.stdout).unwrap();
    assert!(log_str.contains("feat-b"));
    assert!(log_str.contains("feat-a"));
    assert!(log_str.contains("mast"));
    assert!(log_str.contains("c0"));

    // fsck clean.
    let fsck = Command::new("git")
        .args(["fsck", "--full"])
        .current_dir(tmp.path())
        .output()
        .unwrap();
    assert!(fsck.status.success());
}

#[test]
fn rebase_conflict_saves_state() {
    if !has_system_git() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    git(&["init", "-q", "--initial-branch=master", "."], tmp.path());
    commit_file(tmp.path(), "f.txt", b"base\n", "c0");
    git(&["checkout", "-q", "-b", "feature"], tmp.path());
    commit_file(tmp.path(), "f.txt", b"feature-version\n", "feat");
    git(&["checkout", "-q", "master"], tmp.path());
    commit_file(tmp.path(), "f.txt", b"master-version\n", "mast");
    git(&["checkout", "-q", "feature"], tmp.path());

    let r = rustygit(&["rebase", "master"], tmp.path());
    assert!(!r.status.success(), "rebase should conflict");
    // Sequencer state directory should exist.
    assert!(tmp.path().join(".git/sequencer").is_dir());
}

#[test]
fn rebase_abort_restores_head() {
    if !has_system_git() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    git(&["init", "-q", "--initial-branch=master", "."], tmp.path());
    commit_file(tmp.path(), "f.txt", b"base\n", "c0");
    git(&["checkout", "-q", "-b", "feature"], tmp.path());
    commit_file(tmp.path(), "f.txt", b"feature-version\n", "feat");
    let feature_before = String::from_utf8(git(&["rev-parse", "HEAD"], tmp.path()).stdout)
        .unwrap()
        .trim()
        .to_string();
    git(&["checkout", "-q", "master"], tmp.path());
    commit_file(tmp.path(), "f.txt", b"master-version\n", "mast");
    git(&["checkout", "-q", "feature"], tmp.path());

    // Conflict.
    let _ = rustygit(&["rebase", "master"], tmp.path());
    // Abort.
    assert_success(&rustygit(&["rebase", "--abort"], tmp.path()), "abort");
    let feature_after = String::from_utf8(git(&["rev-parse", "HEAD"], tmp.path()).stdout)
        .unwrap()
        .trim()
        .to_string();
    assert_eq!(feature_before, feature_after);
}

// ----------------------------------------------------------------------------
// reflog
// ----------------------------------------------------------------------------

#[test]
fn reflog_records_commits_and_resets() {
    if !has_system_git() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    assert_success(&rustygit(&["init", "-q", "."], tmp.path()), "init");

    std::fs::write(tmp.path().join("a.txt"), b"a").unwrap();
    assert_success(&rustygit(&["add", "."], tmp.path()), "add a");
    assert_success(&rustygit(&["commit", "-m", "c1"], tmp.path()), "commit c1");
    std::fs::write(tmp.path().join("b.txt"), b"b").unwrap();
    assert_success(&rustygit(&["add", "."], tmp.path()), "add b");
    assert_success(&rustygit(&["commit", "-m", "c2"], tmp.path()), "commit c2");

    // HEAD reflog should now have at least 2 entries.
    let r = rustygit(&["reflog"], tmp.path());
    assert!(r.status.success());
    let listing = String::from_utf8(r.stdout).unwrap();
    let n_lines = listing.lines().count();
    assert!(n_lines >= 2, "expected >=2 reflog lines: {listing:?}");
    assert!(listing.contains("HEAD@{0}"));
    assert!(listing.contains("c2"));
    assert!(listing.contains("c1"));
}

#[test]
fn reflog_for_specific_branch() {
    if !has_system_git() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    assert_success(&rustygit(&["init", "-q", "."], tmp.path()), "init");
    std::fs::write(tmp.path().join("a.txt"), b"a").unwrap();
    assert_success(&rustygit(&["add", "."], tmp.path()), "add");
    assert_success(&rustygit(&["commit", "-m", "c"], tmp.path()), "commit");

    let r = rustygit(&["reflog", "refs/heads/master"], tmp.path());
    assert!(r.status.success());
    let listing = String::from_utf8(r.stdout).unwrap();
    assert!(listing.contains("refs/heads/master@{0}"));
}
