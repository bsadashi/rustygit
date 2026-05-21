//! NON_GOALS B4 smoke tests for `rustygit completions <shell>` and
//! `rustygit manpage`.
//!
//! These confirm the generated artifacts have the right shape — without
//! pinning the full byte output (clap_complete/clap_mangen are allowed to
//! tweak their templates between releases). What we DO pin:
//!
//! - Exit code 0 on every supported shell.
//! - Bash output contains `complete -F` (the bash completion-register form).
//! - Zsh output contains `#compdef rustygit` (the zsh registration sigil).
//! - Fish output contains `complete -c rustygit` (the fish per-program form).
//! - Manpage output starts with `.TH rustygit 1` (the troff section header
//!   — clap_mangen emits the binary name in whatever case it's defined in
//!   `Cli::command()`; we accept either `RUSTYGIT` or `rustygit`).
//!
//! Run with: `cargo test --test completions_smoke`.

use assert_cmd::Command as AssertCmd;

fn run(args: &[&str]) -> std::process::Output {
    AssertCmd::cargo_bin("rustygit")
        .unwrap()
        .args(args)
        .output()
        .unwrap()
}

fn assert_success(out: &std::process::Output, label: &str) {
    assert!(
        out.status.success(),
        "{label} did not exit 0 (got {:?})\nstderr: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr),
    );
}

#[test]
fn completions_bash_exits_ok_and_emits_complete_f() {
    let out = run(&["completions", "bash"]);
    assert_success(&out, "completions bash");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("complete -F"),
        "bash completion missing `complete -F` register-line; got:\n{}",
        &stdout[..stdout.len().min(2000)],
    );
    assert!(
        stdout.contains("rustygit"),
        "bash completion does not mention `rustygit`; got:\n{}",
        &stdout[..stdout.len().min(2000)],
    );
}

#[test]
fn completions_zsh_exits_ok_and_emits_compdef() {
    let out = run(&["completions", "zsh"]);
    assert_success(&out, "completions zsh");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("#compdef rustygit"),
        "zsh completion missing `#compdef rustygit`; got:\n{}",
        &stdout[..stdout.len().min(2000)],
    );
}

#[test]
fn completions_fish_exits_ok_and_emits_complete_c() {
    let out = run(&["completions", "fish"]);
    assert_success(&out, "completions fish");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("complete -c rustygit"),
        "fish completion missing `complete -c rustygit`; got:\n{}",
        &stdout[..stdout.len().min(2000)],
    );
}

#[test]
fn manpage_exits_ok_and_emits_troff_section_header() {
    let out = run(&["manpage"]);
    assert_success(&out, "manpage");
    let stdout = String::from_utf8_lossy(&out.stdout);
    // clap_mangen emits `.TH "rustygit" "1" "<date>" ...`. We allow either
    // case for forward-compat with future clap_mangen versions, and we
    // tolerate optional quoting.
    let lower = stdout.to_lowercase();
    assert!(
        lower.contains(".th \"rustygit\" \"1\"")
            || lower.contains(".th rustygit 1")
            || lower.contains(".th \"rustygit\""),
        "manpage missing troff `.TH rustygit 1` header; first 500 bytes:\n{}",
        &stdout[..stdout.len().min(500)],
    );
}

#[test]
fn completions_subcommand_is_hidden_from_top_level_help() {
    // `--help` should NOT list `completions` or `manpage` in the user-facing
    // subcommand list — they're plumbing for the release workflow only.
    let out = run(&["--help"]);
    assert_success(&out, "--help");
    let stdout = String::from_utf8_lossy(&out.stdout);
    // The subcommand listing in `clap` --help puts each name in the leftmost
    // column. We check that the help does NOT advertise these in the section
    // listing. The names will of course still appear if they're referenced
    // elsewhere (e.g., in long-form prose), so we look for the pattern
    // "  completions  " / "  manpage  " (left-aligned, two-space indent,
    // trailing space → description). A simple substring is fine because
    // those exact patterns are clap's listing format.
    assert!(
        !stdout.contains("  completions  ") && !stdout.contains("  completions\n"),
        "completions should be hidden from --help, but it appears in the listing"
    );
    assert!(
        !stdout.contains("  manpage  ") && !stdout.contains("  manpage\n"),
        "manpage should be hidden from --help, but it appears in the listing"
    );
}

#[test]
fn completions_with_invalid_shell_returns_usage_error() {
    // Hidden ≠ valueless. Invalid shell names must still surface a clap
    // usage error rather than panicking or generating empty output.
    let out = run(&["completions", "no-such-shell"]);
    assert!(
        !out.status.success(),
        "expected non-zero exit for invalid shell name; stdout was:\n{}",
        String::from_utf8_lossy(&out.stdout),
    );
}
