//! NON_GOALS.md Batch F — GPG commit signing & verification.
//!
//! Two halves:
//!
//! 1. **Mocked tests** that exercise the wire-up without needing a real gpg
//!    installation. Use `MockSigner` to assert the commit body got signed
//!    BEFORE the gpgsig header was added (i.e. round-trip parsing of the
//!    signed commit yields the same payload the mock received).
//!
//! 2. **gpg-gated tests** that spawn the real binary against a disposable
//!    `GNUPGHOME` with a generated test key. Skipped cleanly when `gpg`
//!    isn't on PATH or key generation fails.

mod common;

use std::path::Path;
use std::process::Command;

use assert_cmd::Command as AssertCmd;
use common::{git, has_system_git};
use tempfile::TempDir;

fn rustygit(args: &[&str], cwd: &Path, gnupghome: Option<&Path>) -> std::process::Output {
    let mut cmd = AssertCmd::cargo_bin("rustygit").unwrap();
    cmd.args(args)
        .current_dir(cwd)
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com");
    if let Some(home) = gnupghome {
        cmd.env("GNUPGHOME", home);
    }
    cmd.output().unwrap()
}

fn has_gpg() -> bool {
    Command::new("gpg")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Generate a passphraseless RSA key in a fresh GNUPGHOME directory.
/// Returns the path and the key id (long form).
fn setup_test_gpg() -> Option<(TempDir, String)> {
    if !has_gpg() {
        return None;
    }
    let home = TempDir::new().ok()?;
    // gpg complains if perms aren't restrictive on macOS.
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
Name-Real: RustygitTest
Name-Email: rustygit-test@example.com
Expire-Date: 0
%commit
";
    let r = Command::new("gpg")
        .env("GNUPGHOME", home.path())
        .args(["--batch", "--quick-gen-key", "--passphrase", ""])
        .arg("--yes")
        .arg("--generate-key")
        .arg("-")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn();
    let mut child = r.ok()?;
    {
        use std::io::Write;
        child.stdin.as_mut()?.write_all(batch.as_bytes()).ok()?;
    }
    let out = child.wait_with_output().ok()?;
    if !out.status.success() {
        eprintln!(
            "skipping: gpg key generation failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        return None;
    }

    // Find the key id we just made.
    let list = Command::new("gpg")
        .env("GNUPGHOME", home.path())
        .args(["--list-secret-keys", "--with-colons"])
        .output()
        .ok()?;
    if !list.status.success() {
        return None;
    }
    let listing = String::from_utf8_lossy(&list.stdout);
    let mut key_id: Option<String> = None;
    for line in listing.lines() {
        // `sec:...` then next `fpr:...` gives the fingerprint.
        if line.starts_with("fpr:") {
            let fields: Vec<&str> = line.split(':').collect();
            if let Some(fpr) = fields.get(9) {
                key_id = Some((*fpr).to_string());
                break;
            }
        }
    }
    let key = key_id?;
    Some((home, key))
}

// ----- mock-based wire-up tests (no real gpg needed) -----

#[test]
fn create_commit_with_mock_signer_folds_signature_into_gpgsig() {
    use rustygit::cli::commit_tree::create_commit_with_signer;
    use rustygit::commit::Commit;
    use rustygit::object::ObjectKind;
    use rustygit::signing::testing::MockSigner;

    // Fresh repo with a tree object to commit against.
    let tmp = TempDir::new().unwrap();
    let gitdir = tmp.path().join(".git");
    for d in [
        "",
        "objects",
        "objects/info",
        "objects/pack",
        "refs/heads",
        "refs/tags",
    ] {
        std::fs::create_dir_all(gitdir.join(d)).unwrap();
    }
    std::fs::write(gitdir.join("HEAD"), b"ref: refs/heads/main\n").unwrap();
    std::fs::write(
        gitdir.join("config"),
        b"[core]\n\trepositoryformatversion = 0\n[user]\n\tname = T\n\temail = t@e\n",
    )
    .unwrap();

    // Stage and write-tree to produce an actual tree oid.
    std::fs::write(tmp.path().join("a.txt"), b"hi\n").unwrap();
    let add = AssertCmd::cargo_bin("rustygit")
        .unwrap()
        .args(["add", "a.txt"])
        .current_dir(tmp.path())
        .output()
        .unwrap();
    assert!(
        add.status.success(),
        "add failed: {}",
        String::from_utf8_lossy(&add.stderr)
    );
    let wt = AssertCmd::cargo_bin("rustygit")
        .unwrap()
        .args(["write-tree"])
        .current_dir(tmp.path())
        .output()
        .unwrap();
    assert!(wt.status.success());
    let tree_oid = String::from_utf8_lossy(&wt.stdout).trim().to_string();

    let repo = rustygit::repo::Repository::discover(tmp.path()).unwrap();
    let mock = MockSigner::good();

    // Set author/committer env via the std env so the helpers pick them up.
    std::env::set_var("GIT_AUTHOR_NAME", "T");
    std::env::set_var("GIT_AUTHOR_EMAIL", "t@e");
    std::env::set_var("GIT_AUTHOR_DATE", "1700000000 +0000");
    std::env::set_var("GIT_COMMITTER_NAME", "T");
    std::env::set_var("GIT_COMMITTER_EMAIL", "t@e");
    std::env::set_var("GIT_COMMITTER_DATE", "1700000000 +0000");

    let oid = create_commit_with_signer(&repo, &tree_oid, &[], "signed test commit\n", Some(&mock))
        .expect("signed commit");

    // The mock should have been called exactly once and received the
    // UNSIGNED body (no gpgsig header).
    let payloads = mock.signed_payloads.lock().unwrap();
    assert_eq!(payloads.len(), 1, "signer called once");
    let payload = &payloads[0];
    assert!(
        !payload.windows(7).any(|w| w == b"gpgsig "),
        "the payload handed to the signer must NOT include a gpgsig header: {}",
        String::from_utf8_lossy(payload)
    );

    // The on-disk commit MUST include the signature folded into gpgsig.
    let obj = repo.odb().read(&oid).unwrap();
    assert_eq!(obj.kind, ObjectKind::Commit);
    let commit = Commit::parse(&obj.data, repo.hash_kind()).unwrap();
    let stored_sig = commit
        .gpgsig
        .clone()
        .expect("gpgsig set on the stored commit");
    assert!(
        stored_sig.starts_with(b"-----BEGIN PGP SIGNATURE-----"),
        "stored gpgsig should be ASCII-armored, got {}",
        String::from_utf8_lossy(&stored_sig)
    );

    // And the unsigned body (commit with gpgsig stripped) must match exactly
    // what we handed to the signer. This is the verify-commit invariant.
    let mut unsigned = commit.clone();
    unsigned.gpgsig = None;
    let rebuilt_payload = unsigned.serialize();
    assert_eq!(
        &rebuilt_payload, payload,
        "stripping gpgsig must recover the exact bytes the signer signed"
    );
}

// ----- real-gpg end-to-end -----

#[test]
fn rustygit_signed_commit_verifies_with_git() {
    if !has_system_git() {
        return;
    }
    let Some((home, key_id)) = setup_test_gpg() else {
        return;
    };

    let tmp = TempDir::new().unwrap();
    assert!(
        rustygit(&["init", "-q", "."], tmp.path(), Some(home.path()))
            .status
            .success()
    );
    // Configure git/rustygit to use our key + program. Set in repo config so
    // both rustygit and `git verify-commit` find it.
    std::fs::write(
        tmp.path().join(".git").join("config"),
        format!(
            "[core]\n\trepositoryformatversion = 0\n\tbare = false\n\
             [user]\n\tname = RustygitTest\n\temail = rustygit-test@example.com\n\
             \tsigningkey = {key_id}\n\
             [gpg]\n\tprogram = gpg\n"
        )
        .as_bytes(),
    )
    .unwrap();

    std::fs::write(tmp.path().join("a.txt"), b"signed!\n").unwrap();
    let add = rustygit(&["add", "a.txt"], tmp.path(), Some(home.path()));
    assert!(
        add.status.success(),
        "add: {}",
        String::from_utf8_lossy(&add.stderr)
    );

    // Commit signed.
    let cm = rustygit(
        &["commit", "-S", "-m", "first signed commit"],
        tmp.path(),
        Some(home.path()),
    );
    assert!(
        cm.status.success(),
        "signed commit failed: {}",
        String::from_utf8_lossy(&cm.stderr)
    );

    // System git must see a good signature.
    let v = Command::new("git")
        .args(["verify-commit", "HEAD"])
        .current_dir(tmp.path())
        .env("GNUPGHOME", home.path())
        .output()
        .unwrap();
    assert!(
        v.status.success(),
        "git verify-commit failed: stdout={} stderr={}",
        String::from_utf8_lossy(&v.stdout),
        String::from_utf8_lossy(&v.stderr)
    );
    let stderr = String::from_utf8_lossy(&v.stderr);
    assert!(
        stderr.contains("Good signature") || stderr.contains("rustygit-test"),
        "expected 'Good signature' in stderr, got: {stderr}"
    );

    // And our own verify-commit should agree.
    let r = rustygit(&["verify-commit", "HEAD"], tmp.path(), Some(home.path()));
    assert!(
        r.status.success(),
        "rustygit verify-commit failed: stdout={} stderr={}",
        String::from_utf8_lossy(&r.stdout),
        String::from_utf8_lossy(&r.stderr)
    );
    let r_stderr = String::from_utf8_lossy(&r.stderr);
    assert!(
        r_stderr.contains("GOODSIG"),
        "expected GOODSIG, got: {r_stderr}"
    );
}

#[test]
fn rustygit_verifies_git_signed_commit() {
    if !has_system_git() {
        return;
    }
    let Some((home, key_id)) = setup_test_gpg() else {
        return;
    };

    let tmp = TempDir::new().unwrap();
    assert!(Command::new("git")
        .args(["init", "-q", "."])
        .current_dir(tmp.path())
        .env("GNUPGHOME", home.path())
        .status()
        .unwrap()
        .success());
    std::fs::write(
        tmp.path().join(".git").join("config"),
        format!(
            "[core]\n\trepositoryformatversion = 0\n\
             [user]\n\tname = RustygitTest\n\temail = rustygit-test@example.com\n\
             \tsigningkey = {key_id}\n\
             [gpg]\n\tprogram = gpg\n"
        )
        .as_bytes(),
    )
    .unwrap();

    // git commit -S.
    std::fs::write(tmp.path().join("a.txt"), b"hello\n").unwrap();
    assert!(Command::new("git")
        .args(["add", "a.txt"])
        .current_dir(tmp.path())
        .env("GNUPGHOME", home.path())
        .status()
        .unwrap()
        .success());
    let r = Command::new("git")
        .args(["commit", "-S", "-m", "git-signed"])
        .current_dir(tmp.path())
        .env("GNUPGHOME", home.path())
        .output()
        .unwrap();
    assert!(
        r.status.success(),
        "git -S commit failed: {}",
        String::from_utf8_lossy(&r.stderr)
    );

    // rustygit verify-commit should accept it.
    let v = rustygit(&["verify-commit", "HEAD"], tmp.path(), Some(home.path()));
    assert!(
        v.status.success(),
        "rustygit verify-commit on git-signed HEAD failed: stdout={} stderr={}",
        String::from_utf8_lossy(&v.stdout),
        String::from_utf8_lossy(&v.stderr)
    );
    assert!(
        String::from_utf8_lossy(&v.stderr).contains("GOODSIG"),
        "expected GOODSIG"
    );
}

#[test]
fn verify_commit_on_unsigned_commit_fails_with_128() {
    if !has_system_git() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    assert!(git(&["init", "-q", "."], tmp.path()).status.success());
    let _ = git(&["config", "user.name", "T"], tmp.path());
    let _ = git(&["config", "user.email", "t@e"], tmp.path());
    std::fs::write(tmp.path().join("a"), b"a").unwrap();
    assert!(git(&["add", "."], tmp.path()).status.success());
    assert!(git(&["commit", "-q", "-m", "no sig"], tmp.path())
        .status
        .success());

    let v = rustygit(&["verify-commit", "HEAD"], tmp.path(), None);
    assert_eq!(v.status.code().unwrap_or(-1), 128);
    assert!(
        String::from_utf8_lossy(&v.stderr).contains("no signature"),
        "stderr: {}",
        String::from_utf8_lossy(&v.stderr)
    );
}

#[test]
fn no_gpg_sign_overrides_config_default() {
    if !has_system_git() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    assert!(rustygit(&["init", "-q", "."], tmp.path(), None)
        .status
        .success());
    // commit.gpgsign=true would normally force signing.
    std::fs::write(
        tmp.path().join(".git").join("config"),
        b"[core]\n\trepositoryformatversion = 0\n\
          [user]\n\tname = T\n\temail = t@e\n\
          [commit]\n\tgpgsign = true\n",
    )
    .unwrap();
    std::fs::write(tmp.path().join("a"), b"a").unwrap();
    assert!(rustygit(&["add", "a"], tmp.path(), None).status.success());

    // --no-gpg-sign must win.
    let cm = rustygit(
        &["commit", "--no-gpg-sign", "-m", "unsigned"],
        tmp.path(),
        None,
    );
    assert!(
        cm.status.success(),
        "commit --no-gpg-sign failed: {}",
        String::from_utf8_lossy(&cm.stderr)
    );

    let v = rustygit(&["verify-commit", "HEAD"], tmp.path(), None);
    assert_eq!(
        v.status.code().unwrap_or(-1),
        128,
        "expected 'no signature' / 128 from verify-commit"
    );
}
