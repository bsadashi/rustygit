//! M2 byte-compatibility for refs / rev-parse / update-ref / show-ref / symbolic-ref.
//!
//! Approach: drive a tempdir in lockstep with system git, asserting that
//! ref-related output and on-disk content match.

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
fn update_ref_then_git_show_ref_agrees() {
    if !has_system_git() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    assert_success(&rustygit(&["init", "-q", "."], tmp.path()), "init");

    // Build a real commit via git so branches can legally point at it.
    std::fs::write(tmp.path().join("a.txt"), b"a").unwrap();
    git(&["add", "a.txt"], tmp.path());
    git(
        &[
            "-c",
            "user.email=t@t",
            "-c",
            "user.name=t",
            "commit",
            "-q",
            "-m",
            "first",
        ],
        tmp.path(),
    );
    let head = git(&["rev-parse", "HEAD"], tmp.path());
    let oid = String::from_utf8(head.stdout).unwrap().trim().to_string();

    // Create three refs via rustygit. (master already exists from the commit.)
    for name in ["refs/heads/feature", "refs/heads/main", "refs/tags/v1"] {
        assert_success(&rustygit(&["update-ref", name, &oid], tmp.path()), name);
    }

    // git show-ref must see all three with the right oid.
    let out = git(&["show-ref"], tmp.path());
    let listing = String::from_utf8(out.stdout).unwrap();
    for name in ["refs/heads/feature", "refs/heads/main", "refs/tags/v1"] {
        assert!(
            listing.contains(name),
            "git show-ref missing {name}\n{listing}"
        );
        let line = listing.lines().find(|l| l.ends_with(name)).unwrap();
        assert!(line.starts_with(&oid), "wrong oid for {name}: {line}");
    }
}

#[test]
fn show_ref_byte_matches_git() {
    if !has_system_git() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    git(&["init", "-q", "."], tmp.path());

    // Real commit so branch refs are valid (git rejects branches that point at
    // non-commits, which would block this test even though our show-ref reads
    // them fine).
    std::fs::write(tmp.path().join("a.txt"), b"a").unwrap();
    git(&["add", "a.txt"], tmp.path());
    git(
        &[
            "-c",
            "user.email=t@t",
            "-c",
            "user.name=t",
            "commit",
            "-q",
            "-m",
            "first",
        ],
        tmp.path(),
    );
    let head_out = git(&["rev-parse", "HEAD"], tmp.path());
    let oid = String::from_utf8(head_out.stdout)
        .unwrap()
        .trim()
        .to_string();

    for name in ["refs/heads/feature", "refs/tags/v0.1"] {
        git(&["update-ref", name, &oid], tmp.path());
    }

    let g = git(&["show-ref"], tmp.path());
    let r = rustygit(&["show-ref"], tmp.path());
    assert!(r.status.success());
    assert_eq!(r.stdout, g.stdout, "show-ref byte mismatch");
}

#[test]
fn rev_parse_resolves_full_short_and_prefix() {
    if !has_system_git() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    git(&["init", "-q", "."], tmp.path());

    // Build something with real history so rev-parse has something interesting.
    std::fs::write(tmp.path().join("a.txt"), b"a").unwrap();
    git(&["add", "a.txt"], tmp.path());
    git(
        &[
            "-c",
            "user.email=t@t",
            "-c",
            "user.name=t",
            "commit",
            "-q",
            "-m",
            "first",
        ],
        tmp.path(),
    );

    // git's rev-parse on HEAD
    let g_head = git(&["rev-parse", "HEAD"], tmp.path());
    let head_oid = String::from_utf8(g_head.stdout.clone())
        .unwrap()
        .trim()
        .to_string();

    let r_head = rustygit(&["rev-parse", "HEAD"], tmp.path());
    assert!(r_head.status.success());
    assert_eq!(r_head.stdout, g_head.stdout);

    // Short prefix
    let prefix = &head_oid[..7];
    let r_pref = rustygit(&["rev-parse", prefix], tmp.path());
    assert!(r_pref.status.success());
    assert_eq!(String::from_utf8(r_pref.stdout).unwrap().trim(), head_oid);

    // HEAD^{tree}
    let g_tree = git(&["rev-parse", "HEAD^{tree}"], tmp.path());
    let r_tree = rustygit(&["rev-parse", "HEAD^{tree}"], tmp.path());
    assert!(r_tree.status.success());
    assert_eq!(r_tree.stdout, g_tree.stdout);
}

