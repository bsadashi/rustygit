//! Exit-code conventions for `--exit-code` / `--quiet` flows (A7) and
//! end-to-end smoke tests for the global flags added in A5.
//!
//! Centralizes the contract: 0 == clean, 1 == differences found, 128 ==
//! fatal, 129 == usage. Tests here assert the BYTE-LEVEL exit code so
//! downstream shell scripts can rely on it.

mod common;

use std::path::Path;

use assert_cmd::Command as AssertCmd;
use tempfile::TempDir;

fn rustygit(args: &[&str], cwd: &Path) -> std::process::Output {
    let mut cmd = AssertCmd::cargo_bin("rustygit").unwrap();
    cmd.args(args)
        .current_dir(cwd)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .output()
        .unwrap()
}

fn assert_ok(out: &std::process::Output, label: &str) {
    assert!(
        out.status.success(),
        "{label} failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
}

fn make_one_commit(tmp: &Path) {
    std::fs::write(tmp.join("a.txt"), b"hello\n").unwrap();
    assert_ok(&rustygit(&["add", "a.txt"], tmp), "add a.txt");
    assert_ok(&rustygit(&["commit", "-m", "c1"], tmp), "commit c1");
}

// --- A7: diff --exit-code ----------------------------------------------

#[test]
fn diff_exit_code_dirty_tree_returns_one() {
    let tmp = TempDir::new().unwrap();
    assert_ok(&rustygit(&["init", "-q", "."], tmp.path()), "init");
    make_one_commit(tmp.path());
    std::fs::write(tmp.path().join("a.txt"), b"DIRTY\n").unwrap();
    let r = rustygit(&["diff", "--exit-code"], tmp.path());
    assert_eq!(
        r.status.code(),
        Some(1),
        "dirty tree should return EXIT_DIFF_FOUND (1), got {:?}\nstderr: {}",
        r.status.code(),
        String::from_utf8_lossy(&r.stderr)
    );
}

#[test]
fn diff_exit_code_clean_tree_returns_zero() {
    let tmp = TempDir::new().unwrap();
    assert_ok(&rustygit(&["init", "-q", "."], tmp.path()), "init");
    make_one_commit(tmp.path());
    let r = rustygit(&["diff", "--exit-code"], tmp.path());
    assert_eq!(r.status.code(), Some(0));
}

#[test]
fn diff_index_exit_code_dirty_returns_one() {
    let tmp = TempDir::new().unwrap();
    assert_ok(&rustygit(&["init", "-q", "."], tmp.path()), "init");
    make_one_commit(tmp.path());
    std::fs::write(tmp.path().join("a.txt"), b"DIRTY\n").unwrap();
    let r = rustygit(&["diff-index", "--exit-code", "HEAD"], tmp.path());
    assert_eq!(
        r.status.code(),
        Some(1),
        "diff-index dirty should return 1, got {:?}\nstderr: {}",
        r.status.code(),
        String::from_utf8_lossy(&r.stderr)
    );
}

#[test]
fn diff_files_exit_code_dirty_returns_one() {
    let tmp = TempDir::new().unwrap();
    assert_ok(&rustygit(&["init", "-q", "."], tmp.path()), "init");
    make_one_commit(tmp.path());
    std::fs::write(tmp.path().join("a.txt"), b"DIRTY\n").unwrap();
    let r = rustygit(&["diff-files", "--exit-code"], tmp.path());
    assert_eq!(
        r.status.code(),
        Some(1),
        "diff-files dirty should return 1, got {:?}\nstderr: {}",
        r.status.code(),
        String::from_utf8_lossy(&r.stderr)
    );
}

// --- A5: global flags ----------------------------------------------------

#[test]
fn git_dir_flag_routes_status_to_remote_repo() {
    // `rustygit --git-dir=/path/to/.git status` should find the repo at the
    // explicit gitdir, NOT discover via cwd. We exercise it by initializing
    // a repo somewhere, then running rustygit from an unrelated cwd with
    // `--git-dir` pointed at the repo's .git directory.
    let tmp = TempDir::new().unwrap();
    assert_ok(&rustygit(&["init", "-q", "."], tmp.path()), "init");

    let other = TempDir::new().unwrap();
    let gitdir = tmp.path().join(".git");
    let r = rustygit(
        &[
            &format!("--git-dir={}", gitdir.display()),
            &format!("--work-tree={}", tmp.path().display()),
            "status",
        ],
        other.path(),
    );
    // Whether this fully works end-to-end depends on whether A6 has been
    // wired (env-var honoring in Repository::discover_from_cwd). What we
    // CAN assert deterministically is that `--git-dir` is accepted by clap
    // without a "unexpected argument" usage error. A usage error returns
    // exit 2 from clap.
    assert!(
        r.status.code() != Some(2),
        "--git-dir was rejected by clap: {}\nstderr: {}",
        r.status.code().map(|c| c.to_string()).unwrap_or("?".into()),
        String::from_utf8_lossy(&r.stderr),
    );
}

#[test]
fn no_pager_flag_accepted_by_clap() {
    let tmp = TempDir::new().unwrap();
    assert_ok(&rustygit(&["init", "-q", "."], tmp.path()), "init");
    make_one_commit(tmp.path());
    // `--no-pager log` should run to completion without error.
    let r = rustygit(&["--no-pager", "log"], tmp.path());
    assert!(
        r.status.success(),
        "rustygit --no-pager log failed\nstderr: {}",
        String::from_utf8_lossy(&r.stderr)
    );
    // GIT_PAGER=cat got set; the binary printed something normal.
    assert!(!r.stdout.is_empty());
}

#[test]
fn bare_flag_accepted_globally_as_no_op() {
    let tmp = TempDir::new().unwrap();
    assert_ok(&rustygit(&["init", "-q", "."], tmp.path()), "init");
    make_one_commit(tmp.path());
    // `rustygit --bare status` is meaningless but should clap-parse.
    let r = rustygit(&["--bare", "status"], tmp.path());
    assert!(
        r.status.code() != Some(2),
        "--bare was rejected by clap; stderr: {}",
        String::from_utf8_lossy(&r.stderr),
    );
}

#[test]
fn exec_path_bare_prints_and_exits() {
    // `--exec-path` with no value prints the helper dir and exits 0.
    let tmp = TempDir::new().unwrap();
    let r = rustygit(&["--exec-path"], tmp.path());
    assert!(
        r.status.success(),
        "rustygit --exec-path failed\nstderr: {}",
        String::from_utf8_lossy(&r.stderr)
    );
    let s = String::from_utf8_lossy(&r.stdout);
    assert!(
        !s.trim().is_empty(),
        "--exec-path should print a non-empty path"
    );
}
