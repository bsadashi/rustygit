//! `rustygit branch` — list, create, delete, or rename branches.
//!
//! Subset implemented in M6:
//!   `branch`                        list local branches
//!   `branch <name> [<start-point>]` create
//!   `branch -d/-D <name>`           delete (D = force; we don't yet check "merged")
//!   `branch -m <old> <new>` /
//!   `branch -m <new>`               rename
//!
//! Out of scope for M6: `-a`/`-r` (remote-tracking branches arrive in M10),
//! `--set-upstream-to`, `--track`, `--contains`, `--merged`, color, sort.

use std::io::{self, Write};

use clap::Args;

use crate::hash::ObjectId;
use crate::refs::{ExpectedOldValue, FullName, NewValue, RefTarget, ReflogMessage};
use crate::repo::Repository;
use crate::revparse::resolve;

#[derive(Debug, Args)]
pub struct BranchArgs {
    /// Delete the named branch.
    #[arg(short = 'd', long = "delete", conflicts_with_all = ["force_delete", "rename"])]
    pub delete: bool,

    /// Force-delete (we currently treat -d == -D since we don't yet check merge state).
    #[arg(short = 'D', conflicts_with_all = ["delete", "rename"])]
    pub force_delete: bool,

    /// Rename a branch. With one positional, renames the current branch.
    #[arg(short = 'm', long = "move", conflicts_with_all = ["delete", "force_delete"])]
    pub rename: bool,

    /// Filter listings to branches that contain the given commit.
    #[arg(long = "contains", value_name = "COMMIT")]
    pub contains: Option<String>,

    /// Filter listings to branches that DO NOT contain the given commit.
    #[arg(long = "no-contains", value_name = "COMMIT")]
    pub no_contains: Option<String>,

    /// Filter listings to branches whose tip is reachable from the named ref.
    #[arg(long = "merged", value_name = "COMMIT")]
    pub merged: Option<String>,

    /// Filter listings to branches whose tip is NOT reachable from the named ref.
    #[arg(long = "no-merged", value_name = "COMMIT")]
    pub no_merged: Option<String>,

    /// Branch name(s).
    ///
    /// list:    no positionals
    /// create:  <name> [<start-point>]
    /// delete:  <name>...
    /// rename:  <new> | <old> <new>
    #[arg(value_name = "NAME")]
    pub names: Vec<String>,
}

pub fn run(args: BranchArgs) -> io::Result<i32> {
    let repo = Repository::discover_from_cwd().map_err(io_err)?;

    if args.delete || args.force_delete {
        return delete(&repo, &args.names);
    }
    if args.rename {
        return rename(&repo, &args.names);
    }
    if args.names.is_empty() {
        return list(&repo, &args);
    }
    create(&repo, &args.names)
}

fn list(repo: &Repository, args: &BranchArgs) -> io::Result<i32> {
    let head_branch = current_branch(repo).map_err(io_err)?;

    // Resolve the filter targets (if any) up-front.
    let contains_oid = args
        .contains
        .as_deref()
        .map(|c| resolve(repo.refs(), repo.odb(), c))
        .transpose()
        .map_err(io_err)?;
    let no_contains_oid = args
        .no_contains
        .as_deref()
        .map(|c| resolve(repo.refs(), repo.odb(), c))
        .transpose()
        .map_err(io_err)?;
    let merged_oid = args
        .merged
        .as_deref()
        .map(|c| resolve(repo.refs(), repo.odb(), c))
        .transpose()
        .map_err(io_err)?;
    let no_merged_oid = args
        .no_merged
        .as_deref()
        .map(|c| resolve(repo.refs(), repo.odb(), c))
        .transpose()
        .map_err(io_err)?;

    let mut branches: Vec<(String, ObjectId)> = repo
        .refs()
        .iter(Some("refs/heads/"))
        .filter_map(Result::ok)
        .filter_map(|r| {
            let short = r
                .name
                .as_str()
                .strip_prefix("refs/heads/")
                .map(|s| s.to_string())?;
            match r.target {
                RefTarget::Direct(o) => Some((short, o)),
                RefTarget::Symbolic(_) => None,
            }
        })
        .collect();
    branches.sort_by(|a, b| a.0.cmp(&b.0));

    let stdout = io::stdout();
    let mut out = stdout.lock();
    for (b, tip) in &branches {
        if let Some(target) = contains_oid {
            if !is_ancestor(repo, target, *tip) {
                continue;
            }
        }
        if let Some(target) = no_contains_oid {
            if is_ancestor(repo, target, *tip) {
                continue;
            }
        }
        if let Some(target) = merged_oid {
            if !is_ancestor(repo, *tip, target) {
                continue;
            }
        }
        if let Some(target) = no_merged_oid {
            if is_ancestor(repo, *tip, target) {
                continue;
            }
        }
        let prefix = if Some(b.as_str()) == head_branch.as_deref() {
            "* "
        } else {
            "  "
        };
        writeln!(out, "{prefix}{b}")?;
    }
    Ok(0)
}

