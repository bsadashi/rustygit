//! `GIT_TRACE` debug-logging (A9b).
//!
//! Verifies that with `GIT_TRACE=1` set in the environment, `rustygit`
//! emits something on stderr matching the `<time> <category>: <message>`
//! format we documented in `src/trace.rs`. The whole point of `GIT_TRACE`
//! is to be a low-friction lever a user can flip in a bug report — so
//! this test guards the user-facing contract: env var on, trace lines out,
//! no special args required.

mod common;

use std::path::Path;
use std::process::Output;

use assert_cmd::Command as AssertCmd;
use tempfile::TempDir;

fn rustygit_in(cwd: &Path) -> AssertCmd {
    let mut cmd = AssertCmd::cargo_bin("rustygit").unwrap();
    cmd.current_dir(cwd)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t");
    cmd
}

fn assert_ok(out: &Output, label: &str) {
    assert!(
        out.status.success(),
        "{label} failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// `GIT_TRACE=1 rustygit status` must print at least one line that
/// looks like a trace line — i.e. has a category. We check for the
/// `repo:` category since `discover_from_cwd` emits one on every command,
/// making this the most reliable line to assert against.
#[test]
fn git_trace_1_emits_trace_lines_on_stderr() {
    let tmp = TempDir::new().unwrap();
    let init = rustygit_in(tmp.path())
        .args(["init", "-q", "."])
        .output()
        .unwrap();
    assert_ok(&init, "init");

    let out = rustygit_in(tmp.path())
        .env("GIT_TRACE", "1")
        .args(["status"])
        .output()
        .unwrap();
    assert_ok(&out, "status with GIT_TRACE=1");

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("repo:") || stderr.contains("refs:") || stderr.contains("odb:"),
        "expected a trace line on stderr, got:\n{stderr}"
    );
}

/// Without `GIT_TRACE` set, stderr must NOT contain trace lines. This is
/// the contrapositive — we'd hate to accidentally leak the trace into
/// every invocation. Some commands legitimately write to stderr (e.g.
/// error messages on bad input), but `status` on a clean repo should
/// produce nothing on stderr.
#[test]
fn no_git_trace_means_no_trace_lines() {
    let tmp = TempDir::new().unwrap();
    let init = rustygit_in(tmp.path())
        .args(["init", "-q", "."])
        .output()
        .unwrap();
    assert_ok(&init, "init");

    let out = rustygit_in(tmp.path())
        .env_remove("GIT_TRACE")
        .args(["status"])
        .output()
        .unwrap();
    assert_ok(&out, "status without GIT_TRACE");

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains("repo:"),
        "expected no trace lines without GIT_TRACE, got:\n{stderr}"
    );
}

/// `GIT_TRACE=0` is the explicit "off" form. Some CI environments set
/// `GIT_TRACE=0` globally; we must not flip on tracing when that's the
/// case.
#[test]
fn git_trace_0_disables_tracing() {
    let tmp = TempDir::new().unwrap();
    let init = rustygit_in(tmp.path())
        .args(["init", "-q", "."])
        .output()
        .unwrap();
    assert_ok(&init, "init");

    let out = rustygit_in(tmp.path())
        .env("GIT_TRACE", "0")
        .args(["status"])
        .output()
        .unwrap();
    assert_ok(&out, "status with GIT_TRACE=0");

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains("repo:"),
        "expected no trace lines with GIT_TRACE=0, got:\n{stderr}"
    );
}

/// `GIT_TRACE=/abs/path` writes to that file instead of stderr.
#[test]
fn git_trace_abs_path_writes_to_file() {
    let tmp = TempDir::new().unwrap();
    let init = rustygit_in(tmp.path())
        .args(["init", "-q", "."])
        .output()
        .unwrap();
    assert_ok(&init, "init");

    let trace_file = tmp.path().join("trace.log");
    let out = rustygit_in(tmp.path())
        .env("GIT_TRACE", &trace_file)
        .args(["status"])
        .output()
        .unwrap();
    assert_ok(&out, "status with GIT_TRACE=<path>");

    // The trace file must exist and contain at least one trace line.
    assert!(
        trace_file.exists(),
        "GIT_TRACE=<path> should have created the file"
    );
    let body = std::fs::read_to_string(&trace_file).unwrap();
    assert!(
        body.contains("repo:") || body.contains("refs:") || body.contains("odb:"),
        "expected trace lines in file, got:\n{body}"
    );

    // And — symmetrically — stderr should NOT have trace lines when they're
    // routed to a file.
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains("repo:"),
        "trace lines should go to the file, not stderr; stderr was:\n{stderr}"
    );
}
