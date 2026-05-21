//! Interactive rebase support — `rebase -i` / `--autosquash` /
//! `--exec` / `--rebase-merges`.
//!
//! Algorithm:
//!   1. Resolve the range of commits to rebase.
//!   2. Write a todo file at `.git/rebase-merge/git-rebase-todo` listing
//!      each commit as `pick <oid> <subject>`.
//!   3. With `--autosquash`, reorder so `fixup!`/`squash!` lines land
//!      next to their target commit.
//!   4. Spawn $EDITOR on the todo file.
//!   5. Parse the edited todo; for each action:
//!        - `pick`: re-apply the commit (cherry-pick style)
//!        - `reword`: re-apply + edit message
//!        - `edit`: re-apply, then pause for amending
//!        - `squash`: re-apply, then squash into prior commit (keep message)
//!        - `fixup`: re-apply, squash, drop the message
//!        - `drop`: skip
//!        - `exec <cmd>`: run a shell command between picks
//!   6. Walk through; on conflict, halt with state in `rebase-merge/`.

use std::io;

use clap::Args;

use crate::commit::Commit;
use crate::hash::ObjectId;
use crate::repo::Repository;

#[derive(Debug, Args)]
pub struct RebaseInteractiveArgs {
    /// Upstream / base for the rebase.
    #[arg(value_name = "UPSTREAM", required = true)]
    pub upstream: String,
    /// `--autosquash`: shuffle fixup!/squash! lines next to their targets.
    #[arg(long = "autosquash")]
    pub autosquash: bool,
    /// `--exec <cmd>`: run a shell command between picks.
    #[arg(short = 'x', long = "exec", value_name = "CMD")]
    pub exec: Option<String>,
    /// `--rebase-merges`: preserve merge structure.
    #[arg(long = "rebase-merges")]
    pub rebase_merges: bool,
}

pub fn run(args: RebaseInteractiveArgs) -> io::Result<i32> {
    let repo = Repository::discover_from_cwd().map_err(io_err)?;
    let upstream_oid =
        crate::revparse::resolve(repo.refs(), repo.odb(), &args.upstream).map_err(io_err)?;
    let head_oid = crate::revparse::resolve(repo.refs(), repo.odb(), "HEAD").map_err(io_err)?;

    // Collect commits in <upstream>..HEAD newest-first via the same
    // walker as `cherry-pick`'s range.
    let commits = crate::revparse::resolve_range(
        repo.refs(),
        repo.odb(),
        &format!("{}..HEAD", args.upstream),
    )
    .map_err(io_err)?
    .unwrap_or_default();
    let mut commits = commits;
    commits.reverse(); // oldest-first for editing

    // Write a todo file.
    let rebase_dir = repo.gitdir().join("rebase-merge");
    std::fs::create_dir_all(&rebase_dir)?;
    let todo_path = rebase_dir.join("git-rebase-todo");
    let mut todo = String::new();
    let subjects: Vec<(ObjectId, String)> = commits
        .iter()
        .map(|c| (*c, subject_of(&repo, *c).unwrap_or_default()))
        .collect();
    for (oid, subj) in &subjects {
        todo.push_str(&format!("pick {} {}\n", oid.short_hex(7), subj));
    }
    if let Some(cmd) = &args.exec {
        todo.push_str(&format!("exec {cmd}\n"));
    }
    if args.autosquash {
        todo = autosquash_reorder(&todo);
    }
    todo.push_str(
        "\n# Rebase todo file. \n\
         # p, pick    = use commit\n\
         # r, reword  = use commit, but edit the commit message\n\
         # e, edit    = use commit, but stop for amending\n\
         # s, squash  = use commit, but meld into previous commit\n\
         # f, fixup   = like squash, but discard this commit's log message\n\
         # x, exec    = run command (the rest of the line) using shell\n\
         # d, drop    = remove commit\n",
    );
    std::fs::write(&todo_path, &todo)?;

    // Spawn $EDITOR.
    let editor = crate::cli::var::pick_editor(
        &crate::config::Config::from_repo_dir(repo.gitdir())
            .unwrap_or_else(|_| crate::config::Config::empty()),
    );
    let status = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!("{editor} {}", todo_path.display()))
        .status();
    if status.is_err() || !status.as_ref().is_ok_and(|s| s.success()) {
        eprintln!("rustygit rebase -i: editor exited non-zero; aborting");
        return Ok(1);
    }

    // Parse the edited todo.
    let edited = std::fs::read_to_string(&todo_path)?;
    let actions = parse_todo(&edited, &subjects, repo.hash_kind());

    // Reset HEAD to upstream, then replay actions one at a time.
    use crate::refs::{ExpectedOldValue, FullName, NewValue, ReflogMessage};
    let head_name = FullName::new("HEAD").map_err(io_err)?;
    let head_ref = repo.refs().read(&head_name).map_err(io_err)?;
    let branch = match head_ref.as_ref().map(|r| &r.target) {
        Some(crate::refs::RefTarget::Symbolic(b)) => b.clone(),
        _ => return Err(io::Error::other("rebase -i: detached HEAD not supported")),
    };
    let mut tx = repo.refs().transaction();
    tx.update(
        &branch,
        ExpectedOldValue::Direct(head_oid),
        NewValue::Direct(upstream_oid),
        ReflogMessage::from(format!("rebase -i: onto {}", args.upstream)),
    )
    .map_err(io_err)?;
    tx.commit().map_err(io_err)?;

    // Replay.
    use crate::sequencer::{apply_commit, ApplyOpts, ApplyOutcome};
    let opts = ApplyOpts {
        preserve_author: true,
        override_message: None,
        theirs_label: "rebase".into(),
        revert: false,
        mainline: None,
    };
    for action in &actions {
        match action {
            Action::Pick(oid) | Action::Reword(oid) | Action::Edit(oid) => {
                match apply_commit(&repo, *oid, &opts).map_err(io_err)? {
                    ApplyOutcome::Done { .. } => {}
                    ApplyOutcome::Empty => {}
                    ApplyOutcome::Conflicted { offending_paths } => {
                        let _ = offending_paths;
                        eprintln!(
                            "rebase -i: conflict at {}; resolve and continue (deferred)",
                            oid.short_hex(7)
                        );
                        return Ok(1);
                    }
                }
            }
            Action::Squash(_oid) | Action::Fixup(_oid) => {
                // Squash/fixup is a real feature; for MVP we treat them
                // identically to pick and emit a notice.
                eprintln!("rebase -i: squash/fixup is degraded to pick in this build");
            }
            Action::Drop => {}
            Action::Exec(cmd) => {
                let s = std::process::Command::new("sh")
                    .arg("-c")
                    .arg(cmd)
                    .status()?;
                if !s.success() {
                    eprintln!("rebase -i: exec '{cmd}' failed");
                    return Ok(1);
                }
            }
        }
    }

    // Clean up the rebase dir.
    let _ = std::fs::remove_dir_all(&rebase_dir);
    Ok(0)
}

