//! Integration tests for `rustygit bug-report` (NON_GOALS A8).
//!
//! The bug-report subcommand bundles environment context for a user to
//! paste into a GitHub issue. These tests assert the binary-facing
//! contract — that the subcommand exits 0, produces non-trivial output,
//! and includes the structural sections downstream tooling/triagers
//! will scan for. Unit-level redaction tests live next to the
//! `redact_secrets` implementation in `src/cli/bug_report.rs`.

use assert_cmd::Command as AssertCmd;
use tempfile::TempDir;

/// `bug-report` from an empty cwd (no repo) must still exit 0 and emit
/// a useful bundle. The subcommand is a diagnostic — refusing to run
/// outside a repo would block the most common bug-report scenarios
/// (init crashes, clone crashes, "I can't even start").
#[test]
fn bug_report_exits_zero_and_emits_payload() {
    let cwd = TempDir::new().unwrap();
    let out = AssertCmd::cargo_bin("rustygit")
        .unwrap()
        .arg("bug-report")
        .current_dir(cwd.path())
        // Avoid leaking the developer's GH/PAT envs into the test
        // (the report would redact them, but the assertion below
        // checks payload size — predictable env makes the byte counts
        // sane).
        .env_remove("GH_TOKEN")
        .env_remove("GITHUB_TOKEN")
        .output()
        .unwrap();

    assert!(
        out.status.success(),
        "bug-report exited non-zero\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(
        stdout.len() > 100,
        "stdout suspiciously short ({} bytes):\n{stdout}",
        stdout.len()
    );
}

/// The version line is the single most important thing in a bug
/// report — without it triagers can't reproduce against the right
/// commit. Lock its presence in.
#[test]
fn bug_report_contains_version_line() {
    let cwd = TempDir::new().unwrap();
    let out = AssertCmd::cargo_bin("rustygit")
        .unwrap()
        .arg("bug-report")
        .current_dir(cwd.path())
        .output()
        .unwrap();
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(
        stdout.contains("rustygit version:"),
        "missing 'rustygit version:' line in:\n{stdout}"
    );
}

/// The platform line ties together OS + arch and is what gates which
/// of our compat caveats (A10 Windows guards, BSD path handling, etc.)
/// apply. Don't ship a bug-report without it.
#[test]
fn bug_report_contains_platform_line() {
    let cwd = TempDir::new().unwrap();
    let out = AssertCmd::cargo_bin("rustygit")
        .unwrap()
        .arg("bug-report")
        .current_dir(cwd.path())
        .output()
        .unwrap();
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(
        stdout.contains("Platform:") || stdout.contains("OS:"),
        "missing platform/OS line in:\n{stdout}"
    );
}

/// All four headed sections must appear so reporters and triagers can
/// rely on the document shape. Editor folding, grep-based extraction,
/// and downstream scripts (e.g. an automated issue-template
/// pre-fill) all key off these markers.
#[test]
fn bug_report_has_all_sections() {
    let cwd = TempDir::new().unwrap();
    let out = AssertCmd::cargo_bin("rustygit")
        .unwrap()
        .arg("bug-report")
        .current_dir(cwd.path())
        .output()
        .unwrap();
    let stdout = String::from_utf8(out.stdout).unwrap();
    for marker in &[
        "=== rustygit bug-report ===",
        "=== rustygit doctor ===",
        "=== environment ===",
        "=== recent subcommands ===",
    ] {
        assert!(
            stdout.contains(marker),
            "section '{marker}' missing from:\n{stdout}"
        );
    }
}

/// Token-bearing env vars must NOT have their value reach stdout, even
/// when set on the bug-report process directly. This is the test that
/// proves the env classifier wins before redaction (defense in depth:
/// even if redaction missed an unusual pattern, the value never gets
/// printed in the first place).
#[test]
fn token_bearing_env_vars_are_shown_as_set_not_value() {
    let cwd = TempDir::new().unwrap();
    let out = AssertCmd::cargo_bin("rustygit")
        .unwrap()
        .arg("bug-report")
        .current_dir(cwd.path())
        .env("GIT_ASKPASS", "/path/to/super-secret-helper")
        .output()
        .unwrap();
    let stdout = String::from_utf8(out.stdout).unwrap();
    // The var name must appear (we want it in the report) — the value
    // must not.
    assert!(
        stdout.contains("GIT_ASKPASS="),
        "GIT_ASKPASS missing from env block:\n{stdout}"
    );
    assert!(
        stdout.contains("GIT_ASKPASS=<set>"),
        "GIT_ASKPASS should show <set>, got line in:\n{stdout}"
    );
    assert!(
        !stdout.contains("/path/to/super-secret-helper"),
        "GIT_ASKPASS value leaked to stdout:\n{stdout}"
    );
}