/// True iff `ancestor` is reachable from `target` (i.e. target's history
/// contains ancestor).
fn is_ancestor(repo: &Repository, ancestor: ObjectId, target: ObjectId) -> bool {
    if ancestor == target {
        return true;
    }
    let mut seen = std::collections::HashSet::new();
    let mut stack = vec![target];
    while let Some(oid) = stack.pop() {
        if !seen.insert(oid) {
            continue;
        }
        if oid == ancestor {
            return true;
        }
        let raw = match repo.odb().read(&oid) {
            Ok(r) => r,
            Err(_) => continue,
        };
        if raw.kind != crate::object::ObjectKind::Commit {
            continue;
        }
        let commit = match crate::commit::Commit::parse(&raw.data, repo.hash_kind()) {
            Ok(c) => c,
            Err(_) => continue,
        };
        for p in &commit.parents {
            stack.push(*p);
        }
    }
    false
}

fn create(repo: &Repository, names: &[String]) -> io::Result<i32> {
    if names.is_empty() {
        eprintln!("rustygit: branch: missing branch name");
        return Ok(129);
    }
    if names.len() > 2 {
        eprintln!("rustygit: branch: too many arguments");
        return Ok(129);
    }
    let new_name = &names[0];
    let start_rev = names.get(1).map(String::as_str).unwrap_or("HEAD");

    let target_oid: ObjectId = match resolve(repo.refs(), repo.odb(), start_rev) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("rustygit: branch: {e}");
            return Ok(128);
        }
    };

    let full = match FullName::new(format!("refs/heads/{new_name}")) {
        Ok(n) => n,
        Err(e) => {
            eprintln!("rustygit: branch: {e}");
            return Ok(128);
        }
    };

    let mut tx = repo.refs().transaction();
    tx.update(
        &full,
        ExpectedOldValue::Missing,
        NewValue::Direct(target_oid),
        ReflogMessage::from(format!("branch: created from {start_rev}")),
    )
    .map_err(io_err)?;
    match tx.commit() {
        Ok(()) => Ok(0),
        Err(crate::refs::RefError::Update(crate::refs::RefUpdateError::ExpectedMissing(_))) => {
            eprintln!("rustygit: branch '{new_name}' already exists");
            Ok(128)
        }
        Err(e) => Err(io_err(e)),
    }
}

