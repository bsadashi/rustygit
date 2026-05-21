//! `rustygit commit` — record the index as a new commit on the current branch.
//!
//! Sequence (matching `git commit -m <msg>`):
//!   1. Read HEAD; if it's a symbolic ref to `refs/heads/<branch>`, that's the branch.
//!   2. Build a tree from the current index (re-uses `write-tree`).
//!   3. Resolve the current branch's tip oid (or none for the first commit).
//!   4. Create a commit object pointing at the tree, with one parent if any.
//!   5. Update the branch ref atomically with reflog.
//!
//! M3 limits: no `-a`/`--amend`/`--allow-empty`/`--allow-empty-message`/`-S`/
//! pre-commit hooks. M14+ wires sign/amend/hooks; M16 adds `-p`/interactive.

use std::io;

use clap::Args;

use crate::cli::commit_tree::{create_commit, create_commit_with_signer};
use crate::cli::write_tree::build_tree_from_index;
use crate::hash::ObjectId;
use crate::hooks::{self, HookRunner};
use crate::refs::{ExpectedOldValue, FullName, NewValue, RefTarget, ReflogMessage};
use crate::repo::Repository;
use crate::signing::GpgSigner;

#[derive(Debug, Args)]
pub struct CommitArgs {
    /// Commit message. When omitted, $EDITOR is opened on
    /// `.git/COMMIT_EDITMSG` (the editor flow).
    #[arg(short = 'm', value_name = "MESSAGE")]
    pub messages: Vec<String>,

    /// Read the commit message from FILE.
    #[arg(
        short = 'F',
        long = "file",
        value_name = "FILE",
        conflicts_with = "messages"
    )]
    pub file: Option<String>,

    /// Open `$EDITOR` even if `-m` is given (combine with the existing
    /// message as the seed).
    #[arg(short = 'e', long = "edit")]
    pub edit: bool,

    /// Allow committing with an empty index. Default: refuse.
    #[arg(long = "allow-empty")]
    pub allow_empty: bool,

    /// GPG-sign the commit. Optional key id; default uses `user.signingkey`.
    /// Mirrors `git commit -S [<keyid>]`.
    #[arg(short = 'S', long = "gpg-sign", value_name = "KEYID", num_args = 0..=1, default_missing_value = "")]
    pub gpg_sign: Option<String>,

    /// Suppress GPG signing even when `commit.gpgsign=true`. Mirrors
    /// `git commit --no-gpg-sign`.
    #[arg(long = "no-gpg-sign", conflicts_with = "gpg_sign")]
    pub no_gpg_sign: bool,

    /// Bypass the `pre-commit` and `commit-msg` hooks. `prepare-commit-msg`
    /// is still run (per githooks(5): "It is not suppressed by the
    /// `--no-verify` option."). Mirrors `git commit --no-verify`.
    #[arg(short = 'n', long = "no-verify")]
    pub no_verify: bool,
}

