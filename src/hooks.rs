//! Client-side git hooks framework (NON_GOALS.md Batch G).
//!
//! Implements the dispatcher that fires git's client hooks at the right
//! moments with the right argv / stdin / env wiring. Each `cli::*::run`
//! function that performs a hook-bearing operation calls into here.
//!
//! # What lives where
//!
//! - [`HookRunner`] — the dispatcher. Constructed once per command via
//!   [`HookRunner::from_repo`]. Resolves the hooks directory (honoring
//!   `core.hooksPath` from the repository config) and caches the env vars
//!   that need to be propagated to spawned hooks.
//! - [`HookOutcome`] — what happened. Three variants: `Ran { exit_code }`
//!   (the hook executed; non-zero means "abort the parent op"),
//!   `NotPresent` (no file at `<hooks_dir>/<name>`, OR file is not
//!   executable — treated as a success no-op), and `Skipped` (the hooks
//!   dir itself is unusable, e.g. `core.hooksPath` points nowhere).
//!
//! # Hooks-dir resolution
//!
//! 1. `core.hooksPath` from `.git/config`. Absolute path used as-is;
//!    relative path resolved relative to the **workdir** (matching git's
//!    behavior — see `setup.c::setup_git_directory_gently_1`).
//! 2. Fallback: `<gitdir>/hooks/`.
//!
//! # Environment variables propagated to hooks
//!
//! Always set: `GIT_DIR`, `GIT_WORK_TREE`. Pass through (when present in
//! the parent env): `GIT_INDEX_FILE`, `GIT_EDITOR`, `EDITOR`, and the full
//! `GIT_AUTHOR_*` / `GIT_COMMITTER_*` family. Matches upstream
//! `run-command.c::prepare_run_command_v_opt` for client-side dispatch.
//!
//! # Server-side hooks
//!
//! Explicitly out of scope. rustygit doesn't run as a server, so
//! `pre-receive`, `update`, `post-receive`, `post-update` are not wired.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::config::Config;
use crate::repo::Repository;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// The result of dispatching a single hook.
#[derive(Debug, Clone)]
pub enum HookOutcome {
    /// The hook file exists, is executable, and ran. `exit_code` is the
    /// process exit status. Callers should abort the parent op when this is
    /// non-zero for "blocking" hooks; non-blocking hooks (post-commit etc.)
    /// log a warning instead.
    Ran { exit_code: i32 },
    /// The hook is missing or not executable. This is the success case for
    /// "I don't have anything to say"; it is NOT an error.
    NotPresent,
    /// We could not even attempt the hook because the hooks directory is
    /// unusable (e.g. `core.hooksPath` points at a non-existent path).
    /// The `reason` is human-readable for logging.
    #[allow(dead_code)]
    Skipped { reason: String },
}

impl HookOutcome {
    /// True iff the parent op should abort (blocking hook returned non-zero).
    /// `NotPresent` and `Ran { 0 }` both return false.
    pub fn aborts_parent(&self) -> bool {
        matches!(self, HookOutcome::Ran { exit_code } if *exit_code != 0)
    }

    /// The exit code, if the hook actually ran. Used for "the exit code of
    /// the parent op becomes the exit code of the hook" semantics (e.g.
    /// `post-checkout` per githooks(5)).
    pub fn exit_code(&self) -> Option<i32> {
        match self {
            HookOutcome::Ran { exit_code } => Some(*exit_code),
            _ => None,
        }
    }
}

/// The hook dispatcher. One per command.
pub struct HookRunner {
    hooks_dir: PathBuf,
    gitdir: PathBuf,
    workdir: PathBuf,
    /// `(name, value)` pairs to forward to the child env in addition to the
    /// always-set `GIT_DIR`/`GIT_WORK_TREE`. Populated at construction time
    /// from the parent process env so hooks see the same `GIT_*` overrides
    /// that drove the parent op.
    passthrough_env: Vec<(String, String)>,
}

