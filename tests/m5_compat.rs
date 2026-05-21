//! M5 byte-compatibility for `diff` family commands against system git.
//!
//! These tests run `git init` + `git add` + `git commit` to lay down a known
//! repository state, then compare `rustygit diff <a> <b>` output against
//! `git diff <a> <b>`. They will only execute once Track A's `xdiff` lands and
//! `lib.rs` / `cli/mod.rs` are wired up — until then, `cargo test` will not
//! compile this file because `rustygit diff` doesn't exist yet.

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

#[allow(dead_code)]
fn assert_success(out: &std::process::Output, label: &str) {
    assert!(
        out.status.success(),
        "{label} failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Run `git rev-parse HEAD` and return the OID as a String.
fn git_head_oid(cwd: &Path) -> String {
    let out = git(&["rev-parse", "HEAD"], cwd);
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

/// Initialize a fresh repo with two commits that exhibit a particular shape.
/// Returns (commit-a-oid, commit-b-oid) so the diff tests have something
/// stable to compare.
fn init_two_commits(
    cwd: &Path,
    before: &[(&str, &[u8])],
    after: &[(&str, &[u8])],
) -> (String, String) {
    // Use system git for setup so the index/commit format is canonical and we
    // know diff inputs are byte-equal across implementations.
    let _ = git(&["init", "-q", "-b", "main", "."], cwd);
    let _ = git(&["config", "user.name", "T"], cwd);
    let _ = git(&["config", "user.email", "t@t"], cwd);

    for (path, content) in before {
        let abs = cwd.join(path);
        if let Some(parent) = abs.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&abs, content).unwrap();
        git(&["add", path], cwd);
    }
    git(&["commit", "-q", "-m", "first"], cwd);
    let a = git_head_oid(cwd);

    // Apply the "after" state. Files present in `after` get their content
    // replaced (or are added if new); files in `before` but not in `after`
    // are deleted.
    let after_paths: std::collections::HashSet<&str> = after.iter().map(|(p, _)| *p).collect();
    for (path, _) in before {
        if !after_paths.contains(path) {
            // `git rm` removes from both disk and index, so we don't pre-delete.
            git(&["rm", "-q", path], cwd);
        }
    }
    for (path, content) in after {
        let abs = cwd.join(path);
        if let Some(parent) = abs.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&abs, content).unwrap();
        git(&["add", path], cwd);
    }
    git(&["commit", "-q", "-m", "second"], cwd);
    let b = git_head_oid(cwd);

    (a, b)
}

#[test]
fn diff_modified_text_file_byte_matches_git() {
    if !has_system_git() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    let (a, b) = init_two_commits(
        tmp.path(),
        &[("a.txt", b"line one\nline two\nline three\n")],
        &[("a.txt", b"line one\nLINE TWO\nline three\n")],
    );

    let g = git(&["diff", &a, &b], tmp.path());
    let r = rustygit(&["diff", &a, &b], tmp.path());
    assert!(r.status.success(), "diff failed: {:?}", r);
    assert_eq!(
        r.stdout,
        g.stdout,
        "diff output mismatch\nours:\n{}\ntheirs:\n{}",
        String::from_utf8_lossy(&r.stdout),
        String::from_utf8_lossy(&g.stdout)
    );
}

#[test]
fn diff_added_file_byte_matches_git() {
    if !has_system_git() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    let (a, b) = init_two_commits(
        tmp.path(),
        &[("keep.txt", b"keep\n")],
        &[("keep.txt", b"keep\n"), ("new.txt", b"new file body\n")],
    );

    let g = git(&["diff", &a, &b], tmp.path());
    let r = rustygit(&["diff", &a, &b], tmp.path());
    assert!(r.status.success());
    assert_eq!(r.stdout, g.stdout);
}

#[test]
fn diff_removed_file_byte_matches_git() {
    if !has_system_git() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    let (a, b) = init_two_commits(
        tmp.path(),
        &[("keep.txt", b"keep\n"), ("gone.txt", b"to be removed\n")],
        &[("keep.txt", b"keep\n")],
    );

    let g = git(&["diff", &a, &b], tmp.path());
    let r = rustygit(&["diff", &a, &b], tmp.path());
    assert!(r.status.success());
    assert_eq!(r.stdout, g.stdout);
}

#[test]
fn diff_multiple_files_byte_matches_git() {
    if !has_system_git() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    let (a, b) = init_two_commits(
        tmp.path(),
        &[
            ("a.txt", b"alpha\n"),
            ("b.txt", b"bravo\n"),
            ("c.txt", b"charlie\n"),
        ],
        &[
            ("a.txt", b"ALPHA\n"),
            ("b.txt", b"bravo\n"),
            // c.txt removed
            ("d.txt", b"delta\n"), // added
        ],
    );

    let g = git(&["diff", &a, &b], tmp.path());
    let r = rustygit(&["diff", &a, &b], tmp.path());
    assert!(r.status.success());
    assert_eq!(
        r.stdout,
        g.stdout,
        "multiple-files diff mismatch\nours:\n{}\ntheirs:\n{}",
        String::from_utf8_lossy(&r.stdout),
        String::from_utf8_lossy(&g.stdout)
    );
}

#[test]
#[cfg(unix)]
fn diff_mode_change_byte_matches_git() {
    use std::os::unix::fs::PermissionsExt;
    if !has_system_git() {
        return;
    }
    let tmp = TempDir::new().unwrap();

    // Probe that the filesystem honors the executable bit; if not, skip.
    let probe = tmp.path().join(".probe");
    std::fs::write(&probe, b"x").unwrap();
    std::fs::set_permissions(&probe, std::fs::Permissions::from_mode(0o755)).unwrap();
    let after_probe = std::fs::metadata(&probe).unwrap().permissions().mode() & 0o111;
    let _ = std::fs::remove_file(&probe);
    if after_probe == 0 {
        eprintln!("skipping: filesystem doesn't honor exec bit");
        return;
    }

    let _ = git(&["init", "-q", "-b", "main", "."], tmp.path());
    let _ = git(&["config", "user.name", "T"], tmp.path());
    let _ = git(&["config", "user.email", "t@t"], tmp.path());
    // Make sure git observes filemode changes.
    let _ = git(&["config", "core.filemode", "true"], tmp.path());

    std::fs::write(tmp.path().join("script"), b"#!/bin/sh\necho hi\n").unwrap();
    git(&["add", "script"], tmp.path());
    git(&["commit", "-q", "-m", "first"], tmp.path());
    let a = git_head_oid(tmp.path());

    // Flip exec bit — same content, different mode.
    std::fs::set_permissions(
        tmp.path().join("script"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    git(&["add", "script"], tmp.path());
    git(&["commit", "-q", "-m", "exec bit"], tmp.path());
    let b = git_head_oid(tmp.path());

    let g = git(&["diff", &a, &b], tmp.path());
    let r = rustygit(&["diff", &a, &b], tmp.path());
    assert!(r.status.success());
    assert_eq!(
        r.stdout,
        g.stdout,
        "mode-only diff mismatch\nours:\n{}\ntheirs:\n{}",
        String::from_utf8_lossy(&r.stdout),
        String::from_utf8_lossy(&g.stdout)
    );
}

#[test]
fn diff_tree_plumbing_matches_git_diff() {
    if !has_system_git() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    let (a, b) = init_two_commits(
        tmp.path(),
        &[("a.txt", b"hello\n")],
        &[("a.txt", b"hello world\n")],
    );

    // `rustygit diff-tree` between two commit-ish OIDs should match
    // `git diff` between the same two OIDs.
    let g = git(&["diff", &a, &b], tmp.path());
    let r = rustygit(&["diff-tree", "-r", &a, &b], tmp.path());
    assert!(r.status.success());
    assert_eq!(
        r.stdout,
        g.stdout,
        "diff-tree mismatch\nours:\n{}\ntheirs:\n{}",
        String::from_utf8_lossy(&r.stdout),
        String::from_utf8_lossy(&g.stdout)
    );
}
