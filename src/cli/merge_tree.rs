//! `rustygit merge-tree` — plumbing: 3-way merge of two trees (or commits).
//!
//! Form: `rustygit merge-tree <base> <ours> <theirs>`
//!
//! Either trees or commits may be passed; commits are auto-peeled to their
//! root tree. Prints the merged tree oid on success, or lists per-path
//! conflicts on stderr and exits non-zero.

use std::io;

use clap::Args;

use crate::commit::Commit;
use crate::hash::ObjectId;
use crate::merge::file::FileMergeLabels;
use crate::merge::tree::{merge_tree, PathMergeState};
use crate::object::ObjectKind;
use crate::repo::Repository;
use crate::revparse;

#[derive(Debug, Args)]
pub struct MergeTreeArgs {
    /// Base tree-ish (use empty string `""` to indicate unrelated histories).
    #[arg(value_name = "BASE")]
    pub base: String,
    /// "Our side" tree-ish.
    #[arg(value_name = "OURS")]
    pub ours: String,
    /// "Their side" tree-ish.
    #[arg(value_name = "THEIRS")]
    pub theirs: String,
}

pub fn run(args: MergeTreeArgs) -> io::Result<i32> {
    let repo = Repository::discover_from_cwd().map_err(io_err)?;
    let base = if args.base.is_empty() {
        None
    } else {
        Some(peel_to_tree(&repo, &args.base)?)
    };
    let ours = peel_to_tree(&repo, &args.ours)?;
    let theirs = peel_to_tree(&repo, &args.theirs)?;

    let labels = FileMergeLabels {
        base: "base",
        ours: "ours",
        theirs: "theirs",
    };
    let outcome = merge_tree(&repo, base, ours, theirs, &labels).map_err(io_err)?;

    if outcome.has_conflicts {
        for entry in &outcome.paths {
            let label = match &entry.state {
                PathMergeState::ContentConflict { .. } => "content",
                PathMergeState::ModifyDelete => "modify/delete",
                PathMergeState::AddAdd => "add/add",
                PathMergeState::TypeMismatch => "type-mismatch",
                _ => continue,
            };
            eprintln!(
                "CONFLICT ({label}): {}",
                String::from_utf8_lossy(&entry.path)
            );
        }
        Ok(1)
    } else {
        match outcome.merged_tree {
            Some(oid) => {
                println!("{oid}");
                Ok(0)
            }
            None => {
                eprintln!("rustygit: merge-tree: no merged tree produced (internal)");
                Ok(128)
            }
        }
    }
}

fn peel_to_tree(repo: &Repository, expr: &str) -> io::Result<ObjectId> {
    let oid = revparse::resolve(repo.refs(), repo.odb(), expr)
        .map_err(|e| io::Error::other(format!("not a valid tree-ish: {e}")))?;
    let obj = repo
        .odb()
        .read(&oid)
        .map_err(|e| io::Error::other(format!("{e}")))?;
    match obj.kind {
        ObjectKind::Tree => Ok(oid),
        ObjectKind::Commit => {
            let commit = Commit::parse(&obj.data, repo.hash_kind())
                .map_err(|e| io::Error::other(format!("{e}")))?;
            Ok(commit.tree)
        }
        other => Err(io::Error::other(format!(
            "not a tree-ish: {oid} is a {other}"
        ))),
    }
}

fn io_err<E: std::fmt::Display>(e: E) -> io::Error {
    io::Error::other(format!("{e}"))
}