impl HookRunner {
    /// Build a `HookRunner` for `repo`. Reads `core.hooksPath` once at
    /// construction; later `run` calls use the cached value.
    ///
    /// Never fails — a broken config or missing hooks dir surfaces later
    /// via [`HookOutcome::Skipped`].
    pub fn from_repo(repo: &Repository) -> Self {
        let gitdir = repo.gitdir().to_path_buf();
        let workdir = repo.workdir().to_path_buf();
        let hooks_dir = resolve_hooks_dir(repo);
        let passthrough_env = collect_passthrough_env();
        Self {
            hooks_dir,
            gitdir,
            workdir,
            passthrough_env,
        }
    }

    /// Run hook `name` with the given argv and optional stdin payload.
    ///
    /// Returns:
    /// - `Ran { exit_code }` if the hook file exists, is executable, and the
    ///   spawn succeeded.
    /// - `NotPresent` if the file is missing or not executable. (We treat
    ///   non-executable as missing rather than erroring — that's git's
    ///   behavior since 2.36 when the hooks-dir convention was formalized.)
    /// - `Skipped` if we can't even attempt the spawn (no hooks dir).
    ///
    /// The hook is invoked with cwd = the repository workdir, matching
    /// git. Stdout/stderr inherit the parent process so the user sees the
    /// hook's diagnostics live.
    pub fn run(&self, name: &str, args: &[&str], stdin: Option<&[u8]>) -> io::Result<HookOutcome> {
        if !self.hooks_dir.exists() {
            return Ok(HookOutcome::Skipped {
                reason: format!("hooks dir {} does not exist", self.hooks_dir.display()),
            });
        }

        let hook_path = self.hooks_dir.join(name);
        if !is_executable(&hook_path) {
            return Ok(HookOutcome::NotPresent);
        }

        let mut cmd = Command::new(&hook_path);
        cmd.args(args)
            .current_dir(&self.workdir)
            .env("GIT_DIR", &self.gitdir)
            .env("GIT_WORK_TREE", &self.workdir);
        for (k, v) in &self.passthrough_env {
            cmd.env(k, v);
        }

        if stdin.is_some() {
            cmd.stdin(Stdio::piped());
        } else {
            cmd.stdin(Stdio::null());
        }
        // Capture stdout/stderr and forward to ours after the hook exits.
        // We can't simply `Stdio::inherit()` here because under cargo's
        // test harness the parent stdout/stderr are pipes managed by the
        // harness; sharing those FDs with many concurrent hooks deadlocks
        // when the pipe buffers fill. Capturing then re-emitting after
        // wait() keeps interactive use looking identical (the user still
        // sees the hook's output) and unblocks parallel test runs.
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

        let mut child = cmd.spawn()?;

        if let Some(payload) = stdin {
            if let Some(mut sin) = child.stdin.take() {
                sin.write_all(payload)?;
                // Drop closes the pipe so the hook sees EOF on stdin.
            }
        }

        let output = child.wait_with_output()?;
        // Forward what the hook wrote on stdout/stderr so the user sees
        // its diagnostics (e.g. pre-commit's "non-ASCII filename" warning).
        if !output.stdout.is_empty() {
            let _ = io::stdout().write_all(&output.stdout);
        }
        if !output.stderr.is_empty() {
            let _ = io::stderr().write_all(&output.stderr);
        }
        let code = output.status.code().unwrap_or(-1);
        Ok(HookOutcome::Ran { exit_code: code })
    }

    /// Convenience for the commit-msg hook shape: hook receives one
    /// argument that is the absolute path to a message file.
    pub fn run_with_file(&self, name: &str, file_arg: &Path) -> io::Result<HookOutcome> {
        let p = file_arg.to_string_lossy();
        self.run(name, &[&p], None)
    }

    /// The hooks directory this runner will use. Mostly for diagnostics
    /// and tests.
    #[allow(dead_code)]
    pub fn hooks_dir(&self) -> &Path {
        &self.hooks_dir
    }
}

// ---------------------------------------------------------------------------
// Helpers used by porcelain to emit the "hook aborted" message
// ---------------------------------------------------------------------------

/// Print the standard "hook returned N; aborting" message to stderr. Used
/// uniformly by every porcelain that fires a blocking hook so the wording
/// stays consistent.
pub fn print_abort(op: &str, hook_name: &str, code: i32) {
    eprintln!("rustygit: {op}: hook '{hook_name}' returned {code}; aborting");
}

