//! NON_GOALS A1 / Launch-readiness A1 — `[alias]` config expansion
//! end-to-end against the rustygit binary.
//!
//! The unit tests in `src/cli/alias.rs::tests` exercise the parser and
//! expansion in isolation. These tests run the actual binary with a
//! hermetic `$HOME/.gitconfig`, the way a real user would experience it.

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

fn write_global_aliases(home: &Path, aliases: &[(&str, &str)]) {
    let mut buf = String::from("[user]\n\tname = T\n\temail = t@e\n[alias]\n");
    for (k, v) in aliases {
        buf.push('\t');
        buf.push_str(k);
        buf.push_str(" = ");
        buf.push_str(v);
        buf.push('\n');
    }
    std::fs::write(home.join(".gitconfig"), buf).unwrap();
}

/// `alias.st = status` → `rustygit st` runs the status command.
#[test]
fn simple_alias_expansion_st_runs_status() {
    let home = TempDir::new().unwrap();
    let repo = TempDir::new().unwrap();
    write_global_aliases(home.path(), &[("st", "status")]);

    assert!(rustygit(&["init", "-q", "."], repo.path(), home.path())
        .status
        .success());
    std::fs::write(repo.path().join("a.txt"), b"x").unwrap();

    let r_alias = rustygit(&["st"], repo.path(), home.path());
    let r_direct = rustygit(&["status"], repo.path(), home.path());

    assert!(
        r_alias.status.success(),
        "rustygit st should succeed: stderr={}",
        String::from_utf8_lossy(&r_alias.stderr)
    );
    assert!(r_direct.status.success());
    // Output must be byte-equal — the alias expansion is invisible.
    assert_eq!(
        r_alias.stdout, r_direct.stdout,
        "alias output != direct output"
    );
}

/// `alias.last = log -n 1 HEAD` runs with the alias body's args inline.
///
/// (Tests the alias EXPANSION, not log's own argument parsing — using
/// `-n 1` rather than the `-1` shorthand because rustygit's log doesn't
/// yet accept the bare-digit shorthand. That's a separate launch-readiness
/// gap; this test isolates the alias-machinery contract.)
#[test]
fn alias_with_args_log_minus_n_one_head() {
    let home = TempDir::new().unwrap();
    let repo = TempDir::new().unwrap();
    write_global_aliases(home.path(), &[("last", "log -n 1 HEAD")]);

    rustygit(&["init", "-q", "."], repo.path(), home.path());
    std::fs::write(repo.path().join("a.txt"), b"v1").unwrap();
    rustygit(&["add", "."], repo.path(), home.path());
    rustygit(&["commit", "-m", "first"], repo.path(), home.path());
    std::fs::write(repo.path().join("b.txt"), b"v1").unwrap();
    rustygit(&["add", "."], repo.path(), home.path());
    rustygit(&["commit", "-m", "second"], repo.path(), home.path());

    let r_alias = rustygit(&["last"], repo.path(), home.path());
    let r_direct = rustygit(&["log", "-n", "1", "HEAD"], repo.path(), home.path());
    assert!(
        r_alias.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&r_alias.stderr)
    );
    assert_eq!(r_alias.stdout, r_direct.stdout, "alias output != direct");
    // The "second" commit must be the one shown (since it's HEAD).
    assert!(String::from_utf8_lossy(&r_alias.stdout).contains("second"));
    assert!(!String::from_utf8_lossy(&r_alias.stdout).contains("first"));
}

/// User-supplied args are appended after the alias body.
#[test]
fn user_args_appended_after_alias_body() {
    let home = TempDir::new().unwrap();
    let repo = TempDir::new().unwrap();
    write_global_aliases(home.path(), &[("lg", "log --oneline")]);

    rustygit(&["init", "-q", "."], repo.path(), home.path());
    std::fs::write(repo.path().join("a.txt"), b"v1").unwrap();
    rustygit(&["add", "."], repo.path(), home.path());
    rustygit(&["commit", "-m", "c1"], repo.path(), home.path());

    // `lg -n 1` should expand to `log --oneline -n 1`.
    let r_alias = rustygit(&["lg", "-n", "1"], repo.path(), home.path());
    let r_direct = rustygit(&["log", "--oneline", "-n", "1"], repo.path(), home.path());
    assert!(
        r_alias.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&r_alias.stderr)
    );
    assert_eq!(r_alias.stdout, r_direct.stdout);
}

