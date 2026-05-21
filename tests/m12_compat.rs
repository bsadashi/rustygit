//! M12 sanity tests: SSH URL routing + credential helpers.
//!
//! Live SSH clones aren't tested here — they require a working ssh agent +
//! deploy key for a known private/public repo, which CI can't reliably
//! provide. We test:
//!   - the URL-scheme router accepts SSH URLs as "network"
//!   - the credential-helper protocol works via a fake script

mod common;

use std::path::Path;

use assert_cmd::Command as AssertCmd;
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
        .timeout(std::time::Duration::from_secs(20))
        .output()
        .unwrap()
}

#[test]
fn ssh_url_routes_to_network_not_local() {
    // A non-existent SSH host should produce a network/SSH error, not a
    // "no such file" error from the local-clone path.
    let dst_root = TempDir::new().unwrap();
    let dst = dst_root.path().join("out");
    let cwd = std::env::current_dir().unwrap();
    let out = rustygit(
        &[
            "clone",
            "git@does.not.resolve.invalid.example:nope/nope.git",
            dst.to_str().unwrap(),
        ],
        &cwd,
    );
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    // The clue that we hit the SSH path is either a DNS failure surfacing as
    // an ssh-process error, or "ssh: " somewhere in the error string. Either
    // way: NOT "not a git repository" (which the local path would emit).
    assert!(
        stderr.contains("ssh")
            || stderr.contains("SSH")
            || stderr.contains("resolve")
            || stderr.contains("Host")
            || stderr.contains("Could not"),
        "expected an SSH-shaped error; got: {stderr}"
    );
    assert!(
        !stderr.contains("not a rustygit repository"),
        "should NOT have gone through local-clone; got: {stderr}"
    );
}

#[test]
fn scp_form_ssh_url_recognized_as_network() {
    // Same as above but using the scp-form (no ssh:// prefix).
    let dst_root = TempDir::new().unwrap();
    let dst = dst_root.path().join("out");
    let cwd = std::env::current_dir().unwrap();
    let out = rustygit(
        &[
            "clone",
            "git@does.not.resolve.invalid.example:user/repo.git",
            dst.to_str().unwrap(),
        ],
        &cwd,
    );
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains("not a rustygit repository"),
        "scp-form SSH URL must NOT route to local clone; got: {stderr}"
    );
}

#[test]
fn ls_remote_accepts_ssh_url() {
    // Same routing check via ls-remote — should hit the SSH transport not error
    // with "unsupported scheme".
    let cwd = std::env::current_dir().unwrap();
    let out = rustygit(
        &[
            "ls-remote",
            "git@does.not.resolve.invalid.example:user/repo.git",
        ],
        &cwd,
    );
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains("unsupported scheme"),
        "ls-remote should accept SSH URLs; got: {stderr}"
    );
}

#[test]
fn credential_helper_protocol_via_fake_script() {
    // Use the credential module directly to verify it can invoke a fake
    // helper. This exercises the same code path real auth would.
    use rustygit::config::Config;
    use rustygit::credential::{fill_credentials, CredentialRequest};

    let tmp = TempDir::new().unwrap();
    let script_path = tmp.path().join("git-credential-fake");

    // Write a fake helper that ignores stdin and prints fixed creds.
    std::fs::write(
        &script_path,
        "#!/bin/sh\n\
         # Consume stdin so the helper protocol round-trip doesn't deadlock.\n\
         cat > /dev/null\n\
         echo 'username=daisy'\n\
         echo 'password=hunter2'\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&script_path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script_path, perms).unwrap();
    }

    let helper_cmd = format!("!{}", script_path.display());
    let cfg_text = format!("[credential]\n\thelper = {helper_cmd}\n");
    let config = Config::parse_str(&cfg_text).unwrap();

    let req = CredentialRequest::from_url("https://github.com/foo/bar.git");
    let resp = fill_credentials(&req, &config).expect("helper should return creds");
    assert_eq!(resp.username, "daisy");
    assert_eq!(resp.password, "hunter2");
}
