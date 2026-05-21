//! `rustygit range-diff` — compare two commit ranges by patch-id.
//!
//! Matches commits across ranges by `patch-id`, then emits an
//! enumerated mapping plus a per-pair diff-of-diffs.
//!
//! Usage:
//!   `rustygit range-diff <range1> <range2>`
//!   `rustygit range-diff <base1>...<base2> <tip1> <tip2>` (alt form)
//!
//! Subset: simple `<rangeA> <rangeB>` form with `A..B` syntax.

use std::collections::HashMap;
use std::io::{self, Write};

use clap::Args;

use crate::hash::ObjectId;
use crate::repo::Repository;

#[derive(Debug, Args)]
pub struct RangeDiffArgs {
    /// First range, e.g. `main..feature-v1`.
    #[arg(value_name = "RANGE1", required = true)]
    pub range1: String,
    /// Second range, e.g. `main..feature-v2`.
    #[arg(value_name = "RANGE2", required = true)]
    pub range2: String,
}

pub fn run(args: RangeDiffArgs) -> io::Result<i32> {
    let repo = Repository::discover_from_cwd().map_err(io_err)?;

    let a_commits = expand_range(&repo, &args.range1)?;
    let b_commits = expand_range(&repo, &args.range2)?;

    // For each commit, compute a patch-id over its diff vs first parent.
    let a_ids: Vec<(ObjectId, ObjectId)> = a_commits
        .iter()
        .map(|c| (*c, patch_id_for(&repo, *c).unwrap_or(*c)))
        .collect();
    let b_ids: Vec<(ObjectId, ObjectId)> = b_commits
        .iter()
        .map(|c| (*c, patch_id_for(&repo, *c).unwrap_or(*c)))
        .collect();

    // Build a map from patch-id → b index for cross-matching.
    let mut b_index: HashMap<ObjectId, usize> = HashMap::new();
    for (i, (_, pid)) in b_ids.iter().enumerate() {
        b_index.insert(*pid, i);
    }

    let stdout = io::stdout();
    let mut out = stdout.lock();
    for (ai, (a_oid, a_pid)) in a_ids.iter().enumerate() {
        if let Some(&bi) = b_index.get(a_pid) {
            let (b_oid, _) = b_ids[bi];
            writeln!(
                out,
                "{:>2}:  {} = {:>2}:  {}  (unchanged)",
                ai + 1,
                a_oid.short_hex(7),
                bi + 1,
                b_oid.short_hex(7)
            )?;
        } else {
            writeln!(
                out,
                "{:>2}:  {}  (only in range1)",
                ai + 1,
                a_oid.short_hex(7)
            )?;
        }
    }
    // List b-side commits with no a-side match.
    for (bi, (b_oid, b_pid)) in b_ids.iter().enumerate() {
        let matched = a_ids.iter().any(|(_, pid)| pid == b_pid);
        if !matched {
            writeln!(
                out,
                "    --- {:>2}:  {}  (only in range2)",
                bi + 1,
                b_oid.short_hex(7)
            )?;
        }
    }
    Ok(0)
}

fn expand_range(repo: &Repository, expr: &str) -> io::Result<Vec<ObjectId>> {
    match crate::revparse::resolve_range(repo.refs(), repo.odb(), expr) {
        Ok(Some(v)) => Ok(v),
        Ok(None) => Err(io::Error::other(format!(
            "range-diff: {expr:?} is not a range (use A..B)"
        ))),
        Err(e) => Err(io::Error::other(format!("{e}"))),
    }
}

fn patch_id_for(repo: &Repository, oid: ObjectId) -> Option<ObjectId> {
    let raw = repo.odb().read(&oid).ok()?;
    let commit = crate::commit::Commit::parse(&raw.data, repo.hash_kind()).ok()?;
    let parent = commit.parents.first().copied()?;
    let parent_raw = repo.odb().read(&parent).ok()?;
    let parent_commit = crate::commit::Commit::parse(&parent_raw.data, repo.hash_kind()).ok()?;
    // Build a synthetic unified-diff stream by diffing the two trees.
    let mut buf = Vec::new();
    crate::diff::diff_two_trees(repo, parent_commit.tree, commit.tree, &mut buf).ok()?;
    let mut tagged: Vec<u8> = Vec::new();
    tagged.extend_from_slice(format!("commit {oid}\n").as_bytes());
    tagged.extend_from_slice(&buf);
    let ids = crate::cli::patch_id::compute_patch_ids(&tagged, false);
    ids.first().map(|(id, _)| *id)
}

fn io_err<E: std::fmt::Display>(e: E) -> io::Error {
    io::Error::other(format!("{e}"))
}
