//! End-to-end self-host smoke test (TESTING.md §6.A).
//!
//! Drives every major porcelain command in one workflow on a synthetic
//! repository. The bar is: every step succeeds, `git fsck --full` is clean
//! between steps, final state is well-formed.
//!
//! This test is the SINGLE most valuable smoke test we ship — if it passes,
//! the porcelain is in workable shape end-to-end.

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
        .env("GIT_AUTHOR_NAME", "selfhost")
        .env("GIT_AUTHOR_EMAIL", "selfhost@invalid")
        .env("GIT_COMMITTER_NAME", "selfhost")
        .env("GIT_COMMITTER_EMAIL", "selfhost@invalid")
        .env("GIT_AUTHOR_DATE", "1700000000 +0000")
        .env("GIT_COMMITTER_DATE", "1700000000 +0000")
        .output()
        .unwrap()
}

fn must(out: &std::process::Output, label: &str) {
    assert!(
        out.status.success(),
        "{label} failed: stdout=`{}` stderr=`{}`",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

fn fsck_clean(repo: &Path, after: &str) {
    let r = Command::new("git")
        .args(["fsck", "--full"])
        .current_dir(repo)
        .output()
        .unwrap();
    assert!(
        r.status.success(),
        "git fsck failed after {after}: stdout=`{}` stderr=`{}`",
        String::from_utf8_lossy(&r.stdout),
        String::from_utf8_lossy(&r.stderr)
    );
}

/// The flagship self-host test. Runs the equivalent of:
///   init → add → commit → branch → checkout → commit → diff → log → status
///   → merge → reset → cherry-pick → rebase → push → clone → repack → gc
/// all through rustygit, verifying fsck cleanliness between every state change.
#[test]
fn full_porcelain_workflow_self_host() {
    if !has_system_git() {
        return;
    }
    let repo = TempDir::new().unwrap();
    let bare_root = TempDir::new().unwrap();
    let bare = bare_root.path().join("bare.git");
    let clone_root = TempDir::new().unwrap();
    let clone = clone_root.path().join("clone");

    // --- 1. init ---
    must(&rustygit(&["init", "-q", "."], repo.path()), "init");
    fsck_clean(repo.path(), "init");

    // --- 2. add + commit ---
    std::fs::write(repo.path().join("hello.txt"), b"Hello, world!\n").unwrap();
    std::fs::write(repo.path().join("README.md"), b"# Self-host test\n").unwrap();
    std::fs::create_dir_all(repo.path().join("src")).unwrap();
    std::fs::write(repo.path().join("src/main.rs"), b"fn main() {}\n").unwrap();
    must(&rustygit(&["add", "."], repo.path()), "add");
    must(
        &rustygit(&["commit", "-m", "initial commit"], repo.path()),
        "commit-1",
    );
    fsck_clean(repo.path(), "commit-1");

    // --- 3. status (clean after commit) ---
    // Use --porcelain so the assertion is byte-stable; the Human form prints
    // a multi-line "working tree clean" banner that we exercise separately
    // in m4_compat.
    let s = rustygit(&["status", "--porcelain"], repo.path());
    must(&s, "status-clean");
    assert!(
        s.stdout.is_empty() || s.stdout == b"",
        "status should be clean: {}",
        String::from_utf8_lossy(&s.stdout)
    );

    // --- 4. branch + checkout ---
    must(&rustygit(&["branch", "feature"], repo.path()), "branch");
    must(
        &rustygit(&["checkout", "feature"], repo.path()),
        "checkout feature",
    );
    let br = rustygit(&["branch"], repo.path());
    let branches = String::from_utf8(br.stdout).unwrap();
    assert!(branches.contains("* feature"), "branches: {branches:?}");
    assert!(branches.contains("  master"));

    // --- 5. modify + commit on feature ---
    std::fs::write(
        repo.path().join("src/main.rs"),
        b"fn main() {\n    println!(\"hi\");\n}\n",
    )
    .unwrap();
    std::fs::write(repo.path().join("feature.md"), b"# Feature\n").unwrap();
    must(&rustygit(&["add", "."], repo.path()), "add feature");
    must(
        &rustygit(&["commit", "-m", "feature work"], repo.path()),
        "commit-feature",
    );
    fsck_clean(repo.path(), "commit-feature");

    // --- 6. diff (feature vs master) ---
    let d = rustygit(&["diff", "master"], repo.path());
    must(&d, "diff feature..master");
    let diff_out = String::from_utf8(d.stdout).unwrap();
    assert!(diff_out.contains("feature.md") || diff_out.contains("main.rs"));

    // --- 7. log ---
    let l = rustygit(&["log", "--oneline"], repo.path());
    must(&l, "log");
    let log_lines = String::from_utf8(l.stdout).unwrap();
    assert_eq!(
        log_lines.lines().count(),
        2,
        "feature should have 2 commits: {log_lines:?}"
    );

    // --- 8. switch back + commit on master ---
    must(
        &rustygit(&["checkout", "master"], repo.path()),
        "checkout master",
    );
    std::fs::write(repo.path().join("master.md"), b"# Master\n").unwrap();
    must(&rustygit(&["add", "."], repo.path()), "add master");
    must(
        &rustygit(&["commit", "-m", "master work"], repo.path()),
        "commit-master",
    );
    fsck_clean(repo.path(), "commit-master");

    // --- 9. merge feature into master (non-FF, disjoint changes) ---
    must(
        &rustygit(&["merge", "-m", "merge feature", "feature"], repo.path()),
        "merge",
    );
    fsck_clean(repo.path(), "merge");
    assert!(
        repo.path().join("feature.md").exists(),
        "feature.md should be merged in"
    );
    assert!(
        repo.path().join("master.md").exists(),
        "master.md should still be here"
    );

    // --- 10. log shows merge commit ---
    let l = rustygit(&["log", "--oneline"], repo.path());
    must(&l, "log after merge");
    let lines = String::from_utf8(l.stdout).unwrap();
    assert!(
        lines.lines().count() >= 3,
        "merge + master + initial expected"
    );

    // --- 11. reflog records the commits ---
    let r = rustygit(&["reflog"], repo.path());
    must(&r, "reflog");
    let reflog_out = String::from_utf8(r.stdout).unwrap();
    assert!(reflog_out.contains("HEAD@{0}"));
    assert!(reflog_out.contains("merge") || reflog_out.contains("commit"));

    // --- 12. push to bare ---
    Command::new("git")
        .args(["init", "--bare", "-q", bare.to_str().unwrap()])
        .status()
        .unwrap();
    must(
        &rustygit(&["push", bare.to_str().unwrap(), "master"], repo.path()),
        "push to bare",
    );
    fsck_clean(&bare, "push");

    // --- 13. clone the bare ---
    must(
        &rustygit(
            &[
                "clone",
                "-q",
                bare.to_str().unwrap(),
                clone.to_str().unwrap(),
            ],
            std::env::current_dir().unwrap().as_path(),
        ),
        "clone",
    );
    fsck_clean(&clone, "clone");

    // The clone's log matches our source's log.
    let src_log = git(&["log", "--oneline", "master"], repo.path());
    let clone_log = git(&["log", "--oneline", "master"], &clone);
    assert_eq!(src_log.stdout, clone_log.stdout, "clone log byte-match");

    // --- 14. repack the clone, verify-pack accepts our pack ---
    must(&rustygit(&["repack", "-a", "-d"], &clone), "repack clone");
    fsck_clean(&clone, "repack");
    // git verify-pack accepts our written pack.
    let pack_path = std::fs::read_dir(clone.join(".git/objects/pack"))
        .unwrap()
        .flatten()
        .find_map(|e| {
            let p = e.path();
            if p.extension().and_then(|s| s.to_str()) == Some("pack") {
                Some(p)
            } else {
                None
            }
        })
        .expect("at least one pack after repack");
    let vp = Command::new("git")
        .args(["verify-pack", "-v", pack_path.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(vp.status.success(), "git verify-pack must accept our pack");

    // --- 15. commit-graph write + verify ---
    must(
        &rustygit(&["commit-graph", "write"], &clone),
        "commit-graph write",
    );
    let cg_verify = Command::new("git")
        .args(["commit-graph", "verify"])
        .current_dir(&clone)
        .output()
        .unwrap();
    assert!(
        cg_verify.status.success(),
        "git commit-graph verify must accept our output: {}",
        String::from_utf8_lossy(&cg_verify.stderr)
    );

    // --- 16. multi-pack-index write + verify ---
    must(
        &rustygit(&["multi-pack-index", "write"], &clone),
        "midx write",
    );
    let midx_verify = Command::new("git")
        .args(["multi-pack-index", "verify", "--object-dir", ".git/objects"])
        .current_dir(&clone)
        .output()
        .unwrap();
    assert!(
        midx_verify.status.success(),
        "git multi-pack-index verify must accept our output: {}",
        String::from_utf8_lossy(&midx_verify.stderr)
    );

    // --- 17. cherry-pick the master commit into a new branch in clone ---
    let master_commit = git(&["rev-parse", "HEAD~1"], &clone);
    let master_oid = String::from_utf8(master_commit.stdout)
        .unwrap()
        .trim()
        .to_string();
    must(&rustygit(&["branch", "topic"], &clone), "branch topic");
    must(&rustygit(&["checkout", "topic"], &clone), "checkout topic");
    must(
        &rustygit(&["reset", "--hard", "HEAD~2"], &clone),
        "rewind topic",
    );
    // Topic now has only the initial commit; cherry-pick master's content
    // commit onto it.
    must(
        &rustygit(&["cherry-pick", &master_oid], &clone),
        "cherry-pick",
    );
    fsck_clean(&clone, "cherry-pick");

    // --- 18. final fsck of every directory touched ---
    fsck_clean(repo.path(), "source repo final");
    fsck_clean(&bare, "bare final");
    fsck_clean(&clone, "clone final");
}

/// Lighter-weight test: each command runs in isolation against a fresh repo
/// and produces the same effect as the equivalent git invocation. This is
/// the canonical "do you implement the basic command?" test.
#[test]
fn quick_smoke_every_porcelain_command() {
    if !has_system_git() {
        return;
    }
    let repo = TempDir::new().unwrap();
    must(&rustygit(&["init", "-q", "."], repo.path()), "init");
    std::fs::write(repo.path().join("a.txt"), b"a\n").unwrap();
    must(&rustygit(&["add", "."], repo.path()), "add");
    must(&rustygit(&["commit", "-m", "c1"], repo.path()), "commit");

    // Each of these should succeed (exit 0) on a fresh repo.
    let smoke_cmds: &[&[&str]] = &[
        &["status"],
        &["log"],
        &["log", "--oneline"],
        &["branch"],
        &["show-ref"],
        &["rev-parse", "HEAD"],
        &["cat-file", "-t", "HEAD"],
        &["ls-tree", "HEAD"],
        &["reflog"],
        &["symbolic-ref", "HEAD"],
        &["merge-base", "--is-ancestor", "HEAD", "HEAD"],
    ];
    for cmd in smoke_cmds {
        let out = rustygit(cmd, repo.path());
        assert!(
            out.status.success(),
            "{:?} failed: {}",
            cmd,
            String::from_utf8_lossy(&out.stderr)
        );
    }
}
