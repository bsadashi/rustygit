//! SIGINT cleanup of lockfiles (A9a).
//!
//! Plain `Drop` rolls back uncommitted lockfiles on normal control flow and
//! `?`-style early-return errors, but it does NOT run when the process is
//! killed by a signal (SIGINT / SIGTERM). For Ctrl-C during `commit` or
//! `add` we must explicitly unlink every outstanding `.lock` file before
//! exiting so the next invocation isn't blocked by stale `.git/index.lock`.
//!
//! This test spawns `rustygit add <big-dir>` against a working tree with
//! enough files to keep the binary alive past our SIGINT window, sends
//! SIGINT, waits for the child to exit, and asserts that no `*.lock` file
//! survives under `.git/`.
//!
//! The test is Unix-only — `ctrlc` does support Console Ctrl-C events on
//! Windows, but the lockfile rename semantics there are different enough
//! that we explicitly carve Windows out (see `install_sigint_cleanup` in
//! `src/main.rs`).

#![cfg(unix)]

mod common;

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

use tempfile::TempDir;

/// Build (if needed) and return the path to the compiled `rustygit` binary.
/// We can't use `assert_cmd` here because we need raw process control —
/// `assert_cmd::Command` returns an `Output` after the process exits, but
/// we need to send SIGINT while it's still running.
fn rustygit_binary() -> PathBuf {
    // assert_cmd has a helper for this — it builds the binary once per test
    // run and returns the absolute path. We borrow it via `cargo_bin` to
    // avoid duplicating the lookup logic.
    assert_cmd::cargo::cargo_bin("rustygit")
}

/// Recursively list every file ending in `.lock` under `root`.
fn find_locks(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    walk(root, &mut out);
    out
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_dir() {
            walk(&path, out);
        } else if path
            .extension()
            .and_then(|s| s.to_str())
            .is_some_and(|ext| ext == "lock")
        {
            out.push(path);
        }
    }
}

/// SIGINT during `add` against a many-file tree must not leave a stale
/// `.git/index.lock` (or any other `*.lock`) behind. The acceptance gate is
/// "no .lock files under .git after the dust settles" — whether SIGINT lands
/// while the lock is held (the case the handler matters for) or earlier
/// (where Drop would clean up anyway), the post-condition is the same.
#[test]
fn sigint_during_add_leaves_no_lock_files() {
    let repo_dir = TempDir::new().unwrap();

    // Init the repo via the binary so it gets the standard config.
    let init = Command::new(rustygit_binary())
        .args(["init", "-q", "."])
        .current_dir(repo_dir.path())
        .output()
        .unwrap();
    assert!(
        init.status.success(),
        "init failed: {}",
        String::from_utf8_lossy(&init.stderr)
    );

    // Populate a tree with enough files that walking + serializing + writing
    // the index takes long enough to land SIGINT before completion. 5000
    // small files is enough on every machine we care about — even fast
    // NVMe with cached fs metadata still measures in hundreds of ms here.
    let bulk = repo_dir.path().join("bulk");
    std::fs::create_dir_all(&bulk).unwrap();
    for i in 0..5_000 {
        std::fs::write(bulk.join(format!("f{i:05}")), b"x").unwrap();
    }

    // Spawn `rustygit add bulk/` and let it start working.
    let mut child = Command::new(rustygit_binary())
        .args(["add", "bulk"])
        .current_dir(repo_dir.path())
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn rustygit add");

    // Wait long enough for the binary to be deep into the file walk. ~50ms
    // is the budget the plan calls for; that's enough on every dev machine
    // for the binary to have hit the registry at least once during a 5000-
    // file add.
    thread::sleep(Duration::from_millis(50));

    // Send SIGINT. We use the libc raw call here instead of the `nix` crate
    // to avoid an extra dev-dependency for a single signal-send.
    let pid = child.id() as i32;
    // SAFETY: `libc::kill` is the standard "send a signal to a known PID"
    // syscall; nothing in this scope is touched by it and the child process
    // exists (we just spawned it).
    unsafe {
        // 2 == SIGINT.
        libc::kill(pid, 2);
    }

    // Wait for the child to exit. The SIGINT handler does
    // `std::process::exit(130)` after cleanup, so we expect status 130 if
    // SIGINT landed mid-add; if the add finished first, we get 0. Both are
    // acceptable for THIS test — what matters is the .lock state.
    let status = child.wait().expect("wait failed");
    let code = status.code();
    assert!(
        code == Some(0) || code == Some(130),
        "unexpected exit code from sigint'd add: {code:?}"
    );

    // The actual assertion: no stale .lock files in .git/.
    let dot_git = repo_dir.path().join(".git");
    let locks = find_locks(&dot_git);
    assert!(
        locks.is_empty(),
        "expected no .lock files under .git, found: {locks:?}"
    );
}

/// As above, but exercising the registry through a *successful* run. The
/// post-condition is the same — no lock files — and the cleanup pathway
/// is `commit`/`Drop`, not the SIGINT handler. This guards against a
/// regression where the registry leaks committed locks (which would then
/// cause the SIGINT handler to try to unlink files that no longer exist;
/// `remove_file` will return ENOENT, which is benign, but it'd also confuse
/// any future logic that inspects the drained set).
#[test]
fn successful_add_leaves_no_lock_files() {
    let repo_dir = TempDir::new().unwrap();

    let init = Command::new(rustygit_binary())
        .args(["init", "-q", "."])
        .current_dir(repo_dir.path())
        .output()
        .unwrap();
    assert!(init.status.success(), "init failed");

    std::fs::write(repo_dir.path().join("a.txt"), b"a\n").unwrap();
    let add = Command::new(rustygit_binary())
        .args(["add", "a.txt"])
        .current_dir(repo_dir.path())
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .output()
        .unwrap();
    assert!(
        add.status.success(),
        "add failed: {}",
        String::from_utf8_lossy(&add.stderr)
    );

    let locks = find_locks(&repo_dir.path().join(".git"));
    assert!(
        locks.is_empty(),
        "expected no .lock files after a successful add, found: {locks:?}"
    );
}
