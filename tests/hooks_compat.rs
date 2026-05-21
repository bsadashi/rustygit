//! NON_GOALS.md Batch G — client-side hooks framework.
//!
//! Each test (a) builds a real git repo with the system `git`, (b) installs a
//! hook script that records a side-effect or returns a chosen exit code, and
//! (c) runs the corresponding rustygit porcelain to assert that:
//!   - the hook ran (sentinel file appeared, or commit reflects the mutation)
//!   - the hook's exit code propagated (non-zero on a blocking hook → exit 1)
//!   - argv / stdin / env were wired correctly per githooks(5)
//!
//! Skipped silently when `git` isn't on PATH.

mod common;

use std::path::Path;

use assert_cmd::Command as AssertCmd;
use common::{git, has_system_git};
use tempfile::TempDir;

#[cfg(unix)]
fn write_executable_hook(hooks_dir: &Path, name: &str, body: &str) {
    use std::os::unix::fs::PermissionsExt;
    let p = hooks_dir.join(name);
    std::fs::write(&p, body).unwrap();
    let mut perms = std::fs::metadata(&p).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&p, perms).unwrap();
}

fn rustygit(args: &[&str], cwd: &Path) -> std::process::Output {
    AssertCmd::cargo_bin("rustygit")
        .unwrap()
        .args(args)
        .current_dir(cwd)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .env("GIT_AUTHOR_DATE", "1700000000 +0000")
        .env("GIT_COMMITTER_DATE", "1700000000 +0000")
        .output()
        .unwrap()
}

/// Initialize a repo and write `f.txt`, staged but not committed. Returns
/// the tempdir handle (keep it alive for the test duration).
fn init_repo_with_stage() -> TempDir {
    let tmp = TempDir::new().unwrap();
    git(&["init", "-q", "-b", "master", "."], tmp.path());
    git(&["config", "user.name", "t"], tmp.path());
    git(&["config", "user.email", "t@t"], tmp.path());
    std::fs::write(tmp.path().join("f.txt"), b"v1\n").unwrap();
    git(&["add", "f.txt"], tmp.path());
    tmp
}

// ----- pre-commit -----

