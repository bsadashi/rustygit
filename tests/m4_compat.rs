//! M4 byte-compatibility for `status`, `rm`, `mv` against system git.

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

#[test]
fn status_untracked_byte_matches_git() {
    if !has_system_git() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    assert_success(&rustygit(&["init", "-q", "."], tmp.path()), "init");
    std::fs::write(tmp.path().join("a.txt"), b"a\n").unwrap();
    std::fs::write(tmp.path().join("b.txt"), b"b\n").unwrap();

    let g = git(&["status", "--porcelain"], tmp.path());
    let r = rustygit(&["status", "--porcelain"], tmp.path());
    assert!(r.status.success());
    assert_eq!(r.stdout, g.stdout, "untracked status byte-match");
}

#[test]
fn status_staged_modified_deleted() {
    if !has_system_git() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    assert_success(&rustygit(&["init", "-q", "."], tmp.path()), "init");
    std::fs::write(tmp.path().join("k.txt"), b"keep\n").unwrap();
    std::fs::write(tmp.path().join("m.txt"), b"mod\n").unwrap();
    std::fs::write(tmp.path().join("d.txt"), b"del\n").unwrap();
    assert_success(&rustygit(&["add", "."], tmp.path()), "add all");
    assert_success(&rustygit(&["commit", "-m", "first"], tmp.path()), "commit");

    // Modify k.txt, leave m.txt clean, delete d.txt, and add a new untracked file.
    std::fs::write(tmp.path().join("k.txt"), b"keep modified\n").unwrap();
    std::fs::remove_file(tmp.path().join("d.txt")).unwrap();
    std::fs::write(tmp.path().join("u.txt"), b"new\n").unwrap();

    let g = git(&["status", "--porcelain"], tmp.path());
    let r = rustygit(&["status", "--porcelain"], tmp.path());
    assert!(r.status.success());
    assert_eq!(
        r.stdout, g.stdout,
        "modified+deleted+untracked status byte-match"
    );
}

#[test]
fn status_staged_then_modified_shows_mm() {
    if !has_system_git() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    assert_success(&rustygit(&["init", "-q", "."], tmp.path()), "init");
    std::fs::write(tmp.path().join("a.txt"), b"orig\n").unwrap();
    assert_success(&rustygit(&["add", "."], tmp.path()), "add");
    assert_success(&rustygit(&["commit", "-m", "first"], tmp.path()), "commit");
    // Modify, stage, modify again.
    std::fs::write(tmp.path().join("a.txt"), b"second\n").unwrap();
    assert_success(&rustygit(&["add", "a.txt"], tmp.path()), "stage v2");
    std::fs::write(tmp.path().join("a.txt"), b"third\n").unwrap();

    let g = git(&["status", "--porcelain"], tmp.path());
    let r = rustygit(&["status", "--porcelain"], tmp.path());
    assert!(r.status.success());
    assert_eq!(r.stdout, g.stdout, "MM status byte-match");
}

#[test]
fn status_respects_gitignore() {
    if !has_system_git() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    assert_success(&rustygit(&["init", "-q", "."], tmp.path()), "init");
    std::fs::write(tmp.path().join("keep.txt"), b"keep").unwrap();
    std::fs::write(tmp.path().join("debug.log"), b"l").unwrap();
    std::fs::write(tmp.path().join("draft.tmp"), b"t").unwrap();
    std::fs::create_dir(tmp.path().join("build")).unwrap();
    std::fs::write(tmp.path().join("build/out.bin"), b"bin").unwrap();
    std::fs::write(tmp.path().join(".gitignore"), b"*.log\n*.tmp\nbuild/\n").unwrap();

    let g = git(&["status", "--porcelain"], tmp.path());
    let r = rustygit(&["status", "--porcelain"], tmp.path());
    assert!(r.status.success());
    assert_eq!(r.stdout, g.stdout, "gitignore-filtered status byte-match");
}

