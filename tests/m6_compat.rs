//! M6 byte-compatibility for branch / checkout / switch / restore / reset.

mod common;

use std::path::Path;

use assert_cmd::Command as AssertCmd;
use common::{git, has_system_git};
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

fn assert_success(out: &std::process::Output, label: &str) {
    assert!(
        out.status.success(),
        "{label} failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

fn make_initial_commit(tmp: &Path) {
    std::fs::write(tmp.join("a.txt"), b"v1\n").unwrap();
    assert_success(&rustygit(&["add", "."], tmp), "add");
    assert_success(&rustygit(&["commit", "-m", "first"], tmp), "commit");
}

#[test]
fn branch_create_list_delete() {
    if !has_system_git() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    assert_success(&rustygit(&["init", "-q", "."], tmp.path()), "init");
    make_initial_commit(tmp.path());

    assert_success(&rustygit(&["branch", "feature"], tmp.path()), "create");
    assert_success(
        &rustygit(&["branch", "develop"], tmp.path()),
        "create develop",
    );

    let r = rustygit(&["branch"], tmp.path());
    assert!(r.status.success());
    let listing = String::from_utf8(r.stdout).unwrap();
    assert!(listing.contains("* master"), "listing: {listing}");
    assert!(listing.contains("  feature"));
    assert!(listing.contains("  develop"));

    // git agrees the branches exist.
    let g = git(&["branch"], tmp.path());
    let g_listing = String::from_utf8(g.stdout).unwrap();
    assert!(g_listing.contains("feature"));
    assert!(g_listing.contains("develop"));

    // Delete one.
    assert_success(
        &rustygit(&["branch", "-d", "develop"], tmp.path()),
        "delete develop",
    );
    let after = rustygit(&["branch"], tmp.path());
    let listing = String::from_utf8(after.stdout).unwrap();
    assert!(!listing.contains("develop"));
    assert!(listing.contains("feature"));
}

#[test]
fn branch_refuses_to_delete_current() {
    if !has_system_git() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    assert_success(&rustygit(&["init", "-q", "."], tmp.path()), "init");
    make_initial_commit(tmp.path());

    let r = rustygit(&["branch", "-d", "master"], tmp.path());
    assert!(
        !r.status.success(),
        "should refuse to delete current branch"
    );
}

#[test]
fn checkout_switches_branches_and_updates_workdir() {
    if !has_system_git() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    assert_success(&rustygit(&["init", "-q", "."], tmp.path()), "init");
    make_initial_commit(tmp.path());

    assert_success(&rustygit(&["branch", "feature"], tmp.path()), "br");
    assert_success(
        &rustygit(&["checkout", "feature"], tmp.path()),
        "co feature",
    );

    // Modify on feature.
    std::fs::write(tmp.path().join("a.txt"), b"v2\n").unwrap();
    assert_success(&rustygit(&["add", "."], tmp.path()), "add v2");
    assert_success(
        &rustygit(&["commit", "-m", "feature update"], tmp.path()),
        "commit v2",
    );
    assert_eq!(std::fs::read(tmp.path().join("a.txt")).unwrap(), b"v2\n");

    // Switch back to master — workdir reverts.
    assert_success(&rustygit(&["checkout", "master"], tmp.path()), "co master");
    assert_eq!(std::fs::read(tmp.path().join("a.txt")).unwrap(), b"v1\n");

    // git verifies HEAD points at master.
    let h = git(&["rev-parse", "--abbrev-ref", "HEAD"], tmp.path());
    assert_eq!(String::from_utf8(h.stdout).unwrap().trim(), "master");
}

#[test]
fn checkout_refuses_dirty_file() {
    if !has_system_git() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    assert_success(&rustygit(&["init", "-q", "."], tmp.path()), "init");
    make_initial_commit(tmp.path());

    assert_success(&rustygit(&["branch", "feature"], tmp.path()), "br");
    assert_success(
        &rustygit(&["checkout", "feature"], tmp.path()),
        "co feature",
    );
    std::fs::write(tmp.path().join("a.txt"), b"v2\n").unwrap();
    assert_success(&rustygit(&["add", "."], tmp.path()), "add");
    assert_success(
        &rustygit(&["commit", "-m", "feature commit"], tmp.path()),
        "c",
    );

    // Now go back, modify locally, then try to checkout feature again.
    assert_success(&rustygit(&["checkout", "master"], tmp.path()), "co master");
    std::fs::write(tmp.path().join("a.txt"), b"local\n").unwrap();
    let r = rustygit(&["checkout", "feature"], tmp.path());
    assert!(!r.status.success(), "should refuse dirty checkout");
    // File should still be the dirty local content.
    assert_eq!(std::fs::read(tmp.path().join("a.txt")).unwrap(), b"local\n");
}

#[test]
fn checkout_force_overrides_dirty() {
    if !has_system_git() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    assert_success(&rustygit(&["init", "-q", "."], tmp.path()), "init");
    make_initial_commit(tmp.path());

    assert_success(&rustygit(&["branch", "feature"], tmp.path()), "br");
    assert_success(
        &rustygit(&["checkout", "feature"], tmp.path()),
        "co feature",
    );
    std::fs::write(tmp.path().join("a.txt"), b"v2\n").unwrap();
    assert_success(&rustygit(&["add", "."], tmp.path()), "add");
    assert_success(
        &rustygit(&["commit", "-m", "feature commit"], tmp.path()),
        "c",
    );

    assert_success(&rustygit(&["checkout", "master"], tmp.path()), "co master");
    std::fs::write(tmp.path().join("a.txt"), b"local\n").unwrap();
    assert_success(
        &rustygit(&["checkout", "-f", "feature"], tmp.path()),
        "co -f",
    );
    // Forced — local content lost.
    assert_eq!(std::fs::read(tmp.path().join("a.txt")).unwrap(), b"v2\n");
}

#[test]
fn switch_create_new_branch() {
    if !has_system_git() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    assert_success(&rustygit(&["init", "-q", "."], tmp.path()), "init");
    make_initial_commit(tmp.path());

    assert_success(
        &rustygit(&["switch", "-c", "topic"], tmp.path()),
        "switch -c topic",
    );

    let h = git(&["rev-parse", "--abbrev-ref", "HEAD"], tmp.path());
    assert_eq!(String::from_utf8(h.stdout).unwrap().trim(), "topic");
}

#[test]
fn reset_soft_only_moves_head() {
    if !has_system_git() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    assert_success(&rustygit(&["init", "-q", "."], tmp.path()), "init");
    make_initial_commit(tmp.path());
    std::fs::write(tmp.path().join("a.txt"), b"v2\n").unwrap();
    assert_success(&rustygit(&["add", "."], tmp.path()), "add");
    assert_success(&rustygit(&["commit", "-m", "c2"], tmp.path()), "commit");

    let head_before = git(&["rev-parse", "HEAD"], tmp.path());
    let head_before = String::from_utf8(head_before.stdout)
        .unwrap()
        .trim()
        .to_string();

    assert_success(
        &rustygit(&["reset", "--soft", "HEAD~1"], tmp.path()),
        "reset --soft",
    );

    // HEAD moved.
    let head_after = git(&["rev-parse", "HEAD"], tmp.path());
    assert_ne!(
        String::from_utf8(head_after.stdout).unwrap().trim(),
        head_before
    );
    // Workdir unchanged.
    assert_eq!(std::fs::read(tmp.path().join("a.txt")).unwrap(), b"v2\n");
    // Index still has the staged change → status shows it as staged.
    let st = String::from_utf8(rustygit(&["status", "--porcelain"], tmp.path()).stdout).unwrap();
    assert!(
        st.contains("M  a.txt"),
        "expected staged-modified, got: {st}"
    );
}

#[test]
fn reset_hard_resets_workdir_and_index() {
    if !has_system_git() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    assert_success(&rustygit(&["init", "-q", "."], tmp.path()), "init");
    make_initial_commit(tmp.path());
    std::fs::write(tmp.path().join("a.txt"), b"v2\n").unwrap();
    assert_success(&rustygit(&["add", "."], tmp.path()), "add");
    assert_success(&rustygit(&["commit", "-m", "c2"], tmp.path()), "commit");

    assert_success(
        &rustygit(&["reset", "--hard", "HEAD~1"], tmp.path()),
        "reset --hard",
    );

    assert_eq!(std::fs::read(tmp.path().join("a.txt")).unwrap(), b"v1\n");
    let st = String::from_utf8(rustygit(&["status", "--porcelain"], tmp.path()).stdout).unwrap();
    assert!(
        st.is_empty() || st.trim().is_empty(),
        "status should be clean, got: {st}"
    );
}

#[test]
fn restore_workdir_from_index() {
    if !has_system_git() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    assert_success(&rustygit(&["init", "-q", "."], tmp.path()), "init");
    make_initial_commit(tmp.path());

    // Modify in workdir without staging.
    std::fs::write(tmp.path().join("a.txt"), b"dirty\n").unwrap();
    assert_eq!(std::fs::read(tmp.path().join("a.txt")).unwrap(), b"dirty\n");

    assert_success(&rustygit(&["restore", "a.txt"], tmp.path()), "restore");
    // File reverted to indexed version.
    assert_eq!(std::fs::read(tmp.path().join("a.txt")).unwrap(), b"v1\n");
}