#[cfg(unix)]
#[test]
fn pre_commit_success_allows_commit() {
    if !has_system_git() {
        return;
    }
    let tmp = init_repo_with_stage();
    let hooks_dir = tmp.path().join(".git").join("hooks");
    let sentinel = tmp.path().join("ran.txt");
    write_executable_hook(
        &hooks_dir,
        "pre-commit",
        &format!("#!/bin/sh\ntouch {}\nexit 0\n", sentinel.display()),
    );
    let out = rustygit(&["commit", "-m", "subject"], tmp.path());
    assert!(
        out.status.success(),
        "commit failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(sentinel.exists(), "pre-commit hook did not run");
}

#[cfg(unix)]
#[test]
fn pre_commit_failure_aborts_commit_with_exit_1() {
    if !has_system_git() {
        return;
    }
    let tmp = init_repo_with_stage();
    let hooks_dir = tmp.path().join(".git").join("hooks");
    write_executable_hook(
        &hooks_dir,
        "pre-commit",
        "#!/bin/sh\necho 'rejected by pre-commit' >&2\nexit 1\n",
    );
    let out = rustygit(&["commit", "-m", "subject"], tmp.path());
    assert_eq!(
        out.status.code().unwrap_or(-1),
        1,
        "expected exit 1; got {:?}",
        out.status.code()
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("pre-commit") && stderr.contains("aborting"),
        "stderr should name the hook and 'aborting': {stderr}"
    );
    // No commit should have been created — `git log` returns non-zero on
    // an empty branch, which is exactly what we expect here.
    let log_out = std::process::Command::new("git")
        .args(["log", "--oneline"])
        .current_dir(tmp.path())
        .output()
        .unwrap();
    assert!(
        !log_out.status.success(),
        "`git log` should fail on a branch with no commits after the aborted commit; got stdout: {}",
        String::from_utf8_lossy(&log_out.stdout)
    );
}

#[cfg(unix)]
#[test]
fn no_verify_skips_pre_commit() {
    if !has_system_git() {
        return;
    }
    let tmp = init_repo_with_stage();
    let hooks_dir = tmp.path().join(".git").join("hooks");
    write_executable_hook(&hooks_dir, "pre-commit", "#!/bin/sh\nexit 99\n");
    let out = rustygit(&["commit", "--no-verify", "-m", "subject"], tmp.path());
    assert!(
        out.status.success(),
        "commit --no-verify should succeed even with failing pre-commit; got {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

// ----- commit-msg mutating the message -----

#[cfg(unix)]
#[test]
fn commit_msg_can_mutate_message() {
    if !has_system_git() {
        return;
    }
    let tmp = init_repo_with_stage();
    let hooks_dir = tmp.path().join(".git").join("hooks");
    write_executable_hook(
        &hooks_dir,
        "commit-msg",
        // Append " (verified)" to the first line.
        "#!/bin/sh\nsed -i.bak '1 s/$/ (verified)/' \"$1\"\nrm -f \"${1}.bak\"\n",
    );
    let out = rustygit(&["commit", "-m", "original"], tmp.path());
    assert!(
        out.status.success(),
        "commit failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let log = String::from_utf8(git(&["log", "--pretty=%s", "-1"], tmp.path()).stdout).unwrap();
    assert!(
        log.contains("(verified)"),
        "commit message should reflect the hook's mutation; got: {log}"
    );
}

// ----- post-commit (best-effort, runs even on non-zero) -----

#[cfg(unix)]
#[test]
fn post_commit_runs_after_successful_commit() {
    if !has_system_git() {
        return;
    }
    let tmp = init_repo_with_stage();
    let hooks_dir = tmp.path().join(".git").join("hooks");
    let sentinel = tmp.path().join("post.txt");
    write_executable_hook(
        &hooks_dir,
        "post-commit",
        &format!("#!/bin/sh\ntouch {}\nexit 0\n", sentinel.display()),
    );
    let out = rustygit(&["commit", "-m", "x"], tmp.path());
    assert!(out.status.success());
    assert!(sentinel.exists(), "post-commit hook did not run");
}

#[cfg(unix)]
#[test]
fn post_commit_nonzero_exit_does_not_abort_commit() {
    if !has_system_git() {
        return;
    }
    let tmp = init_repo_with_stage();
    let hooks_dir = tmp.path().join(".git").join("hooks");
    write_executable_hook(&hooks_dir, "post-commit", "#!/bin/sh\nexit 99\n");
    let out = rustygit(&["commit", "-m", "x"], tmp.path());
    assert!(
        out.status.success(),
        "post-commit failure must not abort commit; got {:?}",
        out.status.code()
    );
}

// ----- pre-push stdin format -----

#[cfg(unix)]
#[test]
fn pre_push_receives_correct_stdin() {
    if !has_system_git() {
        return;
    }
    let src = TempDir::new().unwrap();
    git(&["init", "-q", "-b", "master", "."], src.path());
    git(&["config", "user.name", "t"], src.path());
    git(&["config", "user.email", "t@t"], src.path());
    std::fs::write(src.path().join("a.txt"), b"a\n").unwrap();
    git(&["add", "a.txt"], src.path());
    git(&["commit", "-q", "-m", "c1"], src.path());

    // Bare-style dst.
    let dst = TempDir::new().unwrap();
    git(&["init", "-q", "--bare", "."], dst.path());

    let hooks_dir = src.path().join(".git").join("hooks");
    let sentinel = src.path().join("stdin.txt");
    let argv_sentinel = src.path().join("argv.txt");
    write_executable_hook(
        &hooks_dir,
        "pre-push",
        &format!(
            "#!/bin/sh\nprintf '%s %s\\n' \"$1\" \"$2\" > {}\ncat > {}\n",
            argv_sentinel.display(),
            sentinel.display()
        ),
    );

    let out = rustygit(
        &["push", dst.path().to_str().unwrap(), "master"],
        src.path(),
    );
    assert!(
        out.status.success(),
        "push failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let argv = std::fs::read_to_string(&argv_sentinel).unwrap();
    let body = std::fs::read_to_string(&sentinel).unwrap();
    // argv: remote-name remote-url (we pass URL for both).
    assert!(
        argv.contains(dst.path().to_str().unwrap()),
        "argv should contain the URL: {argv}"
    );
    // stdin: one line `<local-ref> SP <local-sha> SP <remote-ref> SP <remote-sha>\n`
    let line = body.lines().next().unwrap_or("");
    let parts: Vec<&str> = line.split_whitespace().collect();
    assert_eq!(
        parts.len(),
        4,
        "stdin line must have 4 whitespace-separated parts; got: {line:?}"
    );
    assert_eq!(parts[0], "refs/heads/master", "wrong local-ref");
    assert_eq!(parts[2], "refs/heads/master", "wrong remote-ref");
    // local-sha must be a real oid (40 hex chars for sha1).
    assert_eq!(parts[1].len(), 40, "local-sha should be 40 hex chars");
    // remote-sha should be zero since dst has no master yet.
    assert!(
        parts[3].chars().all(|c| c == '0'),
        "remote-sha should be all zeros (no remote ref yet): {parts:?}"
    );
}

#[cfg(unix)]
#[test]
fn pre_push_failure_aborts_push() {
    if !has_system_git() {
        return;
    }
    let src = TempDir::new().unwrap();
    git(&["init", "-q", "-b", "master", "."], src.path());
    git(&["config", "user.name", "t"], src.path());
    git(&["config", "user.email", "t@t"], src.path());
    std::fs::write(src.path().join("a.txt"), b"a\n").unwrap();
    git(&["add", "a.txt"], src.path());
    git(&["commit", "-q", "-m", "c1"], src.path());

    let dst = TempDir::new().unwrap();
    git(&["init", "-q", "--bare", "."], dst.path());

    let hooks_dir = src.path().join(".git").join("hooks");
    write_executable_hook(&hooks_dir, "pre-push", "#!/bin/sh\nexit 1\n");

    let out = rustygit(
        &["push", dst.path().to_str().unwrap(), "master"],
        src.path(),
    );
    assert_eq!(
        out.status.code().unwrap_or(-1),
        1,
        "push should abort with 1 on pre-push failure"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("pre-push") && stderr.contains("aborting"),
        "stderr should mention the hook abort: {stderr}"
    );
}

// ----- core.hooksPath redirection -----

#[cfg(unix)]
#[test]
fn core_hookspath_redirects_discovery() {
    if !has_system_git() {
        return;
    }
    let tmp = init_repo_with_stage();
    // Custom hooks dir outside of .git/hooks.
    let custom = tmp.path().join("custom-hooks");
    std::fs::create_dir_all(&custom).unwrap();
    git(
        &["config", "core.hooksPath", custom.to_str().unwrap()],
        tmp.path(),
    );

    let sentinel = tmp.path().join("custom-ran.txt");
    write_executable_hook(
        &custom,
        "pre-commit",
        &format!("#!/bin/sh\ntouch {}\n", sentinel.display()),
    );
    // Also write a different hook in the default .git/hooks/ to verify it
    // does NOT fire.
    let default = tmp.path().join(".git").join("hooks");
    let wrong_sentinel = tmp.path().join("wrong-ran.txt");
    write_executable_hook(
        &default,
        "pre-commit",
        &format!("#!/bin/sh\ntouch {}\nexit 7\n", wrong_sentinel.display()),
    );

    let out = rustygit(&["commit", "-m", "x"], tmp.path());
    assert!(
        out.status.success(),
        "commit should succeed (custom hook returned 0); got: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(sentinel.exists(), "custom hook did not run");
    assert!(
        !wrong_sentinel.exists(),
        "default hook should NOT have run when core.hooksPath is set"
    );
}

// ----- post-checkout argv -----

#[cfg(unix)]
#[test]
fn post_checkout_argv_contains_old_new_isbranch() {
    if !has_system_git() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    git(&["init", "-q", "-b", "master", "."], tmp.path());
    git(&["config", "user.name", "t"], tmp.path());
    git(&["config", "user.email", "t@t"], tmp.path());
    std::fs::write(tmp.path().join("a.txt"), b"a\n").unwrap();
    git(&["add", "a.txt"], tmp.path());
    git(&["commit", "-q", "-m", "c1"], tmp.path());
    // Make a second branch.
    git(&["branch", "topic"], tmp.path());

    let hooks_dir = tmp.path().join(".git").join("hooks");
    let sentinel = tmp.path().join("argv.txt");
    write_executable_hook(
        &hooks_dir,
        "post-checkout",
        &format!(
            "#!/bin/sh\nprintf '%s\\n%s\\n%s\\n' \"$1\" \"$2\" \"$3\" > {}\n",
            sentinel.display()
        ),
    );

    let out = rustygit(&["checkout", "topic"], tmp.path());
    assert!(
        out.status.success(),
        "checkout failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let body = std::fs::read_to_string(&sentinel).unwrap();
    let lines: Vec<&str> = body.lines().collect();
    assert_eq!(lines.len(), 3, "expected 3 argv lines; got: {body:?}");
    // First line: old HEAD oid (40 hex chars).
    assert_eq!(lines[0].len(), 40, "old HEAD should be 40 hex chars");
    assert_eq!(lines[1].len(), 40, "new HEAD should be 40 hex chars");
    assert_eq!(lines[2], "1", "is-branch-checkout should be 1");
}

// ----- non-executable hooks are skipped silently -----

#[cfg(unix)]
#[test]
fn non_executable_hook_is_silently_skipped() {
    if !has_system_git() {
        return;
    }
    let tmp = init_repo_with_stage();
    let hooks_dir = tmp.path().join(".git").join("hooks");
    // Write a hook WITHOUT +x.
    std::fs::write(
        hooks_dir.join("pre-commit"),
        "#!/bin/sh\necho ran >&2\nexit 1\n",
    )
    .unwrap();
    // Do NOT chmod.

    let out = rustygit(&["commit", "-m", "x"], tmp.path());
    assert!(
        out.status.success(),
        "commit should succeed; non-executable hook must be ignored. stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

// ----- shebang #!/bin/sh works -----

#[cfg(unix)]
#[test]
fn shebang_bin_sh_hook_works() {
    if !has_system_git() {
        return;
    }
    let tmp = init_repo_with_stage();
    let hooks_dir = tmp.path().join(".git").join("hooks");
    let sentinel = tmp.path().join("shebang.txt");
    write_executable_hook(
        &hooks_dir,
        "pre-commit",
        &format!(
            "#!/bin/sh\necho 'hello from /bin/sh' > {}\n",
            sentinel.display()
        ),
    );
    let out = rustygit(&["commit", "-m", "x"], tmp.path());
    assert!(out.status.success());
    let body = std::fs::read_to_string(&sentinel).unwrap();
    assert!(body.contains("hello from /bin/sh"));
}

// ----- prepare-commit-msg sees the source argument -----

#[cfg(unix)]
#[test]
fn prepare_commit_msg_sees_message_source() {
    if !has_system_git() {
        return;
    }
    let tmp = init_repo_with_stage();
    let hooks_dir = tmp.path().join(".git").join("hooks");
    let sentinel = tmp.path().join("source.txt");
    write_executable_hook(
        &hooks_dir,
        "prepare-commit-msg",
        &format!(
            "#!/bin/sh\nprintf '%s\\n' \"$2\" > {}\n",
            sentinel.display()
        ),
    );
    let out = rustygit(&["commit", "-m", "x"], tmp.path());
    assert!(out.status.success());
    let body = std::fs::read_to_string(&sentinel).unwrap();
    assert_eq!(
        body.trim(),
        "message",
        "prepare-commit-msg should see 'message' as source"
    );
}

#[cfg(unix)]
#[test]
fn prepare_commit_msg_runs_even_with_no_verify() {
    if !has_system_git() {
        return;
    }
    let tmp = init_repo_with_stage();
    let hooks_dir = tmp.path().join(".git").join("hooks");
    let sentinel = tmp.path().join("ran.txt");
    write_executable_hook(
        &hooks_dir,
        "prepare-commit-msg",
        &format!("#!/bin/sh\ntouch {}\n", sentinel.display()),
    );
    let out = rustygit(&["commit", "--no-verify", "-m", "x"], tmp.path());
    assert!(
        out.status.success(),
        "commit failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        sentinel.exists(),
        "prepare-commit-msg must run even with --no-verify per githooks(5)"
    );
}

// ----- pre-auto-gc -----

#[cfg(unix)]
#[test]
fn pre_auto_gc_failure_aborts_gc() {
    if !has_system_git() {
        return;
    }
    let tmp = init_repo_with_stage();
    git(&["commit", "-q", "-m", "c1"], tmp.path());
    let hooks_dir = tmp.path().join(".git").join("hooks");
    write_executable_hook(&hooks_dir, "pre-auto-gc", "#!/bin/sh\nexit 1\n");
    let out = rustygit(&["gc"], tmp.path());
    assert_eq!(
        out.status.code().unwrap_or(-1),
        1,
        "gc should abort with 1 on pre-auto-gc failure"
    );
}