#[test]
fn status_respects_nested_gitignore_negation() {
    if !has_system_git() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    assert_success(&rustygit(&["init", "-q", "."], tmp.path()), "init");

    // Root .gitignore: ignore all .log files
    std::fs::write(tmp.path().join(".gitignore"), b"*.log\n").unwrap();

    // Subdir with a nested .gitignore that re-includes important.log
    std::fs::create_dir(tmp.path().join("sub")).unwrap();
    std::fs::write(tmp.path().join("sub/.gitignore"), b"!important.log\n").unwrap();

    // Files exercising the layered rules
    std::fs::write(tmp.path().join("root.log"), b"r").unwrap(); // ignored
    std::fs::write(tmp.path().join("keep.txt"), b"k").unwrap(); // untracked
    std::fs::write(tmp.path().join("sub/important.log"), b"i").unwrap(); // re-included
    std::fs::write(tmp.path().join("sub/scratch.log"), b"s").unwrap(); // still ignored

    let r = rustygit(&["status", "--porcelain"], tmp.path());
    assert!(r.status.success());
    let stdout = String::from_utf8(r.stdout).unwrap();

    // Untracked SHOULD appear:
    assert!(
        stdout.contains(".gitignore"),
        "root .gitignore missing: {stdout}"
    );
    assert!(stdout.contains("keep.txt"), "keep.txt missing: {stdout}");
    assert!(
        stdout.contains("sub/important.log"),
        "nested-negation re-includes failed: {stdout}"
    );
    assert!(
        stdout.contains("sub/.gitignore"),
        "sub/.gitignore missing: {stdout}"
    );

    // Ignored SHOULD NOT appear:
    assert!(
        !stdout.contains("root.log"),
        "root.log should be ignored: {stdout}"
    );
    assert!(
        !stdout.contains("scratch.log"),
        "sub/scratch.log should still be ignored: {stdout}"
    );
}

#[test]
fn rm_removes_from_index_and_disk() {
    if !has_system_git() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    assert_success(&rustygit(&["init", "-q", "."], tmp.path()), "init");
    std::fs::write(tmp.path().join("a.txt"), b"a\n").unwrap();
    std::fs::write(tmp.path().join("b.txt"), b"b\n").unwrap();
    assert_success(&rustygit(&["add", "."], tmp.path()), "add");
    assert_success(&rustygit(&["commit", "-m", "init"], tmp.path()), "commit");

    let r = rustygit(&["rm", "a.txt"], tmp.path());
    assert_success(&r, "rm");
    assert!(!tmp.path().join("a.txt").exists(), "file should be gone");

    // git ls-files agrees that a.txt is no longer tracked.
    let ls = git(&["ls-files"], tmp.path());
    let listing = String::from_utf8(ls.stdout).unwrap();
    assert!(!listing.contains("a.txt"));
    assert!(listing.contains("b.txt"));

    // Status shows the deletion staged.
    let g = git(&["status", "--porcelain"], tmp.path());
    assert!(
        String::from_utf8_lossy(&g.stdout).contains("D  a.txt"),
        "git should see staged deletion"
    );
}

#[test]
fn rm_cached_keeps_file_on_disk() {
    if !has_system_git() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    assert_success(&rustygit(&["init", "-q", "."], tmp.path()), "init");
    std::fs::write(tmp.path().join("a.txt"), b"a\n").unwrap();
    assert_success(&rustygit(&["add", "."], tmp.path()), "add");
    assert_success(&rustygit(&["commit", "-m", "init"], tmp.path()), "commit");

    let r = rustygit(&["rm", "--cached", "a.txt"], tmp.path());
    assert_success(&r, "rm --cached");
    assert!(tmp.path().join("a.txt").exists(), "file should still exist");

    let ls = git(&["ls-files"], tmp.path());
    let listing = String::from_utf8(ls.stdout).unwrap();
    assert!(!listing.contains("a.txt"), "should not be tracked anymore");
}

