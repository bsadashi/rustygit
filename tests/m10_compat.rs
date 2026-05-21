//! M10 compatibility: HTTPS clone + fetch + ls-remote against real public repos.
//!
//! These tests require network access. They skip cleanly if a request fails
//! with a connection error so CI without network doesn't false-fail.

mod common;

use std::path::Path;
use std::process::Command;

use assert_cmd::Command as AssertCmd;
use common::has_system_git;
use tempfile::TempDir;

/// Small, stable public repo to clone from.
const FIXTURE_URL: &str = "https://github.com/octocat/Hello-World.git";

fn rustygit(args: &[&str], cwd: &Path) -> std::process::Output {
    AssertCmd::cargo_bin("rustygit")
        .unwrap()
        .args(args)
        .current_dir(cwd)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .timeout(std::time::Duration::from_secs(60))
        .output()
        .unwrap()
}

fn network_skip_or_assert(out: &std::process::Output, label: &str) -> bool {
    if out.status.success() {
        return true;
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    if stderr.contains("connection")
        || stderr.contains("Connection")
        || stderr.contains("dns")
        || stderr.contains("resolve")
        || stderr.contains("timed out")
        || stderr.contains("getaddrinfo")
    {
        eprintln!("skipping {label}: network unavailable");
        return false;
    }
    panic!("{label} failed (not a network error):\nstderr: {}", stderr);
}

#[test]
fn ls_remote_lists_refs_from_github() {
    let cwd = std::env::current_dir().unwrap();
    let out = rustygit(&["ls-remote", FIXTURE_URL], &cwd);
    if !network_skip_or_assert(&out, "ls-remote") {
        return;
    }
    let stdout = String::from_utf8(out.stdout).unwrap();
    // Expect HEAD + master + a couple of well-known branches on the test repo.
    assert!(stdout.contains("\tHEAD"), "missing HEAD: {stdout}");
    assert!(
        stdout.contains("\trefs/heads/master"),
        "missing master: {stdout}"
    );
    // Each line is <oid>\t<refname>; the oid is 40 hex chars.
    for line in stdout.lines() {
        let parts: Vec<&str> = line.splitn(2, '\t').collect();
        assert_eq!(parts.len(), 2, "expected tab-separated: {line:?}");
        let oid = parts[0];
        assert_eq!(oid.len(), 40, "oid should be 40 hex chars: {oid}");
        assert!(oid.chars().all(|c| c.is_ascii_hexdigit()));
    }
}

#[test]
fn clone_https_succeeds_and_passes_fsck() {
    if !has_system_git() {
        return;
    }
    let dst_root = TempDir::new().unwrap();
    let dst = dst_root.path().join("hello");
    let cwd = std::env::current_dir().unwrap();
    let out = rustygit(&["clone", "-q", FIXTURE_URL, dst.to_str().unwrap()], &cwd);
    if !network_skip_or_assert(&out, "clone") {
        return;
    }

    // git fsck must be happy with the result.
    let fsck = Command::new("git")
        .args(["fsck", "--full"])
        .current_dir(&dst)
        .output()
        .unwrap();
    assert!(
        fsck.status.success(),
        "git fsck failed: {}",
        String::from_utf8_lossy(&fsck.stderr)
    );
}

#[test]
fn clone_https_populates_remote_tracking_refs() {
    if !has_system_git() {
        return;
    }
    let dst_root = TempDir::new().unwrap();
    let dst = dst_root.path().join("hello");
    let cwd = std::env::current_dir().unwrap();
    let out = rustygit(&["clone", "-q", FIXTURE_URL, dst.to_str().unwrap()], &cwd);
    if !network_skip_or_assert(&out, "clone") {
        return;
    }

    let refs = Command::new("git")
        .args(["show-ref"])
        .current_dir(&dst)
        .output()
        .unwrap();
    let listing = String::from_utf8(refs.stdout).unwrap();
    assert!(listing.contains("refs/heads/master"));
    assert!(listing.contains("refs/remotes/origin/master"));
}

#[test]
fn clone_https_default_branch_workdir_materialized() {
    if !has_system_git() {
        return;
    }
    let dst_root = TempDir::new().unwrap();
    let dst = dst_root.path().join("hello");
    let cwd = std::env::current_dir().unwrap();
    let out = rustygit(&["clone", "-q", FIXTURE_URL, dst.to_str().unwrap()], &cwd);
    if !network_skip_or_assert(&out, "clone") {
        return;
    }
    // The octocat repo has a single README at master tip.
    assert!(
        dst.join("README").exists() || dst.join("README.md").exists(),
        "expected README at workdir root, listing: {:?}",
        std::fs::read_dir(&dst)
            .unwrap()
            .flatten()
            .map(|e| e.file_name())
            .collect::<Vec<_>>()
    );
}

#[test]
fn clone_refuses_non_empty_destination_over_network() {
    let dst_root = TempDir::new().unwrap();
    let dst = dst_root.path().join("hello");
    std::fs::create_dir_all(&dst).unwrap();
    std::fs::write(dst.join("preexisting"), b"x").unwrap();
    let cwd = std::env::current_dir().unwrap();
    let out = rustygit(&["clone", FIXTURE_URL, dst.to_str().unwrap()], &cwd);
    assert!(!out.status.success());
    assert!(
        dst.join("preexisting").exists(),
        "must not delete pre-existing files on refusal"
    );
}

#[test]
fn fetch_refuses_bare_remote_name() {
    // Without config-writing for remotes (M11), fetch requires a URL.
    let tmp = TempDir::new().unwrap();
    let _ = rustygit(&["init", "-q", "."], tmp.path());
    let out = rustygit(&["fetch", "origin"], tmp.path());
    assert!(
        !out.status.success(),
        "fetch should refuse bare remote names in M10"
    );
}

#[test]
fn pull_stub_errors_out() {
    let tmp = TempDir::new().unwrap();
    let _ = rustygit(&["init", "-q", "."], tmp.path());
    let out = rustygit(&["pull", FIXTURE_URL], tmp.path());
    // pull is allowed to do the fetch part but must error on merge.
    // We only check it exits non-zero (network failures also exit non-zero).
    assert!(!out.status.success(), "pull stub should exit non-zero");
}
