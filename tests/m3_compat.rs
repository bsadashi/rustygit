//! M3 byte-compatibility for index + add + commit + log.

mod common;

use std::path::Path;
use std::process::Command;

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
fn add_then_git_ls_files_stage_agrees() {
    if !has_system_git() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    assert_success(&rustygit(&["init", "-q", "."], tmp.path()), "init");

    std::fs::create_dir_all(tmp.path().join("src")).unwrap();
    std::fs::write(tmp.path().join("src/lib.rs"), b"// lib\n").unwrap();
    std::fs::write(tmp.path().join("src/main.rs"), b"fn main(){}\n").unwrap();
    std::fs::write(tmp.path().join("README.md"), b"# r\n").unwrap();

    assert_success(&rustygit(&["add", "."], tmp.path()), "add");

    // git's perspective on the index we wrote.
    let g = git(&["ls-files", "--stage"], tmp.path());
    let listing = String::from_utf8(g.stdout).unwrap();
    assert!(listing.contains("src/lib.rs"));
    assert!(listing.contains("src/main.rs"));
    assert!(listing.contains("README.md"));
    // Each line: "<mode> <oid> <stage>\t<path>". All stage-0.
    for line in listing.lines() {
        assert!(line.contains(" 0\t"), "non-zero stage: {line}");
    }
}

#[test]
fn write_tree_byte_matches_git_write_tree() {
    if !has_system_git() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    assert_success(&rustygit(&["init", "-q", "."], tmp.path()), "init");

    std::fs::create_dir_all(tmp.path().join("a/b")).unwrap();
    std::fs::write(tmp.path().join("a/b/leaf"), b"leaf\n").unwrap();
    std::fs::write(tmp.path().join("top.txt"), b"top\n").unwrap();

    assert_success(&rustygit(&["add", "."], tmp.path()), "add");
    let r = rustygit(&["write-tree"], tmp.path());
    assert_success(&r, "write-tree");
    let our_tree = String::from_utf8(r.stdout).unwrap().trim().to_string();

    // Now do the same flow with system git only and compare oids.
    let tmp2 = TempDir::new().unwrap();
    git(&["init", "-q", "."], tmp2.path());
    std::fs::create_dir_all(tmp2.path().join("a/b")).unwrap();
    std::fs::write(tmp2.path().join("a/b/leaf"), b"leaf\n").unwrap();
    std::fs::write(tmp2.path().join("top.txt"), b"top\n").unwrap();
    git(&["add", "."], tmp2.path());
    let g = git(&["write-tree"], tmp2.path());
    let git_tree = String::from_utf8(g.stdout).unwrap().trim().to_string();
    assert_eq!(our_tree, git_tree, "tree oid mismatch");
}

#[test]
fn commit_creates_a_commit_git_can_read() {
    if !has_system_git() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    assert_success(&rustygit(&["init", "-q", "."], tmp.path()), "init");

    std::fs::write(tmp.path().join("a.txt"), b"alpha\n").unwrap();
    assert_success(&rustygit(&["add", "."], tmp.path()), "add");
    assert_success(
        &rustygit(&["commit", "-m", "first commit"], tmp.path()),
        "commit",
    );

    // git can fsck the result.
    let fsck = Command::new("git")
        .args(["fsck", "--full"])
        .current_dir(tmp.path())
        .output()
        .unwrap();
    assert!(
        fsck.status.success(),
        "git fsck failed: {}",
        String::from_utf8_lossy(&fsck.stderr)
    );

    // git rev-parse HEAD resolves to a commit object.
    let h = git(&["rev-parse", "HEAD"], tmp.path());
    let head = String::from_utf8(h.stdout).unwrap().trim().to_string();
    let t = git(&["cat-file", "-t", &head], tmp.path());
    assert_eq!(t.stdout, b"commit\n");

    // Message round-trips
    let p = git(&["log", "-1", "--pretty=%B"], tmp.path());
    let msg = String::from_utf8(p.stdout).unwrap();
    assert_eq!(msg.trim(), "first commit");
}

