//! Previously: NON_GOALS.md Batch A asserted the "drop entirely"
//! subcommands emit a named explanation. Now those subcommands are
//! real, so this test now asserts they DON'T error out with an
//! "unrecognized subcommand" message — they accept the invocation and
//! either run or print a usage hint (a real command's exit code).

use std::path::Path;

use assert_cmd::Command as AssertCmd;
use tempfile::TempDir;

fn rustygit(args: &[&str], cwd: &Path) -> std::process::Output {
    AssertCmd::cargo_bin("rustygit")
        .unwrap()
        .args(args)
        .current_dir(cwd)
        .output()
        .unwrap()
}

/// Each previously-rejected subcommand should now be recognized: stderr
/// MUST NOT say "unrecognized subcommand" or "not part of rustygit".
#[test]
fn formerly_dropped_subcommands_are_now_recognized() {
    // Run each in a temp dir (no git repo) so that even commands that
    // need a repo fail at their own level — what we're verifying here
    // is that they're not rejected by clap as unknown.
    let tmp = TempDir::new().unwrap();
    for name in &[
        "gitweb",
        "gitk",
        "git-gui",
        "svn",
        "p4",
        "instaweb",
        "request-pull",
        "mergetool",
        "difftool",
        "filter-branch",
    ] {
        let out = rustygit(&[*name, "--help"], tmp.path());
        let stderr = String::from_utf8_lossy(&out.stderr);
        let stdout = String::from_utf8_lossy(&out.stdout);
        let combined = format!("{stdout}{stderr}");
        assert!(
            !combined.contains("not part of rustygit"),
            "{name} still hits the old explainer: {combined}"
        );
        assert!(
            !combined.to_lowercase().contains("unrecognized subcommand"),
            "{name} is rejected by clap: {combined}"
        );
    }
}

/// `git-svn`/`git-p4`/`git-instaweb` aliases route to `svn`/`p4`/`instaweb`.
#[test]
fn aliased_long_names_resolve() {
    let tmp = TempDir::new().unwrap();
    for alias in &["git-svn", "git-p4", "git-instaweb"] {
        let out = rustygit(&[*alias, "--help"], tmp.path());
        let combined = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            !combined.to_lowercase().contains("unrecognized subcommand"),
            "{alias}: {combined}"
        );
    }
}

/// Typos still fall through to clap's "unrecognized" error.
#[test]
fn typos_still_get_clap_error() {
    let tmp = TempDir::new().unwrap();
    let out = rustygit(&["gitwev"], tmp.path());
    assert!(!out.status.success(), "typo should fail");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.to_lowercase().contains("unrecognized") || stderr.to_lowercase().contains("invalid"),
        "expected clap-style error for typo: {stderr}"
    );
}
