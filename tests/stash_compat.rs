//! `rustygit stash` — oracle tests against system `git`.
//!
//! Strategy:
//!   * Cross-binary readability — `rustygit stash push` produces a
//!     ref/reflog/object set that `git stash list/show/apply` reads
//!     identically.
//!   * Workdir round-trip — push then pop restores byte-equal state.
//!   * Reflog walk — `rustygit stash list` matches the `git stash list`
//!     ordering (newest first).

mod common;

use std::path::Path;

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
        .env("GIT_AUTHOR_DATE", "1700000000 +0000")
        .env("GIT_COMMITTER_DATE", "1700000000 +0000")
        .output()
        .unwrap()
}

fn assert_ok(out: &std::process::Output, label: &str) {
    assert!(
        out.status.success(),
        "{label} failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

fn commit_file(tmp: &Path, name: &str, contents: &[u8], msg: &str) {
    std::fs::write(tmp.join(name), contents).unwrap();
    git(&["add", name], tmp);
    git(
        &[
            "-c",
            "user.email=t@t",
            "-c",
            "user.name=t",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-q",
            "-m",
            msg,
        ],
        tmp,
    );
}

fn init_repo() -> TempDir {
    let tmp = TempDir::new().unwrap();
    git(&["init", "-q", "-b", "main"], tmp.path());
    git(&["config", "user.email", "t@t"], tmp.path());
    git(&["config", "user.name", "t"], tmp.path());
    git(&["config", "commit.gpgsign", "false"], tmp.path());
    tmp
}

// --- Push -----------------------------------------------------------------

/// `stash push` saves modified-but-not-staged changes and clears the
/// workdir to HEAD's state.
#[test]
fn push_with_workdir_change_clears_workdir() {
    if !has_system_git() {
        return;
    }
    let tmp = init_repo();
    commit_file(tmp.path(), "f", b"original\n", "first");
    std::fs::write(tmp.path().join("f"), b"modified\n").unwrap();

    assert_ok(&rustygit(&["stash"], tmp.path()), "rustygit stash");
    let after = std::fs::read(tmp.path().join("f")).unwrap();
    assert_eq!(after, b"original\n", "workdir should be back to HEAD state");
}

/// `stash push` with no local changes is a no-op (warns, exit 0).
#[test]
fn push_clean_tree_is_noop() {
    if !has_system_git() {
        return;
    }
    let tmp = init_repo();
    commit_file(tmp.path(), "f", b"x\n", "first");
    let r = rustygit(&["stash"], tmp.path());
    assert_eq!(r.status.code(), Some(0));
    // No stash ref should exist.
    let listing = rustygit(&["stash", "list"], tmp.path());
    assert!(listing.stdout.is_empty());
}

/// `stash push -m <msg>` uses the custom message; `git stash list`
/// reads it back.
#[test]
fn push_custom_message_visible_to_git_stash_list() {
    if !has_system_git() {
        return;
    }
    let tmp = init_repo();
    commit_file(tmp.path(), "f", b"x\n", "first");
    std::fs::write(tmp.path().join("f"), b"y\n").unwrap();

    assert_ok(
        &rustygit(&["stash", "push", "-m", "WIP refactor"], tmp.path()),
        "rustygit stash push -m",
    );
    let git_list = String::from_utf8(git(&["stash", "list"], tmp.path()).stdout).unwrap();
    assert!(
        git_list.contains("WIP refactor"),
        "git stash list should show our message: {git_list:?}"
    );
}

// --- List -----------------------------------------------------------------

/// Multiple `stash push`es show up in `stash list` newest-first.
#[test]
fn list_orders_newest_first() {
    if !has_system_git() {
        return;
    }
    let tmp = init_repo();
    commit_file(tmp.path(), "f", b"v0\n", "first");

    std::fs::write(tmp.path().join("f"), b"a\n").unwrap();
    assert_ok(
        &rustygit(&["stash", "push", "-m", "A"], tmp.path()),
        "push A",
    );
    std::fs::write(tmp.path().join("f"), b"b\n").unwrap();
    assert_ok(
        &rustygit(&["stash", "push", "-m", "B"], tmp.path()),
        "push B",
    );
    std::fs::write(tmp.path().join("f"), b"c\n").unwrap();
    assert_ok(
        &rustygit(&["stash", "push", "-m", "C"], tmp.path()),
        "push C",
    );

    let out = String::from_utf8(rustygit(&["stash", "list"], tmp.path()).stdout).unwrap();
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines.len(), 3, "expected 3 entries, got {lines:?}");
    assert!(lines[0].starts_with("stash@{0}:"));
    assert!(lines[0].contains("C"));
    assert!(lines[1].starts_with("stash@{1}:"));
    assert!(lines[1].contains("B"));
    assert!(lines[2].starts_with("stash@{2}:"));
    assert!(lines[2].contains("A"));
}

// --- Pop / apply ---------------------------------------------------------

/// `stash push` then `stash pop` restores the modified file.
#[test]
fn push_then_pop_round_trip_restores_workdir() {
    if !has_system_git() {
        return;
    }
    let tmp = init_repo();
    commit_file(tmp.path(), "f", b"orig\n", "first");
    std::fs::write(tmp.path().join("f"), b"wip\n").unwrap();

    assert_ok(&rustygit(&["stash"], tmp.path()), "stash");
    assert_eq!(std::fs::read(tmp.path().join("f")).unwrap(), b"orig\n");

    assert_ok(&rustygit(&["stash", "pop"], tmp.path()), "pop");
    assert_eq!(
        std::fs::read(tmp.path().join("f")).unwrap(),
        b"wip\n",
        "workdir should be back to the stashed bytes"
    );

    // pop should also have removed the entry.
    let out = String::from_utf8(rustygit(&["stash", "list"], tmp.path()).stdout).unwrap();
    assert!(out.is_empty(), "stash list should be empty after pop");
}

/// `stash apply` restores but does NOT drop.
#[test]
fn apply_restores_but_keeps_entry() {
    if !has_system_git() {
        return;
    }
    let tmp = init_repo();
    commit_file(tmp.path(), "f", b"orig\n", "first");
    std::fs::write(tmp.path().join("f"), b"wip\n").unwrap();
    assert_ok(&rustygit(&["stash"], tmp.path()), "stash");

    assert_ok(&rustygit(&["stash", "apply"], tmp.path()), "apply");
    assert_eq!(std::fs::read(tmp.path().join("f")).unwrap(), b"wip\n");

    let out = String::from_utf8(rustygit(&["stash", "list"], tmp.path()).stdout).unwrap();
    assert_eq!(out.lines().count(), 1, "apply must not drop the entry");
}

/// `git stash apply` can read a rustygit-written stash.
#[test]
fn git_can_apply_rustygit_stash() {
    if !has_system_git() {
        return;
    }
    let tmp = init_repo();
    commit_file(tmp.path(), "f", b"orig\n", "first");
    std::fs::write(tmp.path().join("f"), b"wip\n").unwrap();
    assert_ok(&rustygit(&["stash"], tmp.path()), "rustygit stash");

    assert_eq!(std::fs::read(tmp.path().join("f")).unwrap(), b"orig\n");
    let _ = git(&["stash", "apply"], tmp.path());
    assert_eq!(
        std::fs::read(tmp.path().join("f")).unwrap(),
        b"wip\n",
        "git stash apply should restore the rustygit-stashed content"
    );
}

/// `rustygit stash apply` can read a git-written stash.
#[test]
fn rustygit_can_apply_git_stash() {
    if !has_system_git() {
        return;
    }
    let tmp = init_repo();
    commit_file(tmp.path(), "f", b"orig\n", "first");
    std::fs::write(tmp.path().join("f"), b"wip\n").unwrap();
    // git creates the stash
    let out = git(&["stash", "push", "-m", "git's stash"], tmp.path());
    assert!(out.status.success());

    assert_eq!(std::fs::read(tmp.path().join("f")).unwrap(), b"orig\n");
    assert_ok(&rustygit(&["stash", "apply"], tmp.path()), "rustygit apply");
    assert_eq!(std::fs::read(tmp.path().join("f")).unwrap(), b"wip\n");
}

// --- Drop / clear --------------------------------------------------------

/// `stash drop stash@{N}` removes only that entry.
#[test]
fn drop_specific_index_removes_only_that_entry() {
    if !has_system_git() {
        return;
    }
    let tmp = init_repo();
    commit_file(tmp.path(), "f", b"x\n", "first");

    for m in ["A", "B", "C"] {
        std::fs::write(tmp.path().join("f"), format!("{m}\n").as_bytes()).unwrap();
        assert_ok(
            &rustygit(&["stash", "push", "-m", m], tmp.path()),
            &format!("push {m}"),
        );
    }
    // Drop the middle entry (stash@{1} = B).
    assert_ok(
        &rustygit(&["stash", "drop", "stash@{1}"], tmp.path()),
        "drop",
    );

    let out = String::from_utf8(rustygit(&["stash", "list"], tmp.path()).stdout).unwrap();
    let remaining: Vec<&str> = out.lines().collect();
    assert_eq!(remaining.len(), 2);
    assert!(remaining[0].contains('C'), "newest still C: {remaining:?}");
    assert!(remaining[1].contains('A'), "older still A: {remaining:?}");
}

/// `stash clear` removes all entries and the ref itself.
#[test]
fn clear_removes_all_entries() {
    if !has_system_git() {
        return;
    }
    let tmp = init_repo();
    commit_file(tmp.path(), "f", b"x\n", "first");
    for m in ["A", "B"] {
        std::fs::write(tmp.path().join("f"), format!("{m}\n").as_bytes()).unwrap();
        assert_ok(&rustygit(&["stash", "push", "-m", m], tmp.path()), "push");
    }

    assert_ok(&rustygit(&["stash", "clear"], tmp.path()), "clear");
    let out = String::from_utf8(rustygit(&["stash", "list"], tmp.path()).stdout).unwrap();
    assert!(out.is_empty());
    let git_list = String::from_utf8(git(&["stash", "list"], tmp.path()).stdout).unwrap();
    assert!(git_list.is_empty(), "git stash list should also be empty");
}

/// `stash drop stash@{99}` (out of range) errors.
#[test]
fn drop_invalid_index_errors() {
    if !has_system_git() {
        return;
    }
    let tmp = init_repo();
    commit_file(tmp.path(), "f", b"x\n", "first");
    std::fs::write(tmp.path().join("f"), b"y\n").unwrap();
    assert_ok(&rustygit(&["stash"], tmp.path()), "stash");

    let r = rustygit(&["stash", "drop", "stash@{99}"], tmp.path());
    assert_ne!(
        r.status.code(),
        Some(0),
        "drop of invalid index must not succeed"
    );
}

// --- Show ----------------------------------------------------------------

/// `stash show` prints a diff of the stashed change vs. HEAD-at-stash-time.
#[test]
fn show_emits_diff() {
    if !has_system_git() {
        return;
    }
    let tmp = init_repo();
    commit_file(tmp.path(), "f", b"line\n", "first");
    std::fs::write(tmp.path().join("f"), b"changed\n").unwrap();
    assert_ok(&rustygit(&["stash"], tmp.path()), "stash");

    let out = rustygit(&["stash", "show"], tmp.path());
    assert_eq!(out.status.code(), Some(0));
    let text = String::from_utf8_lossy(&out.stdout);
    // We just check that it's clearly diff output that mentions `f`.
    assert!(
        text.contains("f"),
        "stash show should reference path f: {text:?}"
    );
}

// --- Untracked (-u) ------------------------------------------------------

/// `stash -u` removes untracked files from the workdir AND records them
/// in a 3-parent stash commit.
#[test]
fn push_u_captures_untracked_and_clears_workdir() {
    if !has_system_git() {
        return;
    }
    let tmp = init_repo();
    commit_file(tmp.path(), "f", b"tracked\n", "first");
    std::fs::write(tmp.path().join("new_untracked"), b"hello\n").unwrap();

    assert_ok(
        &rustygit(&["stash", "push", "-u", "-m", "with-u"], tmp.path()),
        "rustygit stash -u",
    );
    assert!(
        !tmp.path().join("new_untracked").exists(),
        "untracked file must be removed from workdir after stash -u"
    );

    // The stash commit should have 3 parents.
    let stash_oid = String::from_utf8(git(&["rev-parse", "stash"], tmp.path()).stdout)
        .unwrap()
        .trim()
        .to_string();
    let parents_out =
        String::from_utf8(git(&["log", "-1", "--format=%P", &stash_oid], tmp.path()).stdout)
            .unwrap();
    let parent_count = parents_out.split_whitespace().count();
    assert_eq!(
        parent_count, 3,
        "stash commit should have 3 parents with -u, got: {parents_out:?}"
    );
}

/// pop restores both tracked changes AND untracked files.
#[test]
fn pop_u_restores_untracked_files() {
    if !has_system_git() {
        return;
    }
    let tmp = init_repo();
    commit_file(tmp.path(), "f", b"orig\n", "first");
    std::fs::write(tmp.path().join("f"), b"changed\n").unwrap();
    std::fs::write(tmp.path().join("untracked.txt"), b"u1\n").unwrap();

    assert_ok(&rustygit(&["stash", "push", "-u"], tmp.path()), "stash -u");
    // workdir clean: tracked file back to orig, untracked gone
    assert_eq!(std::fs::read(tmp.path().join("f")).unwrap(), b"orig\n");
    assert!(!tmp.path().join("untracked.txt").exists());

    assert_ok(&rustygit(&["stash", "pop"], tmp.path()), "stash pop");
    // tracked: back to "changed"
    assert_eq!(std::fs::read(tmp.path().join("f")).unwrap(), b"changed\n");
    // untracked: back as a file
    assert_eq!(
        std::fs::read(tmp.path().join("untracked.txt")).unwrap(),
        b"u1\n"
    );
}

/// `rustygit stash -u` produces an artifact that `git stash apply` reads.
#[test]
fn git_reads_rustygit_stash_u() {
    if !has_system_git() {
        return;
    }
    let tmp = init_repo();
    commit_file(tmp.path(), "f", b"orig\n", "first");
    std::fs::write(tmp.path().join("f"), b"wip\n").unwrap();
    std::fs::write(tmp.path().join("u"), b"untracked\n").unwrap();

    assert_ok(
        &rustygit(&["stash", "push", "-u", "-m", "x"], tmp.path()),
        "rustygit stash -u",
    );

    let _ = git(&["stash", "apply"], tmp.path());
    assert_eq!(std::fs::read(tmp.path().join("f")).unwrap(), b"wip\n");
    assert_eq!(std::fs::read(tmp.path().join("u")).unwrap(), b"untracked\n");
}

/// Reverse: `rustygit stash apply` reads a `git stash -u`-produced stash.
#[test]
fn rustygit_reads_git_stash_u() {
    if !has_system_git() {
        return;
    }
    let tmp = init_repo();
    commit_file(tmp.path(), "f", b"orig\n", "first");
    std::fs::write(tmp.path().join("f"), b"wip\n").unwrap();
    std::fs::write(tmp.path().join("u2"), b"un\n").unwrap();

    let out = git(&["stash", "push", "-u", "-m", "git's"], tmp.path());
    assert!(out.status.success());
    assert!(!tmp.path().join("u2").exists());

    assert_ok(&rustygit(&["stash", "apply"], tmp.path()), "rg apply");
    assert_eq!(std::fs::read(tmp.path().join("f")).unwrap(), b"wip\n");
    assert_eq!(std::fs::read(tmp.path().join("u2")).unwrap(), b"un\n");
}

// --- Index restoration ---------------------------------------------------

/// `stash apply` restores both the index and the workdir.
#[test]
fn apply_restores_both_index_and_workdir() {
    if !has_system_git() {
        return;
    }
    let tmp = init_repo();
    commit_file(tmp.path(), "f", b"orig\n", "first");
    // Stage a change to f, also leave an unstaged-on-top change.
    std::fs::write(tmp.path().join("f"), b"staged\n").unwrap();
    git(&["add", "f"], tmp.path());
    std::fs::write(tmp.path().join("f"), b"wip-on-top\n").unwrap();

    assert_ok(&rustygit(&["stash"], tmp.path()), "stash");
    // Workdir + index should be back to 'orig' state.
    assert_eq!(std::fs::read(tmp.path().join("f")).unwrap(), b"orig\n");

    assert_ok(&rustygit(&["stash", "pop"], tmp.path()), "pop");

    // Workdir should be back to 'wip-on-top'.
    assert_eq!(
        std::fs::read(tmp.path().join("f")).unwrap(),
        b"wip-on-top\n"
    );
    // The index should be back to 'staged' (git status will report unstaged
    // diff between wip-on-top and staged).
    let staged_blob = String::from_utf8(git(&["ls-files", "-s", "f"], tmp.path()).stdout).unwrap();
    // ls-files -s prints "100644 <oid> 0 f"; we just check it doesn't say the original.
    // Easier: cat-file -p :f should give 'staged\n'.
    let staged = git(&["cat-file", "-p", ":f"], tmp.path()).stdout;
    assert_eq!(
        staged, b"staged\n",
        "index entry should be 'staged': {staged_blob:?}"
    );
}
