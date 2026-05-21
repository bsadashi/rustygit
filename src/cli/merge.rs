//! `rustygit merge` — porcelain three-way merge.
//!
//! Algorithm:
//!   1. Resolve `<target>` to a commit oid; read HEAD's current commit (`ours`).
//!   2. Compute `merge_base(ours, target)`.
//!   3. If base == ours: target is descendant → fast-forward (just move HEAD).
//!   4. If base == target: target is ancestor → nothing to do, "Already up to date."
//!   5. Else: three-way `merge_tree(base, ours, target)`.
//!      - On clean merge: write merge commit (two parents), update HEAD's branch,
//!        materialize the merged workdir via unpack_trees.
//!      - On conflicts: write the partial result to index (stages 1/2/3),
//!        materialize files with conflict markers, leave HEAD untouched,
//!        write `.git/MERGE_HEAD` so a follow-up `commit` knows about the
//!        in-progress merge. Exit 1.
//!
//! Out of scope for M13: `--squash`, `--no-ff`, `--ff-only`, `--strategy`,
//! `--abort` (which would restore the pre-merge state via `MERGE_HEAD`),
//! `--continue`, octopus merges (3+ parents). Rename detection lives in M16.

use std::io::{self, Write};

use clap::Args;

use crate::commit::Commit;
use crate::hash::ObjectId;
use crate::hooks::{self, HookRunner};
use crate::identity::{Signature, Time};
use crate::merge::base::merge_base;
use crate::merge::file::FileMergeLabels;
use crate::merge::tree::{merge_tree, MergeOutcome, PathMergeState};
use crate::object::ObjectKind;
use crate::refs::{ExpectedOldValue, FullName, NewValue, RefTarget, ReflogMessage};
use crate::repo::Repository;
use crate::revparse;
use crate::unpack_trees::{checkout_tree, UnpackOpts};

#[derive(Debug, Args)]
pub struct MergeArgs {
    /// Commit message (used when a real merge commit is created).
    #[arg(short = 'm', value_name = "MESSAGE")]
    pub message: Option<String>,

    /// Refuse to merge unless the result is a fast-forward.
    #[arg(long = "ff-only")]
    pub ff_only: bool,

    /// Quiet mode.
    #[arg(short = 'q', long = "quiet")]
    pub quiet: bool,

    /// The branch / commit to merge into HEAD.
    #[arg(value_name = "COMMIT", required = true)]
    pub target: String,
}

