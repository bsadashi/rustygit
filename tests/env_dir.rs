//! `GIT_DIR` / `GIT_WORK_TREE` / `GIT_INDEX_FILE` env-var honoring (A6).
//!
//! Upstream git's wrapper scripts and IDEs commonly set these three vars to
//! decouple the gitdir, working tree, and index file from the process's
//! `$cwd`. `rustygit` needs to behave identically:
//!
//! * `GIT_DIR` short-circuits `Repository::discover_from_cwd` — the parent-dir
//!   walk is skipped, the named path is opened directly.
//! * `GIT_WORK_TREE` overrides the workdir we'd otherwise derive from
//!   `gitdir`'s parent.
//! * `GIT_INDEX_FILE` overrides `repo.index_path()` so reads (`status`) and
//!   writes (`add`) hit an alternate index file.
//!
//! Every test runs from an unrelated `$cwd` (a third tempdir) to prove that
//! the env vars — not directory discovery — are what locate the repo.

mod common;

use std::path::Path;
use std::process::Output;

use assert_cmd::Command as AssertCmd;
use tempfile::TempDir;

fn rustygit_in(cwd: &Path) -> AssertCmd {
    let mut cmd = AssertCmd::cargo_bin("rustygit").unwrap();
    cmd.current_dir(cwd)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t");
    cmd
}

fn assert_ok(out: &Output, label: &str) {
    assert!(
        out.status.success(),
        "{label} failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// `GIT_DIR` + `GIT_WORK_TREE` together let us run `status` from an
/// unrelated `$cwd`. The status engine reads HEAD/refs from `$GIT_DIR` and
/// scans `$GIT_WORK_TREE` for tracked files.
#[test]
fn git_dir_and_work_tree_resolve_from_unrelated_cwd() {
    let repo_dir = TempDir::new().unwrap();
    let unrelated_cwd = TempDir::new().unwrap();

    // Initialize a fresh repo at repo_dir.
    let init = rustygit_in(repo_dir.path())
        .args(["init", "-q", "."])
        .output()
        .unwrap();
    assert_ok(&init, "init");

    // Stage a file so status has something to talk about — but do it via
    // GIT_DIR/GIT_WORK_TREE from the unrelated cwd, proving the env vars
    // are what drives discovery.
    std::fs::write(repo_dir.path().join("hello.txt"), b"hello\n").unwrap();
    let add = rustygit_in(unrelated_cwd.path())
        .env("GIT_DIR", repo_dir.path().join(".git"))
        .env("GIT_WORK_TREE", repo_dir.path())
        .args(["add", "hello.txt"])
        .output()
        .unwrap();
    assert_ok(&add, "GIT_DIR/GIT_WORK_TREE add from unrelated cwd");

    let status = rustygit_in(unrelated_cwd.path())
        .env("GIT_DIR", repo_dir.path().join(".git"))
        .env("GIT_WORK_TREE", repo_dir.path())
        .args(["status", "--porcelain"])
        .output()
        .unwrap();
    assert_ok(&status, "GIT_DIR/GIT_WORK_TREE status from unrelated cwd");
    let s = String::from_utf8_lossy(&status.stdout);
    assert!(
        s.contains("A  hello.txt"),
        "status should report staged hello.txt, got: {s:?}"
    );
}

/// `GIT_INDEX_FILE` redirects index reads and writes to an alternate path
/// while the gitdir and worktree are unchanged. Writing via `add` with the
/// env set, then reading via `status` with the SAME env, must see each other.
/// The default index file must be untouched.
#[test]
fn git_index_file_redirects_add_and_status() {
    let repo_dir = TempDir::new().unwrap();

    let init = rustygit_in(repo_dir.path())
        .args(["init", "-q", "."])
        .output()
        .unwrap();
    assert_ok(&init, "init");

    std::fs::write(repo_dir.path().join("a.txt"), b"a\n").unwrap();

    // Pick an alt path OUTSIDE the gitdir so we can tell it apart from the
    // default. Using a sibling tempdir keeps cleanup clean.
    let alt_dir = TempDir::new().unwrap();
    let alt_index = alt_dir.path().join("custom-index");

    // Add with GIT_INDEX_FILE set — writes go to the alt path.
    let add = rustygit_in(repo_dir.path())
        .env("GIT_INDEX_FILE", &alt_index)
        .args(["add", "a.txt"])
        .output()
        .unwrap();
    assert_ok(&add, "add with GIT_INDEX_FILE");

    assert!(
        alt_index.exists(),
        "GIT_INDEX_FILE path should have been created by add"
    );
    let default_index = repo_dir.path().join(".git/index");
    assert!(
        !default_index.exists(),
        "default index file should be untouched when GIT_INDEX_FILE is set"
    );

    // Status with the SAME alt path must see the staged file.
    let status = rustygit_in(repo_dir.path())
        .env("GIT_INDEX_FILE", &alt_index)
        .args(["status", "--porcelain"])
        .output()
        .unwrap();
    assert_ok(&status, "status with GIT_INDEX_FILE");
    let s = String::from_utf8_lossy(&status.stdout);
    assert!(
        s.contains("A  a.txt"),
        "alt-index status should see staged a.txt, got: {s:?}"
    );

    // Status with NO env set must see a.txt as Untracked (the default index
    // is empty since we redirected the add). This is the contrapositive.
    let status_default = rustygit_in(repo_dir.path())
        .args(["status", "--porcelain"])
        .output()
        .unwrap();
    assert_ok(&status_default, "status without env");
    let s2 = String::from_utf8_lossy(&status_default.stdout);
    assert!(
        s2.contains("?? a.txt"),
        "default-index status should see a.txt as untracked, got: {s2:?}"
    );
}

/// Baseline: with none of the env vars set, behavior is unchanged — `status`
/// run from inside the repo finds it via the parent-dir walk. This guards
/// against regressions where the env-var paths accidentally consume the
/// non-env path.
#[test]
fn no_env_vars_behaves_unchanged() {
    let repo_dir = TempDir::new().unwrap();

    let init = rustygit_in(repo_dir.path())
        .args(["init", "-q", "."])
        .output()
        .unwrap();
    assert_ok(&init, "init");

    std::fs::write(repo_dir.path().join("z.txt"), b"z\n").unwrap();

    // Explicitly clear all three vars so a polluted host env can't fool us.
    let status = rustygit_in(repo_dir.path())
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .args(["status", "--porcelain"])
        .output()
        .unwrap();
    assert_ok(&status, "status no-env");
    let s = String::from_utf8_lossy(&status.stdout);
    assert!(
        s.contains("?? z.txt"),
        "default behavior should see z.txt as untracked, got: {s:?}"
    );
}

/// A `GIT_DIR` pointing at a nonexistent path must produce a clean repo-not-found
/// error, not panic or crash. We don't oracle the exact message — just that
/// the process exits non-zero with something on stderr.
#[test]
fn git_dir_pointing_at_missing_path_errors_cleanly() {
    let cwd = TempDir::new().unwrap();
    let missing = cwd.path().join("definitely-not-here").join(".git");

    let out = rustygit_in(cwd.path())
        .env("GIT_DIR", &missing)
        .args(["status"])
        .output()
        .unwrap();

    assert!(
        !out.status.success(),
        "status with missing GIT_DIR should fail; got success with stdout: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(
        !out.stderr.is_empty(),
        "expected an error on stderr for missing GIT_DIR"
    );
}
