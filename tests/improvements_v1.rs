//! Compat tests for the v1 iteration: `show`, `diff --exit-code`/`--quiet`,
//! and the top-level `-c key=value` config override.
//!
//! Each test creates a tiny throwaway repo, exercises the new feature, and
//! either oracles against system git or asserts the documented exit code.

mod common;

use std::path::Path;

use assert_cmd::Command as AssertCmd;
use common::has_system_git;
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

fn make_two_commits(tmp: &Path) {
    std::fs::write(tmp.join("a.txt"), b"hello\n").unwrap();
    assert_ok(&rustygit(&["add", "a.txt"], tmp), "add hello");
    assert_ok(
        &rustygit(&["commit", "-m", "first commit"], tmp),
        "commit 1",
    );
    std::fs::write(tmp.join("a.txt"), b"world\n").unwrap();
    assert_ok(&rustygit(&["add", "a.txt"], tmp), "add world");
    assert_ok(
        &rustygit(&["commit", "-m", "second commit"], tmp),
        "commit 2",
    );
}

// --- show ----------------------------------------------------------------

/// `rustygit show` (no arg) on the tip commit must produce byte-identical
/// output to `git show` for that commit.
#[test]
fn show_default_matches_git_byte_for_byte() {
    if !has_system_git() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    assert_ok(&rustygit(&["init", "-q", "."], tmp.path()), "init");
    make_two_commits(tmp.path());

    let rg = rustygit(&["show"], tmp.path());
    assert_ok(&rg, "rustygit show");
    let g = std::process::Command::new("git")
        .args(["--no-pager", "show"])
        .current_dir(tmp.path())
        .output()
        .unwrap();
    assert!(g.status.success(), "git show failed");
    assert_eq!(
        String::from_utf8_lossy(&rg.stdout),
        String::from_utf8_lossy(&g.stdout),
        "show output diverged"
    );
}

/// `show <root-commit>` must show every file as a "new file" diff against
/// the empty tree. We oracle against `git show` for the same root commit.
#[test]
fn show_root_commit_matches_git() {
    if !has_system_git() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    assert_ok(&rustygit(&["init", "-q", "."], tmp.path()), "init");
    make_two_commits(tmp.path());

    // The root commit is HEAD~1 since we made two.
    let rg = rustygit(&["show", "HEAD~1"], tmp.path());
    assert_ok(&rg, "rustygit show HEAD~1");
    let g = std::process::Command::new("git")
        .args(["--no-pager", "show", "HEAD~1"])
        .current_dir(tmp.path())
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&rg.stdout),
        String::from_utf8_lossy(&g.stdout),
        "root-commit show output diverged"
    );
}

/// `show <blob-oid>` must print the blob's bytes verbatim.
#[test]
fn show_blob_prints_bytes() {
    if !has_system_git() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    assert_ok(&rustygit(&["init", "-q", "."], tmp.path()), "init");
    std::fs::write(tmp.path().join("data.bin"), b"raw bytes here\n").unwrap();
    let h = rustygit(&["hash-object", "-w", "data.bin"], tmp.path());
    assert_ok(&h, "hash-object");
    let oid = String::from_utf8(h.stdout).unwrap().trim().to_string();

    let rg = rustygit(&["show", &oid], tmp.path());
    assert_ok(&rg, "rustygit show <blob>");
    assert_eq!(rg.stdout, b"raw bytes here\n");
}

// --- diff --exit-code / --quiet ------------------------------------------

#[test]
fn diff_exit_code_zero_on_clean_tree() {
    let tmp = TempDir::new().unwrap();
    assert_ok(&rustygit(&["init", "-q", "."], tmp.path()), "init");
    make_two_commits(tmp.path());
    let r = rustygit(&["diff", "--exit-code"], tmp.path());
    assert_eq!(r.status.code(), Some(0), "clean tree should exit 0");
    assert!(r.stdout.is_empty(), "no output expected");
}

#[test]
fn diff_exit_code_one_with_dirty_workdir() {
    let tmp = TempDir::new().unwrap();
    assert_ok(&rustygit(&["init", "-q", "."], tmp.path()), "init");
    make_two_commits(tmp.path());
    std::fs::write(tmp.path().join("a.txt"), b"DIRTY\n").unwrap();
    let r = rustygit(&["diff", "--exit-code"], tmp.path());
    assert_eq!(r.status.code(), Some(1), "dirty tree should exit 1");
    // Diff output IS still printed (matching git --exit-code).
    let s = String::from_utf8_lossy(&r.stdout);
    assert!(s.contains("--- a/a.txt"), "expected diff body, got {s:?}");
    assert!(s.contains("+DIRTY"), "expected + line, got {s:?}");
}

#[test]
fn diff_quiet_suppresses_output_but_still_exits_one() {
    let tmp = TempDir::new().unwrap();
    assert_ok(&rustygit(&["init", "-q", "."], tmp.path()), "init");
    make_two_commits(tmp.path());
    std::fs::write(tmp.path().join("a.txt"), b"DIRTY\n").unwrap();
    let r = rustygit(&["diff", "--quiet"], tmp.path());
    assert_eq!(r.status.code(), Some(1), "--quiet dirty should exit 1");
    assert!(r.stdout.is_empty(), "--quiet should suppress output");
}

// --- -c key=value --------------------------------------------------------

/// `rustygit -c user.name=... -c user.email=... commit` must use the
/// supplied identity when no env vars or repo config are set. We then read
/// back the commit via `log` to confirm the identity was applied.
#[test]
fn dash_c_overrides_user_identity_for_commit() {
    let tmp = TempDir::new().unwrap();
    assert_ok(&rustygit(&["init", "-q", "."], tmp.path()), "init");
    std::fs::write(tmp.path().join("a.txt"), b"x\n").unwrap();

    // Don't pass GIT_AUTHOR_* env this time — let -c be the source of truth.
    let mut add = AssertCmd::cargo_bin("rustygit").unwrap();
    let add_out = add
        .args(["add", "a.txt"])
        .current_dir(tmp.path())
        .output()
        .unwrap();
    assert_ok(&add_out, "add");

    let mut commit = AssertCmd::cargo_bin("rustygit").unwrap();
    let commit_out = commit
        .args([
            "-c",
            "user.name=Alice",
            "-c",
            "user.email=alice@example.com",
            "commit",
            "-m",
            "via -c",
        ])
        .current_dir(tmp.path())
        .output()
        .unwrap();
    assert_ok(&commit_out, "commit -c");

    let log = rustygit(&["log"], tmp.path());
    assert_ok(&log, "log");
    let body = String::from_utf8_lossy(&log.stdout);
    assert!(
        body.contains("Author: Alice <alice@example.com>"),
        "expected -c-driven author, got: {body}"
    );
}

/// `-c` after the subcommand should NOT be parsed as a global override —
/// `switch -c <branch>` must still create a branch (matches git's behavior
/// where global `-c` only binds before the subcommand).
#[test]
fn switch_dash_c_remains_branch_create_form() {
    let tmp = TempDir::new().unwrap();
    assert_ok(&rustygit(&["init", "-q", "."], tmp.path()), "init");
    make_two_commits(tmp.path());
    let r = rustygit(&["switch", "-c", "topic"], tmp.path());
    assert_ok(&r, "switch -c topic");

    let br = rustygit(&["branch"], tmp.path());
    assert_ok(&br, "branch");
    let listing = String::from_utf8_lossy(&br.stdout);
    assert!(
        listing.contains("* topic"),
        "switch -c should put us on 'topic', got: {listing}"
    );
}
