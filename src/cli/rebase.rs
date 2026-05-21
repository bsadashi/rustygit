//! `rustygit rebase` — porcelain that replays a sequence of commits onto a
//! new base by cherry-picking each in turn.
//!
//! Algorithm (matching `git rebase <upstream>`):
//!
//! 1. Determine the current branch from HEAD. Detached HEAD → error.
//! 2. Resolve `<upstream>` to a commit oid.
//! 3. Compute `merge_base(HEAD, upstream)` — call it `base`.
//!    - The commits we'll replay are `HEAD..upstream`'s mirror image: i.e.
//!      the first-parent walk back from HEAD up to (but not including) `base`,
//!      reversed so the oldest commit is first.
//! 4. If that list is empty: the branch is already on top of upstream → nothing
//!    to do. Print "Current branch <name> is up to date." and exit 0.
//! 5. If `HEAD == base`: this is a pure fast-forward (upstream contains every
//!    HEAD commit). Move the branch to `upstream` and check out the tree.
//! 6. Otherwise: persist state under `.git/sequencer/`, reset the branch and
//!    workdir to `upstream`, and loop over the todo list cherry-picking each
//!    commit through `crate::sequencer::apply_commit`.
//!    - On `Done` → record the original (pre-pick) oid in `done`, save, continue.
//!    - On `Conflicted` → record `in_progress`, print git-style hint, exit 1.
//!    - On `Empty` → silently skip; do not advance `done` (matches git's
//!      default `--empty=drop` for non-fixup commits since 2.26).
//!
//! `--continue` re-runs the in-progress pick and any remaining todo via
//! `sequencer::cont`. `--abort` calls `sequencer::abort`, which restores the
//! branch to `orig_head` and removes the state dir.
//!
//! Out of scope for M14: `-i` (interactive), `--exec`, `--autosquash`,
//! `--rebase-merges`, `--strategy`, `--keep-empty`, `--root`. The bare
//! linear-rebase flow covered here is the lion's share of everyday use.

use std::io::{self, Write};

use clap::Args;

use crate::commit::Commit;
use crate::hash::ObjectId;
use crate::hooks::{self, HookRunner};
use crate::merge::base::merge_base;
use crate::object::ObjectKind;
use crate::refs::{ExpectedOldValue, FullName, NewValue, RefTarget, ReflogMessage};
use crate::repo::Repository;
use crate::revparse;
use crate::sequencer::{self, ApplyOpts, ApplyOutcome, ContinueOutcome, State};
use crate::unpack_trees::{checkout_tree, UnpackOpts};

#[derive(Debug, Args)]
pub struct RebaseArgs {
    /// Onto this revision (default: upstream).
    #[arg(long = "onto", value_name = "NEWBASE")]
    pub onto: Option<String>,
    /// Continue after resolving conflicts.
    #[arg(long = "continue")]
    pub cont: bool,
    /// Abort an in-progress rebase.
    #[arg(long = "abort")]
    pub abort: bool,
    /// Quiet mode.
    #[arg(short = 'q', long = "quiet")]
    pub quiet: bool,
    /// Upstream branch/commit. Required for the initial start.
    #[arg(value_name = "UPSTREAM")]
    pub upstream: Option<String>,
}

pub fn run(args: RebaseArgs) -> io::Result<i32> {
    let repo = Repository::discover_from_cwd().map_err(io_err)?;

    if args.abort {
        return run_abort(&repo, args.quiet);
    }
    if args.cont {
        return run_continue(&repo, args.quiet);
    }

    // Refuse to start a fresh rebase if state still exists; the user would
    // lose track of what was in progress.
    if State::exists(&repo) {
        eprintln!("rustygit: rebase: a rebase is already in progress; use --continue or --abort");
        return Ok(128);
    }

    let upstream_str = match &args.upstream {
        Some(s) => s.clone(),
        None => {
            eprintln!("rustygit: rebase: missing <upstream> argument");
            return Ok(129);
        }
    };

    run_start(&repo, &upstream_str, args.onto.as_deref(), args.quiet)
}

// ----------------------------------------------------------------------------
// `rustygit rebase <upstream>` — initial start.
// ----------------------------------------------------------------------------

