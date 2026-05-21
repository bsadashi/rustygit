//! NON_GOALS.md A2 — layered config loader.
//!
//! `Config::from_repo_dir` reads /etc/gitconfig + XDG + ~/.gitconfig +
//! <gitdir>/config + CLI -c overrides, with later layers winning. These
//! tests verify the precedence order and the hermetic behavior we depend
//! on for production use.
//!
//! Every test sets `HOME` and `GIT_CONFIG_NOSYSTEM=1` to a temp directory
//! so we don't accidentally read the developer's real ~/.gitconfig.

use std::path::Path;

use assert_cmd::Command as AssertCmd;
use tempfile::TempDir;

fn rustygit_with_env(
    args: &[&str],
    cwd: &Path,
    home: &Path,
    extra: &[(&str, &str)],
) -> std::process::Output {
    let mut cmd = AssertCmd::cargo_bin("rustygit").unwrap();
    cmd.args(args)
        .current_dir(cwd)
        .env("HOME", home)
        // We never want to read the developer's actual system config.
        .env("GIT_CONFIG_NOSYSTEM", "1")
        // Clear inherited git env vars that might come from the test
        // harness — the test wants to verify the loader's discovery,
        // not the harness's identity.
        .env_remove("GIT_AUTHOR_NAME")
        .env_remove("GIT_AUTHOR_EMAIL")
        .env_remove("GIT_COMMITTER_NAME")
        .env_remove("GIT_COMMITTER_EMAIL")
        .env_remove("GIT_CONFIG_GLOBAL")
        .env_remove("XDG_CONFIG_HOME");
    for (k, v) in extra {
        cmd.env(*k, *v);
    }
    cmd.output().unwrap()
}

/// `~/.gitconfig` with `user.name` and `user.email` populated → `rustygit
/// commit` in a fresh tempdir repo succeeds without per-repo identity
/// config. This is the headline UX win: a switching user's identity
/// "just works" on first commit.
#[test]
fn home_gitconfig_provides_identity_on_first_commit() {
    let home = TempDir::new().unwrap();
    let repo = TempDir::new().unwrap();

    // Synthetic ~/.gitconfig.
    std::fs::write(
        home.path().join(".gitconfig"),
        b"[user]\n\tname = From Home\n\temail = home@example.com\n",
    )
    .unwrap();

    // Initialize a fresh repo (no local user.name/email).
    let init = rustygit_with_env(&["init", "-q", "."], repo.path(), home.path(), &[]);
    assert!(
        init.status.success(),
        "init: {}",
        String::from_utf8_lossy(&init.stderr)
    );

    std::fs::write(repo.path().join("a.txt"), b"hi\n").unwrap();
    let add = rustygit_with_env(&["add", "a.txt"], repo.path(), home.path(), &[]);
    assert!(
        add.status.success(),
        "add: {}",
        String::from_utf8_lossy(&add.stderr)
    );

    let cm = rustygit_with_env(&["commit", "-m", "first"], repo.path(), home.path(), &[]);
    assert!(
        cm.status.success(),
        "commit should succeed using ~/.gitconfig identity: stderr={}",
        String::from_utf8_lossy(&cm.stderr)
    );

    // Verify the commit author came from the global config.
    let cf = rustygit_with_env(&["cat-file", "-p", "HEAD"], repo.path(), home.path(), &[]);
    assert!(cf.status.success());
    let body = String::from_utf8_lossy(&cf.stdout);
    assert!(
        body.contains("From Home"),
        "expected 'From Home' in commit body, got: {body}"
    );
    assert!(
        body.contains("home@example.com"),
        "expected home@example.com in commit body, got: {body}"
    );
}

/// `<gitdir>/config` wins over `~/.gitconfig` — last layer applied wins.
#[test]
fn local_config_overrides_global() {
    let home = TempDir::new().unwrap();
    let repo = TempDir::new().unwrap();

    std::fs::write(
        home.path().join(".gitconfig"),
        b"[user]\n\tname = Global\n\temail = global@example.com\n",
    )
    .unwrap();
    rustygit_with_env(&["init", "-q", "."], repo.path(), home.path(), &[]);
    // Append a LOCAL [user] section that overrides.
    let mut local_cfg = std::fs::read(repo.path().join(".git/config")).unwrap();
    local_cfg.extend_from_slice(b"[user]\n\tname = Local\n\temail = local@example.com\n");
    std::fs::write(repo.path().join(".git/config"), &local_cfg).unwrap();

    std::fs::write(repo.path().join("a.txt"), b"hi\n").unwrap();
    rustygit_with_env(&["add", "a.txt"], repo.path(), home.path(), &[]);
    let cm = rustygit_with_env(&["commit", "-m", "first"], repo.path(), home.path(), &[]);
    assert!(
        cm.status.success(),
        "commit: {}",
        String::from_utf8_lossy(&cm.stderr)
    );

    let cf = rustygit_with_env(&["cat-file", "-p", "HEAD"], repo.path(), home.path(), &[]);
    let body = String::from_utf8_lossy(&cf.stdout);
    assert!(body.contains("Local"), "expected 'Local' override: {body}");
    assert!(
        !body.contains("Global"),
        "global identity must not appear: {body}"
    );
}

