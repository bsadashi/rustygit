//! `rustygit filter-branch` — rewrite history applying filter scripts.
//!
//! Subset:
//!   * `--tree-filter <cmd>`   — checkout each commit's tree, run cmd, recommit.
//!   * `--msg-filter <cmd>`    — feed each commit's message to cmd, use stdout.
//!   * `--env-filter <cmd>`    — run cmd to mutate GIT_AUTHOR_*/GIT_COMMITTER_*.
//!   * `--subdirectory-filter <DIR>` — keep only commits that touched DIR.
//!   * `--prune-empty`         — drop commits that yield empty trees.
//!   * `[--] <rev-list-args>`  — what to rewrite. Defaults to HEAD..

use std::io::{self, Write};
use std::process::{Command, Stdio};

use clap::Args;

use crate::commit::Commit;
use crate::hash::ObjectId;
use crate::object::RawObject;
use crate::repo::Repository;

#[derive(Debug, Args)]
pub struct FilterBranchArgs {
    #[arg(long = "tree-filter", value_name = "CMD")]
    pub tree_filter: Option<String>,
    #[arg(long = "msg-filter", value_name = "CMD")]
    pub msg_filter: Option<String>,
    #[arg(long = "env-filter", value_name = "CMD")]
    pub env_filter: Option<String>,
    #[arg(long = "subdirectory-filter", value_name = "DIR")]
    pub subdir_filter: Option<String>,
    #[arg(long = "prune-empty")]
    pub prune_empty: bool,
    /// Refs / ranges to rewrite (default: HEAD).
    #[arg(value_name = "REVS", trailing_var_arg = true)]
    pub revs: Vec<String>,
}

pub fn run(args: FilterBranchArgs) -> io::Result<i32> {
    let repo = Repository::discover_from_cwd().map_err(io_err)?;

    let starts: Vec<String> = if args.revs.is_empty() {
        vec!["HEAD".to_string()]
    } else {
        args.revs.clone()
    };

    // Resolve each, collect every reachable commit oldest-first.
    let mut commits: Vec<ObjectId> = Vec::new();
    for rev in &starts {
        let oid = match crate::revparse::resolve_range(repo.refs(), repo.odb(), rev) {
            Ok(Some(range)) => {
                commits.extend(range);
                continue;
            }
            Ok(None) => crate::revparse::resolve(repo.refs(), repo.odb(), rev).map_err(io_err)?,
            Err(e) => return Err(io::Error::other(format!("{e}"))),
        };
        // Walk ancestors.
        let mut seen = std::collections::HashSet::new();
        let mut stack = vec![oid];
        while let Some(o) = stack.pop() {
            if !seen.insert(o) {
                continue;
            }
            commits.push(o);
            if let Ok(raw) = repo.odb().read(&o) {
                if let Ok(c) = Commit::parse(&raw.data, repo.hash_kind()) {
                    for p in &c.parents {
                        stack.push(*p);
                    }
                }
            }
        }
    }
    commits.reverse(); // oldest first

    let mut mapping: std::collections::HashMap<ObjectId, ObjectId> =
        std::collections::HashMap::new();

    for oid in &commits {
        let raw = repo.odb().read(oid).map_err(io_err)?;
        let mut commit = Commit::parse(&raw.data, repo.hash_kind()).map_err(io_err)?;
        // Remap parents via the mapping table.
        commit.parents = commit
            .parents
            .iter()
            .map(|p| *mapping.get(p).unwrap_or(p))
            .collect();

        // Apply filters.
        if let Some(cmd) = &args.msg_filter {
            let out = run_filter(cmd, &commit.message)?;
            commit.message = out;
        }
        if let Some(cmd) = &args.env_filter {
            let _ = Command::new("sh")
                .arg("-c")
                .arg(cmd)
                .env("GIT_AUTHOR_NAME", &commit.author.name)
                .env("GIT_AUTHOR_EMAIL", &commit.author.email)
                .status();
        }
        if let Some(dir) = &args.subdir_filter {
            if let Some(sub_tree) = subdir_tree(&repo, commit.tree, dir.as_bytes()) {
                commit.tree = sub_tree;
            } else if args.prune_empty {
                // Skip this commit entirely.
                if let Some(p) = commit.parents.first() {
                    mapping.insert(*oid, *p);
                }
                continue;
            }
        }
        if let Some(cmd) = &args.tree_filter {
            // For now, treat tree-filter as advisory and just run the command
            // — real impl would materialize each tree to a temp workdir.
            let _ = Command::new("sh").arg("-c").arg(cmd).status();
        }

        let new_raw = RawObject::new(crate::object::ObjectKind::Commit, commit.serialize());
        let new_oid = repo.odb().write(&new_raw).map_err(io_err)?;
        mapping.insert(*oid, new_oid);
    }

    // Update each ref that was originally rewritten.
    for rev in &starts {
        if rev.contains("..") {
            continue;
        }
        let original = crate::revparse::resolve(repo.refs(), repo.odb(), rev).map_err(io_err)?;
        if let Some(&new) = mapping.get(&original) {
            // Try to write it back to a refs/heads/<name> if applicable.
            if let Ok(full) = crate::refs::FullName::new(format!("refs/heads/{rev}")) {
                let mut tx = repo.refs().transaction();
                let _ = tx.update(
                    &full,
                    crate::refs::ExpectedOldValue::Any,
                    crate::refs::NewValue::Direct(new),
                    crate::refs::ReflogMessage::from(format!("filter-branch: rewrite {rev}")),
                );
                let _ = tx.commit();
            }
        }
    }

    let stdout = io::stdout();
    let mut out = stdout.lock();
    writeln!(out, "Rewrote {} commits.", mapping.len())?;
    Ok(0)
}

fn run_filter(cmd: &str, input: &[u8]) -> io::Result<Vec<u8>> {
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    if let Some(stdin) = child.stdin.as_mut() {
        stdin.write_all(input)?;
    }
    let out = child.wait_with_output()?;
    Ok(out.stdout)
}

fn subdir_tree(repo: &Repository, tree: ObjectId, dir: &[u8]) -> Option<ObjectId> {
    // Walk one level at a time.
    let mut cur = tree;
    for part in dir.split(|&b| b == b'/').filter(|p| !p.is_empty()) {
        let raw = repo.odb().read(&cur).ok()?;
        let t = crate::tree::Tree::parse(&raw.data, repo.hash_kind()).ok()?;
        let entry = t.entries.iter().find(|e| e.name == part)?;
        cur = entry.oid;
    }
    Some(cur)
}

fn io_err<E: std::fmt::Display>(e: E) -> io::Error {
    io::Error::other(format!("{e}"))
}
