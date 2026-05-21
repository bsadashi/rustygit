//! M0 byte-compatibility: `rustygit init` vs `git init`.
//!
//! Asserts that the .git directory we produce contains a superset of the files
//! and directories git produces, with byte-identical content for every file
//! we both write. We intentionally do NOT recreate git's `hooks/*.sample` files
//! (M0 non-goal); divergence on hooks is allowed.

mod common;

use std::path::Path;

use assert_cmd::Command;
use common::{git, has_system_git, snapshot_dir, snapshot_dirs};
use tempfile::TempDir;

#[test]
fn init_produces_compatible_layout() {
    if !has_system_git() {
        eprintln!("skipping: system git not on PATH");
        return;
    }

    let our_tmp = TempDir::new().unwrap();
    let git_tmp = TempDir::new().unwrap();

    Command::cargo_bin("rustygit")
        .unwrap()
        .args(["init", "-q", "."])
        .current_dir(our_tmp.path())
        .assert()
        .success();

    git(
        &["init", "-q", "--initial-branch", "master", "."],
        git_tmp.path(),
    );

    let our_git = our_tmp.path().join(".git");
    let git_git = git_tmp.path().join(".git");

    let our_files = snapshot_dir(&our_git);
    let git_files = snapshot_dir(&git_git);

    // 1. Every file we both write must have identical content.
    for (path, git_bytes) in &git_files {
        if path.starts_with("hooks/") {
            continue; // M0 non-goal: hooks samples
        }
        let our = our_files
            .iter()
            .find(|(p, _)| p == path)
            .unwrap_or_else(|| panic!("we did not write {path}"));
        assert_eq!(
            our.1,
            *git_bytes,
            "byte mismatch on {path}\nours:\n{}\ngit's:\n{}",
            String::from_utf8_lossy(&our.1),
            String::from_utf8_lossy(git_bytes)
        );
    }

    // 2. We must create the same set of directories (modulo `hooks/`).
    let our_dirs: Vec<String> = snapshot_dirs(&our_git)
        .into_iter()
        .filter(|d| !d.starts_with("hooks"))
        .collect();
    let git_dirs: Vec<String> = snapshot_dirs(&git_git)
        .into_iter()
        .filter(|d| !d.starts_with("hooks"))
        .collect();
    assert_eq!(our_dirs, git_dirs, "directory layouts differ");
}

#[test]
fn init_writes_head_for_default_branch() {
    let tmp = TempDir::new().unwrap();
    Command::cargo_bin("rustygit")
        .unwrap()
        .args(["init", "-q", "."])
        .current_dir(tmp.path())
        .assert()
        .success();
    let head = std::fs::read_to_string(tmp.path().join(".git/HEAD")).unwrap();
    assert_eq!(head, "ref: refs/heads/master\n");
}

#[test]
fn init_with_initial_branch() {
    let tmp = TempDir::new().unwrap();
    Command::cargo_bin("rustygit")
        .unwrap()
        .args(["init", "-q", "--initial-branch", "trunk", "."])
        .current_dir(tmp.path())
        .assert()
        .success();
    let head = std::fs::read_to_string(tmp.path().join(".git/HEAD")).unwrap();
    assert_eq!(head, "ref: refs/heads/trunk\n");
}

#[test]
fn init_with_object_format_sha256() {
    if !has_system_git() {
        return;
    }
    let our_tmp = TempDir::new().unwrap();
    let git_tmp = TempDir::new().unwrap();

    Command::cargo_bin("rustygit")
        .unwrap()
        .args(["init", "-q", "--object-format", "sha256", "."])
        .current_dir(our_tmp.path())
        .assert()
        .success();

    git(
        &[
            "init",
            "-q",
            "--initial-branch",
            "master",
            "--object-format",
            "sha256",
            ".",
        ],
        git_tmp.path(),
    );

    let our_cfg = std::fs::read_to_string(our_tmp.path().join(".git/config")).unwrap();
    let git_cfg = std::fs::read_to_string(git_tmp.path().join(".git/config")).unwrap();

    // Both should declare repositoryformatversion = 1 and extensions.objectformat = sha256.
    assert!(
        our_cfg.contains("repositoryformatversion = 1"),
        "ours:\n{our_cfg}"
    );
    assert!(
        git_cfg.contains("repositoryformatversion = 1"),
        "git's:\n{git_cfg}"
    );
    assert!(our_cfg.to_lowercase().contains("objectformat = sha256"));
    assert!(git_cfg.to_lowercase().contains("objectformat = sha256"));
}

#[allow(dead_code)]
fn _path_must_exist(p: &Path) {
    assert!(p.exists(), "missing: {}", p.display());
}
