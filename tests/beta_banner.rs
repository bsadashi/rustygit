//! Integration test for the beta-banner UX.
//!
//! The pure decision logic in [`rustygit::cli::beta::should_emit_banner`]
//! is unit-tested in-crate (see `src/cli/beta.rs::tests`). This file
//! covers the end-to-end side of the contract:
//!
//! 1. **GA builds never banner.** With the workspace's current Cargo
//!    version (which does not contain `-beta`), running any rustygit
//!    command must produce zero stderr banner output.
//!
//! 2. **The `--i-know-this-is-beta` flag is stripped before clap sees
//!    it.** Whatever the build version, an invocation with the ack
//!    flag must not produce a clap parse error and the underlying
//!    subcommand must run normally.
//!
//! These two cases together demonstrate that the banner machinery
//! doesn't pollute stdout, doesn't break argv parsing, and silently
//! no-ops when the build is GA.
//!
//! What this file does NOT cover: forcing a banner emission by
//! pretending the binary is `-beta`. We can't easily override
//! `CARGO_PKG_VERSION` from outside the compiler, and the only
//! reliable way would be to ship two test binaries. The pure
//! [`should_emit_banner`] unit tests in-crate already exercise the
//! beta-version path with every config + argv combination that
//! matters, so the integration coverage here is the GA-side
//! complement.

use std::path::Path;
use std::process::Command;

use assert_cmd::Command as AssertCmd;
use tempfile::TempDir;

const BANNER_NEEDLE: &str = "rustygit beta";

fn rustygit_in(tmp: &Path) -> AssertCmd {
    let mut cmd = AssertCmd::cargo_bin("rustygit").unwrap();
    cmd.current_dir(tmp)
        // Pin a clean identity so any `commit`-style subcommand we drive
        // through this test doesn't error out on missing user.name.
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        // Isolate HOME so we don't accidentally read the developer's
        // `~/.gitconfig` (which might have `rustygit.beta.acknowledged`
        // already set, which would mask a banner regression).
        .env("HOME", tmp);
    cmd
}

/// On a GA build (Cargo version without `-beta`), no command should
/// ever print a beta banner to stderr.
#[test]
fn ga_build_does_not_print_banner() {
    if env!("CARGO_PKG_VERSION").contains("-beta") {
        // On a beta build the banner is supposed to fire on
        // unacknowledged invocations; that's the wrong scenario for
        // this test. Skip rather than fail — the should_emit_banner
        // unit tests cover the beta path in-crate.
        eprintln!("skipped: build is `-beta`, this test only meaningful on GA");
        return;
    }

    let tmp = TempDir::new().unwrap();
    rustygit_in(tmp.path())
        .args(["init", "-q", "."])
        .assert()
        .success();

    let out = rustygit_in(tmp.path()).args(["status"]).output().unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains(BANNER_NEEDLE),
        "GA build emitted beta banner; stderr:\n{stderr}"
    );
}

/// The `--i-know-this-is-beta` flag must be stripped from argv before
/// clap parsing. If the flag leaked through clap would reject it as an
/// unknown global option and the command would fail with a usage error.
#[test]
fn ack_flag_is_stripped_before_clap() {
    let tmp = TempDir::new().unwrap();
    rustygit_in(tmp.path())
        .args(["init", "-q", "."])
        .assert()
        .success();

    // Run a status with the ack flag wedged in front of the subcommand.
    // If the flag isn't stripped, clap will fail with exit 2 / 129 and
    // an "unrecognized option" message on stderr.
    let out = rustygit_in(tmp.path())
        .args(["--i-know-this-is-beta", "status"])
        .output()
        .unwrap();

    assert!(
        out.status.success(),
        "rustygit --i-know-this-is-beta status failed (exit {:?})\nstderr: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr),
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains(BANNER_NEEDLE),
        "ack-flag invocation still emitted a banner; stderr:\n{stderr}"
    );
}

/// Sanity: the binary really is the one we're testing against (catches
/// the rare case where `assert_cmd` resolves a different `rustygit` on
/// PATH). Not strictly a beta-banner test, but lives here as a guard.
#[test]
fn binary_under_test_is_cargo_build() {
    let bin = assert_cmd::cargo::cargo_bin("rustygit");
    let out = Command::new(&bin).arg("--version").output().unwrap();
    assert!(out.status.success(), "--version failed for {bin:?}");
    let stdout = String::from_utf8_lossy(&out.stdout);
    // `clap`'s default `--version` output starts with the binary name.
    assert!(
        stdout.starts_with("rustygit "),
        "unexpected --version output: {stdout}"
    );
}
