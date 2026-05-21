//! NON_GOALS.md Batch E — stub subcommands.
//!
//! `rerere` is a pure stub (everything exits 128). `replace --list` is
//! actually implemented; everything else exits 128.

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
        .output()
        .unwrap()
}

// ----- rerere -----

/// rerere is now functional (status/diff/forget/gc/clear/remaining all
/// work against `.git/rr-cache/`). Automatic record-on-resolve and
/// replay-on-merge are still deferred; these tests cover the directly-
/// invoked subcommands.
///
/// Tests below replace the historical "rerere always exits 128" gating.
#[test]
fn rerere_bare_runs_status_in_a_real_repo() {
    if !has_system_git() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    assert!(git(&["init", "-q", "."], tmp.path()).status.success());
    // Empty rr-cache → no output, exit 0.
    let out = rustygit(&["rerere"], tmp.path());
    assert_eq!(out.status.code().unwrap_or(-1), 0);
}

#[test]
fn rerere_subcommands_run_in_a_real_repo() {
    if !has_system_git() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    assert!(git(&["init", "-q", "."], tmp.path()).status.success());
    for sub in &["status", "diff", "gc", "clear", "remaining"] {
        let out = rustygit(&["rerere", sub], tmp.path());
        assert_eq!(
            out.status.code().unwrap_or(-1),
            0,
            "rerere {sub} should exit 0 in an empty repo"
        );
    }
}

#[test]
fn rerere_forget_no_args_is_usage_error() {
    if !has_system_git() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    assert!(git(&["init", "-q", "."], tmp.path()).status.success());
    let out = rustygit(&["rerere", "forget"], tmp.path());
    assert_eq!(out.status.code().unwrap_or(-1), 129);
}

// ----- replace --list -----

#[test]
fn replace_list_on_empty_repo_prints_nothing() {
    if !has_system_git() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    assert!(git(&["init", "-q", "."], tmp.path()).status.success());
    let _ = git(&["config", "user.name", "T"], tmp.path());
    let _ = git(&["config", "user.email", "t@e"], tmp.path());
    // Need a commit so refs are non-empty.
    std::fs::write(tmp.path().join("a.txt"), b"a\n").unwrap();
    assert!(git(&["add", "."], tmp.path()).status.success());
    assert!(git(&["commit", "-q", "-m", "init"], tmp.path())
        .status
        .success());

    let r = rustygit(&["replace", "--list"], tmp.path());
    assert!(
        r.status.success(),
        "replace --list exit nonzero: stderr={}",
        String::from_utf8_lossy(&r.stderr)
    );
    assert!(
        r.stdout.is_empty(),
        "expected empty stdout, got {:?}",
        String::from_utf8_lossy(&r.stdout)
    );
}

#[test]
fn replace_list_reads_git_written_refs() {
    if !has_system_git() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    assert!(git(&["init", "-q", "."], tmp.path()).status.success());
    let _ = git(&["config", "user.name", "T"], tmp.path());
    let _ = git(&["config", "user.email", "t@e"], tmp.path());
    std::fs::write(tmp.path().join("a.txt"), b"v1\n").unwrap();
    assert!(git(&["add", "."], tmp.path()).status.success());
    assert!(git(&["commit", "-q", "-m", "v1"], tmp.path())
        .status
        .success());
    std::fs::write(tmp.path().join("a.txt"), b"v2\n").unwrap();
    assert!(git(&["add", "."], tmp.path()).status.success());
    assert!(git(&["commit", "-q", "-m", "v2"], tmp.path())
        .status
        .success());

    // Resolve the two commit oids.
    let head = String::from_utf8(git(&["rev-parse", "HEAD"], tmp.path()).stdout)
        .unwrap()
        .trim()
        .to_string();
    let parent = String::from_utf8(git(&["rev-parse", "HEAD^"], tmp.path()).stdout)
        .unwrap()
        .trim()
        .to_string();

    // Use upstream git to write a replacement ref. We don't have create.
    let r = git(&["replace", &parent, &head], tmp.path());
    assert!(
        r.status.success(),
        "git replace failed: stderr={}",
        String::from_utf8_lossy(&r.stderr)
    );

    // Now rustygit replace --list should print the original's oid.
    let r = rustygit(&["replace", "--list"], tmp.path());
    assert!(r.status.success());
    let out = String::from_utf8_lossy(&r.stdout);
    assert!(out.trim() == parent, "expected {parent}, got {out:?}");

    // Cross-check: git replace --list should agree byte-for-byte.
    let gout = String::from_utf8(git(&["replace", "--list"], tmp.path()).stdout).unwrap();
    assert_eq!(
        out.into_owned(),
        gout,
        "byte mismatch with git replace --list"
    );
}

#[test]
fn replace_list_with_pattern_filters() {
    if !has_system_git() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    assert!(git(&["init", "-q", "."], tmp.path()).status.success());
    let _ = git(&["config", "user.name", "T"], tmp.path());
    let _ = git(&["config", "user.email", "t@e"], tmp.path());
    std::fs::write(tmp.path().join("a.txt"), b"v1\n").unwrap();
    assert!(git(&["add", "."], tmp.path()).status.success());
    assert!(git(&["commit", "-q", "-m", "v1"], tmp.path())
        .status
        .success());
    std::fs::write(tmp.path().join("a.txt"), b"v2\n").unwrap();
    assert!(git(&["add", "."], tmp.path()).status.success());
    assert!(git(&["commit", "-q", "-m", "v2"], tmp.path())
        .status
        .success());

    let head = String::from_utf8(git(&["rev-parse", "HEAD"], tmp.path()).stdout)
        .unwrap()
        .trim()
        .to_string();
    let parent = String::from_utf8(git(&["rev-parse", "HEAD^"], tmp.path()).stdout)
        .unwrap()
        .trim()
        .to_string();
    let _ = git(&["replace", &parent, &head], tmp.path());

    // Pattern that DOES match the parent's prefix.
    let prefix4 = &parent[..4];
    let pattern = format!("{prefix4}*");
    let r = rustygit(&["replace", "--list", &pattern], tmp.path());
    assert!(
        r.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&r.stderr)
    );
    assert!(
        String::from_utf8_lossy(&r.stdout).trim() == parent,
        "expected {parent} via pattern {pattern}, got {:?}",
        String::from_utf8_lossy(&r.stdout)
    );

    // Pattern that doesn't match anything.
    let r = rustygit(&["replace", "--list", "zzzz*"], tmp.path());
    assert!(r.status.success());
    assert!(
        r.stdout.is_empty(),
        "expected empty for non-matching pattern, got {:?}",
        String::from_utf8_lossy(&r.stdout)
    );
}

// ----- replace mutating ops (now functional) -----

/// `--edit` is still deferred (it'd spawn $EDITOR on the dumped commit/tree).
#[test]
fn replace_edit_still_exits_128() {
    if !has_system_git() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    assert!(git(&["init", "-q", "."], tmp.path()).status.success());
    let out = rustygit(&["replace", "--edit", "deadbeef"], tmp.path());
    assert_eq!(out.status.code().unwrap_or(-1), 128);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("deferred") || stderr.contains("not implemented"));
}

/// `--delete <missing>` errors with non-zero exit.
#[test]
fn replace_delete_missing_object_errors() {
    if !has_system_git() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    assert!(git(&["init", "-q", "."], tmp.path()).status.success());
    // Resolving "deadbeef" (no such object) fails inside replace -d.
    let out = rustygit(
        &[
            "replace",
            "--delete",
            "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef",
        ],
        tmp.path(),
    );
    assert_ne!(out.status.code().unwrap_or(-1), 0);
}
