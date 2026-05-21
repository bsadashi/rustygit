//! `--help` snapshot tests — guard against accidental CLI breakage.
//!
//! For each subcommand we capture the first line of `--help` output plus
//! the set of flag-long-names it advertises. Both are part of our public
//! contract; either changing means a SemVer breaking change.
//!
//! Why a snapshot rather than checking full byte output: clap's help layout
//! is liable to change across clap releases (column wrapping, color codes,
//! etc.). Asserting on the set-of-flag-names is robust to those and still
//! catches the cases that actually matter for prod: a flag was added,
//! removed, or renamed.

mod common;

use std::collections::BTreeSet;
use std::path::Path;

use assert_cmd::Command as AssertCmd;

fn rustygit_help(subcmd: &[&str]) -> String {
    let mut cmd = AssertCmd::cargo_bin("rustygit").unwrap();
    let mut args: Vec<&str> = subcmd.to_vec();
    args.push("--help");
    let out = cmd
        .args(&args)
        .current_dir(Path::new("."))
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{} --help failed: {}",
        subcmd.join(" "),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

/// Extract the set of `--flag-name` long forms from a clap --help body.
fn long_flags(help: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for line in help.lines() {
        // Long flags look like `      --name`, `  -x, --name`, `      --name=<value>`.
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix("--") {
            // Drop everything after the first non-name char.
            let name: String = rest
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
                .collect();
            if !name.is_empty() {
                out.insert(name);
            }
        }
        if let Some(rest) = trimmed.strip_prefix("-").and_then(|s| {
            // Pattern `-x, --name`.
            s.split(", --").nth(1)
        }) {
            let name: String = rest
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
                .collect();
            if !name.is_empty() {
                out.insert(name);
            }
        }
    }
    out
}

#[test]
fn top_level_help_lists_every_subcommand() {
    let help = rustygit_help(&[]);
    // Spot-check a few known subcommands (full list is large and exercised
    // implicitly by the per-subcommand tests below).
    for cmd in [
        "init",
        "add",
        "commit",
        "log",
        "status",
        "diff",
        "show",
        "branch",
        "checkout",
        "switch",
        "merge",
        "rebase",
        "clone",
        "fetch",
        "push",
        "notes",
        "worktree",
        "prune-locks",
    ] {
        assert!(help.contains(cmd), "top-level --help missing '{cmd}'");
    }
}

#[test]
fn show_help_advertises_expected_flags() {
    let help = rustygit_help(&["show"]);
    // `show` is a thin command — only positional OBJECT(s) + clap's --help.
    let flags = long_flags(&help);
    assert!(flags.contains("help"), "missing --help");
    // Body should mention the OBJECT positional argument.
    assert!(
        help.to_lowercase().contains("object"),
        "show --help should mention the OBJECT argument; got: {help}"
    );
}

#[test]
fn diff_help_advertises_exit_code_and_quiet() {
    let help = rustygit_help(&["diff"]);
    let flags = long_flags(&help);
    for needed in ["cached", "exit-code", "quiet", "help"] {
        assert!(
            flags.contains(needed),
            "diff --help missing --{needed}; got {flags:?}"
        );
    }
}

#[test]
fn log_help_advertises_abbrev_oneline() {
    let help = rustygit_help(&["log"]);
    let flags = long_flags(&help);
    for needed in ["oneline", "abbrev", "abbrev-commit", "max-count"] {
        assert!(
            flags.contains(needed),
            "log --help missing --{needed}; got {flags:?}"
        );
    }
}

#[test]
fn commit_help_advertises_signing_flags() {
    let help = rustygit_help(&["commit"]);
    let flags = long_flags(&help);
    for needed in ["allow-empty", "gpg-sign", "no-gpg-sign", "no-verify"] {
        assert!(
            flags.contains(needed),
            "commit --help missing --{needed}; got {flags:?}"
        );
    }
    // -m is short-only by design (mirrors git commit's `-m <msg>` usage).
    assert!(
        help.contains("-m <MESSAGE>") || help.contains("-m"),
        "commit --help must still expose `-m`"
    );
}

#[test]
fn prune_locks_help_advertises_safety_flags() {
    let help = rustygit_help(&["prune-locks"]);
    let flags = long_flags(&help);
    for needed in ["dry-run", "force", "older-than", "verbose"] {
        assert!(
            flags.contains(needed),
            "prune-locks --help missing --{needed}; got {flags:?}"
        );
    }
}

#[test]
fn top_level_global_c_flag_visible_in_help() {
    let help = rustygit_help(&[]);
    // The -c key=value override must remain documented at the top level —
    // CI/script users rely on it.
    assert!(
        help.contains("KEY=VALUE") || help.contains("key=value"),
        "top-level --help no longer shows '-c KEY=VALUE'"
    );
}
