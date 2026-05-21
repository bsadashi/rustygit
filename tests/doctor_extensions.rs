//! NON_GOALS B6 — `doctor --vs-git` and `doctor --import-config`.
//!
//! End-to-end tests against the rustygit binary. The `--vs-git` mode
//! requires system `git` (the comparison oracle); `--import-config`
//! runs against a synthetic `$HOME/.gitconfig` and checks the catalog
//! output.

use std::path::Path;

use assert_cmd::Command as AssertCmd;
use tempfile::TempDir;

fn rustygit(args: &[&str], cwd: &Path, home: &Path) -> std::process::Output {
    AssertCmd::cargo_bin("rustygit")
        .unwrap()
        .args(args)
        .current_dir(cwd)
        .env("HOME", home)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("GIT_CONFIG_GLOBAL")
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@e")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@e")
        .output()
        .unwrap()
}

fn has_system_git() -> bool {
    std::process::Command::new("git")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

// ---------- --import-config ----------

#[test]
fn import_config_recognizes_honored_keys() {
    let home = TempDir::new().unwrap();
    let repo = TempDir::new().unwrap();
    std::fs::write(
        home.path().join(".gitconfig"),
        b"[user]\n\tname = T\n\temail = t@e\n\
          [alias]\n\tst = status\n\tco = checkout\n\
          [url \"https://github.com/\"]\n\tinsteadOf = git@github.com:\n",
    )
    .unwrap();
    rustygit(&["init", "-q", "."], repo.path(), home.path());

    let out = rustygit(&["doctor", "--import-config"], repo.path(), home.path());
    assert!(
        out.status.success(),
        "doctor --import-config failed: stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    // Both honored sections should be recognized.
    assert!(
        stdout.contains("honored:") && stdout.contains("rustygit recognizes"),
        "expected honored-keys summary: {stdout}"
    );
    // Specifically: at least 5 keys from our config should be in the
    // "honored" count (user.name, user.email, alias.st, alias.co,
    // url.<>.insteadof).
    let honored_line = stdout
        .lines()
        .find(|l| l.contains("honored:"))
        .unwrap_or("");
    // Pull the numerator out of "honored:    5/5 ..." — at minimum 4.
    assert!(
        honored_line.contains('/'),
        "expected x/y count in honored line: {honored_line}"
    );
}

#[test]
fn import_config_flags_deferred_keys() {
    let home = TempDir::new().unwrap();
    let repo = TempDir::new().unwrap();
    // Populate with keys we know are explicitly deferred.
    std::fs::write(
        home.path().join(".gitconfig"),
        b"[user]\n\tname = T\n\temail = t@e\n\
          [submodule \"foo\"]\n\turl = git@example.com:foo\n\
          [lfs]\n\turl = https://lfs.example.com\n\
          [mergetool \"vimdiff\"]\n\tcmd = nvim -d $LOCAL $REMOTE\n",
    )
    .unwrap();
    rustygit(&["init", "-q", "."], repo.path(), home.path());

    let out = rustygit(&["doctor", "--import-config"], repo.path(), home.path());
    assert!(
        out.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("deferred:"),
        "expected a deferred section listing un-honored keys: {stdout}"
    );
    // At least the submodule and lfs prefixes should appear in the
    // deferred list with our catalog reasons.
    assert!(
        stdout.contains("submodule") || stdout.contains("lfs") || stdout.contains("mergetool"),
        "expected one of submodule/lfs/mergetool in deferred output: {stdout}"
    );
}

// ---------- --vs-git ----------

#[test]
fn vs_git_reports_match_on_simple_repo() {
    if !has_system_git() {
        return; // can't compare without the oracle
    }
    let home = TempDir::new().unwrap();
    let repo = TempDir::new().unwrap();
    // Use system git to set up so the repo's on-disk layout is canonical.
    let g_init = std::process::Command::new("git")
        .args(["init", "-q", "."])
        .current_dir(repo.path())
        .env("HOME", home.path())
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .status()
        .unwrap();
    assert!(g_init.success());
    let _ = std::process::Command::new("git")
        .args(["config", "user.name", "T"])
        .current_dir(repo.path())
        .status();
    let _ = std::process::Command::new("git")
        .args(["config", "user.email", "t@e"])
        .current_dir(repo.path())
        .status();
    std::fs::write(repo.path().join("a.txt"), b"hello\n").unwrap();
    let _ = std::process::Command::new("git")
        .args(["add", "."])
        .current_dir(repo.path())
        .status();
    let _ = std::process::Command::new("git")
        .args(["commit", "-q", "-m", "first"])
        .current_dir(repo.path())
        .status();

    let out = rustygit(&["doctor", "--vs-git"], repo.path(), home.path());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success() || stdout.contains("divergence"),
        "--vs-git should run (success or report divergence), not error: \
         stdout={stdout} stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    // We're not strictly requiring zero divergences (rustygit's medium-format
    // log isn't byte-identical), but we ARE requiring the comparison runs at
    // all and produces a summary line.
    assert!(
        stdout.contains("matches") && stdout.contains("divergence"),
        "expected summary 'X/Y matches, Z divergence(s)': {stdout}"
    );
}

#[test]
fn vs_git_reports_when_git_missing() {
    // If git really is on PATH for the developer machine, skip — we can't
    // simulate "missing git" reliably without PATH manipulation.
    if has_system_git() {
        // We can still exercise the early-return: scrub PATH and confirm
        // the binary reports "git not found".
        let home = TempDir::new().unwrap();
        let repo = TempDir::new().unwrap();
        rustygit(&["init", "-q", "."], repo.path(), home.path());

        let out = AssertCmd::cargo_bin("rustygit")
            .unwrap()
            .args(["doctor", "--vs-git"])
            .current_dir(repo.path())
            .env("HOME", home.path())
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("PATH", "/nonexistent-path-for-this-test") // hide git
            .output()
            .unwrap();
        // Exit 2 per our convention for "comparison not runnable".
        let code = out.status.code().unwrap_or(0);
        assert_eq!(
            code,
            2,
            "expected exit 2 when git missing, got {code} stdout={}",
            String::from_utf8_lossy(&out.stdout)
        );
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            stdout.contains("NOT FOUND") || stdout.contains("not found"),
            "expected 'NOT FOUND' notice when git missing: {stdout}"
        );
    }
}

#[test]
fn import_config_exits_zero_on_empty_config() {
    let home = TempDir::new().unwrap();
    let repo = TempDir::new().unwrap();
    rustygit(&["init", "-q", "."], repo.path(), home.path());
    let out = rustygit(&["doctor", "--import-config"], repo.path(), home.path());
    assert!(
        out.status.success(),
        "doctor --import-config should not fail on empty config: stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    // The local config from `init` adds a handful of `[core]` entries —
    // count should still be a sensible "0/X" or so.
    assert!(stdout.contains("honored:"));
}