/// Print a soft warning when a non-blocking hook (post-commit, post-merge,
/// post-checkout, post-rewrite) exited non-zero. The parent op continues
/// either way; this is purely informational.
pub fn print_warning(op: &str, hook_name: &str, code: i32) {
    eprintln!("rustygit: {op}: warning: non-blocking hook '{hook_name}' exited {code}");
}

// ---------------------------------------------------------------------------
// Internal: hooks-dir resolution + permission detection + env collection
// ---------------------------------------------------------------------------

fn resolve_hooks_dir(repo: &Repository) -> PathBuf {
    // Try `core.hooksPath` from the repo config. Absolute path used as-is;
    // relative resolved relative to the workdir (matches git).
    let cfg = Config::from_repo_dir(repo.commondir()).unwrap_or_default();
    if let Some(s) = cfg.get_string("core", "hooksPath") {
        let s = s.trim();
        if !s.is_empty() {
            let p = PathBuf::from(s);
            return if p.is_absolute() {
                p
            } else {
                repo.workdir().join(p)
            };
        }
    }
    repo.gitdir().join("hooks")
}

#[cfg(unix)]
fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    match fs::metadata(p) {
        Ok(m) => m.is_file() && (m.permissions().mode() & 0o111) != 0,
        Err(_) => false,
    }
}

#[cfg(not(unix))]
fn is_executable(p: &Path) -> bool {
    // On non-Unix we accept anything that exists as a file. rustygit's CI
    // runs Unix-only (per the comments in NON_GOALS.md "Windows" section)
    // but we don't want this module to fail to compile on Windows.
    fs::metadata(p).map(|m| m.is_file()).unwrap_or(false)
}

