//! M11 compatibility: push to local bare repos.
//!
//! HTTPS push isn't tested here (no test server). The wire protocol is
//! exercised by unit tests in `transport::send_pack`; this suite verifies
//! the local-bare backend end-to-end against system git.

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

fn assert_success(out: &std::process::Output, label: &str) {
    assert!(
        out.status.success(),
        "{label} failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

fn make_source_repo(at: &Path, commits: usize) {
    git(&["init", "-q", "."], at);
    for i in 0..commits {
        std::fs::write(at.join("f.txt"), format!("v{i}\n")).unwrap();
        git(&["add", "f.txt"], at);
        git(
            &[
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "commit",
                "-q",
                "-m",
                &format!("c{i}"),
            ],
            at,
        );
    }
}

#[test]
fn push_to_bare_creates_branch() {
    if !has_system_git() {
        return;
    }
    let src = TempDir::new().unwrap();
    make_source_repo(src.path(), 2);

    let dst_root = TempDir::new().unwrap();
    let dst = dst_root.path().join("dst.git");
    Command::new("git")
        .args(["init", "--bare", "-q", dst.to_str().unwrap()])
        .status()
        .unwrap();

    assert_success(
        &rustygit(&["push", dst.to_str().unwrap(), "master"], src.path()),
        "push",
    );

    // git can read the pushed ref.
    let refs = Command::new("git")
        .args(["--git-dir", dst.to_str().unwrap(), "show-ref"])
        .output()
        .unwrap();
    let listing = String::from_utf8(refs.stdout).unwrap();
    assert!(listing.contains("refs/heads/master"));

    // git fsck must be happy.
    let fsck = Command::new("git")
        .args(["--git-dir", dst.to_str().unwrap(), "fsck", "--full"])
        .output()
        .unwrap();
    assert!(
        fsck.status.success(),
        "fsck failed: {}",
        String::from_utf8_lossy(&fsck.stderr)
    );

    // Source and destination logs match.
    let src_log = git(&["log", "--oneline", "master"], src.path());
    let dst_log = Command::new("git")
        .args([
            "--git-dir",
            dst.to_str().unwrap(),
            "log",
            "--oneline",
            "master",
        ])
        .output()
        .unwrap();
    assert_eq!(src_log.stdout, dst_log.stdout);
}

#[test]
fn push_fast_forwards_existing_branch() {
    if !has_system_git() {
        return;
    }
    let src = TempDir::new().unwrap();
    make_source_repo(src.path(), 2);

    let dst_root = TempDir::new().unwrap();
    let dst = dst_root.path().join("dst.git");
    Command::new("git")
        .args(["init", "--bare", "-q", dst.to_str().unwrap()])
        .status()
        .unwrap();

    assert_success(
        &rustygit(&["push", dst.to_str().unwrap(), "master"], src.path()),
        "first push",
    );

    // Add a commit, push again.
    std::fs::write(src.path().join("f.txt"), b"after-push\n").unwrap();
    git(&["add", "f.txt"], src.path());
    git(
        &[
            "-c",
            "user.email=t@t",
            "-c",
            "user.name=t",
            "commit",
            "-q",
            "-m",
            "c-ff",
        ],
        src.path(),
    );
    assert_success(
        &rustygit(&["push", dst.to_str().unwrap(), "master"], src.path()),
        "ff push",
    );

    // The destination has the new tip.
    let head = Command::new("git")
        .args(["--git-dir", dst.to_str().unwrap(), "rev-parse", "master"])
        .output()
        .unwrap();
    let src_head = git(&["rev-parse", "master"], src.path());
    assert_eq!(head.stdout, src_head.stdout);
}

#[test]
fn push_refuses_non_fast_forward_without_force() {
    if !has_system_git() {
        return;
    }
    let src = TempDir::new().unwrap();
    make_source_repo(src.path(), 2);

    let dst_root = TempDir::new().unwrap();
    let dst = dst_root.path().join("dst.git");
    Command::new("git")
        .args(["init", "--bare", "-q", dst.to_str().unwrap()])
        .status()
        .unwrap();

    // Push the second commit.
    assert_success(
        &rustygit(&["push", dst.to_str().unwrap(), "master"], src.path()),
        "first push",
    );

    // Rewind src by one commit (creating a divergent history).
    git(&["reset", "--hard", "HEAD~1"], src.path());

    // Now the src tip is BEHIND the dst tip → push should refuse.
    let out = rustygit(&["push", dst.to_str().unwrap(), "master"], src.path());
    assert!(
        !out.status.success(),
        "should refuse non-fast-forward; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // With --force, succeeds.
    assert_success(
        &rustygit(
            &["push", "--force", dst.to_str().unwrap(), "master"],
            src.path(),
        ),
        "force push",
    );
    let dst_head = Command::new("git")
        .args(["--git-dir", dst.to_str().unwrap(), "rev-parse", "master"])
        .output()
        .unwrap();
    let src_head = git(&["rev-parse", "master"], src.path());
    assert_eq!(dst_head.stdout, src_head.stdout);
}

#[test]
fn push_delete_refspec_removes_branch() {
    if !has_system_git() {
        return;
    }
    let src = TempDir::new().unwrap();
    make_source_repo(src.path(), 2);
    git(&["branch", "feature"], src.path());

    let dst_root = TempDir::new().unwrap();
    let dst = dst_root.path().join("dst.git");
    Command::new("git")
        .args(["init", "--bare", "-q", dst.to_str().unwrap()])
        .status()
        .unwrap();

    // Push both branches.
    assert_success(
        &rustygit(
            &["push", dst.to_str().unwrap(), "master", "feature"],
            src.path(),
        ),
        "push both",
    );

    // Delete `feature` on the remote via `:feature`.
    assert_success(
        &rustygit(&["push", dst.to_str().unwrap(), ":feature"], src.path()),
        "delete push",
    );

    let refs = Command::new("git")
        .args(["--git-dir", dst.to_str().unwrap(), "show-ref"])
        .output()
        .unwrap();
    let listing = String::from_utf8(refs.stdout).unwrap();
    assert!(listing.contains("refs/heads/master"));
    assert!(!listing.contains("refs/heads/feature"));
}

#[test]
fn push_multiple_refs_in_one_command() {
    if !has_system_git() {
        return;
    }
    let src = TempDir::new().unwrap();
    make_source_repo(src.path(), 2);
    git(&["branch", "feature"], src.path());
    git(&["branch", "develop"], src.path());

    let dst_root = TempDir::new().unwrap();
    let dst = dst_root.path().join("dst.git");
    Command::new("git")
        .args(["init", "--bare", "-q", dst.to_str().unwrap()])
        .status()
        .unwrap();

    assert_success(
        &rustygit(
            &[
                "push",
                dst.to_str().unwrap(),
                "master",
                "feature",
                "develop",
            ],
            src.path(),
        ),
        "multi push",
    );

    let refs = Command::new("git")
        .args(["--git-dir", dst.to_str().unwrap(), "show-ref"])
        .output()
        .unwrap();
    let listing = String::from_utf8(refs.stdout).unwrap();
    for name in [
        "refs/heads/master",
        "refs/heads/feature",
        "refs/heads/develop",
    ] {
        assert!(listing.contains(name), "missing {name} in {listing}");
    }
}

#[test]
fn push_round_trip_via_clone() {
    if !has_system_git() {
        return;
    }
    let src = TempDir::new().unwrap();
    make_source_repo(src.path(), 3);

    let dst_root = TempDir::new().unwrap();
    let dst = dst_root.path().join("hub.git");
    Command::new("git")
        .args(["init", "--bare", "-q", dst.to_str().unwrap()])
        .status()
        .unwrap();

    assert_success(
        &rustygit(&["push", dst.to_str().unwrap(), "master"], src.path()),
        "push",
    );

    // Clone the bare repo back out via rustygit; logs must match.
    let cloned_root = TempDir::new().unwrap();
    let cloned = cloned_root.path().join("back");
    assert_success(
        &rustygit(
            &[
                "clone",
                "-q",
                dst.to_str().unwrap(),
                cloned.to_str().unwrap(),
            ],
            std::env::current_dir().unwrap().as_path(),
        ),
        "clone back",
    );

    let src_log = git(&["log", "--oneline", "master"], src.path());
    let cloned_log = git(&["log", "--oneline", "master"], &cloned);
    assert_eq!(src_log.stdout, cloned_log.stdout);
}