#[test]
fn rm_refuses_modified_without_force() {
    if !has_system_git() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    assert_success(&rustygit(&["init", "-q", "."], tmp.path()), "init");
    std::fs::write(tmp.path().join("a.txt"), b"a\n").unwrap();
    assert_success(&rustygit(&["add", "."], tmp.path()), "add");
    assert_success(&rustygit(&["commit", "-m", "init"], tmp.path()), "commit");
    // Modify the file and try to rm it.
    std::fs::write(tmp.path().join("a.txt"), b"modified\n").unwrap();

    let r = rustygit(&["rm", "a.txt"], tmp.path());
    assert!(!r.status.success(), "should refuse to rm modified file");
    assert!(tmp.path().join("a.txt").exists(), "file must still exist");

    // With -f, should succeed.
    let r = rustygit(&["rm", "-f", "a.txt"], tmp.path());
    assert_success(&r, "rm -f");
    assert!(!tmp.path().join("a.txt").exists());
}

#[test]
fn mv_renames_and_updates_index() {
    if !has_system_git() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    assert_success(&rustygit(&["init", "-q", "."], tmp.path()), "init");
    std::fs::write(tmp.path().join("old.txt"), b"contents\n").unwrap();
    assert_success(&rustygit(&["add", "."], tmp.path()), "add");
    assert_success(&rustygit(&["commit", "-m", "init"], tmp.path()), "commit");

    let r = rustygit(&["mv", "old.txt", "new.txt"], tmp.path());
    assert_success(&r, "mv");
    assert!(!tmp.path().join("old.txt").exists());
    assert!(tmp.path().join("new.txt").exists());

    // Index should now have new.txt instead of old.txt.
    let ls = git(&["ls-files"], tmp.path());
    let listing = String::from_utf8(ls.stdout).unwrap();
    assert!(!listing.contains("old.txt"));
    assert!(listing.contains("new.txt"));
}

#[test]
fn mv_into_directory() {
    if !has_system_git() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    assert_success(&rustygit(&["init", "-q", "."], tmp.path()), "init");
    std::fs::write(tmp.path().join("a.txt"), b"a\n").unwrap();
    std::fs::write(tmp.path().join("b.txt"), b"b\n").unwrap();
    std::fs::create_dir(tmp.path().join("sub")).unwrap();
    assert_success(&rustygit(&["add", "."], tmp.path()), "add");
    assert_success(&rustygit(&["commit", "-m", "init"], tmp.path()), "commit");

    let r = rustygit(&["mv", "a.txt", "b.txt", "sub"], tmp.path());
    assert_success(&r, "mv into sub/");
    assert!(tmp.path().join("sub/a.txt").exists());
    assert!(tmp.path().join("sub/b.txt").exists());
    assert!(!tmp.path().join("a.txt").exists());
    assert!(!tmp.path().join("b.txt").exists());
}

#[test]
fn mv_refuses_to_clobber_existing() {
    if !has_system_git() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    assert_success(&rustygit(&["init", "-q", "."], tmp.path()), "init");
    std::fs::write(tmp.path().join("a.txt"), b"a\n").unwrap();
    std::fs::write(tmp.path().join("b.txt"), b"b\n").unwrap();
    assert_success(&rustygit(&["add", "."], tmp.path()), "add");
    assert_success(&rustygit(&["commit", "-m", "init"], tmp.path()), "commit");

    let r = rustygit(&["mv", "a.txt", "b.txt"], tmp.path());
    assert!(!r.status.success(), "should refuse to clobber existing dst");
    assert!(tmp.path().join("a.txt").exists());
    assert!(tmp.path().join("b.txt").exists());
}

// ----------------------------------------------------------------------------
// Human-readable status — `git status` with no flag.
// ----------------------------------------------------------------------------

