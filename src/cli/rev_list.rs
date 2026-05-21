//! `rustygit rev-list` — print commit oids reachable from given starts.
//!
//! Subset:
//!   * Positional refs / oids / ranges (`A..B`).
//!   * `--all` — every ref under refs/ + HEAD as a starting set.
//!   * `--reverse` — oldest first.
//!   * `--count` — print only the count.
//!   * `--max-count=N` / `-n N` — limit.

use std::collections::HashSet;
use std::io::{self, Write};

use clap::Args;

use crate::commit::Commit;
use crate::hash::ObjectId;
use crate::refs::RefTarget;
use crate::repo::Repository;

#[derive(Debug, Args)]
pub struct RevListArgs {
    /// Walk from every ref (heads + tags + remotes) — not just the args.
    #[arg(long = "all")]
    pub all: bool,
    /// Print only the number of commits.
    #[arg(long = "count")]
    pub count: bool,
    /// Limit output to <N> commits.
    #[arg(short = 'n', long = "max-count")]
    pub max_count: Option<usize>,
    /// Print in reverse (oldest-first) order.
    #[arg(long = "reverse")]
    pub reverse: bool,
    /// Starting revisions (oids, refs, or `A..B` ranges).
    #[arg(value_name = "REVISION")]
    pub revisions: Vec<String>,
}

pub fn run(args: RevListArgs) -> io::Result<i32> {
    let repo = Repository::discover_from_cwd().map_err(io_err)?;

    let mut starts: Vec<ObjectId> = Vec::new();
    let mut excludes: HashSet<ObjectId> = HashSet::new();
    for arg in &args.revisions {
        if let Some(rest) = arg.strip_prefix('^') {
            let oid = crate::revparse::resolve(repo.refs(), repo.odb(), rest).map_err(io_err)?;
            walk_into_set(&repo, oid, &mut excludes)?;
            continue;
        }
        match crate::revparse::resolve_range(repo.refs(), repo.odb(), arg) {
            Ok(Some(range)) => starts.extend(range),
            Ok(None) => {
                let o = crate::revparse::resolve(repo.refs(), repo.odb(), arg).map_err(io_err)?;
                starts.push(o);
            }
            Err(e) => return Err(io::Error::other(format!("{e}"))),
        }
    }
    if args.all {
        for r in repo.refs().iter(None) {
            let r = r.map_err(io_err)?;
            if r.name.as_str() == "HEAD" {
                continue;
            }
            if let RefTarget::Direct(o) = r.target {
                starts.push(o);
            }
        }
    }
    if starts.is_empty() {
        let head = crate::revparse::resolve(repo.refs(), repo.odb(), "HEAD").map_err(io_err)?;
        starts.push(head);
    }

    // BFS from every start, skipping excludes; record discovery order.
    let mut visited: HashSet<ObjectId> = excludes.clone();
    let mut order: Vec<ObjectId> = Vec::new();
    let mut stack = starts;
    while let Some(oid) = stack.pop() {
        if !visited.insert(oid) {
            continue;
        }
        order.push(oid);
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
                stack.push(*p);
            }
        }
    }

    if args.reverse {
        order.reverse();
    }
    if let Some(max) = args.max_count {
        order.truncate(max);
    }

    let stdout = io::stdout();
    let mut out = stdout.lock();
    if args.count {
        writeln!(out, "{}", order.len())?;
    } else {
        for o in &order {
            writeln!(out, "{o}")?;
        }
    }
    Ok(0)
}

fn walk_into_set(
    repo: &Repository,
    start: ObjectId,
    set: &mut HashSet<ObjectId>,
) -> io::Result<()> {
    let mut stack = vec![start];
    while let Some(oid) = stack.pop() {
        if !set.insert(oid) {
            continue;
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
            stack.push(*p);
        }
    }
    Ok(())
}

fn io_err<E: std::fmt::Display>(e: E) -> io::Error {
    io::Error::other(format!("{e}"))
}