pub fn run(args: MergeArgs) -> io::Result<i32> {
    let repo = Repository::discover_from_cwd().map_err(io_err)?;
    let hash_kind = repo.hash_kind();

    // 1. Resolve HEAD's branch + current commit.
    let head_name = FullName::new("HEAD").map_err(io_err)?;
    let head_ref = repo
        .refs()
        .read(&head_name)
        .map_err(io_err)?
        .ok_or_else(|| io::Error::other("HEAD missing"))?;
    let branch_name = match head_ref.target {
        RefTarget::Symbolic(b) => b,
        RefTarget::Direct(_) => {
            eprintln!("rustygit: merge: detached HEAD not supported in M13");
            return Ok(128);
        }
    };
    let ours_oid = match repo.refs().read(&branch_name).map_err(io_err)? {
        Some(r) => match r.target {
            RefTarget::Direct(o) => o,
            RefTarget::Symbolic(_) => {
                eprintln!("rustygit: merge: branch points at symbolic ref");
                return Ok(128);
            }
        },
        None => {
            eprintln!("rustygit: merge: no initial commit yet");
            return Ok(128);
        }
    };

    // 2. Resolve target.
    let theirs_oid = revparse::resolve(repo.refs(), repo.odb(), &args.target)
        .map_err(|e| io::Error::other(format!("not a valid object name: {e}")))?;

    if ours_oid == theirs_oid {
        if !args.quiet {
            println!("Already up to date.");
        }
        return Ok(0);
    }

    // 3. Merge base.
    let base_oid = merge_base(&repo, ours_oid, theirs_oid).map_err(io_err)?;

    // 4. Special cases.
    if let Some(b) = base_oid {
        if b == theirs_oid {
            if !args.quiet {
                println!("Already up to date.");
            }
            return Ok(0);
        }
        if b == ours_oid {
            // Fast-forward.
            return do_fast_forward(&repo, &branch_name, ours_oid, theirs_oid, args.quiet);
        }
    }

    if args.ff_only {
        eprintln!("fatal: Not possible to fast-forward, aborting.");
        return Ok(128);
    }

    // 5. Three-way merge.
    let base_tree = match base_oid {
        Some(b) => Some(commit_to_tree(&repo, b)?),
        None => None,
    };
    let ours_tree = commit_to_tree(&repo, ours_oid)?;
    let theirs_tree = commit_to_tree(&repo, theirs_oid)?;

    let branch_short = short_branch(&branch_name);
    let target_short = first_line_summary(&args.target);
    let labels = FileMergeLabels {
        base: "base",
        ours: branch_short,
        theirs: &target_short,
    };

    let outcome = merge_tree(&repo, base_tree, ours_tree, theirs_tree, &labels).map_err(io_err)?;

    if outcome.has_conflicts {
        // Materialize the workdir with conflict markers.
        materialize_conflicted_workdir(&repo, &outcome)?;
        // Write the index (stages 1/2/3 for conflicts).
        outcome.index.write(&repo).map_err(io_err)?;
        // Record MERGE_HEAD so a subsequent commit knows what we're merging.
        write_merge_head(&repo, theirs_oid)?;
        write_merge_msg(&repo, &args.message, &args.target)?;

        let n_conflicts = outcome
            .paths
            .iter()
            .filter(|p| {
                matches!(
                    p.state,
                    PathMergeState::ContentConflict { .. }
                        | PathMergeState::ModifyDelete
                        | PathMergeState::AddAdd
                        | PathMergeState::TypeMismatch
                )
            })
            .count();
        for p in &outcome.paths {
            let kind = match p.state {
                PathMergeState::ContentConflict { .. } => "CONFLICT (content)",
                PathMergeState::ModifyDelete => "CONFLICT (modify/delete)",
                PathMergeState::AddAdd => "CONFLICT (add/add)",
                PathMergeState::TypeMismatch => "CONFLICT (type)",
                _ => continue,
            };
            eprintln!(
                "{kind}: Merge conflict in {}",
                String::from_utf8_lossy(&p.path)
            );
        }
        eprintln!(
            "Automatic merge failed; fix conflicts and then commit the result. ({n_conflicts} conflict{})",
            if n_conflicts == 1 { "" } else { "s" }
        );
        return Ok(1);
    }

    // Clean merge → write a merge commit.
    let merged_tree = outcome
        .merged_tree
        .ok_or_else(|| io::Error::other("no merged tree from clean merge"))?;

    // Update the workdir to the merged tree.
    let unpack_opts = UnpackOpts {
        force: false,
        keep_extra: false,
        update_workdir: true,
        update_index: true,
    };
    checkout_tree(&repo, merged_tree, &unpack_opts).map_err(io_err)?;

    // pre-merge-commit: after the merge has been computed cleanly, before
    // creating the merge commit. Non-zero aborts. No params.
    {
        let runner = HookRunner::from_repo(&repo);
        let outcome = runner.run("pre-merge-commit", &[], None)?;
        if outcome.aborts_parent() {
            let code = outcome.exit_code().unwrap_or(1);
            hooks::print_abort("merge", "pre-merge-commit", code);
            return Ok(1);
        }
    }

    let config = crate::config::Config::from_repo_dir(repo.gitdir()).map_err(io_err)?;
    let now = Time::now_local();
    let author = Signature::author_from_env_or_config(&config, now).map_err(io_err)?;
    let committer = Signature::committer_from_env_or_config(&config, now).map_err(io_err)?;

    let message = args
        .message
        .unwrap_or_else(|| format!("Merge {} into {}", args.target, branch_short));
    let mut body_msg = message.into_bytes();
    if !body_msg.ends_with(b"\n") {
        body_msg.push(b'\n');
    }

    let commit = Commit {
        tree: merged_tree,
        parents: vec![ours_oid, theirs_oid],
        author,
        committer,
        message: body_msg,
        encoding: None,
        gpgsig: None,
    };
    let commit_obj = commit.to_object();
    let commit_oid = repo.odb().write(&commit_obj).map_err(io_err)?;

    let mut tx = repo.refs().transaction();
    let reflog = ReflogMessage::from(format!(
        "merge {}: Merge made by the 'recursive' strategy.",
        args.target
    ));
    tx.update(
        &branch_name,
        ExpectedOldValue::Direct(ours_oid),
        NewValue::Direct(commit_oid),
        reflog,
    )
    .map_err(io_err)?;
    tx.commit().map_err(io_err)?;

    if !args.quiet {
        println!("Merge made by the 'recursive' strategy.");
    }
    let _ = hash_kind;

    // post-merge: best-effort. argv = "0" (not a squash merge).
    fire_post_merge(&repo, /*squash=*/ false);

    Ok(0)
}

fn fire_post_merge(repo: &Repository, squash: bool) {
    let runner = HookRunner::from_repo(repo);
    let flag = if squash { "1" } else { "0" };
    match runner.run("post-merge", &[flag], None) {
        Ok(crate::hooks::HookOutcome::Ran { exit_code }) if exit_code != 0 => {
            hooks::print_warning("merge", "post-merge", exit_code);
        }
        _ => {}
    }
}