#[test]
fn log_byte_matches_git_log_single_commit() {
    if !has_system_git() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    assert_success(&rustygit(&["init", "-q", "."], tmp.path()), "init");

    std::fs::write(tmp.path().join("a.txt"), b"a").unwrap();
    assert_success(&rustygit(&["add", "."], tmp.path()), "add");
    assert_success(&rustygit(&["commit", "-m", "single"], tmp.path()), "commit");

    let g = git(&["log"], tmp.path());
    let r = rustygit(&["log"], tmp.path());
    assert!(r.status.success());
    assert_eq!(r.stdout, g.stdout, "log byte-match");
}

#[test]
fn log_byte_matches_git_log_multi_commit() {
    if !has_system_git() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    assert_success(&rustygit(&["init", "-q", "."], tmp.path()), "init");

    for (i, body) in ["one", "two", "three"].iter().enumerate() {
        std::fs::write(tmp.path().join("f.txt"), body).unwrap();
        assert_success(&rustygit(&["add", "."], tmp.path()), "add");
        assert_success(
            &rustygit(&["commit", "-m", &format!("msg {i}")], tmp.path()),
            "commit",
        );
    }

    let g = git(&["log"], tmp.path());
    let r = rustygit(&["log"], tmp.path());
    assert!(r.status.success());
    assert_eq!(r.stdout, g.stdout, "multi-commit log byte-match");
}

#[test]
fn log_oneline_format() {
    if !has_system_git() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    assert_success(&rustygit(&["init", "-q", "."], tmp.path()), "init");
    std::fs::write(tmp.path().join("a.txt"), b"a").unwrap();
    assert_success(&rustygit(&["add", "."], tmp.path()), "add");
    assert_success(&rustygit(&["commit", "-m", "topic"], tmp.path()), "c");

    let r = rustygit(&["log", "--oneline"], tmp.path());
    assert!(r.status.success());
    let r_stdout = r.stdout.clone();
    let line = String::from_utf8(r.stdout).unwrap();
    let line = line.trim_end();
    // git's --oneline abbreviates to 7 chars by default; format is
    //   <short-oid> <message>
    let parts: Vec<&str> = line.splitn(2, ' ').collect();
    assert_eq!(parts.len(), 2);
    assert_eq!(
        parts[0].len(),
        7,
        "oid should be 7-char abbrev: {}",
        parts[0]
    );
    assert_eq!(parts[1], "topic");

    // And byte-match git's own --oneline output.
    let g = git(&["log", "--oneline"], tmp.path());
    assert_eq!(r_stdout, g.stdout, "log --oneline byte-match");
}

#[test]
fn log_abbrev_width_matches_git() {
    if !has_system_git() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    assert_success(&rustygit(&["init", "-q", "."], tmp.path()), "init");
    std::fs::write(tmp.path().join("a.txt"), b"a").unwrap();
    assert_success(&rustygit(&["add", "."], tmp.path()), "add");
    assert_success(&rustygit(&["commit", "-m", "c1"], tmp.path()), "c1");
    std::fs::write(tmp.path().join("a.txt"), b"a2").unwrap();
    assert_success(&rustygit(&["add", "."], tmp.path()), "add2");
    assert_success(&rustygit(&["commit", "-m", "c2"], tmp.path()), "c2");

    // --abbrev=<N> width respected and matches git.
    for n in [4, 7, 10, 14] {
        let arg = format!("--abbrev={n}");
        let r = rustygit(&["log", "--oneline", &arg], tmp.path());
        let g = git(&["log", "--oneline", &arg], tmp.path());
        assert!(r.status.success());
        assert_eq!(
            r.stdout, g.stdout,
            "log --oneline {arg} should byte-match git"
        );
    }
}