pub fn run(args: CommitArgs) -> io::Result<i32> {
    let repo = Repository::discover_from_cwd().map_err(io_err)?;
    let hook_runner = HookRunner::from_repo(&repo);

    // 0. pre-commit hook (unless --no-verify). Runs before we do any real
    //    work — the hook is supposed to inspect the staged changes via the
    //    index, which is exactly what's about to be committed.
    if !args.no_verify {
        let outcome = hook_runner.run("pre-commit", &[], None)?;
        if outcome.aborts_parent() {
            let code = outcome.exit_code().unwrap_or(1);
            hooks::print_abort("commit", "pre-commit", code);
            return Ok(1);
        }
    }

    // 1. Resolve HEAD.
    let head_name = FullName::new("HEAD").map_err(io_err)?;
    let head_ref = repo
        .refs()
        .read(&head_name)
        .map_err(io_err)?
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "HEAD does not exist"))?;
    let branch_name = match head_ref.target {
        RefTarget::Symbolic(b) => b,
        RefTarget::Direct(_) => {
            eprintln!("rustygit: commit: HEAD is detached; not yet supported (M6)");
            return Ok(128);
        }
    };

    // Current parent oid: branch tip if it exists, else none (root commit).
    let parent_oid = match repo.refs().read(&branch_name).map_err(io_err)? {
        Some(r) => match r.target {
            RefTarget::Direct(o) => Some(o),
            RefTarget::Symbolic(_) => {
                eprintln!(
                    "rustygit: commit: {} is itself a symbolic ref; refusing",
                    branch_name
                );
                return Ok(128);
            }
        },
        None => None,
    };

    // 2. Build tree from index.
    let tree_oid = match build_tree_from_index(&repo) {
        Ok(o) => o,
        Err(crate::cli::write_tree::WriteTreeError::EmptyIndex) if args.allow_empty => {
            // Synthesize an empty tree.
            let empty = crate::tree::Tree {
                entries: Vec::new(),
            };
            repo.odb().write(&empty.to_object()).map_err(io_err)?
        }
        Err(e) => {
            eprintln!("rustygit: commit: {e}");
            return Ok(1);
        }
    };

    // Refuse to commit if the parent's tree equals our tree (no changes).
    if let Some(parent) = parent_oid {
        if let Ok(parent_obj) = repo.odb().read(&parent) {
            if let Ok(parent_commit) =
                crate::commit::Commit::parse(&parent_obj.data, repo.hash_kind())
            {
                if parent_commit.tree == tree_oid && !args.allow_empty {
                    eprintln!("rustygit: nothing to commit (working tree clean)");
                    return Ok(1);
                }
            }
        }
    }

    // 3. Create the commit object. Possibly sign.
    let parents: Vec<String> = parent_oid.iter().map(|o| o.to_string()).collect();
    let parent_refs: Vec<&str> = parents.iter().map(String::as_str).collect();

    // Compose the initial message. Sources, in order of precedence:
    //   1. `-F <file>` (`--file`)
    //   2. `-m <msg>` flags joined with blank lines
    //   3. otherwise empty, and we WILL spawn $EDITOR below.
    let mut initial_message: String = if let Some(file) = &args.file {
        std::fs::read_to_string(file)?
    } else {
        args.messages.join("\n\n")
    };
    let need_editor = args.edit || (args.messages.is_empty() && args.file.is_none());

    let msg_path = repo.gitdir().join("COMMIT_EDITMSG");
    if need_editor {
        // Write seed (existing message + template comments) and spawn
        // $EDITOR. After it exits, strip lines beginning with '#' (git's
        // default cleanup) and re-read the message.
        let mut seed = initial_message.clone();
        if !seed.is_empty() && !seed.ends_with('\n') {
            seed.push('\n');
        }
        seed.push_str(
            "\n# Please enter the commit message for your changes.\n\
             # Lines starting with '#' will be ignored, and an empty message\n\
             # aborts the commit.\n",
        );
        std::fs::write(&msg_path, &seed)?;
        let editor = crate::cli::var::pick_editor(
            &crate::config::Config::from_repo_dir(repo.gitdir())
                .unwrap_or_else(|_| crate::config::Config::empty()),
        );
        let status = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("{editor} {}", msg_path.display()))
            .status();
        if status.is_err() || !status.as_ref().is_ok_and(|s| s.success()) {
            eprintln!("rustygit: commit: editor exited with non-zero status; aborting");
            return Ok(1);
        }
        let edited = std::fs::read_to_string(&msg_path)?;
        // Strip comment lines (git's "strip" cleanup; "verbatim" mode is
        // a separate config we don't support yet).
        let cleaned: String = edited
            .lines()
            .filter(|l| !l.trim_start().starts_with('#'))
            .collect::<Vec<_>>()
            .join("\n");
        let cleaned = crate::cli::stripspace::strip(&cleaned, false);
        if cleaned.trim().is_empty() {
            eprintln!("Aborting commit due to empty commit message.");
            return Ok(1);
        }
        initial_message = cleaned;
    }
    std::fs::write(&msg_path, &initial_message)?;

    // prepare-commit-msg: <msgfile> message
    // Per githooks(5), "the purpose of the hook is to edit the message file
    // in place". This runs regardless of --no-verify.
    {
        let p = msg_path.to_string_lossy();
        let outcome = hook_runner.run("prepare-commit-msg", &[&p, "message"], None)?;
        if outcome.aborts_parent() {
            let code = outcome.exit_code().unwrap_or(1);
            hooks::print_abort("commit", "prepare-commit-msg", code);
            return Ok(1);
        }
    }

    // commit-msg: <msgfile> (unless --no-verify).
    if !args.no_verify {
        let outcome = hook_runner.run_with_file("commit-msg", &msg_path)?;
        if outcome.aborts_parent() {
            let code = outcome.exit_code().unwrap_or(1);
            hooks::print_abort("commit", "commit-msg", code);
            return Ok(1);
        }
    }

    // Read back the (possibly mutated) message.
    let message = std::fs::read_to_string(&msg_path)?;

    // Decide whether to sign:
    //   * --no-gpg-sign always wins.
    //   * Explicit -S / --gpg-sign forces signing (with optional key override).
    //   * Otherwise, honor commit.gpgsign=true from config.
    let should_sign = if args.no_gpg_sign {
        false
    } else if args.gpg_sign.is_some() {
        true
    } else {
        let cfg = crate::config::Config::from_repo_dir(repo.gitdir()).map_err(io_err)?;
        cfg.get_bool("commit", "gpgsign").unwrap_or(false)
    };

    let commit_oid: ObjectId = if should_sign {
        let cfg = crate::config::Config::from_repo_dir(repo.gitdir()).map_err(io_err)?;
        let mut signer = GpgSigner::from_config(&cfg);
        // CLI `-S <keyid>` (non-empty) overrides the config key.
        if let Some(cli_key) = args.gpg_sign.as_deref() {
            if !cli_key.is_empty() {
                signer.key_id = Some(cli_key.to_string());
            }
        }
        create_commit_with_signer(
            &repo,
            &tree_oid.to_string(),
            &parent_refs,
            &message,
            Some(&signer),
        )?
    } else {
        create_commit(&repo, &tree_oid.to_string(), &parent_refs, &message)?
    };

    // 4. Update branch ref atomically (with reflog).
    let expected = match parent_oid {
        Some(o) => ExpectedOldValue::Direct(o),
        None => ExpectedOldValue::Missing,
    };
    let reflog = ReflogMessage::from(format!(
        "commit{}: {}",
        if parent_oid.is_none() {
            " (initial)"
        } else {
            ""
        },
        first_line(&message)
    ));
    let mut tx = repo.refs().transaction();
    tx.update(&branch_name, expected, NewValue::Direct(commit_oid), reflog)
        .map_err(io_err)?;
    tx.commit().map_err(io_err)?;

    // Match git's stdout summary.
    let short = commit_oid.short_hex(7);
    let branch_short = short_branch(&branch_name);
    println!(
        "[{branch_short} {label}{short}] {first}",
        label = if parent_oid.is_none() {
            "(root-commit) "
        } else {
            ""
        },
        first = first_line(&message),
    );

    // post-commit: best-effort. Exit code ignored per githooks(5).
    let outcome = hook_runner.run("post-commit", &[], None)?;
    if let crate::hooks::HookOutcome::Ran { exit_code } = outcome {
        if exit_code != 0 {
            hooks::print_warning("commit", "post-commit", exit_code);
        }
    }

    Ok(0)
}

fn first_line(s: &str) -> String {
    s.lines().next().unwrap_or("").to_string()
}

fn short_branch(full: &FullName) -> &str {
    full.as_str()
        .strip_prefix("refs/heads/")
        .unwrap_or(full.as_str())
}

fn io_err<E: std::fmt::Display>(e: E) -> io::Error {
    io::Error::other(format!("{e}"))
}
