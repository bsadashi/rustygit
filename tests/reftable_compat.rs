//! Byte-compatibility for the reftable backend.
//!
//! These tests round-trip with system `git` against `--ref-format=reftable`
//! repos. They skip cleanly when git is too old or doesn't support reftable.

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

/// Does the system git support `--ref-format=reftable`? Added in git 2.45.
fn supports_reftable() -> bool {
    if !has_system_git() {
        return false;
    }
    let out = Command::new("git")
        .args(["init", "--ref-format=reftable", "--help"])
        .output();
    match out {
        Ok(o) => o.status.success(),
        Err(_) => false,
    }
}

fn try_init_reftable(at: &Path) -> bool {
    // We do an actual init (not just --help) because some git builds print a
    // help page but still reject the flag at runtime.
    let out = Command::new("git")
        .args(["init", "-q", "--ref-format=reftable", "."])
        .current_dir(at)
        .output();
    matches!(out, Ok(o) if o.status.success())
}

/// Make a single commit in `cwd` (assumes a git repo already exists there).
fn commit_one(cwd: &Path, content: &str, msg: &str) {
    std::fs::write(cwd.join("a.txt"), content).unwrap();
    git(&["add", "a.txt"], cwd);
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
        cwd,
    );
}

#[test]
fn rustygit_reads_git_written_reftable_head() {
    if !supports_reftable() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    if !try_init_reftable(tmp.path()) {
        return;
    }
    commit_one(tmp.path(), "first\n", "first");

    let head = git(&["rev-parse", "HEAD"], tmp.path());
    let expected_oid = String::from_utf8(head.stdout).unwrap().trim().to_string();

    // Run rustygit rev-parse HEAD.
    let out = rustygit(&["rev-parse", "HEAD"], tmp.path());
    assert_success(&out, "rustygit rev-parse HEAD");
    let got = String::from_utf8(out.stdout).unwrap().trim().to_string();
    assert_eq!(
        got, expected_oid,
        "rustygit rev-parse HEAD on a reftable-backed repo must match git"
    );
}

#[test]
fn rustygit_show_ref_matches_git_on_reftable() {
    if !supports_reftable() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    if !try_init_reftable(tmp.path()) {
        return;
    }
    commit_one(tmp.path(), "first\n", "first");
    // Create an additional branch.
    git(&["branch", "topic"], tmp.path());
    let head = git(&["rev-parse", "HEAD"], tmp.path());
    let oid = String::from_utf8(head.stdout).unwrap().trim().to_string();

    // git show-ref output (we sort lines because order across backends might
    // differ).
    let git_out = git(&["show-ref"], tmp.path());
    let mut git_lines: Vec<String> = String::from_utf8(git_out.stdout)
        .unwrap()
        .lines()
        .map(|s| s.to_string())
        .collect();
    git_lines.sort();

    let our_out = rustygit(&["show-ref"], tmp.path());
    assert_success(&our_out, "rustygit show-ref");
    let mut our_lines: Vec<String> = String::from_utf8(our_out.stdout)
        .unwrap()
        .lines()
        .map(|s| s.to_string())
        .collect();
    our_lines.sort();
    assert_eq!(
        our_lines, git_lines,
        "rustygit show-ref must match git show-ref on a reftable-backed repo"
    );
    // Sanity: both branches resolved.
    assert!(our_lines.iter().any(|l| l.contains("refs/heads/topic")));
    assert!(our_lines.iter().any(|l| l.starts_with(&oid)));
}

#[test]
fn rustygit_update_ref_writes_reftable_git_reads_it_back() {
    if !supports_reftable() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    if !try_init_reftable(tmp.path()) {
        return;
    }
    commit_one(tmp.path(), "first\n", "first");

    let head = git(&["rev-parse", "HEAD"], tmp.path());
    let oid = String::from_utf8(head.stdout).unwrap().trim().to_string();

    // Write a new ref via rustygit. The store must produce a new `.ref` table
    // file and append to tables.list.
    assert_success(
        &rustygit(
            &["update-ref", "refs/heads/from-rustygit", &oid],
            tmp.path(),
        ),
        "rustygit update-ref",
    );

    // git must now see the new branch.
    let out = git(&["show-ref", "refs/heads/from-rustygit"], tmp.path());
    let listing = String::from_utf8(out.stdout).unwrap();
    let line = listing.lines().next().unwrap_or("");
    assert!(
        line.starts_with(&oid),
        "git did not see rustygit-written ref: {listing:?}"
    );
}

#[test]
fn reftable_round_trip_branch_create_and_delete() {
    if !supports_reftable() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    if !try_init_reftable(tmp.path()) {
        return;
    }
    commit_one(tmp.path(), "first\n", "first");
    let head = git(&["rev-parse", "HEAD"], tmp.path());
    let oid = String::from_utf8(head.stdout).unwrap().trim().to_string();

    // Create via rustygit, delete via git, check both backends see the
    // change. We delete via git because rustygit's `update-ref -d` should
    // also work, but git is the oracle.
    assert_success(
        &rustygit(&["update-ref", "refs/heads/staging", &oid], tmp.path()),
        "rustygit create staging",
    );
    let show = git(&["show-ref", "refs/heads/staging"], tmp.path());
    assert!(
        String::from_utf8(show.stdout).unwrap().contains(&oid),
        "git must see rustygit-created branch"
    );

    // Now delete via git.
    git(&["update-ref", "-d", "refs/heads/staging"], tmp.path());
    // rustygit must now report missing.
    let out = rustygit(&["show-ref", "refs/heads/staging"], tmp.path());
    assert!(
        !out.status.success() || String::from_utf8_lossy(&out.stdout).is_empty(),
        "rustygit must NOT see the deleted branch after git deleted it"
    );
}

#[test]
fn reftable_symbolic_head_read() {
    if !supports_reftable() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    if !try_init_reftable(tmp.path()) {
        return;
    }
    commit_one(tmp.path(), "first\n", "first");

    // Read HEAD via rustygit's symbolic-ref subcommand (it must resolve
    // symbolic chain → matches git's symbolic-ref HEAD).
    let git_sym = git(&["symbolic-ref", "HEAD"], tmp.path());
    let expected = String::from_utf8(git_sym.stdout)
        .unwrap()
        .trim()
        .to_string();
    let out = rustygit(&["symbolic-ref", "HEAD"], tmp.path());
    assert_success(&out, "rustygit symbolic-ref HEAD");
    let got = String::from_utf8(out.stdout).unwrap().trim().to_string();
    assert_eq!(got, expected, "HEAD symref target must match");
}

#[test]
fn reftable_branch_listing_via_iter() {
    if !supports_reftable() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    if !try_init_reftable(tmp.path()) {
        return;
    }
    commit_one(tmp.path(), "first\n", "first");
    git(&["branch", "feature"], tmp.path());
    git(&["branch", "release/v1"], tmp.path());

    // rustygit show-ref must enumerate all three branches under refs/heads.
    let out = rustygit(&["show-ref"], tmp.path());
    assert_success(&out, "rustygit show-ref");
    let listing = String::from_utf8(out.stdout).unwrap();
    for name in ["refs/heads/feature", "refs/heads/release/v1"] {
        assert!(
            listing.lines().any(|l| l.ends_with(name)),
            "rustygit show-ref missing {name}\n{listing}"
        );
    }
}