/// `$GIT_CONFIG_GLOBAL=/path/to/file` overrides the default `$HOME/.gitconfig`.
#[test]
fn git_config_global_env_overrides_home_gitconfig() {
    let home = TempDir::new().unwrap();
    let alt = TempDir::new().unwrap();
    let repo = TempDir::new().unwrap();

    // Default ~/.gitconfig that would normally apply.
    std::fs::write(
        home.path().join(".gitconfig"),
        b"[user]\n\tname = Default\n\temail = default@e\n",
    )
    .unwrap();
    // Alternate file, pointed at by $GIT_CONFIG_GLOBAL.
    let alt_path = alt.path().join("alt-gitconfig");
    std::fs::write(&alt_path, b"[user]\n\tname = Alt\n\temail = alt@e\n").unwrap();

    rustygit_with_env(&["init", "-q", "."], repo.path(), home.path(), &[]);
    std::fs::write(repo.path().join("a.txt"), b"hi\n").unwrap();
    rustygit_with_env(
        &["add", "a.txt"],
        repo.path(),
        home.path(),
        &[("GIT_CONFIG_GLOBAL", alt_path.to_str().unwrap())],
    );
    let cm = rustygit_with_env(
        &["commit", "-m", "first"],
        repo.path(),
        home.path(),
        &[("GIT_CONFIG_GLOBAL", alt_path.to_str().unwrap())],
    );
    assert!(cm.status.success());

    let cf = rustygit_with_env(
        &["cat-file", "-p", "HEAD"],
        repo.path(),
        home.path(),
        &[("GIT_CONFIG_GLOBAL", alt_path.to_str().unwrap())],
    );
    let body = String::from_utf8_lossy(&cf.stdout);
    assert!(
        body.contains("Alt"),
        "expected 'Alt' from env-override: {body}"
    );
    assert!(
        !body.contains("Default"),
        "default config must not apply: {body}"
    );
}

/// `$XDG_CONFIG_HOME/git/config` IS consulted when set.
#[test]
fn xdg_config_home_is_consulted() {
    let home = TempDir::new().unwrap();
    let xdg = TempDir::new().unwrap();
    let repo = TempDir::new().unwrap();

    let xdg_git_dir = xdg.path().join("git");
    std::fs::create_dir_all(&xdg_git_dir).unwrap();
    std::fs::write(
        xdg_git_dir.join("config"),
        b"[user]\n\tname = Via XDG\n\temail = xdg@e\n",
    )
    .unwrap();

    rustygit_with_env(&["init", "-q", "."], repo.path(), home.path(), &[]);
    std::fs::write(repo.path().join("a.txt"), b"hi\n").unwrap();
    let xdg_env = ("XDG_CONFIG_HOME", xdg.path().to_str().unwrap());
    rustygit_with_env(&["add", "a.txt"], repo.path(), home.path(), &[xdg_env]);
    let cm = rustygit_with_env(
        &["commit", "-m", "first"],
        repo.path(),
        home.path(),
        &[xdg_env],
    );
    assert!(
        cm.status.success(),
        "commit should succeed using XDG config: {}",
        String::from_utf8_lossy(&cm.stderr)
    );
    let cf = rustygit_with_env(
        &["cat-file", "-p", "HEAD"],
        repo.path(),
        home.path(),
        &[xdg_env],
    );
    assert!(String::from_utf8_lossy(&cf.stdout).contains("Via XDG"));
}

/// `[includeIf]` directives are silently skipped with a one-time stderr
/// warning, rather than the old hard-error behavior. This is the
/// compatibility fix for users with conditional configs in their
/// `~/.gitconfig` — the very common pattern of "different identity by
/// directory".
#[test]
fn includeif_in_global_config_is_silently_skipped() {
    let home = TempDir::new().unwrap();
    let repo = TempDir::new().unwrap();

    std::fs::write(
        home.path().join(".gitconfig"),
        b"[user]\n\tname = Test\n\temail = test@e\n\
          [includeIf \"gitdir:~/work/\"]\n\
          \tpath = ~/.gitconfig.work\n",
    )
    .unwrap();

    rustygit_with_env(&["init", "-q", "."], repo.path(), home.path(), &[]);
    std::fs::write(repo.path().join("a.txt"), b"hi\n").unwrap();
    rustygit_with_env(&["add", "a.txt"], repo.path(), home.path(), &[]);
    let cm = rustygit_with_env(&["commit", "-m", "first"], repo.path(), home.path(), &[]);

    // The commit should succeed (the includeIf is silently dropped, not a
    // hard error). And the warning should appear on stderr.
    assert!(
        cm.status.success(),
        "commit should succeed despite includeIf in global config: stderr={}",
        String::from_utf8_lossy(&cm.stderr)
    );
    let stderr = String::from_utf8_lossy(&cm.stderr);
    assert!(
        stderr.contains("[include]")
            || stderr.contains("includeIf")
            || stderr.contains("[includeIf]")
            || stderr.contains("include"),
        "expected one-time warning about ignored include directives: {stderr}"
    );
}

/// Missing `~/.gitconfig` is a no-op, not an error. Verifies the
/// "no global identity" baseline still produces a useful error from
/// `commit` (not a parse error from the loader).
#[test]
fn missing_global_config_is_a_noop() {
    let home = TempDir::new().unwrap();
    let repo = TempDir::new().unwrap();

    rustygit_with_env(&["init", "-q", "."], repo.path(), home.path(), &[]);
    std::fs::write(repo.path().join("a.txt"), b"hi\n").unwrap();
    rustygit_with_env(&["add", "a.txt"], repo.path(), home.path(), &[]);
    // No identity anywhere; commit should fail BUT with a clear
    // identity-missing error, not a config-parse error.
    let cm = rustygit_with_env(&["commit", "-m", "first"], repo.path(), home.path(), &[]);
    assert!(
        !cm.status.success(),
        "expected commit to fail with no identity"
    );
    let stderr = String::from_utf8_lossy(&cm.stderr);
    // The error should mention `user.name` / `user.email` so the user
    // knows what to set.
    assert!(
        stderr.to_lowercase().contains("user")
            && (stderr.contains("name") || stderr.contains("email")),
        "expected identity-missing error, got: {stderr}"
    );
}