#[test]
fn second_commit_chains_to_first() {
    if !has_system_git() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    assert_success(&rustygit(&["init", "-q", "."], tmp.path()), "init");
    std::fs::write(tmp.path().join("a.txt"), b"a").unwrap();
    assert_success(&rustygit(&["add", "."], tmp.path()), "add a");
    assert_success(
        &rustygit(&["commit", "-m", "first"], tmp.path()),
        "commit 1",
    );
    let h1 = String::from_utf8(rustygit(&["rev-parse", "HEAD"], tmp.path()).stdout)
        .unwrap()
        .trim()
        .to_string();

    std::fs::write(tmp.path().join("b.txt"), b"b").unwrap();
    assert_success(&rustygit(&["add", "."], tmp.path()), "add b");
    assert_success(
        &rustygit(&["commit", "-m", "second"], tmp.path()),
        "commit 2",
    );
    let h2 = String::from_utf8(rustygit(&["rev-parse", "HEAD"], tmp.path()).stdout)
        .unwrap()
        .trim()
        .to_string();

    assert_ne!(h1, h2);

    let h2_parent = git(&["rev-parse", "HEAD^"], tmp.path());
    assert_eq!(
        String::from_utf8(h2_parent.stdout).unwrap().trim(),
        h1,
        "second commit's parent should be first"
    );
}

#[test]
fn log_on_synthetic_shallow_clone_stops_at_boundary() {
    // We build a tiny "synthetic shallow" repo manually: drop a fake
    // `.git/shallow` file whose lone oid is the current HEAD. The HEAD
    // commit's `parent` line points at an earlier real commit; without
    // shallow-awareness, `rustygit log` would walk back past HEAD and
    // (since we haven't actually removed the parent from the odb) keep
    // going. With shallow-awareness it stops at the listed boundary.
    if !has_system_git() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    git(&["init", "-q", "."], tmp.path());
    std::fs::write(tmp.path().join("f.txt"), b"v1").unwrap();
    git(&["add", "f.txt"], tmp.path());
    git(
        &[
            "-c",
            "user.email=t@t",
            "-c",
            "user.name=t",
            "commit",
            "-q",
            "-m",
            "c1",
        ],
        tmp.path(),
    );
    std::fs::write(tmp.path().join("f.txt"), b"v2").unwrap();
    git(&["add", "f.txt"], tmp.path());
    git(
        &[
            "-c",
            "user.email=t@t",
            "-c",
            "user.name=t",
            "commit",
            "-q",
            "-m",
            "c2",
        ],
        tmp.path(),
    );

    let head_out = git(&["rev-parse", "HEAD"], tmp.path());
    let head = String::from_utf8(head_out.stdout)
        .unwrap()
        .trim()
        .to_string();

    // Mark HEAD as the shallow boundary.
    std::fs::write(tmp.path().join(".git/shallow"), format!("{head}\n")).unwrap();

    let r = rustygit(&["log"], tmp.path());
    assert!(r.status.success(), "log should succeed on shallow repo");
    let stdout = String::from_utf8(r.stdout).unwrap();
    let commit_lines = stdout.matches("\ncommit ").count() + stdout.starts_with("commit ") as usize;
    assert_eq!(
        commit_lines, 1,
        "shallow-aware log should stop at boundary; got:\n{stdout}"
    );
}

#[test]
fn rustygit_can_read_an_index_written_by_git() {
    // Inverse round-trip: git writes the index, we read it back via write-tree.
    if !has_system_git() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    git(&["init", "-q", "."], tmp.path());
    std::fs::create_dir_all(tmp.path().join("subdir")).unwrap();
    std::fs::write(tmp.path().join("subdir/x"), b"x").unwrap();
    std::fs::write(tmp.path().join("y"), b"y").unwrap();
    git(&["add", "."], tmp.path());

    let r = rustygit(&["write-tree"], tmp.path());
    assert!(r.status.success());
    let our = String::from_utf8(r.stdout).unwrap().trim().to_string();
    let g = git(&["write-tree"], tmp.path());
    let theirs = String::from_utf8(g.stdout).unwrap().trim().to_string();
    assert_eq!(our, theirs);
}
