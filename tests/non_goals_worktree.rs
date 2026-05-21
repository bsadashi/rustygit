//! NON_GOALS.md Batch I — linked worktrees (`rustygit worktree`).
//!
//! The acid test: a worktree created by rustygit must be a valid linked
//! worktree from upstream git's perspective, and vice versa. Tests run the
//! oracle cross-check (`cd <linked>; git status` after rustygit creates it,
//! `rustygit worktree list` after `git worktree add`).

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
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@e")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@e")
        .output()
        .unwrap()
}

/// Build a tiny repo with one commit; return the resulting HEAD oid.
fn make_repo_with_commit(tmp: &Path) -> String {
    assert!(rustygit(&["init", "-q", "."], tmp).status.success());
    std::fs::write(tmp.join("a.txt"), b"a\n").unwrap();
    assert!(rustygit(&["add", "a.txt"], tmp).status.success());
    let cm = rustygit(&["commit", "-m", "c1"], tmp);
    assert!(
        cm.status.success(),
        "commit: {}",
        String::from_utf8_lossy(&cm.stderr)
    );
    let rp = rustygit(&["rev-parse", "HEAD"], tmp);
    String::from_utf8_lossy(&rp.stdout).trim().to_string()
}

#[test]
fn worktree_list_on_main_only_repo_prints_main() {
    let tmp = TempDir::new().unwrap();
    let head = make_repo_with_commit(tmp.path());
    let out = rustygit(&["worktree", "list"], tmp.path());
    assert!(out.status.success());
    let s = String::from_utf8_lossy(&out.stdout);
    let short = &head[..7];
    assert!(s.contains(short), "list should include short oid: {s}");
    assert!(
        s.contains("master") || s.contains("main"),
        "list should name a branch: {s}"
    );
    // Only one entry on a no-secondary repo.
    assert_eq!(s.lines().count(), 1, "expected 1 line, got: {s}");
}

#[test]
fn worktree_add_with_b_creates_branch_and_checks_out() {
    if !has_system_git() {
        return;
    }
    let main = TempDir::new().unwrap();
    let secondary = TempDir::new().unwrap();
    let secondary_path = secondary.path().join("linked");

    let _head = make_repo_with_commit(main.path());

    let out = rustygit(
        &[
            "worktree",
            "add",
            secondary_path.to_str().unwrap(),
            "-b",
            "feature",
        ],
        main.path(),
    );
    assert!(
        out.status.success(),
        "worktree add failed: stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    // The secondary worktree should exist with the file checked out.
    assert!(
        secondary_path.join("a.txt").exists(),
        "expected a.txt in the secondary worktree"
    );
    let content = std::fs::read(secondary_path.join("a.txt")).unwrap();
    assert_eq!(content, b"a\n");

    // The `.git` pointer file is what git expects.
    let dot_git = std::fs::read(secondary_path.join(".git")).unwrap();
    let dot_git_str = String::from_utf8_lossy(&dot_git);
    assert!(
        dot_git_str.starts_with("gitdir:"),
        "secondary .git should be a 'gitdir:' pointer, got {dot_git_str}"
    );
    assert!(
        dot_git_str.contains("worktrees/linked"),
        ".git pointer should target worktrees/linked: {dot_git_str}"
    );

    // git itself should now recognize the linked worktree.
    // On macOS, `/var` symlinks to `/private/var`; tmpdirs may surface
    // under either path depending on whose canonicalize ran. Strip the
    // `/private` prefix from canonical paths so the comparison matches
    // whatever form git's output uses.
    let g = git(&["worktree", "list"], main.path());
    let g_out = String::from_utf8_lossy(&g.stdout);
    let want = secondary_path.canonicalize().unwrap();
    let want_str = want.to_string_lossy().into_owned();
    let want_alt = want_str.strip_prefix("/private").unwrap_or(&want_str);
    assert!(
        g_out.contains(&want_str) || g_out.contains(want_alt),
        "git worktree list should mention the secondary path ({want_str} or {want_alt}): {g_out}"
    );
    assert!(
        g_out.contains("feature"),
        "git worktree list should name the new branch: {g_out}"
    );

    // And operating IN the linked worktree with git must work — it reads
    // commondir + per-worktree HEAD correctly.
    let st = git(&["status", "--porcelain"], &secondary_path);
    assert!(st.status.success(), "git status in linked worktree failed");
    assert!(
        st.stdout.is_empty(),
        "linked worktree should be clean, got {:?}",
        String::from_utf8_lossy(&st.stdout)
    );
}

#[test]
fn rustygit_list_after_git_worktree_add() {
    if !has_system_git() {
        return;
    }
    let main = TempDir::new().unwrap();
    let secondary = TempDir::new().unwrap();
    let secondary_path = secondary.path().join("via-git");
    let _head = make_repo_with_commit(main.path());

    // Use git to add the worktree.
    let r = git(
        &[
            "worktree",
            "add",
            "-b",
            "git-branch",
            secondary_path.to_str().unwrap(),
        ],
        main.path(),
    );
    assert!(
        r.status.success(),
        "git worktree add failed: {}",
        String::from_utf8_lossy(&r.stderr)
    );

    // rustygit worktree list should see both.
    let l = rustygit(&["worktree", "list"], main.path());
    assert!(l.status.success());
    let listing = String::from_utf8_lossy(&l.stdout);
    let want = secondary_path.canonicalize().unwrap();
    let want_str = want.to_string_lossy().into_owned();
    let want_alt = want_str.strip_prefix("/private").unwrap_or(&want_str);
    assert!(
        listing.contains(&want_str) || listing.contains(want_alt),
        "rustygit should list the git-created secondary ({want_str} or {want_alt}): {listing}"
    );
    assert!(
        listing.contains("git-branch"),
        "rustygit should name the secondary's branch: {listing}"
    );
}

#[test]
fn worktree_remove_drops_admin_and_worktree() {
    if !has_system_git() {
        return;
    }
    let main = TempDir::new().unwrap();
    let secondary = TempDir::new().unwrap();
    let secondary_path = secondary.path().join("temp-wt");
    let _head = make_repo_with_commit(main.path());

    let add = rustygit(
        &[
            "worktree",
            "add",
            secondary_path.to_str().unwrap(),
            "-b",
            "tmpbr",
        ],
        main.path(),
    );
    assert!(
        add.status.success(),
        "add: {}",
        String::from_utf8_lossy(&add.stderr)
    );

    let admin = main.path().join(".git/worktrees/temp-wt");
    assert!(admin.exists(), "admin dir should exist before remove");

    let rm = rustygit(
        &["worktree", "remove", secondary_path.to_str().unwrap()],
        main.path(),
    );
    assert!(
        rm.status.success(),
        "remove failed: {}",
        String::from_utf8_lossy(&rm.stderr)
    );

    assert!(!secondary_path.exists(), "secondary path should be deleted");
    assert!(!admin.exists(), "admin dir should be deleted");
}

#[test]
fn worktree_prune_drops_orphaned_admin_entries() {
    if !has_system_git() {
        return;
    }
    let main = TempDir::new().unwrap();
    let secondary = TempDir::new().unwrap();
    let secondary_path = secondary.path().join("orphan");
    let _head = make_repo_with_commit(main.path());

    assert!(rustygit(
        &[
            "worktree",
            "add",
            secondary_path.to_str().unwrap(),
            "-b",
            "x"
        ],
        main.path()
    )
    .status
    .success());

    // Manually rm -rf the secondary worktree to make the admin entry orphan.
    std::fs::remove_dir_all(&secondary_path).unwrap();

    let admin = main.path().join(".git/worktrees/orphan");
    assert!(admin.exists(), "admin should still exist before prune");

    let p = rustygit(&["worktree", "prune"], main.path());
    assert!(p.status.success());
    let stdout = String::from_utf8_lossy(&p.stdout);
    assert!(
        stdout.contains("orphan"),
        "prune should announce the removal: {stdout}"
    );
    assert!(!admin.exists(), "admin should be gone after prune");
}

#[test]
fn worktree_add_refuses_existing_path() {
    let main = TempDir::new().unwrap();
    let _head = make_repo_with_commit(main.path());

    // Existing dir → refuse.
    let exists_path = main.path().join("existing");
    std::fs::create_dir(&exists_path).unwrap();
    let out = rustygit(
        &["worktree", "add", exists_path.to_str().unwrap(), "-b", "x"],
        main.path(),
    );
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("already exists"),
        "stderr should explain refusal: {stderr}"
    );
}