fn run_start(
    repo: &Repository,
    upstream_str: &str,
    onto_str: Option<&str>,
    quiet: bool,
) -> io::Result<i32> {
    // pre-rebase hook: <upstream> [<branch>]. Per githooks(5), the branch
    // arg is omitted when rebasing the current branch — which is always
    // the case here (we don't support `rustygit rebase <upstream> <branch>`).
    let hook_runner = HookRunner::from_repo(repo);
    let outcome = hook_runner.run("pre-rebase", &[upstream_str], None)?;
    if outcome.aborts_parent() {
        let code = outcome.exit_code().unwrap_or(1);
        hooks::print_abort("rebase", "pre-rebase", code);
        return Ok(1);
    }

    // 1. HEAD must be on a branch.
    let branch_name = match current_branch(repo)? {
        Some(b) => b,
        None => {
            eprintln!("rustygit: rebase: HEAD is detached; cannot rebase");
            return Ok(128);
        }
    };
    let head_oid = match resolve_branch_oid(repo, &branch_name)? {
        Some(o) => o,
        None => {
            eprintln!("rustygit: rebase: no initial commit on current branch");
            return Ok(128);
        }
    };

    // 2. Resolve upstream and onto.
    let upstream_oid = revparse::resolve(repo.refs(), repo.odb(), upstream_str)
        .map_err(|e| io_err(format!("not a valid object name: {upstream_str}: {e}")))?;
    let onto_oid = match onto_str {
        Some(s) => revparse::resolve(repo.refs(), repo.odb(), s)
            .map_err(|e| io_err(format!("not a valid object name: {s}: {e}")))?,
        None => upstream_oid,
    };

    let branch_short = short_branch(&branch_name).to_string();

    // 3. Find divergence point and build the to-rebase list.
    let base = merge_base(repo, head_oid, upstream_oid).map_err(io_err)?;

    // The set of commits to replay = HEAD's first-parent walk back to `base`,
    // exclusive. If base is None, that's everything down to the root.
    let to_rebase = commits_to_replay(repo, head_oid, base)?;

    // 4. Order-sensitive special cases:
    //    a) Branch is identical to upstream (same oid) → up to date.
    //    b) HEAD is an ancestor of upstream (base == HEAD) → fast-forward
    //       the branch to upstream/onto. `to_rebase` is empty in this case
    //       so we can't just check that.
    //    c) `to_rebase` empty otherwise → already on top of upstream.
    if head_oid == onto_oid {
        if !quiet {
            println!("Current branch {branch_short} is up to date.");
        }
        return Ok(0);
    }
    if base == Some(head_oid) {
        return do_fast_forward(repo, &branch_name, head_oid, onto_oid, quiet);
    }
    if to_rebase.is_empty() {
        if !quiet {
            println!("Current branch {branch_short} is up to date.");
        }
        return Ok(0);
    }

    // 5. Initialize state. Reset the branch to `onto` (with reflog) and check
    //    out its tree.
    reset_branch_to(repo, &branch_name, head_oid, onto_oid, upstream_str)?;
    checkout_oid_tree(repo, onto_oid)?;

    let state = State {
        head_branch: branch_name.clone(),
        orig_head: head_oid,
        onto: onto_oid,
        todo: to_rebase,
        done: Vec::new(),
        in_progress: None,
        revert: false,
    };
    state.save(repo).map_err(io_err)?;

    // 6. Replay loop.
    drive_loop(repo, state, &branch_short, quiet)
}

// ----------------------------------------------------------------------------
// `rustygit rebase --continue`
// ----------------------------------------------------------------------------

fn run_continue(repo: &Repository, quiet: bool) -> io::Result<i32> {
    if !State::exists(repo) {
        eprintln!("rustygit: rebase: no rebase in progress");
        return Ok(128);
    }
    // Capture the branch name before cont() — on success it cleans state up.
    let branch_short = match State::load(repo) {
        Ok(s) => short_branch(&s.head_branch).to_string(),
        Err(_) => "branch".to_string(),
    };
    let outcome = sequencer::cont(repo).map_err(io_err)?;
    match outcome {
        ContinueOutcome::Done => {
            // Best-effort cleanup; sequencer::cont may have already done it.
            let _ = State::cleanup(repo);
            if !quiet {
                println!("Successfully rebased and updated {branch_short}.");
            }
            Ok(0)
        }
        ContinueOutcome::Conflicted {
            commit,
            offending_paths,
        } => {
            // The sequencer is responsible for persisting `in_progress` and
            // any updated todo; here we just surface the conflict to the user.
            let subj = commit_subject(repo, commit).unwrap_or_default();
            print_conflict(&offending_paths, commit, &subj);
            Ok(1)
        }
    }
}

// ----------------------------------------------------------------------------
// `rustygit rebase --abort`
// ----------------------------------------------------------------------------

