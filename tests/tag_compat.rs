//! `rustygit tag` — exhaustive oracle tests against system `git`.

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

// --- Lightweight tag ------------------------------------------------------

/// `rustygit tag v1` creates a lightweight tag at HEAD that git
/// reads back identically (same target oid).
#[test]
fn lightweight_tag_at_head_round_trips_via_git() {
    if !has_system_git() {
        return;
    }
    let tmp = init_repo();
    commit_file(tmp.path(), "f", b"x\n", "first");
    let head = String::from_utf8(git(&["rev-parse", "HEAD"], tmp.path()).stdout)
        .unwrap()
        .trim()
        .to_string();

    assert_ok(&rustygit(&["tag", "v1"], tmp.path()), "rustygit tag v1");

    let target = String::from_utf8(git(&["rev-parse", "v1"], tmp.path()).stdout)
        .unwrap()
        .trim()
        .to_string();
    assert_eq!(target, head, "v1 should point at HEAD");
    // Lightweight tag is a direct ref to a commit (not a tag object).
    let kind = String::from_utf8(git(&["cat-file", "-t", "v1"], tmp.path()).stdout).unwrap();
    assert_eq!(kind.trim(), "commit");
}

/// `rustygit tag v1 <oid>` creates a lightweight tag at the given oid.
#[test]
fn lightweight_tag_at_explicit_oid() {
    if !has_system_git() {
        return;
    }
    let tmp = init_repo();
    commit_file(tmp.path(), "f", b"x\n", "first");
    let first = String::from_utf8(git(&["rev-parse", "HEAD"], tmp.path()).stdout)
        .unwrap()
        .trim()
        .to_string();
    commit_file(tmp.path(), "f", b"y\n", "second");

    assert_ok(
        &rustygit(&["tag", "v1", &first], tmp.path()),
        "tag v1 <first>",
    );

    let target = String::from_utf8(git(&["rev-parse", "v1"], tmp.path()).stdout)
        .unwrap()
        .trim()
        .to_string();
    assert_eq!(target, first);
}

// --- List -----------------------------------------------------------------

/// Bare `tag` lists every tag, one per line, alphabetically — byte-equal
/// to `git tag`.
#[test]
fn list_byte_matches_git() {
    if !has_system_git() {
        return;
    }
    let tmp = init_repo();
    commit_file(tmp.path(), "f", b"x\n", "first");

    // Create three tags via git directly.
    git(&["tag", "z-tag"], tmp.path());
    git(&["tag", "a-tag"], tmp.path());
    git(&["tag", "m-tag"], tmp.path());

    let ours = rustygit(&["tag"], tmp.path()).stdout;
    let theirs = git(&["tag"], tmp.path()).stdout;
    assert_eq!(ours, theirs, "tag list byte-mismatch");
}

/// `tag -l <pattern>` filters by shell glob.
#[test]
fn list_with_pattern_filters() {
    if !has_system_git() {
        return;
    }
    let tmp = init_repo();
    commit_file(tmp.path(), "f", b"x\n", "first");
    git(&["tag", "v1.0"], tmp.path());
    git(&["tag", "v1.1"], tmp.path());
    git(&["tag", "v2.0"], tmp.path());

    let ours = rustygit(&["tag", "-l", "v1.*"], tmp.path()).stdout;
    let theirs = git(&["tag", "-l", "v1.*"], tmp.path()).stdout;
    assert_eq!(ours, theirs, "filtered list byte-mismatch");
}

// --- Annotated tag --------------------------------------------------------

/// `tag -a -m <msg> <name>` creates a tag object that git reads back as
/// a tag with the expected target, name, and message.
#[test]
fn annotated_tag_creates_tag_object() {
    if !has_system_git() {
        return;
    }
    let tmp = init_repo();
    commit_file(tmp.path(), "f", b"x\n", "first");
    let head = String::from_utf8(git(&["rev-parse", "HEAD"], tmp.path()).stdout)
        .unwrap()
        .trim()
        .to_string();

    assert_ok(
        &rustygit(&["tag", "-a", "-m", "the release", "v1.0"], tmp.path()),
        "rustygit tag -a",
    );

    // `cat-file -t v1.0` should be `tag` (not `commit`).
    let kind = String::from_utf8(git(&["cat-file", "-t", "v1.0"], tmp.path()).stdout).unwrap();
    assert_eq!(kind.trim(), "tag");
    // Peel to the underlying commit.
    let target = String::from_utf8(git(&["rev-parse", "v1.0^{commit}"], tmp.path()).stdout)
        .unwrap()
        .trim()
        .to_string();
    assert_eq!(target, head);

    // The tag body should include the message.
    let body = String::from_utf8(git(&["cat-file", "tag", "v1.0"], tmp.path()).stdout).unwrap();
    assert!(body.contains("\nthe release\n"), "tag body: {body:?}");
    assert!(body.contains("\ntag v1.0\n"), "tag body: {body:?}");
    assert!(body.contains("\ntype commit\n"), "tag body: {body:?}");
}