fn do_fast_forward(
    repo: &Repository,
    branch: &FullName,
    old: ObjectId,
    new: ObjectId,
    quiet: bool,
) -> io::Result<i32> {
    let tree = commit_to_tree(repo, new)?;
    let opts = UnpackOpts {
        force: false,
        keep_extra: false,
        update_workdir: true,
        update_index: true,
    };
    checkout_tree(repo, tree, &opts).map_err(io_err)?;

    let mut tx = repo.refs().transaction();
    tx.update(
        branch,
        ExpectedOldValue::Direct(old),
        NewValue::Direct(new),
        ReflogMessage::from(format!("merge {}: fast-forward", short_oid(&new))),
    )
    .map_err(io_err)?;
    tx.commit().map_err(io_err)?;
    if !quiet {
        println!("Updating {}..{}", short_oid(&old), short_oid(&new));
        println!("Fast-forward");
    }

    // post-merge fires for fast-forward merges too (just like upstream git).
    fire_post_merge(repo, /*squash=*/ false);

    Ok(0)
}

fn commit_to_tree(repo: &Repository, oid: ObjectId) -> io::Result<ObjectId> {
    let obj = repo
        .odb()
        .read(&oid)
        .map_err(|e| io::Error::other(format!("{e}")))?;
    if obj.kind != ObjectKind::Commit {
        return Err(io::Error::other(format!("{oid} is not a commit")));
    }
    let commit =
        Commit::parse(&obj.data, repo.hash_kind()).map_err(|e| io::Error::other(format!("{e}")))?;
    Ok(commit.tree)
}

fn materialize_conflicted_workdir(repo: &Repository, outcome: &MergeOutcome) -> io::Result<()> {
    for p in &outcome.paths {
        // The path's workdir representation depends on its state:
        //   - ContentConflict carries the conflict-marker blob oid directly.
        //   - MergedCleanly carries the merged blob oid.
        //   - AddAdd: write the "ours" side as the workdir snapshot (matches
        //     git which leaves stage-2's literal bytes — git itself emits
        //     conflict markers here too if both sides are text, but our
        //     simpler approach is acceptable for M13).
        let target_oid = match &p.state {
            PathMergeState::ContentConflict { conflict_body_oid } => Some(*conflict_body_oid),
            PathMergeState::MergedCleanly { new_oid } => Some(*new_oid),
            PathMergeState::AddAdd => outcome
                .index
                .entries
                .iter()
                .find(|e| e.path == p.path && e.stage == 2)
                .map(|e| e.oid),
            _ => None,
        };
        if let Some(oid) = target_oid {
            let obj = repo
                .odb()
                .read(&oid)
                .map_err(|e| io::Error::other(format!("{e}")))?;
            let rel = bytes_to_path_checked(&p.path).map_err(io_err)?;
            let abs = repo.workdir().join(rel);
            if let Some(parent) = abs.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&abs, &obj.data)?;
        }
    }
    Ok(())
}

fn write_merge_head(repo: &Repository, theirs: ObjectId) -> io::Result<()> {
    let path = repo.gitdir().join("MERGE_HEAD");
    std::fs::write(&path, format!("{theirs}\n"))
}

fn write_merge_msg(repo: &Repository, user_msg: &Option<String>, target: &str) -> io::Result<()> {
    let path = repo.gitdir().join("MERGE_MSG");
    let body = match user_msg {
        Some(m) => m.clone(),
        None => format!("Merge branch '{target}'"),
    };
    let mut f = std::fs::File::create(&path)?;
    f.write_all(body.as_bytes())?;
    if !body.ends_with('\n') {
        f.write_all(b"\n")?;
    }
    Ok(())
}

fn short_oid(oid: &ObjectId) -> String {
    oid.short_hex(7)
}

fn short_branch(full: &FullName) -> &str {
    full.as_str()
        .strip_prefix("refs/heads/")
        .unwrap_or(full.as_str())
}

fn first_line_summary(s: &str) -> String {
    s.lines().next().unwrap_or("").to_string()
}

fn bytes_to_path(b: &[u8]) -> std::path::PathBuf {
    #[cfg(unix)]
    {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;
        std::path::PathBuf::from(OsStr::from_bytes(b))
    }
    #[cfg(not(unix))]
    {
        std::path::PathBuf::from(String::from_utf8_lossy(b).into_owned())
    }
}

/// Strict variant: refuses non-UTF-8 names on non-Unix hosts. Identical to
/// `bytes_to_path` on Unix.
fn bytes_to_path_checked(b: &[u8]) -> Result<std::path::PathBuf, crate::unpack_trees::UnpackError> {
    #[cfg(unix)]
    {
        Ok(bytes_to_path(b))
    }
    #[cfg(not(unix))]
    {
        match std::str::from_utf8(b) {
            Ok(s) => Ok(std::path::PathBuf::from(s)),
            Err(_) => Err(crate::unpack_trees::UnpackError::PathEncodingError {
                bytes: b.to_vec(),
                op: "merge".to_string(),
            }),
        }
    }
}

fn io_err<E: std::fmt::Display>(e: E) -> io::Error {
    io::Error::other(format!("{e}"))
}