#[test]
fn worktree_list_porcelain_format() {
    let tmp = TempDir::new().unwrap();
    let head = make_repo_with_commit(tmp.path());
    let out = rustygit(&["worktree", "list", "--porcelain"], tmp.path());
    assert!(out.status.success());
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("worktree "), "porcelain output: {s}");
    assert!(
        s.contains(&format!("HEAD {head}")),
        "porcelain HEAD line: {s}"
    );
    assert!(
        s.contains("branch refs/heads/master") || s.contains("branch refs/heads/main"),
        "porcelain branch line: {s}"
    );
}

#[test]
fn rustygit_in_linked_worktree_reads_correct_head() {
    if !has_system_git() {
        return;
    }
    let main = TempDir::new().unwrap();
    let secondary = TempDir::new().unwrap();
    let secondary_path = secondary.path().join("readback");
    let main_head = make_repo_with_commit(main.path());

    assert!(rustygit(
        &[
            "worktree",
            "add",
            secondary_path.to_str().unwrap(),
            "-b",
            "read"
        ],
        main.path()
    )
    .status
    .success());

    // From inside the linked worktree, rustygit rev-parse HEAD should
    // resolve to the same oid as the main (since "read" was started at HEAD).
    let r = rustygit(&["rev-parse", "HEAD"], &secondary_path);
    assert!(
        r.status.success(),
        "rev-parse from linked worktree failed: stderr={}",
        String::from_utf8_lossy(&r.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&r.stdout).trim(),
        main_head,
        "linked HEAD should match the main repo's HEAD"
    );

    // And `status --porcelain` should be empty (clean).
    let st = rustygit(&["status", "--porcelain"], &secondary_path);
    assert!(st.status.success());
    assert!(
        st.stdout.is_empty(),
        "linked worktree status not clean: {:?}",
        String::from_utf8_lossy(&st.stdout)
    );
}

#[test]
fn cross_oracle_fsck_after_rustygit_add() {
    if !has_system_git() {
        return;
    }
    let main = TempDir::new().unwrap();
    let secondary = TempDir::new().unwrap();
    let secondary_path = secondary.path().join("fsck-wt");
    let _head = make_repo_with_commit(main.path());

    assert!(rustygit(
        &[
            "worktree",
            "add",
            secondary_path.to_str().unwrap(),
            "-b",
            "fsck"
        ],
        main.path()
    )
    .status
    .success());

    // git fsck on the main repo must be clean (no dangling/broken refs from
    // the worktree create).
    let f = Command::new("git")
        .args(["fsck", "--full"])
        .current_dir(main.path())
        .output()
        .unwrap();
    assert!(
        f.status.success(),
        "git fsck failed after worktree add: stderr={}",
        String::from_utf8_lossy(&f.stderr)
    );
}
