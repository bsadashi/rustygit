//! Integration tests for `rustygit add -p` (interactive hunk staging).
//!
//! Driven via stdin scripts; matches the y/n/q/a/d/s/? subset shipped per
//! POLISH.md item 7. Each test compares the resulting staging state against
//! the equivalent flat `add <path>` outcome (or a controlled subset thereof).

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

fn rustygit_stdin(args: &[&str], cwd: &Path, stdin: &str) -> std::process::Output {
    let mut cmd = AssertCmd::cargo_bin("rustygit").unwrap();
    cmd.args(args)
        .current_dir(cwd)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .write_stdin(stdin)
        .output()
        .unwrap()
}

fn assert_success(out: &std::process::Output, label: &str) {
    assert!(
        out.status.success(),
        "{label} failed: status={:?}\nstdout: {}\nstderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Create a tracked file with `content`, commit it, then return the temp dir.
fn init_with_committed_file(name: &str, content: &[u8]) -> TempDir {
    let tmp = TempDir::new().unwrap();
    assert_success(&rustygit(&["init", "-q", "."], tmp.path()), "init");
    std::fs::write(tmp.path().join(name), content).unwrap();
    assert_success(&rustygit(&["add", name], tmp.path()), "add");
    assert_success(
        &rustygit(&["commit", "-m", "initial"], tmp.path()),
        "commit",
    );
    tmp
}

// ----------------------------------------------------------------------------
// Single-hunk path: y / n / q
// ----------------------------------------------------------------------------

#[test]
fn add_patch_y_stages_the_change() {
    let tmp = init_with_committed_file("foo.txt", b"line1\nline2\nline3\n");
    // Modify
    std::fs::write(tmp.path().join("foo.txt"), b"LINE1\nline2\nline3\n").unwrap();

    let out = rustygit_stdin(&["add", "-p"], tmp.path(), "y\n");
    assert_success(&out, "add -p y");

    let porcelain = rustygit(&["status", "--porcelain"], tmp.path());
    assert!(porcelain.status.success());
    let body = String::from_utf8_lossy(&porcelain.stdout);
    // 'M' in column 0, space in column 1 → fully staged.
    assert!(body.contains("M  foo.txt"), "expected staged, got: {body}");
}

#[test]
fn add_patch_n_does_not_stage() {
    let tmp = init_with_committed_file("foo.txt", b"line1\nline2\nline3\n");
    std::fs::write(tmp.path().join("foo.txt"), b"LINE1\nline2\nline3\n").unwrap();

    let out = rustygit_stdin(&["add", "-p"], tmp.path(), "n\n");
    assert_success(&out, "add -p n");

    let porcelain = rustygit(&["status", "--porcelain"], tmp.path());
    let body = String::from_utf8_lossy(&porcelain.stdout);
    // ' M' = unstaged modification, not ' ' or 'M '.
    assert!(
        body.contains(" M foo.txt"),
        "expected unstaged, got: {body}"
    );
}

#[test]
fn add_patch_q_skips_remaining() {
    // Two modified files; `q` after the first means the second never gets
    // prompted and should remain unstaged.
    let tmp = TempDir::new().unwrap();
    assert_success(&rustygit(&["init", "-q", "."], tmp.path()), "init");
    std::fs::write(tmp.path().join("a.txt"), b"a1\n").unwrap();
    std::fs::write(tmp.path().join("b.txt"), b"b1\n").unwrap();
    assert_success(&rustygit(&["add", "."], tmp.path()), "add");
    assert_success(&rustygit(&["commit", "-m", "init"], tmp.path()), "commit");
    std::fs::write(tmp.path().join("a.txt"), b"A1\n").unwrap();
    std::fs::write(tmp.path().join("b.txt"), b"B1\n").unwrap();

    // First file: `q` quits the whole session.
    let out = rustygit_stdin(&["add", "-p"], tmp.path(), "q\n");
    assert_success(&out, "add -p q");

    let porcelain = rustygit(&["status", "--porcelain"], tmp.path());
    let body = String::from_utf8_lossy(&porcelain.stdout);
    // Neither file should be staged.
    assert!(
        body.contains(" M a.txt"),
        "a.txt should be unstaged: {body}"
    );
    assert!(
        body.contains(" M b.txt"),
        "b.txt should be unstaged: {body}"
    );
}

// ----------------------------------------------------------------------------
// Multi-hunk in one file: choose first, skip second
// ----------------------------------------------------------------------------

#[test]
fn add_patch_multi_hunk_first_yes_second_no() {
    // 10-line file; modify lines 1 and 10 → two distinct hunks under
    // default 3-line context. Pipe y\n n\n and verify only first applied.
    let original = b"l1\nl2\nl3\nl4\nl5\nl6\nl7\nl8\nl9\nl10\n";
    let modified = b"X\nl2\nl3\nl4\nl5\nl6\nl7\nl8\nl9\nY\n";
    let tmp = init_with_committed_file("foo.txt", original);
    std::fs::write(tmp.path().join("foo.txt"), modified).unwrap();

    let out = rustygit_stdin(&["add", "-p"], tmp.path(), "y\nn\n");
    assert_success(&out, "add -p y/n");

    // The staged blob should be original with hunk1 applied → "X" on line 1,
    // "l10" still on line 10. ls-files -s and cat-file -p give us the blob.
    let ls = rustygit(&["ls-files", "-s", "foo.txt"], tmp.path());
    // We can also use cat-file via the staged blob oid extracted from ls-files
    // output, but that's harder; instead reset workdir and add back to compare.

    // Reset worktree to the index state and verify it equals the "first-only" content.
    assert_success(&rustygit(&["restore", "foo.txt"], tmp.path()), "restore");
    let staged_content = std::fs::read(tmp.path().join("foo.txt")).unwrap();
    let expected = b"X\nl2\nl3\nl4\nl5\nl6\nl7\nl8\nl9\nl10\n";
    assert_eq!(
        staged_content,
        expected,
        "expected only first hunk applied to the index, got: {:?}\nls-files: {}",
        String::from_utf8_lossy(&staged_content),
        String::from_utf8_lossy(&ls.stdout)
    );
}

#[test]
fn add_patch_multi_hunk_second_yes_first_no() {
    let original = b"l1\nl2\nl3\nl4\nl5\nl6\nl7\nl8\nl9\nl10\n";
    let modified = b"X\nl2\nl3\nl4\nl5\nl6\nl7\nl8\nl9\nY\n";
    let tmp = init_with_committed_file("foo.txt", original);
    std::fs::write(tmp.path().join("foo.txt"), modified).unwrap();

    let out = rustygit_stdin(&["add", "-p"], tmp.path(), "n\ny\n");
    assert_success(&out, "add -p n/y");

    assert_success(&rustygit(&["restore", "foo.txt"], tmp.path()), "restore");
    let staged_content = std::fs::read(tmp.path().join("foo.txt")).unwrap();
    let expected = b"l1\nl2\nl3\nl4\nl5\nl6\nl7\nl8\nl9\nY\n";
    assert_eq!(staged_content, expected);
}

// ----------------------------------------------------------------------------
// `a` / `d`: bulk-stage / bulk-skip the rest of a file
// ----------------------------------------------------------------------------

#[test]
fn add_patch_a_stages_all_remaining_hunks() {
    let original = b"l1\nl2\nl3\nl4\nl5\nl6\nl7\nl8\nl9\nl10\n";
    let modified = b"X\nl2\nl3\nl4\nl5\nl6\nl7\nl8\nl9\nY\n";
    let tmp = init_with_committed_file("foo.txt", original);
    std::fs::write(tmp.path().join("foo.txt"), modified).unwrap();

    // `a` on the first hunk should accept it AND every subsequent hunk in
    // this file with no further prompts.
    let out = rustygit_stdin(&["add", "-p"], tmp.path(), "a\n");
    assert_success(&out, "add -p a");

    assert_success(&rustygit(&["restore", "foo.txt"], tmp.path()), "restore");
    let staged_content = std::fs::read(tmp.path().join("foo.txt")).unwrap();
    assert_eq!(
        staged_content, modified,
        "expected all hunks staged via `a`"
    );
}

#[test]
fn add_patch_d_skips_all_remaining_hunks() {
    let original = b"l1\nl2\nl3\nl4\nl5\nl6\nl7\nl8\nl9\nl10\n";
    let modified = b"X\nl2\nl3\nl4\nl5\nl6\nl7\nl8\nl9\nY\n";
    let tmp = init_with_committed_file("foo.txt", original);
    std::fs::write(tmp.path().join("foo.txt"), modified).unwrap();

    let out = rustygit_stdin(&["add", "-p"], tmp.path(), "d\n");
    assert_success(&out, "add -p d");

    // Nothing should be staged.
    let porcelain = rustygit(&["status", "--porcelain"], tmp.path());
    let body = String::from_utf8_lossy(&porcelain.stdout);
    assert!(
        body.contains(" M foo.txt"),
        "expected ' M' (unstaged): {body}"
    );
    assert!(!body.contains("M  foo.txt"), "should NOT be staged: {body}");
}

// ----------------------------------------------------------------------------
// `?` help
// ----------------------------------------------------------------------------

#[test]
fn add_patch_question_mark_prints_help_and_reprompts() {
    let tmp = init_with_committed_file("foo.txt", b"line1\nline2\n");
    std::fs::write(tmp.path().join("foo.txt"), b"LINE1\nline2\n").unwrap();

    // `?\n` then `n\n` to skip.
    let out = rustygit_stdin(&["add", "-p"], tmp.path(), "?\nn\n");
    assert_success(&out, "add -p ?/n");

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("y - stage this hunk"),
        "help text missing y line: {stderr}"
    );
    assert!(
        stderr.contains("? - print help"),
        "help text missing ? line: {stderr}"
    );
}

// ----------------------------------------------------------------------------
// `s` split: a file with two changes that COULD be one hunk gets split.
// ----------------------------------------------------------------------------

#[test]
fn add_patch_s_split_creates_smaller_hunks() {
    // Two changes 1 line apart → default context=3 merges them into one
    // hunk. Splitting should yield 2 sub-hunks (or at least more than 1).
    let original = b"l1\nl2\nl3\nl4\nl5\n";
    let modified = b"X\nl2\nl3\nl4\nY\n";
    let tmp = init_with_committed_file("foo.txt", original);
    std::fs::write(tmp.path().join("foo.txt"), modified).unwrap();

    // Split, then say `y` to first sub-hunk, `n` to second.
    let out = rustygit_stdin(&["add", "-p"], tmp.path(), "s\ny\nn\n");
    assert_success(&out, "add -p s/y/n");

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("Split into"),
        "expected split message: {stderr}"
    );

    // Should have only the first change staged.
    assert_success(&rustygit(&["restore", "foo.txt"], tmp.path()), "restore");
    let staged_content = std::fs::read(tmp.path().join("foo.txt")).unwrap();
    let expected = b"X\nl2\nl3\nl4\nl5\n";
    assert_eq!(
        staged_content,
        expected,
        "expected only first split-hunk staged; got: {:?}",
        String::from_utf8_lossy(&staged_content)
    );
}