/// rustygit-annotated tag is verifiable via `git tag -v`-style checks
/// (no signature; just structure): tag body parses, target peels.
#[test]
fn annotated_tag_byte_matches_git_for_same_inputs() {
    if !has_system_git() {
        return;
    }
    let ours = init_repo();
    let theirs = init_repo();

    for tmp in [ours.path(), theirs.path()] {
        commit_file(tmp, "f", b"x\n", "first");
    }

    assert_ok(
        &rustygit(&["tag", "-a", "-m", "v1 release", "v1"], ours.path()),
        "rustygit tag -a",
    );
    let out = AssertCmd::new("git")
        .args(["-c", "user.email=t@t", "-c", "user.name=t"])
        .args(["tag", "-a", "-m", "v1 release", "v1"])
        .current_dir(theirs.path())
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .env("GIT_AUTHOR_DATE", "1700000000 +0000")
        .env("GIT_COMMITTER_DATE", "1700000000 +0000")
        .output()
        .unwrap();
    assert!(out.status.success());

    // Same tagger ident + date + message + target ⇒ same tag oid.
    let our_oid = String::from_utf8(git(&["rev-parse", "v1"], ours.path()).stdout)
        .unwrap()
        .trim()
        .to_string();
    let their_oid = String::from_utf8(git(&["rev-parse", "v1"], theirs.path()).stdout)
        .unwrap()
        .trim()
        .to_string();
    assert_eq!(our_oid, their_oid, "tag oid byte-mismatch");
}

// --- Delete ---------------------------------------------------------------

/// `tag -d <name>` removes the tag and exit 0.
#[test]
fn delete_existing_tag() {
    if !has_system_git() {
        return;
    }
    let tmp = init_repo();
    commit_file(tmp.path(), "f", b"x\n", "first");
    git(&["tag", "v1"], tmp.path());

    assert_ok(&rustygit(&["tag", "-d", "v1"], tmp.path()), "tag -d v1");

    let r = AssertCmd::new("git")
        .args(["rev-parse", "v1"])
        .current_dir(tmp.path())
        .output()
        .unwrap();
    assert!(!r.status.success(), "tag should be gone after delete");
}

/// `tag -d <missing>` errors with exit 1 and a clear message.
#[test]
fn delete_missing_tag_errors() {
    if !has_system_git() {
        return;
    }
    let tmp = init_repo();
    commit_file(tmp.path(), "f", b"x\n", "first");

    let r = rustygit(&["tag", "-d", "nope"], tmp.path());
    assert_eq!(r.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&r.stderr).contains("not found"));
}

/// `tag -d <a> <b>` deletes multiple tags.
#[test]
fn delete_multiple_tags() {
    if !has_system_git() {
        return;
    }
    let tmp = init_repo();
    commit_file(tmp.path(), "f", b"x\n", "first");
    git(&["tag", "a"], tmp.path());
    git(&["tag", "b"], tmp.path());
    git(&["tag", "c"], tmp.path());

    assert_ok(
        &rustygit(&["tag", "-d", "a", "b"], tmp.path()),
        "tag -d a b",
    );

    let listing = String::from_utf8(git(&["tag"], tmp.path()).stdout).unwrap();
    assert_eq!(listing, "c\n");
}

// --- Force overwrite ------------------------------------------------------

/// Creating a tag that already exists errors with exit 128 (matches git).
#[test]
fn create_duplicate_without_f_errors() {
    if !has_system_git() {
        return;
    }
    let tmp = init_repo();
    commit_file(tmp.path(), "f", b"x\n", "first");
    git(&["tag", "v1"], tmp.path());

    let r = rustygit(&["tag", "v1"], tmp.path());
    assert_eq!(r.status.code(), Some(128));
    assert!(String::from_utf8_lossy(&r.stderr).contains("already exists"));
}

/// `tag -f <name> <new-oid>` reassigns an existing tag.
#[test]
fn force_reassigns_existing_tag() {
    if !has_system_git() {
        return;
    }
    let tmp = init_repo();
    commit_file(tmp.path(), "f", b"x\n", "first");
    let first = String::from_utf8(git(&["rev-parse", "HEAD"], tmp.path()).stdout)
        .unwrap()
        .trim()
        .to_string();
    commit_file(tmp.path(), "f", b"y\n", "second");
    let second = String::from_utf8(git(&["rev-parse", "HEAD"], tmp.path()).stdout)
        .unwrap()
        .trim()
        .to_string();
    git(&["tag", "v1", &first], tmp.path());

    assert_ok(
        &rustygit(&["tag", "-f", "v1", &second], tmp.path()),
        "tag -f",
    );

    let now = String::from_utf8(git(&["rev-parse", "v1"], tmp.path()).stdout)
        .unwrap()
        .trim()
        .to_string();
    assert_eq!(now, second);
}

