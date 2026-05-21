//! M8 byte-compatibility for `clone` (local) and `unpack-objects`.

mod common;

use std::path::Path;
use std::process::{Command, Stdio};

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

fn make_source_repo(at: &Path) {
    git(&["init", "-q", "."], at);
    for (i, content) in ["one", "two", "three"].iter().enumerate() {
        std::fs::write(at.join("f.txt"), content).unwrap();
        git(&["add", "f.txt"], at);
        git(
            &[
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "commit",
                "-q",
                "-m",
                &format!("c{i}"),
            ],
            at,
        );
    }
}

#[test]
fn clone_local_log_matches_source() {
    if !has_system_git() {
        return;
    }
    let src = TempDir::new().unwrap();
    make_source_repo(src.path());

    let dst_root = TempDir::new().unwrap();
    let dst = dst_root.path().join("clone");

    assert_success(
        &rustygit(
            &[
                "clone",
                "-q",
                src.path().to_str().unwrap(),
                dst.to_str().unwrap(),
            ],
            std::env::current_dir().unwrap().as_path(),
        ),
        "clone",
    );

    // git fsck must pass on the result.
    let fsck = Command::new("git")
        .args(["fsck", "--full"])
        .current_dir(&dst)
        .output()
        .unwrap();
    assert!(
        fsck.status.success(),
        "git fsck failed: {}",
        String::from_utf8_lossy(&fsck.stderr)
    );

    // Logs byte-match.
    let src_log = git(&["log", "--oneline"], src.path());
    let dst_log = git(&["log", "--oneline"], &dst);
    assert_eq!(src_log.stdout, dst_log.stdout);
}

#[test]
fn clone_creates_remote_tracking_refs() {
    if !has_system_git() {
        return;
    }
    let src = TempDir::new().unwrap();
    make_source_repo(src.path());
    git(&["branch", "feature"], src.path());

    let dst_root = TempDir::new().unwrap();
    let dst = dst_root.path().join("clone");

    assert_success(
        &rustygit(
            &[
                "clone",
                "-q",
                src.path().to_str().unwrap(),
                dst.to_str().unwrap(),
            ],
            std::env::current_dir().unwrap().as_path(),
        ),
        "clone",
    );

    let r = git(&["show-ref"], &dst);
    let refs = String::from_utf8(r.stdout).unwrap();
    assert!(refs.contains("refs/heads/master"));
    assert!(refs.contains("refs/remotes/origin/master"));
    assert!(refs.contains("refs/remotes/origin/feature"));
}

#[test]
fn clone_refuses_non_empty_destination() {
    if !has_system_git() {
        return;
    }
    let src = TempDir::new().unwrap();
    make_source_repo(src.path());

    let dst_root = TempDir::new().unwrap();
    let dst = dst_root.path().join("clone");
    std::fs::create_dir_all(&dst).unwrap();
    std::fs::write(dst.join("preexisting.txt"), b"x").unwrap();

    let r = rustygit(
        &["clone", src.path().to_str().unwrap(), dst.to_str().unwrap()],
        std::env::current_dir().unwrap().as_path(),
    );
    assert!(!r.status.success(), "should refuse non-empty dst");
    assert!(
        dst.join("preexisting.txt").exists(),
        "must not delete pre-existing files on refusal"
    );
}

#[test]
fn clone_no_checkout_skips_workdir() {
    if !has_system_git() {
        return;
    }
    let src = TempDir::new().unwrap();
    make_source_repo(src.path());

    let dst_root = TempDir::new().unwrap();
    let dst = dst_root.path().join("clone");

    assert_success(
        &rustygit(
            &[
                "clone",
                "-q",
                "--no-checkout",
                src.path().to_str().unwrap(),
                dst.to_str().unwrap(),
            ],
            std::env::current_dir().unwrap().as_path(),
        ),
        "clone --no-checkout",
    );

    assert!(dst.join(".git").is_dir());
    assert!(!dst.join("f.txt").exists());
}

#[test]
fn unpack_objects_explodes_a_pack() {
    if !has_system_git() {
        return;
    }
    let src = TempDir::new().unwrap();
    make_source_repo(src.path());
    git(&["gc", "--aggressive", "-q"], src.path());

    // Find the pack.
    let packs: Vec<_> = std::fs::read_dir(src.path().join(".git/objects/pack"))
        .unwrap()
        .flatten()
        .filter_map(|e| {
            let p = e.path();
            if p.extension().and_then(|s| s.to_str()) == Some("pack") {
                Some(p)
            } else {
                None
            }
        })
        .collect();
    assert!(!packs.is_empty(), "expected at least one pack");
    let pack_bytes = std::fs::read(&packs[0]).unwrap();

    // New empty repo to receive the unpacked objects.
    let dst = TempDir::new().unwrap();
    assert_success(&rustygit(&["init", "-q", "."], dst.path()), "init");

    // Pipe the pack bytes through unpack-objects via a raw process::Command
    // (assert_cmd's wrapper doesn't expose stdin piping directly).
    let bin = AssertCmd::cargo_bin("rustygit")
        .unwrap()
        .get_program()
        .to_owned();
    let mut child = Command::new(bin)
        .args(["unpack-objects", "-q"])
        .current_dir(dst.path())
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    use std::io::Write;
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(&pack_bytes)
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "unpack-objects failed: stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Every object the source pack contained should now be loose in dst.
    // We approximate by counting loose-object files.
    let loose_count = walk_loose(&dst.path().join(".git/objects"));
    assert!(loose_count > 0, "expected unpacked loose objects");

    // git fsck should be happy with the resulting object database.
    let fsck = Command::new("git")
        .args(["fsck", "--full", "--no-dangling"])
        .current_dir(dst.path())
        .output()
        .unwrap();
    assert!(
        fsck.status.success(),
        "git fsck failed after unpack-objects: {}",
        String::from_utf8_lossy(&fsck.stderr)
    );
}

fn walk_loose(objects_dir: &Path) -> usize {
    let mut count = 0;
    let entries = match std::fs::read_dir(objects_dir) {
        Ok(it) => it,
        Err(_) => return 0,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name == "pack" || name == "info" {
            continue;
        }
        if path.is_dir() && name.len() == 2 && name.chars().all(|c| c.is_ascii_hexdigit()) {
            if let Ok(inner) = std::fs::read_dir(&path) {
                count += inner.flatten().count();
            }
        }
    }
    count
}