// ----------------------------------------------------------------------------
// Compat with system git: `add -p` accepting EVERYTHING should byte-match
// `git add file` for the same modifications.
// ----------------------------------------------------------------------------

#[test]
fn add_patch_yes_to_all_matches_git_add_porcelain() {
    if !has_system_git() {
        return;
    }
    let original = b"alpha\nbeta\ngamma\ndelta\nepsilon\n";
    let modified = b"ALPHA\nbeta\ngamma\ndelta\nEPSILON\n";

    // Two separate temp dirs: one driven by rustygit add -p with 'a' the
    // first prompt, one driven by `git add file`. Compare status output.
    let tmp_rg = init_with_committed_file("foo.txt", original);
    std::fs::write(tmp_rg.path().join("foo.txt"), modified).unwrap();
    let out_rg = rustygit_stdin(&["add", "-p"], tmp_rg.path(), "a\n");
    assert_success(&out_rg, "rustygit add -p a");

    // Now do the same with system git.
    let tmp_g = TempDir::new().unwrap();
    let g_init_cmd = std::process::Command::new("git")
        .args(["init", "-q", "."])
        .current_dir(tmp_g.path())
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .output()
        .unwrap();
    assert!(g_init_cmd.status.success());
    std::fs::write(tmp_g.path().join("foo.txt"), original).unwrap();
    git(&["add", "foo.txt"], tmp_g.path());
    let g_commit = std::process::Command::new("git")
        .args(["commit", "-q", "-m", "initial"])
        .current_dir(tmp_g.path())
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .output()
        .unwrap();
    assert!(g_commit.status.success());
    std::fs::write(tmp_g.path().join("foo.txt"), modified).unwrap();
    git(&["add", "foo.txt"], tmp_g.path());

    let rg_porcelain = rustygit(&["status", "--porcelain"], tmp_rg.path());
    let g_porcelain = git(&["status", "--porcelain"], tmp_g.path());
    assert_eq!(
        rg_porcelain.stdout,
        g_porcelain.stdout,
        "porcelain mismatch after rustygit add -p `a` vs git add\nrg: {}\ng:  {}",
        String::from_utf8_lossy(&rg_porcelain.stdout),
        String::from_utf8_lossy(&g_porcelain.stdout)
    );
}

// ----------------------------------------------------------------------------
// Prompt format: the `[y,n,q,a,d,s,?]?` advert is emitted on stderr.
// ----------------------------------------------------------------------------

#[test]
fn add_patch_prompt_format_matches_subset() {
    let tmp = init_with_committed_file("foo.txt", b"x\n");
    std::fs::write(tmp.path().join("foo.txt"), b"y\n").unwrap();

    // Use `q` so we don't actually stage.
    let out = rustygit_stdin(&["add", "-p"], tmp.path(), "q\n");
    assert_success(&out, "add -p q");

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("Stage this hunk [y,n,q,a,d,s,?]?"),
        "missing prompt advert: {stderr}"
    );
}