// --- Cross-reads ----------------------------------------------------------

/// A tag written by `git tag -a` is listed correctly by `rustygit tag`.
#[test]
fn rustygit_list_reads_git_written_tags() {
    if !has_system_git() {
        return;
    }
    let tmp = init_repo();
    commit_file(tmp.path(), "f", b"x\n", "first");
    // git creates an annotated tag
    let out = AssertCmd::new("git")
        .args(["-c", "user.email=t@t", "-c", "user.name=t"])
        .args(["tag", "-a", "-m", "x", "v1"])
        .current_dir(tmp.path())
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .env("GIT_AUTHOR_DATE", "1700000000 +0000")
        .env("GIT_COMMITTER_DATE", "1700000000 +0000")
        .output()
        .unwrap();
    assert!(out.status.success());

    let listing = String::from_utf8(rustygit(&["tag"], tmp.path()).stdout).unwrap();
    assert!(listing.contains("v1\n"));
}

/// A tag written by `rustygit tag -a` is read correctly by `git tag --list`.
#[test]
fn git_list_reads_rustygit_written_tags() {
    if !has_system_git() {
        return;
    }
    let tmp = init_repo();
    commit_file(tmp.path(), "f", b"x\n", "first");
    assert_ok(
        &rustygit(&["tag", "-a", "-m", "x", "v1"], tmp.path()),
        "rg tag",
    );

    let listing = String::from_utf8(git(&["tag"], tmp.path()).stdout).unwrap();
    assert!(listing.contains("v1\n"));
}

// --- Tagging non-commit objects ------------------------------------------

/// `tag -a v-blob -m … <blob-oid>` creates a tag pointing at a blob.
/// The tag body's `type` header must be `blob`.
#[test]
fn annotated_tag_can_point_at_blob() {
    if !has_system_git() {
        return;
    }
    let tmp = init_repo();
    commit_file(tmp.path(), "f", b"hello\n", "first");
    // Get the blob oid from the tree.
    let blob_oid = String::from_utf8(git(&["rev-parse", "HEAD:f"], tmp.path()).stdout)
        .unwrap()
        .trim()
        .to_string();

    assert_ok(
        &rustygit(
            &["tag", "-a", "-m", "blob tag", "v-blob", &blob_oid],
            tmp.path(),
        ),
        "tag blob",
    );

    let body = String::from_utf8(git(&["cat-file", "tag", "v-blob"], tmp.path()).stdout).unwrap();
    assert!(
        body.contains("type blob"),
        "tag body should say type blob: {body:?}"
    );
    assert!(body.contains(&format!("object {blob_oid}")));
}

// --- Edge cases -----------------------------------------------------------

/// Bare `tag` in a repo with no tags prints nothing and exits 0.
#[test]
fn list_empty_repo_no_tags_prints_nothing() {
    if !has_system_git() {
        return;
    }
    let tmp = init_repo();
    commit_file(tmp.path(), "f", b"x\n", "first");

    let r = rustygit(&["tag"], tmp.path());
    assert_eq!(r.status.code(), Some(0));
    assert!(
        r.stdout.is_empty(),
        "stdout should be empty: {:?}",
        String::from_utf8_lossy(&r.stdout)
    );
}

/// `tag -a` without `-m` errors (editor flow deferred).
#[test]
fn annotated_without_message_errors() {
    if !has_system_git() {
        return;
    }
    let tmp = init_repo();
    commit_file(tmp.path(), "f", b"x\n", "first");

    let r = rustygit(&["tag", "-a", "v1"], tmp.path());
    assert_eq!(r.status.code(), Some(128));
    assert!(String::from_utf8_lossy(&r.stderr).contains("editor flow"));
}

/// `-m` alone (no -a) implies -a.
#[test]
fn message_implies_annotated() {
    if !has_system_git() {
        return;
    }
    let tmp = init_repo();
    commit_file(tmp.path(), "f", b"x\n", "first");

    assert_ok(
        &rustygit(&["tag", "-m", "implicit annot", "v1"], tmp.path()),
        "tag -m",
    );
    let kind = String::from_utf8(git(&["cat-file", "-t", "v1"], tmp.path()).stdout).unwrap();
    assert_eq!(kind.trim(), "tag");
}