#[test]
fn rev_parse_walks_parent_and_ancestor_suffixes() {
    if !has_system_git() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    git(&["init", "-q", "."], tmp.path());

    // Three sequential commits.
    for (i, content) in ["one", "two", "three"].iter().enumerate() {
        std::fs::write(tmp.path().join("f.txt"), content).unwrap();
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
                &format!("commit {i}"),
            ],
            tmp.path(),
        );
    }

    for expr in ["HEAD", "HEAD~1", "HEAD~2", "HEAD^", "HEAD^^"] {
        let g = git(&["rev-parse", expr], tmp.path());
        let r = rustygit(&["rev-parse", expr], tmp.path());
        assert!(r.status.success(), "rustygit rev-parse {expr} failed");
        assert_eq!(r.stdout, g.stdout, "mismatch on {expr}");
    }
}

#[test]
fn symbolic_ref_read_and_write() {
    if !has_system_git() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    assert_success(&rustygit(&["init", "-q", "."], tmp.path()), "init");

    // HEAD should already point at refs/heads/master after init.
    let r = rustygit(&["symbolic-ref", "HEAD"], tmp.path());
    assert!(r.status.success());
    assert_eq!(
        String::from_utf8(r.stdout).unwrap().trim(),
        "refs/heads/master"
    );

    // Repoint HEAD at a different branch.
    assert_success(
        &rustygit(&["symbolic-ref", "HEAD", "refs/heads/develop"], tmp.path()),
        "symbolic-ref write",
    );
    let g = git(&["symbolic-ref", "HEAD"], tmp.path());
    assert_eq!(
        String::from_utf8(g.stdout).unwrap().trim(),
        "refs/heads/develop"
    );

    // --short form
    let r_short = rustygit(&["symbolic-ref", "--short", "HEAD"], tmp.path());
    assert!(r_short.status.success());
    assert_eq!(String::from_utf8(r_short.stdout).unwrap().trim(), "develop");
}

#[test]
fn update_ref_old_value_check_blocks_mismatched_update() {
    if !has_system_git() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    assert_success(&rustygit(&["init", "-q", "."], tmp.path()), "init");

    let mk = |content: &[u8]| {
        AssertCmd::cargo_bin("rustygit")
            .unwrap()
            .args(["hash-object", "-w", "--stdin"])
            .current_dir(tmp.path())
            .write_stdin(content)
            .output()
            .unwrap()
    };
    let oid_a = String::from_utf8(mk(b"alpha").stdout)
        .unwrap()
        .trim()
        .to_string();
    let oid_b = String::from_utf8(mk(b"beta").stdout)
        .unwrap()
        .trim()
        .to_string();

    // Initial create.
    assert_success(
        &rustygit(&["update-ref", "refs/heads/x", &oid_a], tmp.path()),
        "create x",
    );
    // Update with correct oldvalue must succeed.
    assert_success(
        &rustygit(&["update-ref", "refs/heads/x", &oid_b, &oid_a], tmp.path()),
        "advance x",
    );
    // Update with wrong oldvalue must fail (exit code 1 = our convention for
    // ref-update old-value mismatch).
    let bad = rustygit(&["update-ref", "refs/heads/x", &oid_a, &oid_a], tmp.path());
    assert!(!bad.status.success(), "stale oldvalue should have rejected");
}

#[test]
fn reflog_appended_on_update() {
    if !has_system_git() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    assert_success(&rustygit(&["init", "-q", "."], tmp.path()), "init");

    let oid_out = AssertCmd::cargo_bin("rustygit")
        .unwrap()
        .args(["hash-object", "-w", "--stdin"])
        .current_dir(tmp.path())
        .write_stdin("payload")
        .output()
        .unwrap();
    let oid = String::from_utf8(oid_out.stdout)
        .unwrap()
        .trim()
        .to_string();

    assert_success(
        &rustygit(
            &["update-ref", "-m", "test create", "refs/heads/loggy", &oid],
            tmp.path(),
        ),
        "update-ref",
    );

    let log_path = tmp.path().join(".git/logs/refs/heads/loggy");
    assert!(log_path.exists(), "reflog file not created at {log_path:?}");
    let line = std::fs::read_to_string(&log_path).unwrap();
    let line = line.lines().next().unwrap();
    // Format: <40-or-64 hex> <40-or-64 hex> <ident> <ts> <offset>\t<msg>
    assert!(line.starts_with("0000000000000000000000000000000000000000"));
    assert!(line.contains(&oid));
    assert!(line.ends_with("test create"));
}
