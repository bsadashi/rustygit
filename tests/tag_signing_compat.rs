//! `tag -s`, `mktag`, and `verify-tag` oracle tests.
//!
//! gpg-gated tests use the same scaffold as `non_goals_signing.rs`:
//! a disposable `GNUPGHOME` with a passphraseless RSA key generated on
//! the fly. Skipped cleanly when `gpg` isn't on PATH.

mod common;

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use assert_cmd::Command as AssertCmd;
use common::{git, has_system_git};
use tempfile::TempDir;

fn rustygit_in(
    args: &[&str],
    cwd: &Path,
    gnupghome: Option<&Path>,
    stdin: Option<&[u8]>,
) -> std::process::Output {
    let mut cmd = AssertCmd::cargo_bin("rustygit").unwrap();
    cmd.args(args)
        .current_dir(cwd)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .env("GIT_AUTHOR_DATE", "1700000000 +0000")
        .env("GIT_COMMITTER_DATE", "1700000000 +0000");
    if let Some(home) = gnupghome {
        cmd.env("GNUPGHOME", home);
    }
    if let Some(data) = stdin {
        cmd.write_stdin(data.to_vec());
    }
    cmd.output().unwrap()
}

fn rustygit(args: &[&str], cwd: &Path) -> std::process::Output {
    rustygit_in(args, cwd, None, None)
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

fn has_gpg() -> bool {
    Command::new("gpg")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Returns (GNUPGHOME, key fingerprint) or None if gpg isn't usable.
fn setup_test_gpg() -> Option<(TempDir, String)> {
    if !has_gpg() {
        return None;
    }
    let home = TempDir::new().ok()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(home.path(), std::fs::Permissions::from_mode(0o700)).ok()?;
    }
    let batch = "\
%no-protection
Key-Type: RSA
Key-Length: 2048
Subkey-Type: RSA
Subkey-Length: 2048
Name-Real: RustygitTagTest
Name-Email: tag-test@example.com
Expire-Date: 0
%commit
";
    let mut child = Command::new("gpg")
        .env("GNUPGHOME", home.path())
        .args([
            "--batch",
            "--quick-gen-key",
            "--passphrase",
            "",
            "--yes",
            "--generate-key",
            "-",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .ok()?;
    child.stdin.as_mut()?.write_all(batch.as_bytes()).ok()?;
    let out = child.wait_with_output().ok()?;
    if !out.status.success() {
        eprintln!(
            "skipping: gpg key gen failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        return None;
    }
    let list = Command::new("gpg")
        .env("GNUPGHOME", home.path())
        .args(["--list-secret-keys", "--with-colons"])
        .output()
        .ok()?;
    if !list.status.success() {
        return None;
    }
    let listing = String::from_utf8_lossy(&list.stdout);
    let mut fpr: Option<String> = None;
    for line in listing.lines() {
        if line.starts_with("fpr:") {
            let fields: Vec<&str> = line.split(':').collect();
            if let Some(f) = fields.get(9) {
                fpr = Some((*f).to_string());
                break;
            }
        }
    }
    Some((home, fpr?))
}

// =========================================================================
// mktag plumbing
// =========================================================================

/// `rustygit mktag` reads a well-formed tag body on stdin and produces
/// the SAME oid as `git mktag` for the same input. This is the
/// strongest possible cross-binary test: byte-equality of the framed
/// object and SHA-1 over it.
#[test]
fn mktag_oid_matches_git() {
    if !has_system_git() {
        return;
    }
    let tmp = init_repo();
    commit_file(tmp.path(), "f", b"x\n", "first");
    let head = String::from_utf8(git(&["rev-parse", "HEAD"], tmp.path()).stdout)
        .unwrap()
        .trim()
        .to_string();

    let body = format!(
        "object {head}\n\
         type commit\n\
         tag v-mktag\n\
         tagger t <t@t> 1700000000 +0000\n\
         \n\
         mktag oracle\n"
    );

    let our_out = rustygit_in(&["mktag"], tmp.path(), None, Some(body.as_bytes()));
    assert_ok(&our_out, "rustygit mktag");
    let our_oid = String::from_utf8(our_out.stdout)
        .unwrap()
        .trim()
        .to_string();

    // Run git mktag the same way.
    let mut child = Command::new("git")
        .args(["-C", tmp.path().to_str().unwrap(), "mktag"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(body.as_bytes())
        .unwrap();
    let git_out = child.wait_with_output().unwrap();
    assert!(
        git_out.status.success(),
        "git mktag failed: {}",
        String::from_utf8_lossy(&git_out.stderr)
    );
    let git_oid = String::from_utf8(git_out.stdout)
        .unwrap()
        .trim()
        .to_string();

    assert_eq!(our_oid, git_oid, "mktag oid byte-mismatch vs git");

    // Sanity: the object stored under our_oid is the same bytes we fed in.
    let stored = git(&["cat-file", "tag", &our_oid], tmp.path()).stdout;
    assert_eq!(stored, body.as_bytes());
}

/// `rustygit mktag` rejects a malformed body (missing required header).
#[test]
fn mktag_rejects_missing_object_header() {
    if !has_system_git() {
        return;
    }
    let tmp = init_repo();
    commit_file(tmp.path(), "f", b"x\n", "first");

    let body = b"type commit\ntag v-bad\n\nmsg\n";
    let r = rustygit_in(&["mktag"], tmp.path(), None, Some(body));
    assert_eq!(r.status.code(), Some(128));
    let err = String::from_utf8_lossy(&r.stderr);
    assert!(err.contains("malformed"), "stderr: {err:?}");
}

// =========================================================================
// tag -s (signed annotated tag)
// =========================================================================

/// `rustygit tag -s -m … <name>` creates a PGP-signed tag that
/// **`git verify-tag`** accepts as a good signature.
#[test]
fn rustygit_signed_tag_verifies_with_git() {
    if !has_system_git() {
        return;
    }
    let Some((gpg_home, fpr)) = setup_test_gpg() else {
        eprintln!("skip: no gpg / key gen failed");
        return;
    };
    let tmp = init_repo();
    commit_file(tmp.path(), "f", b"x\n", "first");
    // Point user.signingkey at our test key so gpg picks it up.
    git(&["config", "user.signingkey", &fpr], tmp.path());

    let r = rustygit_in(
        &["tag", "-s", "-m", "signed v1", "v1"],
        tmp.path(),
        Some(gpg_home.path()),
        None,
    );
    assert_ok(&r, "rustygit tag -s");

    let body = String::from_utf8(git(&["cat-file", "tag", "v1"], tmp.path()).stdout).unwrap();
    assert!(
        body.contains("-----BEGIN PGP SIGNATURE-----"),
        "signed tag body should embed a PGP block: {body:?}"
    );

    // git verify-tag should accept our signature.
    let v = Command::new("git")
        .args(["-C", tmp.path().to_str().unwrap(), "verify-tag", "v1"])
        .env("GNUPGHOME", gpg_home.path())
        .output()
        .unwrap();
    assert!(
        v.status.success(),
        "git verify-tag should accept rustygit-signed tag\nstderr: {}",
        String::from_utf8_lossy(&v.stderr)
    );
    // git verify-tag emits "Good signature" on stderr.
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&v.stdout),
        String::from_utf8_lossy(&v.stderr)
    );
    assert!(
        combined.contains("Good signature"),
        "verify-tag output should mention Good signature: {combined:?}"
    );
}

// =========================================================================
// verify-tag
// =========================================================================

/// `rustygit verify-tag` accepts a git-signed tag (reverse oracle).
#[test]
fn rustygit_verify_tag_accepts_git_signed_tag() {
    if !has_system_git() {
        return;
    }
    let Some((gpg_home, fpr)) = setup_test_gpg() else {
        eprintln!("skip: no gpg / key gen failed");
        return;
    };
    let tmp = init_repo();
    commit_file(tmp.path(), "f", b"x\n", "first");
    git(&["config", "user.signingkey", &fpr], tmp.path());

    let s = Command::new("git")
        .args([
            "-C",
            tmp.path().to_str().unwrap(),
            "-c",
            &format!("user.signingkey={fpr}"),
            "tag",
            "-s",
            "-m",
            "git's signed",
            "v1",
        ])
        .env("GNUPGHOME", gpg_home.path())
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .env("GIT_AUTHOR_DATE", "1700000000 +0000")
        .env("GIT_COMMITTER_DATE", "1700000000 +0000")
        .output()
        .unwrap();
    assert!(
        s.status.success(),
        "git tag -s should succeed: stderr={}",
        String::from_utf8_lossy(&s.stderr)
    );

    let v = rustygit_in(
        &["verify-tag", "v1"],
        tmp.path(),
        Some(gpg_home.path()),
        None,
    );
    assert_eq!(
        v.status.code(),
        Some(0),
        "verify-tag should exit 0 for a good signature\nstderr: {}",
        String::from_utf8_lossy(&v.stderr)
    );
    let err = String::from_utf8_lossy(&v.stderr);
    assert!(
        err.contains("GOODSIG"),
        "verify-tag should print GOODSIG: {err:?}"
    );
}

/// `verify-tag` on an unsigned tag exits 128 with a "no signature" message.
#[test]
fn verify_tag_unsigned_exits_128() {
    if !has_system_git() {
        return;
    }
    let tmp = init_repo();
    commit_file(tmp.path(), "f", b"x\n", "first");
    // Annotated but unsigned.
    assert_ok(
        &rustygit(&["tag", "-a", "-m", "plain", "v1"], tmp.path()),
        "tag -a",
    );

    let r = rustygit(&["verify-tag", "v1"], tmp.path());
    assert_eq!(r.status.code(), Some(128));
    let err = String::from_utf8_lossy(&r.stderr);
    assert!(err.contains("no signature"), "stderr: {err:?}");
}

/// `verify-tag` on a non-tag oid (e.g. a commit) exits 128.
#[test]
fn verify_tag_non_tag_target_exits_128() {
    if !has_system_git() {
        return;
    }
    let tmp = init_repo();
    commit_file(tmp.path(), "f", b"x\n", "first");

    let r = rustygit(&["verify-tag", "HEAD"], tmp.path());
    assert_eq!(r.status.code(), Some(128));
    let err = String::from_utf8_lossy(&r.stderr);
    assert!(err.contains("not a tag"), "stderr: {err:?}");
}
