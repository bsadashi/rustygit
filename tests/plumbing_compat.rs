//! M1 byte-compatibility for the loose object plumbing (`hash-object`,
//! `cat-file`, `ls-tree`).

mod common;

use std::io::Write;
use std::process::{Command, Stdio};

use assert_cmd::Command as AssertCmd;
use common::{git, has_system_git};
use tempfile::TempDir;

fn rustygit_path() -> std::path::PathBuf {
    AssertCmd::cargo_bin("rustygit")
        .unwrap()
        .get_program()
        .into()
}

fn rustygit_stdin(args: &[&str], cwd: &std::path::Path, stdin: &[u8]) -> std::process::Output {
    let mut child = Command::new(rustygit_path())
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.as_mut().unwrap().write_all(stdin).unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "rustygit {args:?} failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    out
}

fn git_stdin(args: &[&str], cwd: &std::path::Path, stdin: &[u8]) -> std::process::Output {
    let mut child = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.as_mut().unwrap().write_all(stdin).unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "git {args:?} failed\nstderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out
}

#[test]
fn hash_object_stdin_matches_git() {
    if !has_system_git() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    git(&["init", "-q", "."], tmp.path());

    let payloads: &[&[u8]] = &[
        b"",
        b"hello world",
        b"line 1\nline 2\nline 3\n",
        &[0u8, 1, 2, 3, 0xff, 0xfe], // binary payload
    ];
    for payload in payloads {
        let ours = rustygit_stdin(&["hash-object", "--stdin"], tmp.path(), payload);
        let theirs = git_stdin(&["hash-object", "--stdin"], tmp.path(), payload);
        assert_eq!(ours.stdout, theirs.stdout, "byte mismatch on {payload:?}");
    }
}

#[test]
fn write_with_rustygit_read_with_git() {
    if !has_system_git() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    AssertCmd::cargo_bin("rustygit")
        .unwrap()
        .args(["init", "-q", "."])
        .current_dir(tmp.path())
        .assert()
        .success();

    let payload = b"this object was written by rustygit\n";
    let ours = rustygit_stdin(&["hash-object", "-w", "--stdin"], tmp.path(), payload);
    let oid = String::from_utf8(ours.stdout).unwrap().trim().to_string();

    let g_t = git(&["cat-file", "-t", &oid], tmp.path());
    assert_eq!(g_t.stdout, b"blob\n");
    let g_p = git(&["cat-file", "-p", &oid], tmp.path());
    assert_eq!(g_p.stdout, payload);
}

#[test]
fn write_with_git_read_with_rustygit() {
    if !has_system_git() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    git(&["init", "-q", "."], tmp.path());

    let payload = b"this object was written by git\n";
    let theirs = git_stdin(&["hash-object", "-w", "--stdin"], tmp.path(), payload);
    let oid = String::from_utf8(theirs.stdout).unwrap().trim().to_string();

    let ours_t = AssertCmd::cargo_bin("rustygit")
        .unwrap()
        .args(["cat-file", "-t", &oid])
        .current_dir(tmp.path())
        .output()
        .unwrap();
    assert!(ours_t.status.success());
    assert_eq!(ours_t.stdout, b"blob\n");

    let ours_p = AssertCmd::cargo_bin("rustygit")
        .unwrap()
        .args(["cat-file", "-p", &oid])
        .current_dir(tmp.path())
        .output()
        .unwrap();
    assert!(ours_p.status.success());
    assert_eq!(ours_p.stdout, payload);
}

#[test]
fn cat_file_size_matches_git() {
    if !has_system_git() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    AssertCmd::cargo_bin("rustygit")
        .unwrap()
        .args(["init", "-q", "."])
        .current_dir(tmp.path())
        .assert()
        .success();

    let payload = b"twelve bytes";
    let ours = rustygit_stdin(&["hash-object", "-w", "--stdin"], tmp.path(), payload);
    let oid = String::from_utf8(ours.stdout).unwrap().trim().to_string();

    let s = AssertCmd::cargo_bin("rustygit")
        .unwrap()
        .args(["cat-file", "-s", &oid])
        .current_dir(tmp.path())
        .output()
        .unwrap();
    assert!(s.status.success());
    assert_eq!(s.stdout, format!("{}\n", payload.len()).into_bytes());

    let theirs_s = git(&["cat-file", "-s", &oid], tmp.path());
    assert_eq!(s.stdout, theirs_s.stdout);
}

#[test]
fn cat_file_exists_returns_correct_exit_code() {
    if !has_system_git() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    AssertCmd::cargo_bin("rustygit")
        .unwrap()
        .args(["init", "-q", "."])
        .current_dir(tmp.path())
        .assert()
        .success();

    let payload = b"x";
    let ours = rustygit_stdin(&["hash-object", "-w", "--stdin"], tmp.path(), payload);
    let oid = String::from_utf8(ours.stdout).unwrap().trim().to_string();

    let exists = AssertCmd::cargo_bin("rustygit")
        .unwrap()
        .args(["cat-file", "-e", &oid])
        .current_dir(tmp.path())
        .output()
        .unwrap();
    assert_eq!(exists.status.code(), Some(0));

    let absent = "0000000000000000000000000000000000000000";
    let absent_out = AssertCmd::cargo_bin("rustygit")
        .unwrap()
        .args(["cat-file", "-e", absent])
        .current_dir(tmp.path())
        .output()
        .unwrap();
    assert_ne!(absent_out.status.code(), Some(0));
}

#[test]
fn ls_tree_matches_git_for_real_repo() {
    if !has_system_git() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    git(&["init", "-q", "."], tmp.path());

    // Build a small tree with subdirectories using git.
    std::fs::create_dir_all(tmp.path().join("src")).unwrap();
    std::fs::create_dir_all(tmp.path().join("docs")).unwrap();
    std::fs::write(tmp.path().join("src/main.rs"), b"fn main(){}\n").unwrap();
    std::fs::write(tmp.path().join("src/lib.rs"), b"// lib\n").unwrap();
    std::fs::write(tmp.path().join("docs/README.md"), b"# docs\n").unwrap();
    std::fs::write(tmp.path().join("top.txt"), b"hello\n").unwrap();

    git(&["add", "."], tmp.path());
    git(
        &[
            "-c",
            "user.email=x@y.z",
            "-c",
            "user.name=t",
            "commit",
            "-q",
            "-m",
            "init",
        ],
        tmp.path(),
    );

    let tree_oid_out = git(&["rev-parse", "HEAD^{tree}"], tmp.path());
    let tree_oid = String::from_utf8(tree_oid_out.stdout)
        .unwrap()
        .trim()
        .to_string();

    for flags in [&[][..], &["-r"], &["-r", "-t"], &["--name-only", "-r"]] {
        let mut g_args = vec!["ls-tree"];
        g_args.extend_from_slice(flags);
        g_args.push(&tree_oid);
        let theirs = git(&g_args, tmp.path());

        let mut o_args = vec!["ls-tree"];
        o_args.extend_from_slice(flags);
        o_args.push(&tree_oid);
        let ours = AssertCmd::cargo_bin("rustygit")
            .unwrap()
            .args(&o_args)
            .current_dir(tmp.path())
            .output()
            .unwrap();
        assert!(ours.status.success(), "rustygit {o_args:?} failed");
        assert_eq!(
            ours.stdout, theirs.stdout,
            "ls-tree byte mismatch for flags {flags:?}"
        );
    }
}
