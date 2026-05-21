//! `rustygit commit-tree` — create a commit object given a tree and parent oids.
//!
//! Form: `commit-tree <tree-oid> [-p <parent-oid> ...] -m <message> [-m <message> ...]`
//!
//! Reads identity from `GIT_AUTHOR_*` / `GIT_COMMITTER_*` env vars, falling back
//! to `user.name`/`user.email` from `.git/config`. Multiple `-m` messages are
//! concatenated with a blank line between them, matching git.

use std::io;

use clap::Args;

use crate::commit::Commit;
use crate::config::Config;
use crate::hash::ObjectId;
use crate::identity::{Signature, Time};
use crate::repo::Repository;
use crate::signing::Signer;

#[derive(Debug, Args)]
pub struct CommitTreeArgs {
    /// The tree object to commit.
    #[arg(value_name = "TREE")]
    pub tree: String,

    /// Each occurrence adds a parent commit oid. Order matters.
    #[arg(short = 'p', value_name = "PARENT")]
    pub parents: Vec<String>,

    /// The commit message. May be repeated; messages are joined with a blank line.
    #[arg(short = 'm', value_name = "MESSAGE")]
    pub messages: Vec<String>,
}

pub fn run(args: CommitTreeArgs) -> io::Result<i32> {
    let repo = Repository::discover_from_cwd().map_err(io_err)?;
    let oid = create_commit(
        &repo,
        &args.tree,
        &args.parents.iter().map(String::as_str).collect::<Vec<_>>(),
        &join_messages(&args.messages),
    )?;
    println!("{oid}");
    Ok(0)
}

/// Library entry point for porcelain reuse: the `commit` command calls this
/// after `write-tree`.
///
/// Unsigned: passes `None` for the signer; the commit is written verbatim.
pub fn create_commit(
    repo: &Repository,
    tree_str: &str,
    parents: &[&str],
    message: &str,
) -> io::Result<ObjectId> {
    create_commit_with_signer(repo, tree_str, parents, message, None)
}

/// Same as [`create_commit`] but optionally signs the commit. When `signer`
/// is `Some`, we:
///
/// 1. Build the commit body WITHOUT a `gpgsig` header.
/// 2. Sign those bytes via the signer.
/// 3. Fold the signature into a `gpgsig` header and re-serialize.
/// 4. Write the resulting (signed) commit object. The returned oid is the
///    sha of the SIGNED body — `git verify-commit` will see this oid.
///
/// Step ordering matches upstream git's `commit.c::sign_with_header`.
pub fn create_commit_with_signer(
    repo: &Repository,
    tree_str: &str,
    parents: &[&str],
    message: &str,
    signer: Option<&dyn Signer>,
) -> io::Result<ObjectId> {
    let hash_kind = repo.hash_kind();
    let tree = ObjectId::parse_hex(hash_kind, tree_str)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, format!("{e}")))?;

    let mut parent_oids = Vec::with_capacity(parents.len());
    for p in parents {
        parent_oids.push(
            ObjectId::parse_hex(hash_kind, p)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, format!("{e}")))?,
        );
    }

    let config = Config::from_repo_dir(repo.gitdir()).map_err(io_err)?;
    let now = Time::now_local();
    let author = Signature::author_from_env_or_config(&config, now).map_err(io_err)?;
    let committer = Signature::committer_from_env_or_config(&config, now).map_err(io_err)?;

    let mut body_msg = message.as_bytes().to_vec();
    if !body_msg.ends_with(b"\n") {
        body_msg.push(b'\n');
    }

    let mut commit = Commit {
        tree,
        parents: parent_oids,
        author,
        committer,
        message: body_msg,
        encoding: None,
        gpgsig: None,
    };

    // If a signer is provided, sign the UNSIGNED body (no gpgsig header),
    // then fold the resulting signature into `gpgsig` and re-serialize.
    if let Some(signer) = signer {
        let unsigned = commit.serialize();
        let signature = signer
            .sign(&unsigned)
            .map_err(|e| io::Error::other(format!("gpg signing failed: {e}")))?;
        // Strip a trailing newline if present — Commit::serialize folds
        // multi-line gpgsig values; an extra LF would produce a spurious
        // empty continuation line.
        let mut sig = signature;
        while sig.last() == Some(&b'\n') {
            sig.pop();
        }
        commit.gpgsig = Some(sig);
    }

    let obj = commit.to_object();
    let oid = repo
        .odb()
        .write(&obj)
        .map_err(|e| io::Error::other(format!("{e}")))?;
    Ok(oid)
}

fn join_messages(msgs: &[String]) -> String {
    if msgs.is_empty() {
        return String::new();
    }
    msgs.join("\n\n")
}

fn io_err<E: std::fmt::Display>(e: E) -> io::Error {
    io::Error::other(format!("{e}"))
}
