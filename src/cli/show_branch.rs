//! `rustygit show-branch` — show commits and which branch tips can
//! reach each one.
//!
//! Simplified subset of upstream:
//!   * One column per input ref (or every local branch by default).
//!   * Each row: `<col-markers> [ref] <subject>`.
//!   * `*` in column N means commit is reachable from input N.
//!   * Walk stops at the merge base of all inputs (or HEAD depth limit).

use std::io::{self, Write};

use clap::Args;

use crate::commit::Commit;
use crate::hash::ObjectId;
use crate::refs::RefTarget;
use crate::repo::Repository;
use crate::revparse::resolve;

#[derive(Debug, Args)]
pub struct ShowBranchArgs {
    /// Limit display to <count> commits per branch.
    #[arg(long = "count", value_name = "N", default_value_t = 10)]
    pub count: usize,
    /// One or more refs to show. Defaults to every refs/heads/* + HEAD.
    #[arg(value_name = "BRANCH")]
    pub branches: Vec<String>,
}

pub fn run(args: ShowBranchArgs) -> io::Result<i32> {
    let repo = Repository::discover_from_cwd().map_err(io_err)?;

    // Resolve input refs (or default to every local branch).
    let mut tips: Vec<(String, ObjectId)> = Vec::new();
    if args.branches.is_empty() {
        for r in repo.refs().iter(Some("refs/heads/")) {
            let r = r.map_err(io_err)?;
            if let RefTarget::Direct(oid) = r.target {
                let short = r
                    .name
                    .as_str()
                    .strip_prefix("refs/heads/")
                    .unwrap_or(r.name.as_str())
                    .to_string();
                tips.push((short, oid));
            }
        }
        tips.sort_by(|a, b| a.0.cmp(&b.0));
    } else {
        for arg in &args.branches {
            match resolve(repo.refs(), repo.odb(), arg) {
                Ok(oid) => tips.push((arg.clone(), oid)),
                Err(e) => {
                    eprintln!("rustygit: show-branch: bad revision {arg:?}: {e}");
                    return Ok(128);
                }
            }
        }
    }

    if tips.is_empty() {
        return Ok(0);
    }

    let stdout = io::stdout();
    let mut out = stdout.lock();

    // Header: one line per branch describing its symbol.
    for (i, (name, oid)) in tips.iter().enumerate() {
        let bang = if i == 0 { "*" } else { " " };
        writeln!(
            out,
            "{bang} [{name}] {}",
            commit_subject(&repo, *oid).unwrap_or_default()
        )?;
    }
    writeln!(out, "{}", "-".repeat(tips.len() + 2))?;

    // For each commit we encounter (BFS from every tip), compute the
    // reachable-from-tip bitmask and print one row.
    let mut printed: Vec<(ObjectId, u64)> = Vec::new();
    let mut rows = 0;
    let max_rows = args.count;

    // Compute reachability for each tip independently (small bitmask).
    if tips.len() > 64 {
        eprintln!("rustygit: show-branch: too many branches (>64)");
        return Ok(128);
    }
    let mut reach: std::collections::HashMap<ObjectId, u64> = std::collections::HashMap::new();
    for (i, (_name, oid)) in tips.iter().enumerate() {
        let bit = 1u64 << i;
        let mut stack = vec![*oid];
        while let Some(o) = stack.pop() {
            let entry = reach.entry(o).or_insert(0);
            if *entry & bit != 0 {
                continue;
            }
            *entry |= bit;
            let raw = match repo.odb().read(&o) {
                Ok(r) => r,
                Err(_) => continue,
            };
            if raw.kind != crate::object::ObjectKind::Commit {
                continue;
            }
            let commit = match Commit::parse(&raw.data, repo.hash_kind()) {
                Ok(c) => c,
                Err(_) => continue,
            };
            for p in &commit.parents {
                stack.push(*p);
            }
        }
    }

    // BFS from every tip in lockstep, oldest-on-each-side first.
    let mut visited = std::collections::HashSet::new();
    let mut frontier: Vec<ObjectId> = tips.iter().map(|(_, o)| *o).collect();
    while !frontier.is_empty() && rows < max_rows {
        // Pop the youngest-by-commit-time? We don't track time here;
        // emit in BFS order which is good enough for the simple case.
        let mut next_frontier: Vec<ObjectId> = Vec::new();
        for oid in frontier.drain(..) {
            if !visited.insert(oid) {
                continue;
            }
            let mask = *reach.get(&oid).unwrap_or(&0);
            printed.push((oid, mask));
            rows += 1;
            if rows >= max_rows {
                break;
            }
            let raw = match repo.odb().read(&oid) {
                Ok(r) => r,
                Err(_) => continue,
            };
            if raw.kind != crate::object::ObjectKind::Commit {
                continue;
            }
            let commit = match Commit::parse(&raw.data, repo.hash_kind()) {
                Ok(c) => c,
                Err(_) => continue,
            };
            for p in &commit.parents {
                if !visited.contains(p) {
                    next_frontier.push(*p);
                }
            }
        }
        frontier = next_frontier;
    }

    for (oid, mask) in printed {
        let mut marks = String::with_capacity(tips.len());
        for i in 0..tips.len() {
            if mask & (1u64 << i) != 0 {
                marks.push('+');
            } else {
                marks.push(' ');
            }
        }
        let subject = commit_subject(&repo, oid).unwrap_or_default();
        writeln!(out, "{marks} [{}] {subject}", oid.short_hex(7))?;
    }
    Ok(0)
}

fn commit_subject(repo: &Repository, oid: ObjectId) -> Option<String> {
    let raw = repo.odb().read(&oid).ok()?;
    let commit = Commit::parse(&raw.data, repo.hash_kind()).ok()?;
    let nl = commit
        .message
        .iter()
        .position(|&b| b == b'\n')
        .unwrap_or(commit.message.len());
    Some(String::from_utf8_lossy(&commit.message[..nl]).into_owned())
}

fn io_err<E: std::fmt::Display>(e: E) -> io::Error {
    io::Error::other(format!("{e}"))
}
