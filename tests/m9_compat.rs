//! M9 compatibility: pack writing, repack, gc.

mod common;

use std::path::Path;
use std::process::{Command, Stdio};

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

fn make_source_repo(at: &Path, commits: usize) {
    git(&["init", "-q", "."], at);
    for i in 0..commits {
        std::fs::write(at.join("f.txt"), format!("v{i}\n")).unwrap();
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

fn count_loose(at: &Path) -> usize {
    let objects = at.join(".git/objects");
    let mut count = 0;
    if let Ok(entries) = std::fs::read_dir(&objects) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name == "pack" || name == "info" {
                continue;
            }
            if entry.path().is_dir()
                && name.len() == 2
                && name.chars().all(|c| c.is_ascii_hexdigit())
            {
                if let Ok(inner) = std::fs::read_dir(entry.path()) {
                    count += inner.flatten().count();
                }
            }
        }
    }
    count
}

fn count_packs(at: &Path) -> usize {
    let pack_dir = at.join(".git/objects/pack");
    let mut count = 0;
    if let Ok(entries) = std::fs::read_dir(&pack_dir) {
        for entry in entries.flatten() {
            if entry.path().extension().and_then(|s| s.to_str()) == Some("pack") {
                count += 1;
            }
        }
    }
    count
}

#[test]
fn repack_creates_pack_and_removes_loose_with_d() {
    if !has_system_git() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    make_source_repo(tmp.path(), 5);

    let loose_before = count_loose(tmp.path());
    assert!(loose_before > 0, "expected loose objects before repack");

    assert_success(
        &rustygit(&["repack", "-a", "-d"], tmp.path()),
        "repack -a -d",
    );

    assert_eq!(count_loose(tmp.path()), 0, "loose objects should be gone");
    assert_eq!(count_packs(tmp.path()), 1);

    // git fsck must pass.
    let fsck = Command::new("git")
        .args(["fsck", "--full"])
        .current_dir(tmp.path())
        .output()
        .unwrap();
    assert!(
        fsck.status.success(),
        "git fsck failed after rustygit repack: {}",
        String::from_utf8_lossy(&fsck.stderr)
    );
}

#[test]
fn rustygit_pack_is_readable_by_git_verify_pack() {
    if !has_system_git() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    make_source_repo(tmp.path(), 3);

    assert_success(&rustygit(&["repack", "-a", "-d"], tmp.path()), "repack");

    let pack_path: std::path::PathBuf = std::fs::read_dir(tmp.path().join(".git/objects/pack"))
        .unwrap()
        .flatten()
        .find_map(|e| {
            let p = e.path();
            if p.extension().and_then(|s| s.to_str()) == Some("pack") {
                Some(p)
            } else {
                None
            }
        })
        .unwrap();

    let out = Command::new("git")
        .args(["verify-pack", "-v", pack_path.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git verify-pack rejected our pack: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn log_still_works_after_repack() {
    if !has_system_git() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    make_source_repo(tmp.path(), 5);

    let log_before = git(&["log", "--oneline"], tmp.path());
    assert_success(&rustygit(&["repack", "-a", "-d"], tmp.path()), "repack");
    let log_after = git(&["log", "--oneline"], tmp.path());

    // git itself agrees the log content is unchanged.
    assert_eq!(log_before.stdout, log_after.stdout);

    // rustygit's log reads through the new pack.
    let our_log = rustygit(&["log", "--oneline"], tmp.path());
    assert!(our_log.status.success());
}

#[test]
fn pack_objects_round_trips_via_unpack_objects() {
    if !has_system_git() {
        return;
    }
    let src = TempDir::new().unwrap();
    make_source_repo(src.path(), 4);

    // Collect every oid via git rev-list (covers commits + trees + blobs).
    let oids_out = git(&["rev-list", "--all", "--objects"], src.path());
    let oids: Vec<String> = String::from_utf8(oids_out.stdout)
        .unwrap()
        .lines()
        .filter_map(|l| l.split_whitespace().next().map(|s| s.to_string()))
        .collect();
    let stdin_payload = oids.join("\n") + "\n";

    // Pack them via rustygit pack-objects --stdout.
    let bin = AssertCmd::cargo_bin("rustygit")
        .unwrap()
        .get_program()
        .to_owned();
    let mut child = Command::new(&bin)
        .args(["pack-objects", "--stdout", "pack"])
        .current_dir(src.path())
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
        .write_all(stdin_payload.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "pack-objects --stdout failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let pack_bytes = out.stdout;
    assert!(
        pack_bytes.starts_with(b"PACK"),
        "stdout is not a valid pack"
    );

    // Receive into a fresh repo via rustygit unpack-objects.
    let dst = TempDir::new().unwrap();
    assert_success(&rustygit(&["init", "-q", "."], dst.path()), "init");
    let mut child = Command::new(&bin)
        .args(["unpack-objects", "-q"])
        .current_dir(dst.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(&pack_bytes)
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "unpack-objects failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Every oid that was in the source pack is now loose in dst.
    let dst_loose = count_loose(dst.path());
    assert_eq!(
        dst_loose,
        oids.len(),
        "expected {} loose, got {}",
        oids.len(),
        dst_loose
    );
}

#[test]
fn gc_consolidates_to_single_pack() {
    if !has_system_git() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    make_source_repo(tmp.path(), 3);
    // First repack to get one pack.
    assert_success(
        &rustygit(&["repack", "-a", "-d"], tmp.path()),
        "first repack",
    );
    // Add more loose objects via another commit.
    std::fs::write(tmp.path().join("f.txt"), b"after-pack\n").unwrap();
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
            "more",
        ],
        tmp.path(),
    );
    let loose_mid = count_loose(tmp.path());
    assert!(loose_mid > 0);

    assert_success(&rustygit(&["gc"], tmp.path()), "gc");

    assert_eq!(count_loose(tmp.path()), 0);
    assert_eq!(count_packs(tmp.path()), 1);

    let fsck = Command::new("git")
        .args(["fsck", "--full"])
        .current_dir(tmp.path())
        .output()
        .unwrap();
    assert!(
        fsck.status.success(),
        "fsck after gc failed: {}",
        String::from_utf8_lossy(&fsck.stderr)
    );
}