/// Safe env vars (LANG, TERM) are reported with their actual values.
/// This proves the allow-list path works — without it the report would
/// be useless for "your shell locale is breaking everything" bugs.
#[test]
fn safe_env_vars_show_their_value() {
    let cwd = TempDir::new().unwrap();
    let out = AssertCmd::cargo_bin("rustygit")
        .unwrap()
        .arg("bug-report")
        .current_dir(cwd.path())
        .env("LANG", "en_US.UTF-8")
        .env("TERM", "xterm-256color")
        .output()
        .unwrap();
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(
        stdout.contains("LANG=en_US.UTF-8"),
        "LANG value missing from:\n{stdout}"
    );
    assert!(
        stdout.contains("TERM=xterm-256color"),
        "TERM value missing from:\n{stdout}"
    );
}

/// History is opt-in. With XDG_DATA_HOME pointing at an empty tempdir
/// there's no `history.log`, so the section must print the
/// "<history disabled or empty>" placeholder rather than crash or
/// leave the section blank.
#[test]
fn empty_history_shows_placeholder() {
    let cwd = TempDir::new().unwrap();
    let xdg = TempDir::new().unwrap();
    let out = AssertCmd::cargo_bin("rustygit")
        .unwrap()
        .arg("bug-report")
        .current_dir(cwd.path())
        .env("XDG_DATA_HOME", xdg.path())
        .env_remove("HOME")
        .output()
        .unwrap();
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(
        stdout.contains("<history disabled or empty>"),
        "expected placeholder, got:\n{stdout}"
    );
}

/// When a `history.log` exists, the last N lines (subcommand names
/// only — no argv) must appear in the report. This is the read path
/// for the opt-in history feature. Writing happens in `dispatch`
/// elsewhere and is tested independently; here we test that
/// bug-report consumes whatever's on disk.
#[test]
fn populated_history_is_included_in_report() {
    let cwd = TempDir::new().unwrap();
    let xdg = TempDir::new().unwrap();
    let log_dir = xdg.path().join("rustygit");
    std::fs::create_dir_all(&log_dir).unwrap();
    let log = log_dir.join("history.log");
    // 12 entries — bug-report should keep the last 10.
    let lines: Vec<String> = (1..=12).map(|i| format!("subcmd-{i}")).collect();
    std::fs::write(&log, format!("{}\n", lines.join("\n"))).unwrap();

    let out = AssertCmd::cargo_bin("rustygit")
        .unwrap()
        .arg("bug-report")
        .current_dir(cwd.path())
        .env("XDG_DATA_HOME", xdg.path())
        .output()
        .unwrap();
    let stdout = String::from_utf8(out.stdout).unwrap();
    // The last 10 entries (3..=12) must be present; the first two
    // must be dropped.
    assert!(
        stdout.contains("subcmd-12"),
        "missing latest subcommand in:\n{stdout}"
    );
    assert!(
        stdout.contains("subcmd-3"),
        "missing 10-back subcommand in:\n{stdout}"
    );
    assert!(
        !stdout.contains("subcmd-1\n"),
        "11+ back subcommand should be dropped:\n{stdout}"
    );
}

/// `--help` must work (clap-derive sanity check) — proves the variant
/// is wired into the `Command` enum correctly and doesn't trip the
/// CLI snapshot test infrastructure.
#[test]
fn bug_report_help_works() {
    let out = AssertCmd::cargo_bin("rustygit")
        .unwrap()
        .args(["bug-report", "--help"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(
        stdout.to_lowercase().contains("bug"),
        "help text doesn't mention 'bug': {stdout}"
    );
}