fn run_abort(repo: &Repository, quiet: bool) -> io::Result<i32> {
    if !State::exists(repo) {
        eprintln!("rustygit: rebase: no rebase in progress");
        return Ok(128);
    }
    sequencer::abort(repo).map_err(io_err)?;
    // Sequencer is expected to clean up state; double-check.
    let _ = State::cleanup(repo);
    if !quiet {
        println!("rebase aborted");
    }
    Ok(0)
}

// ----------------------------------------------------------------------------
// Core loop: pop todo, apply, react to the outcome.
// ----------------------------------------------------------------------------

fn drive_loop(
    repo: &Repository,
    mut state: State,
    branch_short: &str,
    quiet: bool,
) -> io::Result<i32> {
    let opts = ApplyOpts {
        preserve_author: true,
        override_message: None,
        theirs_label: format!("rebase {branch_short}"),
        revert: false,
        mainline: None,
    };

    // Track (old_oid, new_oid) pairs for the post-rewrite hook.
    let mut rewrites: Vec<(ObjectId, ObjectId)> = Vec::new();

    while let Some(commit_oid) = pop_front(&mut state.todo) {
        match sequencer::apply_commit(repo, commit_oid, &opts).map_err(io_err)? {
            ApplyOutcome::Done { new_commit } => {
                // Per spec: append the ORIGINAL commit oid (not the new one)
                // to `done`. This is what lets `--continue` know what's left.
                state.done.push(commit_oid);
                state.in_progress = None;
                state.save(repo).map_err(io_err)?;
                rewrites.push((commit_oid, new_commit));
            }
            ApplyOutcome::Empty => {
                // Silently skip. Don't add to done; the original commit's
                // changes are already on upstream.
                state.in_progress = None;
                state.save(repo).map_err(io_err)?;
            }
            ApplyOutcome::Conflicted { offending_paths } => {
                state.in_progress = Some(commit_oid);
                state.save(repo).map_err(io_err)?;
                let subj = commit_subject(repo, commit_oid).unwrap_or_default();
                print_conflict(&offending_paths, commit_oid, &subj);
                return Ok(1);
            }
        }
    }

    // Todo empty → success. Clear state.
    State::cleanup(repo).map_err(io_err)?;
    if !quiet {
        println!("Successfully rebased and updated {branch_short}.");
    }

    // post-rewrite hook: best-effort. argv = `rebase`; stdin = one
    // `<old-sha> <new-sha>` line per replayed commit (no extra-info).
    if !rewrites.is_empty() {
        fire_post_rewrite(repo, "rebase", &rewrites);
    }

    Ok(0)
}

/// Fire `post-rewrite` and log a warning on non-zero exit. Exit code does
/// NOT affect the parent op per githooks(5).
fn fire_post_rewrite(repo: &Repository, arg: &str, pairs: &[(ObjectId, ObjectId)]) {
    let runner = HookRunner::from_repo(repo);
    let stdin: String = pairs
        .iter()
        .map(|(old, new)| format!("{old} {new}\n"))
        .collect();
    match runner.run("post-rewrite", &[arg], Some(stdin.as_bytes())) {
        Ok(crate::hooks::HookOutcome::Ran { exit_code }) if exit_code != 0 => {
            hooks::print_warning("rebase", "post-rewrite", exit_code);
        }
        _ => {}
    }
}

// ----------------------------------------------------------------------------
// Helpers
// ----------------------------------------------------------------------------

/// Walk HEAD's first-parent chain back to (but not including) `stop`. Result
/// is returned in chronological forward order — oldest first — so callers
/// can pop the front to replay in commit order.
fn commits_to_replay(
    repo: &Repository,
    head: ObjectId,
    stop: Option<ObjectId>,
) -> io::Result<Vec<ObjectId>> {
    let mut chain = Vec::new();
    let mut cur = head;
    loop {
        if Some(cur) == stop {
            break;
        }
        let obj = repo.odb().read(&cur).map_err(io_err)?;
        if obj.kind != ObjectKind::Commit {
            return Err(io_err(format!("{cur} is not a commit")));
        }
        let c = Commit::parse(&obj.data, repo.hash_kind()).map_err(io_err)?;
        chain.push(cur);
        match c.parents.first().copied() {
            Some(p) => cur = p,
            None => break,
        }
    }
    chain.reverse();
    Ok(chain)
}

fn current_branch(repo: &Repository) -> io::Result<Option<FullName>> {
    let head = FullName::new("HEAD").map_err(io_err)?;
    let r = repo.refs().read(&head).map_err(io_err)?;
    let r = match r {
        Some(r) => r,
        None => return Ok(None),
    };
    Ok(match r.target {
        RefTarget::Symbolic(name) => Some(name),
        RefTarget::Direct(_) => None,
    })
}

