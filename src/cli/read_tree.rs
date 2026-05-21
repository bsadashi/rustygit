//! `rustygit read-tree` — replace (or merge into) the index from a tree.
//!
//! Subset: `read-tree <tree>` replaces the index outright. `--reset`
//! plus a tree resets workdir-untracked-but-also-in-index paths.

use std::io;

use clap::Args;

use crate::repo::Repository;
use crate::unpack_trees::{checkout_tree, UnpackOpts};

#[derive(Debug, Args)]
pub struct ReadTreeArgs {
    /// Discard local modifications to indexed files. (We always overwrite
    /// the index regardless; this flag also wipes workdir entries that
    /// would conflict.)
    #[arg(long = "reset")]
    pub reset: bool,
    /// Update the workdir to match the tree as we read it.
    #[arg(short = 'u', long = "update")]
    pub update_workdir: bool,
    /// Tree-ish to read.
    #[arg(value_name = "TREE-ISH", required = true)]
    pub treeish: String,
}

pub fn run(args: ReadTreeArgs) -> io::Result<i32> {
    let repo = Repository::discover_from_cwd().map_err(io_err)?;
    let oid = crate::revparse::resolve(repo.refs(), repo.odb(), &args.treeish).map_err(io_err)?;
    // Peel commit/tag to tree.
    let tree_oid = peel_to_tree(&repo, oid)?;
    let opts = UnpackOpts {
        force: args.reset,
        keep_extra: false,
        update_workdir: args.update_workdir,
        update_index: true,
    };
    checkout_tree(&repo, tree_oid, &opts).map_err(io_err)?;
    Ok(0)
}

fn peel_to_tree(
    repo: &Repository,
    oid: crate::hash::ObjectId,
) -> io::Result<crate::hash::ObjectId> {
    let raw = repo.odb().read(&oid).map_err(io_err)?;
    match raw.kind {
        crate::object::ObjectKind::Tree => Ok(oid),
        crate::object::ObjectKind::Commit => {
            let commit =
                crate::commit::Commit::parse(&raw.data, repo.hash_kind()).map_err(io_err)?;
            Ok(commit.tree)
        }
        crate::object::ObjectKind::Tag => {
            let tag = crate::tag::Tag::parse(&raw.data, repo.hash_kind()).map_err(io_err)?;
            peel_to_tree(repo, tag.object)
        }
        _ => Err(io::Error::other(format!(
            "read-tree: {oid} is not tree-ish"
        ))),
    }
}

fn io_err<E: std::fmt::Display>(e: E) -> io::Error {
    io::Error::other(format!("{e}"))
}