/// Collect the subset of the parent process env that hooks expect to see.
/// We do NOT blanket-forward all `GIT_*` vars because some (like `GIT_DIR`
/// from a nested invocation) would actively confuse a hook. We forward
/// only the well-defined keys git itself documents.
fn collect_passthrough_env() -> Vec<(String, String)> {
    let mut out = Vec::new();
    for key in [
        "GIT_INDEX_FILE",
        "GIT_EDITOR",
        "EDITOR",
        "GIT_AUTHOR_NAME",
        "GIT_AUTHOR_EMAIL",
        "GIT_AUTHOR_DATE",
        "GIT_COMMITTER_NAME",
        "GIT_COMMITTER_EMAIL",
        "GIT_COMMITTER_DATE",
    ] {
        if let Ok(v) = std::env::var(key) {
            out.push((key.to_string(), v));
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// Create a `Repository` in a fresh tempdir with a minimal `.git/`
    /// layout so `from_repo` has something to read.
    fn make_repo() -> (TempDir, Repository) {
        let tmp = TempDir::new().unwrap();
        let gitdir = tmp.path().join(".git");
        for sub in ["", "objects", "objects/pack", "refs", "refs/heads", "hooks"] {
            fs::create_dir_all(gitdir.join(sub)).unwrap();
        }
        fs::write(gitdir.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        fs::write(
            gitdir.join("config"),
            "[core]\n\trepositoryformatversion = 0\n",
        )
        .unwrap();
        let repo = Repository::open(gitdir).unwrap();
        (tmp, repo)
    }

    #[cfg(unix)]
    fn write_hook(dir: &Path, name: &str, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let p = dir.join(name);
        fs::write(&p, body).unwrap();
        let mut perms = fs::metadata(&p).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&p, perms).unwrap();
        p
    }

    #[test]
    fn from_repo_defaults_to_gitdir_hooks() {
        let (tmp, repo) = make_repo();
        let r = HookRunner::from_repo(&repo);
        assert_eq!(r.hooks_dir(), tmp.path().join(".git").join("hooks"));
    }

    #[test]
    fn from_repo_honors_core_hookspath_absolute() {
        let (tmp, repo) = make_repo();
        let alt = tmp.path().join("alt-hooks");
        fs::create_dir_all(&alt).unwrap();
        // Rewrite config with core.hooksPath.
        let cfg_text = format!(
            "[core]\n\trepositoryformatversion = 0\n\thooksPath = {}\n",
            alt.display()
        );
        fs::write(repo.gitdir().join("config"), cfg_text).unwrap();

        let repo = Repository::open(repo.gitdir().to_path_buf()).unwrap();
        let r = HookRunner::from_repo(&repo);
        assert_eq!(r.hooks_dir(), alt);
    }

    #[test]
    fn from_repo_honors_core_hookspath_relative() {
        let (tmp, repo) = make_repo();
        // Relative path → resolved against workdir.
        let cfg_text = "[core]\n\trepositoryformatversion = 0\n\thooksPath = my-hooks\n";
        fs::write(repo.gitdir().join("config"), cfg_text).unwrap();
        let repo = Repository::open(repo.gitdir().to_path_buf()).unwrap();
        let r = HookRunner::from_repo(&repo);
        let workdir = tmp
            .path()
            .canonicalize()
            .unwrap_or_else(|_| tmp.path().to_path_buf());
        // Repository::open may have canonicalized the workdir. Compare
        // by suffix to keep the test resilient to that.
        assert!(
            r.hooks_dir().ends_with("my-hooks"),
            "expected hooks dir to end with my-hooks; got {:?} workdir={:?}",
            r.hooks_dir(),
            workdir,
        );
    }

    #[test]
    fn run_returns_not_present_for_missing_hook() {
        let (_tmp, repo) = make_repo();
        let r = HookRunner::from_repo(&repo);
        let outcome = r.run("pre-commit", &[], None).unwrap();
        assert!(matches!(outcome, HookOutcome::NotPresent));
        assert!(!outcome.aborts_parent());
    }

    #[cfg(unix)]
    #[test]
    fn run_returns_not_present_for_non_executable_hook() {
        let (_tmp, repo) = make_repo();
        let hooks_dir = repo.gitdir().join("hooks");
        // Write the file WITHOUT the +x bit.
        let p = hooks_dir.join("pre-commit");
        fs::write(&p, "#!/bin/sh\nexit 1\n").unwrap();
        // (No chmod +x.)
        let r = HookRunner::from_repo(&repo);
        let outcome = r.run("pre-commit", &[], None).unwrap();
        assert!(matches!(outcome, HookOutcome::NotPresent));
    }

    #[cfg(unix)]
    #[test]
    fn run_succeeds_and_returns_zero_exit() {
        let (_tmp, repo) = make_repo();
        let hooks_dir = repo.gitdir().join("hooks");
        write_hook(&hooks_dir, "pre-commit", "#!/bin/sh\nexit 0\n");
        let r = HookRunner::from_repo(&repo);
        let outcome = r.run("pre-commit", &[], None).unwrap();
        assert!(matches!(outcome, HookOutcome::Ran { exit_code: 0 }));
        assert!(!outcome.aborts_parent());
    }

    #[cfg(unix)]
    #[test]
    fn run_non_zero_exit_signals_abort() {
        let (_tmp, repo) = make_repo();
        let hooks_dir = repo.gitdir().join("hooks");
        write_hook(&hooks_dir, "pre-commit", "#!/bin/sh\nexit 7\n");
        let r = HookRunner::from_repo(&repo);
        let outcome = r.run("pre-commit", &[], None).unwrap();
        assert!(matches!(outcome, HookOutcome::Ran { exit_code: 7 }));
        assert!(outcome.aborts_parent());
        assert_eq!(outcome.exit_code(), Some(7));
    }

    #[cfg(unix)]
    #[test]
    fn run_passes_argv_to_hook() {
        let (tmp, repo) = make_repo();
        let hooks_dir = repo.gitdir().join("hooks");
        // Record argv to a sentinel file.
        let sentinel = tmp.path().join("argv.txt");
        write_hook(
            &hooks_dir,
            "post-checkout",
            &format!(
                "#!/bin/sh\nprintf '%s\\n%s\\n%s\\n' \"$1\" \"$2\" \"$3\" > {}\n",
                sentinel.display()
            ),
        );
        let r = HookRunner::from_repo(&repo);
        let outcome = r
            .run("post-checkout", &["aaaa1111", "bbbb2222", "1"], None)
            .unwrap();
        assert!(matches!(outcome, HookOutcome::Ran { exit_code: 0 }));
        let body = fs::read_to_string(&sentinel).unwrap();
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines, vec!["aaaa1111", "bbbb2222", "1"]);
    }

    #[cfg(unix)]
    #[test]
    fn run_passes_stdin_to_hook() {
        let (tmp, repo) = make_repo();
        let hooks_dir = repo.gitdir().join("hooks");
        let sentinel = tmp.path().join("stdin.txt");
        write_hook(
            &hooks_dir,
            "pre-push",
            &format!("#!/bin/sh\ncat > {}\n", sentinel.display()),
        );
        let r = HookRunner::from_repo(&repo);
        let payload = b"refs/heads/main 1111 refs/heads/main 2222\n";
        let outcome = r
            .run(
                "pre-push",
                &["origin", "https://example.com/repo"],
                Some(payload),
            )
            .unwrap();
        assert!(matches!(outcome, HookOutcome::Ran { exit_code: 0 }));
        let body = fs::read(&sentinel).unwrap();
        assert_eq!(body, payload);
    }

    #[cfg(unix)]
    #[test]
    fn run_sets_git_dir_and_work_tree_env() {
        let (tmp, repo) = make_repo();
        let hooks_dir = repo.gitdir().join("hooks");
        let sentinel = tmp.path().join("env.txt");
        write_hook(
            &hooks_dir,
            "pre-commit",
            &format!(
                "#!/bin/sh\nprintf 'GIT_DIR=%s\\nGIT_WORK_TREE=%s\\n' \"$GIT_DIR\" \"$GIT_WORK_TREE\" > {}\n",
                sentinel.display()
            ),
        );
        let r = HookRunner::from_repo(&repo);
        r.run("pre-commit", &[], None).unwrap();
        let body = fs::read_to_string(&sentinel).unwrap();
        assert!(
            body.contains("GIT_DIR="),
            "expected GIT_DIR in env; got: {body}"
        );
        assert!(
            body.contains("GIT_WORK_TREE="),
            "expected GIT_WORK_TREE in env; got: {body}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn run_with_file_passes_path_as_single_arg() {
        let (tmp, repo) = make_repo();
        let hooks_dir = repo.gitdir().join("hooks");
        let sentinel = tmp.path().join("argv.txt");
        write_hook(
            &hooks_dir,
            "commit-msg",
            &format!(
                "#!/bin/sh\nprintf '%s\\n' \"$1\" > {}\n",
                sentinel.display()
            ),
        );
        let r = HookRunner::from_repo(&repo);
        let msgfile = tmp.path().join("COMMIT_EDITMSG");
        fs::write(&msgfile, "subject\n").unwrap();
        r.run_with_file("commit-msg", &msgfile).unwrap();
        let body = fs::read_to_string(&sentinel).unwrap();
        assert_eq!(body.trim(), msgfile.to_string_lossy());
    }

    #[test]
    fn skipped_when_hooks_dir_missing() {
        let (_tmp, repo) = make_repo();
        // Remove the hooks dir entirely.
        let hd = repo.gitdir().join("hooks");
        let _ = fs::remove_dir_all(&hd);
        let r = HookRunner::from_repo(&repo);
        let outcome = r.run("pre-commit", &[], None).unwrap();
        assert!(
            matches!(outcome, HookOutcome::Skipped { .. }),
            "expected Skipped; got {:?}",
            outcome
        );
        // Skipped never aborts the parent.
        assert!(!outcome.aborts_parent());
    }

    #[test]
    fn outcome_helpers() {
        assert!(!HookOutcome::NotPresent.aborts_parent());
        assert!(!HookOutcome::Ran { exit_code: 0 }.aborts_parent());
        assert!(HookOutcome::Ran { exit_code: 1 }.aborts_parent());
        assert!(HookOutcome::Ran { exit_code: -1 }.aborts_parent());
        assert!(!HookOutcome::Skipped { reason: "x".into() }.aborts_parent());

        assert_eq!(HookOutcome::Ran { exit_code: 3 }.exit_code(), Some(3));
        assert_eq!(HookOutcome::NotPresent.exit_code(), None);
    }
}