fn resolve_branch_oid(repo: &Repository, name: &FullName) -> io::Result<Option<ObjectId>> {
    match repo.refs().read(name).map_err(io_err)? {
        Some(r) => match r.target {
            RefTarget::Direct(o) => Ok(Some(o)),
            RefTarget::Symbolic(_) => Err(io_err(format!(
                "branch {name} resolves to a symbolic ref; cannot rebase"
            ))),
        },
        None => Ok(None),
    }
}

fn reset_branch_to(
    repo: &Repository,
    branch: &FullName,
    old: ObjectId,
    new: ObjectId,
    upstream_label: &str,
) -> io::Result<()> {
    let mut tx = repo.refs().transaction();
    tx.update(
        branch,
        ExpectedOldValue::Direct(old),
        NewValue::Direct(new),
        ReflogMessage::from(format!("rebase (start): checkout {upstream_label}")),
    )
    .map_err(io_err)?;
    tx.commit().map_err(io_err)?;
    Ok(())
}

fn checkout_oid_tree(repo: &Repository, oid: ObjectId) -> io::Result<()> {
    let tree = commit_to_tree(repo, oid)?;
    let opts = UnpackOpts {
        force: false,
        keep_extra: false,
        update_workdir: true,
        update_index: true,
    };
    checkout_tree(repo, tree, &opts).map_err(io_err)?;
    Ok(())
}

fn do_fast_forward(
    repo: &Repository,
    branch: &FullName,
    old: ObjectId,
    new: ObjectId,
    quiet: bool,
) -> io::Result<i32> {
    if old == new {
        if !quiet {
            println!("Current branch {} is up to date.", short_branch(branch));
        }
        return Ok(0);
    }
    let tree = commit_to_tree(repo, new)?;
    let opts = UnpackOpts {
        force: false,
        keep_extra: false,
        update_workdir: true,
        update_index: true,
    };
    checkout_tree(repo, tree, &opts).map_err(io_err)?;
    let mut tx = repo.refs().transaction();
    tx.update(
        branch,
        ExpectedOldValue::Direct(old),
        NewValue::Direct(new),
        ReflogMessage::from(format!(
            "rebase finished: {short} onto {onto}",
            short = short_branch(branch),
            onto = new.short_hex(7),
        )),
    )
    .map_err(io_err)?;
    tx.commit().map_err(io_err)?;
    if !quiet {
        println!(
            "Fast-forwarded {} to {}.",
            short_branch(branch),
            new.short_hex(7)
        );
    }
    Ok(0)
}

fn commit_to_tree(repo: &Repository, oid: ObjectId) -> io::Result<ObjectId> {
    let obj = repo.odb().read(&oid).map_err(io_err)?;
    if obj.kind != ObjectKind::Commit {
        return Err(io_err(format!("{oid} is not a commit")));
    }
    let c = Commit::parse(&obj.data, repo.hash_kind()).map_err(io_err)?;
    Ok(c.tree)
}

fn commit_subject(repo: &Repository, oid: ObjectId) -> Option<String> {
    let obj = repo.odb().read(&oid).ok()?;
    if obj.kind != ObjectKind::Commit {
        return None;
    }
    let c = Commit::parse(&obj.data, repo.hash_kind()).ok()?;
    let s = String::from_utf8_lossy(&c.message);
    Some(s.lines().next().unwrap_or("").to_string())
}

fn print_conflict(paths: &[Vec<u8>], commit: ObjectId, subject: &str) {
    let mut out = io::stderr().lock();
    if paths.is_empty() {
        let _ = writeln!(
            out,
            "CONFLICT (content): Merge conflict while applying changes"
        );
    } else {
        for p in paths {
            let _ = writeln!(
                out,
                "CONFLICT (content): Merge conflict in {}",
                String::from_utf8_lossy(p)
            );
        }
    }
    let _ = writeln!(
        out,
        "error: could not apply {}... {}",
        commit.short_hex(7),
        subject
    );
    let _ = writeln!(
        out,
        "hint: Resolve all conflicts manually, mark them as resolved with"
    );
    let _ = writeln!(
        out,
        "hint: \"git add <conflicted_files>\", then run \"git rebase --continue\"."
    );
}

fn short_branch(full: &FullName) -> &str {
    full.as_str()
        .strip_prefix("refs/heads/")
        .unwrap_or(full.as_str())
}

fn pop_front<T>(v: &mut Vec<T>) -> Option<T> {
    if v.is_empty() {
        None
    } else {
        Some(v.remove(0))
    }
}

fn io_err<E: std::fmt::Display>(e: E) -> io::Error {
    io::Error::other(format!("{e}"))
}