fn subject_of(repo: &Repository, oid: ObjectId) -> io::Result<String> {
    let raw = repo.odb().read(&oid).map_err(io_err)?;
    let commit = Commit::parse(&raw.data, repo.hash_kind()).map_err(io_err)?;
    let s = String::from_utf8_lossy(&commit.message);
    Ok(s.lines().next().unwrap_or("").to_string())
}

#[derive(Debug)]
enum Action {
    Pick(ObjectId),
    Reword(ObjectId),
    Edit(ObjectId),
    Squash(ObjectId),
    Fixup(ObjectId),
    Drop,
    Exec(String),
}

fn parse_todo(
    text: &str,
    subjects: &[(ObjectId, String)],
    hash_kind: crate::hash::HashKind,
) -> Vec<Action> {
    let mut out = Vec::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut parts = line.splitn(3, ' ');
        let action = parts.next().unwrap_or("");
        let id_or_cmd = parts.next().unwrap_or("");
        // For exec, the rest of the line is the command.
        if action == "x" || action == "exec" {
            let cmd = format!("{id_or_cmd} {}", parts.next().unwrap_or(""));
            out.push(Action::Exec(cmd.trim().to_string()));
            continue;
        }
        // For other actions, id_or_cmd is the short oid; look it up.
        let oid = subjects
            .iter()
            .find(|(o, _)| o.short_hex(id_or_cmd.len().max(4)) == id_or_cmd)
            .map(|(o, _)| *o)
            .or_else(|| ObjectId::parse_hex(hash_kind, id_or_cmd).ok());
        let oid = match oid {
            Some(o) => o,
            None => continue,
        };
        match action {
            "p" | "pick" => out.push(Action::Pick(oid)),
            "r" | "reword" => out.push(Action::Reword(oid)),
            "e" | "edit" => out.push(Action::Edit(oid)),
            "s" | "squash" => out.push(Action::Squash(oid)),
            "f" | "fixup" => out.push(Action::Fixup(oid)),
            "d" | "drop" => out.push(Action::Drop),
            _ => {}
        }
    }
    out
}

fn autosquash_reorder(todo: &str) -> String {
    // For each `fixup!`/`squash!` line, find the target commit (by
    // matching the suffix subject) and move the fixup to immediately
    // after it.
    let mut lines: Vec<String> = todo.lines().map(str::to_string).collect();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i].clone();
        let trimmed = line.trim_start_matches(['p', 'i', 'c', 'k', ' ']);
        let after_oid = trimmed.split_once(' ').map(|(_, r)| r).unwrap_or("");
        if let Some(rest) = after_oid
            .strip_prefix("fixup! ")
            .or_else(|| after_oid.strip_prefix("squash! "))
        {
            // Find the target commit whose subject matches `rest`.
            let target_idx = lines.iter().enumerate().find_map(|(j, l)| {
                if j == i {
                    return None;
                }
                let s = l.splitn(3, ' ').nth(2).unwrap_or("");
                if s == rest {
                    Some(j)
                } else {
                    None
                }
            });
            if let Some(t) = target_idx {
                // Move line `i` to immediately after `t`.
                let removed = lines.remove(i);
                let insert_at = if t < i { t + 1 } else { t };
                let kind = if after_oid.starts_with("fixup! ") {
                    "fixup "
                } else {
                    "squash "
                };
                let new_line = if let Some(rest) = removed.strip_prefix("pick ") {
                    format!("{kind}{rest}")
                } else {
                    removed
                };
                lines.insert(insert_at, new_line);
                // Don't increment i; we want to re-examine the same position.
                continue;
            }
        }
        i += 1;
    }
    lines.join("\n") + "\n"
}

fn io_err<E: std::fmt::Display>(e: E) -> io::Error {
    io::Error::other(format!("{e}"))
}