fn delete(repo: &Repository, names: &[String]) -> io::Result<i32> {
    if names.is_empty() {
        eprintln!("rustygit: branch: -d requires a branch name");
        return Ok(129);
    }
    let head_branch = current_branch(repo).map_err(io_err)?;
    let mut had_error = false;
    for name in names {
        if Some(name.as_str()) == head_branch.as_deref() {
            eprintln!("rustygit: cannot delete branch '{name}' checked out at HEAD");
            had_error = true;
            continue;
        }
        let full = match FullName::new(format!("refs/heads/{name}")) {
            Ok(n) => n,
            Err(e) => {
                eprintln!("rustygit: branch: {e}");
                had_error = true;
                continue;
            }
        };

        let existed = repo.refs().read(&full).map_err(io_err)?.is_some();
        if !existed {
            eprintln!("rustygit: branch '{name}' not found");
            had_error = true;
            continue;
        }
        let mut tx = repo.refs().transaction();
        tx.delete(&full, ExpectedOldValue::Any).map_err(io_err)?;
        tx.commit().map_err(io_err)?;
        println!("Deleted branch {name}.");
    }
    Ok(if had_error { 1 } else { 0 })
}

fn rename(repo: &Repository, names: &[String]) -> io::Result<i32> {
    let (old_name, new_name) = match names.len() {
        1 => {
            let cur = current_branch(repo).map_err(io_err)?;
            let cur = match cur {
                Some(c) => c,
                None => {
                    eprintln!("rustygit: branch: HEAD is detached; cannot rename");
                    return Ok(128);
                }
            };
            (cur, names[0].clone())
        }
        2 => (names[0].clone(), names[1].clone()),
        _ => {
            eprintln!("rustygit: branch -m: expected 1 or 2 arguments");
            return Ok(129);
        }
    };

    let old_full = FullName::new(format!("refs/heads/{old_name}"))
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, format!("{e}")))?;
    let new_full = FullName::new(format!("refs/heads/{new_name}"))
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, format!("{e}")))?;

    let r = repo.refs().read(&old_full).map_err(io_err)?;
    let old_target = match r {
        Some(reference) => match reference.target {
            RefTarget::Direct(o) => o,
            RefTarget::Symbolic(_) => {
                eprintln!("rustygit: branch '{old_name}' is symbolic; refusing to rename");
                return Ok(128);
            }
        },
        None => {
            eprintln!("rustygit: branch '{old_name}' not found");
            return Ok(128);
        }
    };

    if repo.refs().read(&new_full).map_err(io_err)?.is_some() {
        eprintln!("rustygit: branch '{new_name}' already exists");
        return Ok(128);
    }

    // Create new ref + update HEAD if it pointed at old + delete old.
    let mut tx = repo.refs().transaction();
    tx.update(
        &new_full,
        ExpectedOldValue::Missing,
        NewValue::Direct(old_target),
        ReflogMessage::from(format!("branch: renamed from {old_name}")),
    )
    .map_err(io_err)?;
    tx.commit().map_err(io_err)?;

    if current_branch(repo).map_err(io_err)?.as_deref() == Some(old_name.as_str()) {
        let head = FullName::new("HEAD").unwrap();
        let mut tx = repo.refs().transaction();
        tx.update(
            &head,
            ExpectedOldValue::Any,
            NewValue::Symbolic(new_full.clone()),
            ReflogMessage::from(format!("branch: renamed {old_name} to {new_name}")),
        )
        .map_err(io_err)?;
        tx.commit().map_err(io_err)?;
    }

    let mut tx = repo.refs().transaction();
    tx.delete(&old_full, ExpectedOldValue::Any)
        .map_err(io_err)?;
    tx.commit().map_err(io_err)?;

    Ok(0)
}

/// Return the short name of the branch HEAD points at, or `None` for a
/// detached HEAD or missing HEAD.
fn current_branch(repo: &Repository) -> Result<Option<String>, crate::refs::RefError> {
    let head = FullName::new("HEAD").unwrap();
    let r = repo.refs().read(&head)?;
    let r = match r {
        Some(r) => r,
        None => return Ok(None),
    };
    Ok(match r.target {
        RefTarget::Symbolic(t) => t
            .as_str()
            .strip_prefix("refs/heads/")
            .map(|s| s.to_string()),
        RefTarget::Direct(_) => None,
    })
}

fn io_err<E: std::fmt::Display>(e: E) -> io::Error {
    io::Error::other(format!("{e}"))
}
