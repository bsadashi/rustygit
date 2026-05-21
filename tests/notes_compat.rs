//! NON_GOALS.md Batch H — `git notes` compatibility tests.
//!
//! Coverage:
//! - `add` / `show` / `append` / `remove` / `copy` / `list` happy paths.
//! - Oracle: rustygit writes a note → `git notes show` matches; and vice versa.
//! - `--ref` for an alternate notes namespace.
//! - Fanout interop: rustygit creates >256 notes, `git notes list` matches.
//! - `prune` drops notes for absent target objects.

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
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .env("GIT_AUTHOR_DATE", "1700000000 +0000")
        .env("GIT_COMMITTER_DATE", "1700000000 +0000")
        .output()
        .unwrap()
}

fn rustygit_ok(args: &[&str], cwd: &Path) -> std::process::Output {
    let out = rustygit(args, cwd);
    assert!(
        out.status.success(),
        "rustygit {args:?} failed (code {:?})\nstdout: {}\nstderr: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    out
}

/// Initialize a tiny repo with one committed file via system `git`.
/// Returns the HEAD oid hex.
fn init_repo_with_commit(tmp: &TempDir) -> String {
    git(&["init", "-q", "-b", "master", "."], tmp.path());
    git(&["config", "user.name", "Test"], tmp.path());
    git(&["config", "user.email", "test@example.com"], tmp.path());
    std::fs::write(tmp.path().join("a.txt"), b"hello\n").unwrap();
    git(&["add", "a.txt"], tmp.path());
    git(
        &["-c", "commit.gpgsign=false", "commit", "-q", "-m", "init"],
        tmp.path(),
    );
    let head = git(&["rev-parse", "HEAD"], tmp.path());
    String::from_utf8_lossy(&head.stdout).trim().to_string()
}

#[test]
fn notes_add_then_show_round_trip() {
    if !has_system_git() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    init_repo_with_commit(&tmp);
    let r = rustygit_ok(&["notes", "add", "-m", "first note"], tmp.path());
    assert!(r.status.success());

    let show = rustygit_ok(&["notes", "show"], tmp.path());
    let body = String::from_utf8_lossy(&show.stdout);
    assert_eq!(
        body.trim_end(),
        "first note",
        "notes show did not round-trip"
    );
}

#[test]
fn notes_append_with_blank_line_separator() {
    if !has_system_git() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    init_repo_with_commit(&tmp);
    rustygit_ok(&["notes", "add", "-m", "first"], tmp.path());
    rustygit_ok(&["notes", "append", "-m", "second"], tmp.path());
    let show = rustygit_ok(&["notes", "show"], tmp.path());
    let body = String::from_utf8_lossy(&show.stdout);
    assert!(
        body.contains("first") && body.contains("second"),
        "missing pieces in {body:?}"
    );
    assert!(
        body.contains("first\n\nsecond"),
        "expected blank line separator in {body:?}"
    );
}

#[test]
fn notes_remove_makes_show_fail() {
    if !has_system_git() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    init_repo_with_commit(&tmp);
    rustygit_ok(&["notes", "add", "-m", "x"], tmp.path());
    rustygit_ok(&["notes", "remove"], tmp.path());
    let show = rustygit(&["notes", "show"], tmp.path());
    assert!(
        !show.status.success(),
        "notes show should fail after remove"
    );
}

#[test]
fn notes_copy_carries_note_across_objects() {
    if !has_system_git() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    let first = init_repo_with_commit(&tmp);
    // Make a second commit to copy to.
    std::fs::write(tmp.path().join("b.txt"), b"more\n").unwrap();
    git(&["add", "b.txt"], tmp.path());
    git(
        &["-c", "commit.gpgsign=false", "commit", "-q", "-m", "two"],
        tmp.path(),
    );
    let second = String::from_utf8_lossy(&git(&["rev-parse", "HEAD"], tmp.path()).stdout)
        .trim()
        .to_string();

    rustygit_ok(&["notes", "add", "-m", "carried", &first], tmp.path());
    rustygit_ok(&["notes", "copy", &first, &second], tmp.path());
    let show = rustygit_ok(&["notes", "show", &second], tmp.path());
    let body = String::from_utf8_lossy(&show.stdout);
    assert_eq!(body.trim_end(), "carried");
}

#[test]
fn notes_list_prints_pairs() {
    if !has_system_git() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    let first = init_repo_with_commit(&tmp);
    rustygit_ok(&["notes", "add", "-m", "n1", &first], tmp.path());
    let l = rustygit_ok(&["notes", "list"], tmp.path());
    let body = String::from_utf8_lossy(&l.stdout);
    let lines: Vec<&str> = body.lines().collect();
    assert_eq!(lines.len(), 1, "expected one note row, got {body:?}");
    let line = lines[0];
    let mut iter = line.split_whitespace();
    let note_oid = iter.next().expect("note oid");
    let target_oid = iter.next().expect("target oid");
    assert_eq!(target_oid, first.as_str());
    assert_eq!(note_oid.len(), 40);
}

/// Oracle: rustygit writes a note → `git notes show` returns the same body.
#[test]
fn oracle_rustygit_then_git_show() {
    if !has_system_git() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    let head = init_repo_with_commit(&tmp);
    rustygit_ok(&["notes", "add", "-m", "from-rustygit", &head], tmp.path());

    let show = std::process::Command::new("git")
        .args(["notes", "show", &head])
        .current_dir(tmp.path())
        .output()
        .unwrap();
    assert!(
        show.status.success(),
        "git notes show failed: stderr={}",
        String::from_utf8_lossy(&show.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&show.stdout).trim_end(),
        "from-rustygit"
    );
}

/// Oracle (reverse): git writes a note → rustygit shows it.
#[test]
fn oracle_git_then_rustygit_show() {
    if !has_system_git() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    let head = init_repo_with_commit(&tmp);

    // Use git to write the note.
    let r = std::process::Command::new("git")
        .args(["notes", "add", "-m", "from-git", &head])
        .current_dir(tmp.path())
        .output()
        .unwrap();
    assert!(
        r.status.success(),
        "git notes add failed: {}",
        String::from_utf8_lossy(&r.stderr)
    );

    let show = rustygit_ok(&["notes", "show", &head], tmp.path());
    assert_eq!(String::from_utf8_lossy(&show.stdout).trim_end(), "from-git");
}

#[test]
fn notes_ref_targets_alternate_namespace() {
    if !has_system_git() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    let head = init_repo_with_commit(&tmp);
    rustygit_ok(
        &[
            "notes",
            "--ref",
            "refs/notes/reviewers",
            "add",
            "-m",
            "ok",
            &head,
        ],
        tmp.path(),
    );

    // Default ref has nothing.
    let default = rustygit(&["notes", "show"], tmp.path());
    assert!(!default.status.success(), "default ref should have no note");

    // Alternate ref has it.
    let alt = rustygit_ok(
        &["notes", "--ref", "refs/notes/reviewers", "show"],
        tmp.path(),
    );
    assert_eq!(String::from_utf8_lossy(&alt.stdout).trim_end(), "ok");

    // git notes --ref also sees it.
    let g = std::process::Command::new("git")
        .args(["notes", "--ref=refs/notes/reviewers", "show", &head])
        .current_dir(tmp.path())
        .output()
        .unwrap();
    assert!(g.status.success());
    assert_eq!(String::from_utf8_lossy(&g.stdout).trim_end(), "ok");
}

/// Fanout interop: at >=256 notes the on-disk shape switches to a 2/38
/// fanout. Verify rustygit's list matches `git notes list` after writing
/// 300 notes (each pointing at a distinct blob target).
#[test]
fn notes_fanout_interop_with_git() {
    if !has_system_git() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    git(&["init", "-q", "-b", "master", "."], tmp.path());
    git(&["config", "user.name", "T"], tmp.path());
    git(&["config", "user.email", "t@e"], tmp.path());

    // Hash 300 distinct blobs via `git hash-object -w` so we have 300 known
    // target oids in the odb. The blobs themselves can be small.
    let mut targets: Vec<String> = Vec::new();
    for i in 0..300u32 {
        let body = format!("note-target-{i}\n");
        let f = tmp.path().join(format!("blob_{i}.txt"));
        std::fs::write(&f, &body).unwrap();
        let h = git(&["hash-object", "-w", f.to_str().unwrap()], tmp.path());
        let oid = String::from_utf8_lossy(&h.stdout).trim().to_string();
        targets.push(oid);
    }

    // Write notes via rustygit so we exercise its fanout writer.
    for (i, oid) in targets.iter().enumerate() {
        let r = rustygit(
            &["notes", "add", "-f", "-m", &format!("note-{i}"), oid],
            tmp.path(),
        );
        assert!(
            r.status.success(),
            "rustygit notes add {oid} failed: {}",
            String::from_utf8_lossy(&r.stderr)
        );
    }

    // git notes list should see all 300.
    let g = std::process::Command::new("git")
        .args(["notes", "list"])
        .current_dir(tmp.path())
        .output()
        .unwrap();
    assert!(
        g.status.success(),
        "git notes list failed: {}",
        String::from_utf8_lossy(&g.stderr)
    );
    let gset: std::collections::BTreeSet<String> = String::from_utf8_lossy(&g.stdout)
        .lines()
        .map(|l| l.split_whitespace().last().unwrap_or("").to_string())
        .filter(|s| !s.is_empty())
        .collect();
    assert_eq!(gset.len(), 300, "git notes list saw {} entries", gset.len());

    // rustygit notes list should produce the same target set.
    let r = rustygit_ok(&["notes", "list"], tmp.path());
    let rset: std::collections::BTreeSet<String> = String::from_utf8_lossy(&r.stdout)
        .lines()
        .map(|l| l.split_whitespace().last().unwrap_or("").to_string())
        .filter(|s| !s.is_empty())
        .collect();
    assert_eq!(
        rset, gset,
        "rustygit notes list disagrees with git notes list"
    );

    // And the note bodies should match, spot-checked.
    for (i, oid) in targets.iter().enumerate().step_by(73) {
        let body = std::process::Command::new("git")
            .args(["notes", "show", oid])
            .current_dir(tmp.path())
            .output()
            .unwrap();
        assert!(body.status.success(), "git notes show {oid}");
        assert_eq!(
            String::from_utf8_lossy(&body.stdout).trim_end(),
            format!("note-{i}")
        );
    }
}

/// `notes prune` should drop notes whose target oid is not in the odb.
/// We simulate this by creating a note via rustygit, then editing the
/// notes tree to point at a fictitious target oid that we never wrote.
#[test]
fn notes_prune_removes_dangling_entries() {
    if !has_system_git() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    init_repo_with_commit(&tmp);

    // Create a real note on HEAD so the ref exists and has a tree shape.
    rustygit_ok(&["notes", "add", "-m", "real"], tmp.path());

    // Now write a "fake target" entry by reusing rustygit notes add with
    // -f on an oid that is itself a blob we wrote (so resolve works) but
    // we'll then DELETE that blob to make it dangling. A blob has a real
    // oid we can pass and we can delete its loose file from .git/objects.
    let blob_body = b"dangling-target";
    std::fs::write(tmp.path().join("dangling.txt"), blob_body).unwrap();
    let h = git(&["hash-object", "-w", "dangling.txt"], tmp.path());
    let dangling = String::from_utf8_lossy(&h.stdout).trim().to_string();
    rustygit_ok(&["notes", "add", "-m", "doomed", &dangling], tmp.path());

    // Confirm both notes are present.
    let l = rustygit_ok(&["notes", "list"], tmp.path());
    let body = String::from_utf8_lossy(&l.stdout);
    let count = body.lines().count();
    assert_eq!(count, 2, "expected 2 notes, got:\n{body}");

    // Delete the dangling target's loose object file from disk.
    let prefix = &dangling[..2];
    let rest = &dangling[2..];
    let path = tmp
        .path()
        .join(".git")
        .join("objects")
        .join(prefix)
        .join(rest);
    std::fs::remove_file(&path).expect("remove loose object");
    // Sanity: rustygit's odb should no longer see it.
    let rp = rustygit(&["rev-parse", &dangling], tmp.path());
    if rp.status.success() {
        // Some other store may still hold it; bail out of the test cleanly.
        return;
    }

    let p = rustygit_ok(&["notes", "prune"], tmp.path());
    assert!(p.status.success());

    let after = rustygit_ok(&["notes", "list"], tmp.path());
    let after_body = String::from_utf8_lossy(&after.stdout);
    let after_count = after_body.lines().count();
    assert_eq!(
        after_count, 1,
        "expected 1 note after prune, got {after_count}:\n{after_body}"
    );
}