/// Recursive aliases (`foo` → `bar` → `status`) resolve all the way.
#[test]
fn recursive_alias_resolves_through_chain() {
    let home = TempDir::new().unwrap();
    let repo = TempDir::new().unwrap();
    write_global_aliases(home.path(), &[("foo", "bar"), ("bar", "status")]);
    rustygit(&["init", "-q", "."], repo.path(), home.path());

    let r = rustygit(&["foo"], repo.path(), home.path());
    assert!(
        r.status.success(),
        "recursive alias failed: stderr={}",
        String::from_utf8_lossy(&r.stderr)
    );
    // status's "On branch" header should appear.
    assert!(String::from_utf8_lossy(&r.stdout).contains("On branch"));
}

/// `!` (shell) aliases are refused with a clear error.
#[test]
fn shell_alias_rejected_with_clear_error() {
    let home = TempDir::new().unwrap();
    let repo = TempDir::new().unwrap();
    write_global_aliases(home.path(), &[("sync", "!git fetch && git rebase")]);
    rustygit(&["init", "-q", "."], repo.path(), home.path());

    let r = rustygit(&["sync"], repo.path(), home.path());
    assert!(!r.status.success(), "shell alias should be rejected");
    let stderr = String::from_utf8_lossy(&r.stderr);
    assert!(
        stderr.contains("shell") || stderr.contains("'!'") || stderr.contains("not supported"),
        "expected shell-alias rejection message: {stderr}"
    );
}

/// Built-in subcommand names always win over aliases of the same name.
/// `alias.status = log` MUST NOT shadow the real `status` subcommand.
#[test]
fn builtin_subcommand_overrides_like_named_alias() {
    let home = TempDir::new().unwrap();
    let repo = TempDir::new().unwrap();
    write_global_aliases(home.path(), &[("status", "log --oneline")]);

    rustygit(&["init", "-q", "."], repo.path(), home.path());
    std::fs::write(repo.path().join("a.txt"), b"x").unwrap();

    let r = rustygit(&["status"], repo.path(), home.path());
    assert!(
        r.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&r.stderr)
    );
    // The real status command's "On branch" header should appear.
    let out = String::from_utf8_lossy(&r.stdout);
    assert!(
        out.contains("On branch") || out.contains("Untracked"),
        "expected real status output (got log output?): {out}"
    );
}

/// Aliases defined in `<gitdir>/.git/config` (LOCAL layer) work — the
/// alias loader reads the full layered config.
#[test]
fn alias_defined_in_local_config_resolves() {
    let home = TempDir::new().unwrap();
    let repo = TempDir::new().unwrap();
    // No global aliases — populate the local config directly.
    std::fs::write(
        home.path().join(".gitconfig"),
        b"[user]\n\tname = T\n\temail = t@e\n",
    )
    .unwrap();
    rustygit(&["init", "-q", "."], repo.path(), home.path());
    // Append `[alias] st = status` to the LOCAL config.
    let mut cfg = std::fs::read(repo.path().join(".git/config")).unwrap();
    cfg.extend_from_slice(b"\n[alias]\n\tst = status\n");
    std::fs::write(repo.path().join(".git/config"), cfg).unwrap();

    let r = rustygit(&["st"], repo.path(), home.path());
    assert!(
        r.status.success(),
        "local alias should resolve: stderr={}",
        String::from_utf8_lossy(&r.stderr)
    );
    assert!(String::from_utf8_lossy(&r.stdout).contains("On branch"));
}

/// Alias loops are detected and produce a clear error.
#[test]
fn alias_loop_detected() {
    let home = TempDir::new().unwrap();
    let repo = TempDir::new().unwrap();
    write_global_aliases(home.path(), &[("a", "b"), ("b", "a")]);
    rustygit(&["init", "-q", "."], repo.path(), home.path());

    let r = rustygit(&["a"], repo.path(), home.path());
    assert!(!r.status.success());
    let stderr = String::from_utf8_lossy(&r.stderr);
    assert!(
        stderr.to_lowercase().contains("loop") || stderr.contains("limit"),
        "expected loop-detected error: {stderr}"
    );
}