/// Default `rustygit status` (no flag) should byte-match `git status` on a
/// freshly-initialized empty repo.
///
/// The header includes `On branch <name>` + `No commits yet` + the standard
/// "nothing to commit" footer. The branch name is whichever default
/// `git init` chose — git 2.30+ honors `init.defaultBranch` (commonly `main`)
/// but distros may still default to `master`. We use whatever the system git
/// produced when we ran `git init`.
#[test]
fn status_human_form_byte_matches_git_on_empty_repo() {
    if !has_system_git() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    // Initialize via system git so the branch name and gitdir layout are
    // canonical for whatever git version is on PATH.
    let g_init = git(&["init", "-q", "."], tmp.path());
    assert!(g_init.status.success(), "git init failed");

    let g = git(&["status"], tmp.path());
    let r = rustygit(&["status"], tmp.path());
    assert!(r.status.success(), "rustygit status failed");
    assert_eq!(
        r.stdout,
        g.stdout,
        "empty-repo human status mismatch\nours:\n{}\ntheirs:\n{}",
        String::from_utf8_lossy(&r.stdout),
        String::from_utf8_lossy(&g.stdout),
    );
}

/// Default `rustygit status` (no flag) byte-matches `git status` after the
/// first commit on a clean tree ("nothing to commit, working tree clean").
#[test]
fn status_human_form_byte_matches_git_on_clean_tree() {
    if !has_system_git() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    assert_success(&rustygit(&["init", "-q", "."], tmp.path()), "init");
    std::fs::write(tmp.path().join("a.txt"), b"a\n").unwrap();
    assert_success(&rustygit(&["add", "."], tmp.path()), "add");
    assert_success(&rustygit(&["commit", "-m", "first"], tmp.path()), "commit");

    let g = git(&["status"], tmp.path());
    let r = rustygit(&["status"], tmp.path());
    assert!(r.status.success(), "rustygit status failed");
    assert_eq!(
        r.stdout,
        g.stdout,
        "clean-tree human status mismatch\nours:\n{}\ntheirs:\n{}",
        String::from_utf8_lossy(&r.stdout),
        String::from_utf8_lossy(&g.stdout),
    );
}

/// After modifying a tracked file the Human form shows the
/// "Changes not staged for commit" section with `modified:   a.txt`.
#[test]
fn status_human_form_byte_matches_git_with_unstaged_mod() {
    if !has_system_git() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    assert_success(&rustygit(&["init", "-q", "."], tmp.path()), "init");
    std::fs::write(tmp.path().join("a.txt"), b"a\n").unwrap();
    assert_success(&rustygit(&["add", "."], tmp.path()), "add");
    assert_success(&rustygit(&["commit", "-m", "first"], tmp.path()), "commit");
    // Modify the tracked file.
    std::fs::write(tmp.path().join("a.txt"), b"a-modified\n").unwrap();

    let g = git(&["status"], tmp.path());
    let r = rustygit(&["status"], tmp.path());
    assert!(r.status.success(), "rustygit status failed");
    assert_eq!(
        r.stdout,
        g.stdout,
        "unstaged-mod human status mismatch\nours:\n{}\ntheirs:\n{}",
        String::from_utf8_lossy(&r.stdout),
        String::from_utf8_lossy(&g.stdout),
    );
}

/// Untracked files appear in the "Untracked files:" section in the Human
/// form (no staging, no prior commit needed beyond an initial one).
#[test]
fn status_human_form_byte_matches_git_with_untracked() {
    if !has_system_git() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    assert_success(&rustygit(&["init", "-q", "."], tmp.path()), "init");
    std::fs::write(tmp.path().join("a.txt"), b"a\n").unwrap();
    assert_success(&rustygit(&["add", "."], tmp.path()), "add");
    assert_success(&rustygit(&["commit", "-m", "first"], tmp.path()), "commit");
    // Drop a brand new file with no `add`.
    std::fs::write(tmp.path().join("u.txt"), b"u\n").unwrap();

    let g = git(&["status"], tmp.path());
    let r = rustygit(&["status"], tmp.path());
    assert!(r.status.success(), "rustygit status failed");
    assert_eq!(
        r.stdout,
        g.stdout,
        "untracked human status mismatch\nours:\n{}\ntheirs:\n{}",
        String::from_utf8_lossy(&r.stdout),
        String::from_utf8_lossy(&g.stdout),
    );
}