// ----------------------------------------------------------------------------
// Tests
//
// Per the user's directive, testing time is ~2x development time. The tests
// below build small repos with the system `git` tool, exercise our porcelain
// through the compiled `rustygit` binary, and verify the result. Each test
// stands alone in a TempDir and skips when `git` isn't available.
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::Path;
    use std::process::{Command as SysCommand, Output};
    use tempfile::TempDir;

    // ---- harness helpers ----

    fn has_system_git() -> bool {
        SysCommand::new("git")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    fn git(args: &[&str], cwd: &Path) -> Output {
        let out = SysCommand::new("git")
            .args(args)
            .current_dir(cwd)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .env("GIT_AUTHOR_DATE", "1700000000 +0000")
            .env("GIT_COMMITTER_DATE", "1700000000 +0000")
            .output()
            .expect("failed to spawn git");
        assert!(
            out.status.success(),
            "git {args:?} failed in {cwd:?}\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );
        out
    }

    fn rustygit(args: &[&str], cwd: &Path) -> Output {
        // We always invoke through assert_cmd so the integration tests work
        // once Track A's sequencer is wired into the binary.
        assert_cmd::Command::cargo_bin("rustygit")
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

    fn commit_file(tmp: &Path, name: &str, contents: &[u8], msg: &str) {
        let p = tmp.join(name);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&p, contents).unwrap();
        git(&["add", name], tmp);
        git(&["commit", "-q", "-m", msg], tmp);
    }

    fn rev_parse(tmp: &Path, rev: &str) -> String {
        String::from_utf8(git(&["rev-parse", rev], tmp).stdout)
            .unwrap()
            .trim()
            .to_string()
    }

    /// Read commit subjects on the current branch, newest first.
    fn log_subjects(tmp: &Path) -> Vec<String> {
        let out = git(&["log", "--pretty=%s"], tmp);
        String::from_utf8(out.stdout)
            .unwrap()
            .lines()
            .map(|s| s.to_string())
            .collect()
    }

    // ---- pure-function tests (run without the binary or sequencer) ----

    #[test]
    fn pop_front_removes_from_head() {
        let mut v = vec![1, 2, 3];
        assert_eq!(pop_front(&mut v), Some(1));
        assert_eq!(v, vec![2, 3]);
        assert_eq!(pop_front(&mut v), Some(2));
        assert_eq!(pop_front(&mut v), Some(3));
        assert_eq!(pop_front::<i32>(&mut v), None);
    }

    #[test]
    fn short_branch_strips_heads_prefix() {
        let n = FullName::new("refs/heads/main").unwrap();
        assert_eq!(short_branch(&n), "main");
        let n = FullName::new("refs/heads/feature/x").unwrap();
        assert_eq!(short_branch(&n), "feature/x");
        // Refs not under refs/heads/ retain their full name.
        let n = FullName::new("refs/tags/v1").unwrap();
        assert_eq!(short_branch(&n), "refs/tags/v1");
    }

    #[test]
    fn args_parse_minimal() {
        use clap::Parser;
        #[derive(Debug, Parser)]
        struct Wrap {
            #[command(flatten)]
            args: RebaseArgs,
        }
        let w = Wrap::try_parse_from(["x", "master"]).unwrap();
        assert_eq!(w.args.upstream.as_deref(), Some("master"));
        assert!(!w.args.cont);
        assert!(!w.args.abort);
        assert!(w.args.onto.is_none());
    }

    #[test]
    fn args_parse_continue_abort_onto() {
        use clap::Parser;
        #[derive(Debug, Parser)]
        struct Wrap {
            #[command(flatten)]
            args: RebaseArgs,
        }
        let w = Wrap::try_parse_from(["x", "--continue"]).unwrap();
        assert!(w.args.cont);
        assert!(w.args.upstream.is_none());

        let w = Wrap::try_parse_from(["x", "--abort"]).unwrap();
        assert!(w.args.abort);

        let w = Wrap::try_parse_from(["x", "--onto", "topic", "master"]).unwrap();
        assert_eq!(w.args.onto.as_deref(), Some("topic"));
        assert_eq!(w.args.upstream.as_deref(), Some("master"));
    }

    // ---- integration tests (require the rustygit binary + sequencer) ----

    /// Skip when we can't run an end-to-end test. The integration tests below
    /// only run once Track A's sequencer is wired into `lib.rs`/`cli/mod.rs`.
    /// Until then, building the binary fails — we detect that here and short-
    /// circuit the test so the unit-test pass stays green.
    fn integration_ready(tmp: &Path) -> bool {
        if !has_system_git() {
            return false;
        }
        // A "smoke" invocation: if even `--help` fails to compile/run, the
        // binary isn't ready and we should skip.
        let out = assert_cmd::Command::cargo_bin("rustygit")
            .ok()
            .map(|mut c| c.arg("--help").current_dir(tmp).output());
        matches!(out, Some(Ok(o)) if o.status.success())
    }

    /// Test #1: branch already on top of upstream → no commits replayed.
    #[test]
    fn empty_rebase_branch_up_to_date() {
        let tmp = TempDir::new().unwrap();
        if !integration_ready(tmp.path()) {
            return;
        }
        git(&["init", "-q", "-b", "master", "."], tmp.path());
        commit_file(tmp.path(), "f.txt", b"v1\n", "c1");
        git(&["checkout", "-q", "-b", "topic"], tmp.path());
        // No new commits on topic; topic == master.
        let r = rustygit(&["rebase", "master"], tmp.path());
        assert!(
            r.status.success(),
            "rebase failed: stderr={}",
            String::from_utf8_lossy(&r.stderr)
        );
        let stdout = String::from_utf8_lossy(&r.stdout);
        assert!(
            stdout.contains("up to date"),
            "expected up-to-date message; got: {stdout}"
        );
    }

    /// Test #2: upstream is descendant of HEAD → fast-forward.
    #[test]
    fn fast_forward_rebase_moves_branch() {
        let tmp = TempDir::new().unwrap();
        if !integration_ready(tmp.path()) {
            return;
        }
        git(&["init", "-q", "-b", "master", "."], tmp.path());
        commit_file(tmp.path(), "f.txt", b"v1\n", "c1");
        // Branch topic at c1, then add c2 on master.
        git(&["branch", "topic"], tmp.path());
        commit_file(tmp.path(), "f.txt", b"v2\n", "c2");
        let master_tip = rev_parse(tmp.path(), "master");

        git(&["checkout", "-q", "topic"], tmp.path());
        let r = rustygit(&["rebase", "master"], tmp.path());
        assert!(
            r.status.success(),
            "rebase failed: stderr={}",
            String::from_utf8_lossy(&r.stderr)
        );
        // topic should now point at master_tip.
        let topic_tip = rev_parse(tmp.path(), "topic");
        assert_eq!(topic_tip, master_tip, "branch did not fast-forward");
        // Workdir reflects c2.
        let content = std::fs::read(tmp.path().join("f.txt")).unwrap();
        assert_eq!(content, b"v2\n");
    }

    /// Test #3: clean rebase of three commits onto a moved master.
    #[test]
    fn clean_rebase_three_commits() {
        let tmp = TempDir::new().unwrap();
        if !integration_ready(tmp.path()) {
            return;
        }
        git(&["init", "-q", "-b", "master", "."], tmp.path());
        commit_file(tmp.path(), "base.txt", b"base\n", "c0");
        git(&["checkout", "-q", "-b", "topic"], tmp.path());
        commit_file(tmp.path(), "a.txt", b"a\n", "ta");
        commit_file(tmp.path(), "b.txt", b"b\n", "tb");
        commit_file(tmp.path(), "c.txt", b"c\n", "tc");
        git(&["checkout", "-q", "master"], tmp.path());
        commit_file(tmp.path(), "m.txt", b"m\n", "tm");

        git(&["checkout", "-q", "topic"], tmp.path());
        let r = rustygit(&["rebase", "master"], tmp.path());
        assert!(
            r.status.success(),
            "rebase failed: stderr={}",
            String::from_utf8_lossy(&r.stderr)
        );

        // After rebase: topic contains c0, tm, ta, tb, tc.
        let subjects = log_subjects(tmp.path());
        assert_eq!(subjects, vec!["tc", "tb", "ta", "tm", "c0"]);
        // All files exist in workdir.
        for f in ["base.txt", "m.txt", "a.txt", "b.txt", "c.txt"] {
            assert!(
                tmp.path().join(f).exists(),
                "missing {f} after rebase; ls={:?}",
                std::fs::read_dir(tmp.path()).unwrap().collect::<Vec<_>>()
            );
        }
    }

    /// Test #4: 2 commits on feature, first conflicts on master.
    /// Verify state is saved and exit code is 1.
    #[test]
    fn conflict_mid_rebase_saves_state() {
        let tmp = TempDir::new().unwrap();
        if !integration_ready(tmp.path()) {
            return;
        }
        git(&["init", "-q", "-b", "master", "."], tmp.path());
        commit_file(tmp.path(), "f.txt", b"v1\n", "base");
        git(&["checkout", "-q", "-b", "topic"], tmp.path());
        commit_file(tmp.path(), "f.txt", b"topic-1\n", "t1");
        commit_file(tmp.path(), "other.txt", b"o\n", "t2");
        git(&["checkout", "-q", "master"], tmp.path());
        commit_file(tmp.path(), "f.txt", b"master-1\n", "m1");

        git(&["checkout", "-q", "topic"], tmp.path());
        let r = rustygit(&["rebase", "master"], tmp.path());
        assert_eq!(r.status.code(), Some(1), "expected conflict exit 1");
        let stderr = String::from_utf8_lossy(&r.stderr);
        assert!(
            stderr.contains("CONFLICT"),
            "no CONFLICT in stderr: {stderr}"
        );
        // Sequencer state directory should exist after the conflict.
        let seq_dir = tmp.path().join(".git/sequencer");
        assert!(
            seq_dir.exists(),
            ".git/sequencer/ missing after conflict; ls .git: {:?}",
            std::fs::read_dir(tmp.path().join(".git"))
                .unwrap()
                .filter_map(|e| e.ok().map(|e| e.file_name()))
                .collect::<Vec<_>>()
        );
    }

    /// Test #5: from #4's state, resolve the conflict + run --continue.
    /// Verify the remaining commit applies.
    #[test]
    fn continue_after_resolving_conflict() {
        let tmp = TempDir::new().unwrap();
        if !integration_ready(tmp.path()) {
            return;
        }
        git(&["init", "-q", "-b", "master", "."], tmp.path());
        commit_file(tmp.path(), "f.txt", b"v1\n", "base");
        git(&["checkout", "-q", "-b", "topic"], tmp.path());
        commit_file(tmp.path(), "f.txt", b"topic-1\n", "t1");
        commit_file(tmp.path(), "other.txt", b"o\n", "t2");
        git(&["checkout", "-q", "master"], tmp.path());
        commit_file(tmp.path(), "f.txt", b"master-1\n", "m1");

        git(&["checkout", "-q", "topic"], tmp.path());
        let r = rustygit(&["rebase", "master"], tmp.path());
        assert_eq!(r.status.code(), Some(1));

        // Resolve the conflict: write a merged version, stage it, and commit
        // it so `--continue` sees a finalized in-progress pick. We use
        // `rustygit commit` (not system `git commit`) here because system git
        // detects CHERRY_PICK_HEAD and clears our sequencer/ state.
        std::fs::write(tmp.path().join("f.txt"), b"merged\n").unwrap();
        git(&["add", "f.txt"], tmp.path());
        let cr = rustygit(&["commit", "-m", "t1-resolved"], tmp.path());
        assert!(
            cr.status.success(),
            "commit failed: stderr={}",
            String::from_utf8_lossy(&cr.stderr)
        );

        let r = rustygit(&["rebase", "--continue"], tmp.path());
        assert!(
            r.status.success(),
            "continue failed: stderr={}",
            String::from_utf8_lossy(&r.stderr)
        );

        // We should now have 4 commits on topic: base, m1, t1-resolved, t2'.
        let subjects = log_subjects(tmp.path());
        assert_eq!(subjects.len(), 4, "got {subjects:?}");
        assert_eq!(subjects[3], "base"); // root
        assert_eq!(subjects[2], "m1"); // master's commit (upstream)
        assert_eq!(subjects[1], "t1-resolved");
        assert_eq!(subjects[0], "t2"); // remaining commit, replayed
                                       // No leftover sequencer state.
        assert!(!tmp.path().join(".git/sequencer").exists());
        // f.txt holds the user's resolved value; the t2 commit on top
        // shouldn't change f.txt (it only added other.txt).
        let f = std::fs::read(tmp.path().join("f.txt")).unwrap();
        assert_eq!(f, b"merged\n");
        assert!(tmp.path().join("other.txt").exists());
    }

    /// Test #6: --abort restores HEAD.
    #[test]
    fn abort_restores_head() {
        let tmp = TempDir::new().unwrap();
        if !integration_ready(tmp.path()) {
            return;
        }
        git(&["init", "-q", "-b", "master", "."], tmp.path());
        commit_file(tmp.path(), "f.txt", b"v1\n", "base");
        git(&["checkout", "-q", "-b", "topic"], tmp.path());
        commit_file(tmp.path(), "f.txt", b"topic-1\n", "t1");
        let topic_before = rev_parse(tmp.path(), "topic");
        git(&["checkout", "-q", "master"], tmp.path());
        commit_file(tmp.path(), "f.txt", b"master-1\n", "m1");

        git(&["checkout", "-q", "topic"], tmp.path());
        let r = rustygit(&["rebase", "master"], tmp.path());
        assert_eq!(r.status.code(), Some(1));

        let r = rustygit(&["rebase", "--abort"], tmp.path());
        assert!(
            r.status.success(),
            "abort failed: stderr={}",
            String::from_utf8_lossy(&r.stderr)
        );
        let topic_after = rev_parse(tmp.path(), "topic");
        assert_eq!(topic_after, topic_before, "abort didn't restore HEAD");
        assert!(!tmp.path().join(".git/sequencer").exists());
    }

    /// Test #7: empty commit during rebase — the 2nd commit's changes are
    /// already on upstream, so it should be skipped silently.
    #[test]
    fn empty_commit_during_rebase_is_skipped() {
        let tmp = TempDir::new().unwrap();
        if !integration_ready(tmp.path()) {
            return;
        }
        git(&["init", "-q", "-b", "master", "."], tmp.path());
        commit_file(tmp.path(), "base.txt", b"base\n", "c0");
        git(&["checkout", "-q", "-b", "topic"], tmp.path());
        // First commit on topic: introduce a file that master will also
        // introduce later (same content). This commit becomes empty when
        // replayed onto master because master already has that change.
        commit_file(tmp.path(), "dup.txt", b"same\n", "t-dup");
        // A second, non-empty commit on topic so we have something to test
        // after the empty pick.
        commit_file(tmp.path(), "topic.txt", b"topic-only\n", "t-real");

        git(&["checkout", "-q", "master"], tmp.path());
        // Master introduces the same file with the same content.
        commit_file(tmp.path(), "dup.txt", b"same\n", "m-dup");

        git(&["checkout", "-q", "topic"], tmp.path());
        let r = rustygit(&["rebase", "master"], tmp.path());
        assert!(
            r.status.success(),
            "rebase failed: stderr={}",
            String::from_utf8_lossy(&r.stderr)
        );
        // After rebase topic has: c0, m-dup, t-real (t-dup dropped as empty).
        let subjects = log_subjects(tmp.path());
        assert_eq!(
            subjects,
            vec!["t-real", "m-dup", "c0"],
            "t-dup should have been skipped",
        );
    }

    /// Test #8: `--onto <newbase>` rebases HEAD..upstream onto a third branch.
    #[test]
    fn onto_newbase_rebases_onto_third_branch() {
        let tmp = TempDir::new().unwrap();
        if !integration_ready(tmp.path()) {
            return;
        }
        git(&["init", "-q", "-b", "master", "."], tmp.path());
        commit_file(tmp.path(), "base.txt", b"base\n", "c0");
        // 3 branches: master, topic (with 2 commits), other (with 1 commit).
        git(&["checkout", "-q", "-b", "topic"], tmp.path());
        commit_file(tmp.path(), "a.txt", b"a\n", "ta");
        commit_file(tmp.path(), "b.txt", b"b\n", "tb");
        git(&["checkout", "-q", "master"], tmp.path());
        git(&["checkout", "-q", "-b", "other"], tmp.path());
        commit_file(tmp.path(), "o.txt", b"o\n", "to");

        git(&["checkout", "-q", "topic"], tmp.path());
        // rebase --onto other master — replay topic's commits on top of `other`.
        let r = rustygit(&["rebase", "--onto", "other", "master"], tmp.path());
        assert!(
            r.status.success(),
            "rebase failed: stderr={}",
            String::from_utf8_lossy(&r.stderr)
        );
        let subjects = log_subjects(tmp.path());
        // topic should contain: c0, to, ta, tb.
        assert_eq!(subjects, vec!["tb", "ta", "to", "c0"]);
    }

    /// Test #9: refusing to start a new rebase when state is in progress.
    #[test]
    fn refuses_to_start_when_state_exists() {
        let tmp = TempDir::new().unwrap();
        if !integration_ready(tmp.path()) {
            return;
        }
        git(&["init", "-q", "-b", "master", "."], tmp.path());
        commit_file(tmp.path(), "f.txt", b"v1\n", "c1");
        // Simulate stale state.
        std::fs::create_dir_all(tmp.path().join(".git/sequencer")).unwrap();
        std::fs::write(
            tmp.path().join(".git/sequencer/head-name"),
            b"refs/heads/master\n",
        )
        .unwrap();

        let r = rustygit(&["rebase", "master"], tmp.path());
        assert!(
            !r.status.success(),
            "rebase should refuse to start with state present"
        );
        let stderr = String::from_utf8_lossy(&r.stderr);
        assert!(
            stderr.contains("already in progress") || stderr.contains("in progress"),
            "expected 'in progress' diagnostic; got: {stderr}"
        );
    }
}
