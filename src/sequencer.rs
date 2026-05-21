//! Sequencer primitives.
//!
//! This module implements the "apply one commit on top of HEAD" operation that
//! sits at the heart of `cherry-pick` and `rebase`. The operation is split into
//! two layers:
//!
//! 1. [`apply_commit`] — pure single-commit application:
//!    * Computes the three-way merge `(C.parent.tree, HEAD.tree, C.tree)`.
//!    * Writes the merge result either as a new commit (clean) or as a
//!      conflicted workdir+index pair (with `CHERRY_PICK_HEAD` so a follow-up
//!      `commit` preserves authorship).
//!
//! 2. [`State`] + [`abort`] / [`cont`] — persistent state for multi-commit
//!    sequences. The state lives under `<gitdir>/sequencer/` so that
//!    `--continue` / `--abort` work across process restarts.
//!
//! ## State-file layout
//!
//! Plain text, deterministic ordering. Lives under `<gitdir>/sequencer/`:
//!
//! | file        | contents                                                  |
//! |-------------|-----------------------------------------------------------|
//! | `head-name` | branch full-name like `refs/heads/feature\n`              |
//! | `orig-head` | 40-char hex oid + newline (original HEAD pre-sequence)    |
//! | `onto`      | 40-char hex oid + newline (where we're applying onto)     |
//! | `todo`      | one `pick <oid> <subject>` per line (commits remaining)   |
//! | `done`      | same format as `todo` (commits already applied)           |
//! | `current`   | hex oid of the commit currently mid-apply (if conflicted) |
//!
//! ## `CHERRY_PICK_HEAD`
//!
//! When `apply_commit` hits a conflict, it writes `<gitdir>/CHERRY_PICK_HEAD`
//! holding the oid of the commit being applied, plus `MERGE_MSG` holding the
//! commit's message. This matches git's invariant that a follow-up `commit`
//! after the user resolves conflicts can preserve authorship.

use std::path::PathBuf;

use thiserror::Error;

use crate::commit::Commit;
use crate::config::Config;
use crate::hash::ObjectId;
use crate::identity::{Signature, Time};
use crate::merge::file::FileMergeLabels;
use crate::merge::tree::{merge_tree, MergeOutcome, PathMergeState};
use crate::object::ObjectKind;
use crate::refs::{ExpectedOldValue, FullName, NewValue, RefTarget, ReflogMessage};
use crate::repo::Repository;
use crate::unpack_trees::{checkout_tree, UnpackOpts};

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Result of [`apply_commit`].
#[derive(Debug)]
pub enum ApplyOutcome {
    /// Commit applied cleanly; HEAD updated to the new commit oid.
    Done { new_commit: ObjectId },
    /// Conflicts; workdir and index reflect the partial merge. Caller should
    /// stop the sequence and let the user resolve.
    Conflicted { offending_paths: Vec<Vec<u8>> },
    /// The commit is empty after applying (its diff is identical to HEAD's
    /// state). Caller decides whether to skip (--allow-empty=keep) or stop.
    Empty,
}

/// Options for [`apply_commit`].
#[derive(Debug, Clone)]
pub struct ApplyOpts {
    /// If true, preserve C.author as the new commit's author. (Cherry-pick default.)
    /// If false, use the current identity. Rebase uses true unless `--reset-author`.
    pub preserve_author: bool,
    /// Optional message override. Default: use C's message verbatim.
    pub override_message: Option<String>,
    /// Label for the "theirs" side in conflict markers (e.g. `cherry-pick <oid>`
    /// or the upstream branch name during rebase).
    pub theirs_label: String,
    /// Revert mode: apply the INVERSE of C's diff (HEAD - C.tree + C.parent.tree).
    /// Default false (cherry-pick). When true, conflict markers land in
    /// `REVERT_HEAD` instead of `CHERRY_PICK_HEAD`, and the default message
    /// becomes `Revert "<title>"\n\nThis reverts commit <oid>.\n`.
    pub revert: bool,
    /// `--mainline N` for revert / cherry-pick of merge commits: picks
    /// `C.parents[N-1]` as the "kept" parent. `None` means single-parent
    /// only; multi-parent commits error out.
    pub mainline: Option<usize>,
}

impl Default for ApplyOpts {
    fn default() -> Self {
        Self {
            preserve_author: true,
            override_message: None,
            theirs_label: "theirs".into(),
            revert: false,
            mainline: None,
        }
    }
}

/// Sequencer state on disk under `<gitdir>/sequencer/`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct State {
    /// The branch we're operating on (e.g. `refs/heads/feature`).
    pub head_branch: FullName,
    /// HEAD's oid BEFORE the sequence began (for `--abort` restore).
    pub orig_head: ObjectId,
    /// Where we're applying onto (rebase `--onto`, or HEAD-when-started for cherry-pick).
    pub onto: ObjectId,
    /// Commits yet to apply, in order.
    pub todo: Vec<ObjectId>,
    /// Commits already applied successfully.
    pub done: Vec<ObjectId>,
    /// The commit that's currently mid-apply (set when we hit a conflict).
    pub in_progress: Option<ObjectId>,
    /// True when the sequence is a `revert` (inverse diff); false for cherry-pick / rebase.
    /// Persisted so `--continue` resumes with the right semantics.
    pub revert: bool,
}

/// Result of [`cont`].
#[derive(Debug)]
pub enum ContinueOutcome {
    /// All remaining commits applied; sequence finished cleanly.
    Done,
    /// Hit another conflict at this commit.
    Conflicted {
        commit: ObjectId,
        offending_paths: Vec<Vec<u8>>,
    },
}

/// Errors from the sequencer.
#[derive(Error, Debug)]
pub enum SequencerError {
    #[error(transparent)]
    Odb(#[from] crate::odb::OdbError),
    #[error(transparent)]
    Commit(#[from] crate::commit::CommitError),
    #[error(transparent)]
    Tree(#[from] crate::tree::TreeError),
    #[error(transparent)]
    Index(#[from] crate::index::IndexError),
    #[error(transparent)]
    Refs(#[from] crate::refs::RefError),
    #[error(transparent)]
    Hash(#[from] crate::hash::HashError),
    #[error(transparent)]
    Unpack(#[from] crate::unpack_trees::UnpackError),
    #[error(transparent)]
    TreeMerge(#[from] crate::merge::tree::TreeMergeError),
    #[error(transparent)]
    Identity(#[from] crate::identity::IdentityError),
    #[error(transparent)]
    Config(#[from] crate::config::ConfigError),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("HEAD is detached and the operation requires a branch")]
    DetachedHead,
    #[error("commit {0} not found")]
    NoSuchCommit(ObjectId),
    #[error("no sequencer state to resume")]
    NoState,
    #[error("sequencer state is corrupt: {0}")]
    Corrupt(String),
    #[error("commit {0} is a merge — pass -m/--mainline <N> to pick which parent's diff to keep")]
    MergeNeedsMainline(ObjectId),
}

// ---------------------------------------------------------------------------
// apply_commit
// ---------------------------------------------------------------------------

/// Apply commit `to_apply` on top of HEAD. Updates HEAD's branch ref on
/// success. Caller is responsible for setting up workdir/index/refs before
/// the call (e.g. for rebase: ensure HEAD is at the rebase target first).
///
/// Algorithm:
/// 1. Read C → get C.tree, C.parents\[0\] (if any), C.author, C.message.
/// 2. base = C's parent's tree (the state BEFORE the change C introduced).
///    For a root commit (no parents), base is the empty tree.
/// 3. ours = HEAD's current commit's tree.
/// 4. theirs = C.tree.
/// 5. Call `merge_tree(repo, base, ours, theirs, labels)`.
/// 6. If clean: write a new commit on top of HEAD, update the branch, checkout
///    the merged tree.
/// 7. If conflicts: materialize markers, write CHERRY_PICK_HEAD + MERGE_MSG,
///    leave HEAD alone.
pub fn apply_commit(
    repo: &Repository,
    to_apply: ObjectId,
    opts: &ApplyOpts,
) -> Result<ApplyOutcome, SequencerError> {
    // 1. Resolve HEAD's branch + tip oid.
    let (head_branch, head_oid) = current_head(repo)?;
    let head_tree = read_commit_tree(repo, head_oid)?;

    // 2. Read the commit we're applying.
    let to_apply_commit = read_commit(repo, to_apply)?;

    // 3. Pick `base` and `theirs` trees.
    //
    // Cherry-pick (revert=false): base = C's parent's tree, theirs = C's tree.
    //   → 3-way merge applies C's diff (parent → C) on top of HEAD.
    //
    // Revert (revert=true): base = C's tree, theirs = C's parent's tree.
    //   → 3-way merge applies the INVERSE of C's diff (C → parent) on top of HEAD.
    //
    // For merge commits (≥2 parents), the caller MUST pass `--mainline N`
    // to disambiguate which parent's diff is being kept; otherwise we
    // refuse rather than silently picking parents[0].
    //
    // Root commits in revert mode have no parent tree; we use the empty tree
    // as theirs (so the revert removes everything the root commit added).
    let parent_oid_for_base = match (to_apply_commit.parents.len(), opts.mainline) {
        (0, _) => None,
        (1, _) => Some(to_apply_commit.parents[0]),
        (_, Some(n)) => {
            if n == 0 || n > to_apply_commit.parents.len() {
                return Err(SequencerError::Corrupt(format!(
                    "mainline {n} out of range for commit {to_apply} (has {} parents)",
                    to_apply_commit.parents.len()
                )));
            }
            Some(to_apply_commit.parents[n - 1])
        }
        (_, None) => {
            return Err(SequencerError::MergeNeedsMainline(to_apply));
        }
    };
    let parent_tree = match parent_oid_for_base {
        Some(p) => Some(read_commit_tree(repo, p)?),
        None => None,
    };
    let (base_tree, theirs_tree) = if opts.revert {
        (
            Some(to_apply_commit.tree),
            parent_tree.unwrap_or_else(empty_tree_oid),
        )
    } else {
        (parent_tree, to_apply_commit.tree)
    };

    // 4. If theirs_tree == head_tree, this commit's diff is empty relative
    //    to HEAD. (Common case: the change is already present.)
    if theirs_tree == head_tree {
        return Ok(ApplyOutcome::Empty);
    }

    // 5. Run the 3-way merge.
    let theirs_label = opts.theirs_label.clone();
    let labels = FileMergeLabels {
        base: "base",
        ours: "HEAD",
        theirs: theirs_label.as_str(),
    };
    let outcome = merge_tree(repo, base_tree, head_tree, theirs_tree, &labels)?;

    if outcome.has_conflicts {
        materialize_conflicted_workdir(repo, &outcome)?;
        outcome.index.write(repo)?;
        if opts.revert {
            write_revert_head(repo, to_apply)?;
        } else {
            write_cherry_pick_head(repo, to_apply)?;
        }
        // The MERGE_MSG body matches git: cherry-pick uses C's message
        // verbatim; revert uses the canonical revert message.
        let merge_msg = if opts.revert {
            canonical_revert_message(to_apply, &to_apply_commit.message)
        } else {
            to_apply_commit.message.clone()
        };
        write_merge_msg(repo, &merge_msg)?;
        let offending = collect_conflicted_paths(&outcome);
        return Ok(ApplyOutcome::Conflicted {
            offending_paths: offending,
        });
    }

    // 6. Clean merge.
    let merged_tree = outcome
        .merged_tree
        .ok_or_else(|| SequencerError::Corrupt("clean merge produced no tree oid".to_string()))?;

    // If the merged tree equals HEAD's tree, the commit is empty.
    if merged_tree == head_tree {
        return Ok(ApplyOutcome::Empty);
    }

    // Update the workdir + index to the merged tree.
    let unpack_opts = UnpackOpts {
        force: false,
        keep_extra: false,
        update_workdir: true,
        update_index: true,
    };
    checkout_tree(repo, merged_tree, &unpack_opts)?;

    // 7. Build the new commit.
    let config = Config::from_repo_dir(repo.gitdir())?;
    let now = Time::now_local();
    let committer = Signature::committer_from_env_or_config(&config, now)?;
    let author = if opts.preserve_author {
        to_apply_commit.author.clone()
    } else {
        Signature::author_from_env_or_config(&config, now)?
    };

    let mut message = match &opts.override_message {
        Some(m) => m.as_bytes().to_vec(),
        None if opts.revert => canonical_revert_message(to_apply, &to_apply_commit.message),
        None => to_apply_commit.message.clone(),
    };
    if !message.ends_with(b"\n") {
        message.push(b'\n');
    }

    let new_commit = Commit {
        tree: merged_tree,
        parents: vec![head_oid],
        author,
        committer,
        message,
        encoding: None,
        gpgsig: None,
    };
    let new_obj = new_commit.to_object();
    let new_oid = repo.odb().write(&new_obj)?;

    // 8. Update branch atomically with reflog.
    let mut tx = repo.refs().transaction();
    let summary = first_line(&to_apply_commit.message);
    // Reflog tag: rebase uses `commit:`, cherry-pick uses `cherry-pick:`,
    // revert uses `revert:` — matches upstream wording.
    let reflog_tag = if opts.revert {
        "revert"
    } else if opts.preserve_author {
        "cherry-pick"
    } else {
        "commit"
    };
    let reflog_summary = if opts.revert {
        format!("Revert \"{}\"", summary)
    } else {
        summary
    };
    tx.update(
        &head_branch,
        ExpectedOldValue::Direct(head_oid),
        NewValue::Direct(new_oid),
        ReflogMessage::from(format!("{}: {}", reflog_tag, reflog_summary)),
    )?;
    tx.commit()?;

    // 9. Clean up any leftover CHERRY_PICK_HEAD/REVERT_HEAD/MERGE_MSG.
    let _ = std::fs::remove_file(repo.gitdir().join("CHERRY_PICK_HEAD"));
    let _ = std::fs::remove_file(repo.gitdir().join("REVERT_HEAD"));
    let _ = std::fs::remove_file(repo.gitdir().join("MERGE_MSG"));

    Ok(ApplyOutcome::Done {
        new_commit: new_oid,
    })
}

// ---------------------------------------------------------------------------
// State (on-disk sequencer dir)
// ---------------------------------------------------------------------------

impl State {
    /// Path: `<gitdir>/sequencer/`.
    pub fn dir(repo: &Repository) -> PathBuf {
        repo.gitdir().join("sequencer")
    }

    /// Write all state files atomically (one file at a time; we don't bother
    /// with a cross-file lock — the entire dir is owned by an in-flight
    /// sequence anyway).
    pub fn save(&self, repo: &Repository) -> Result<(), SequencerError> {
        let dir = Self::dir(repo);
        std::fs::create_dir_all(&dir)?;

        std::fs::write(dir.join("head-name"), format!("{}\n", self.head_branch))?;
        std::fs::write(dir.join("orig-head"), format!("{}\n", self.orig_head))?;
        std::fs::write(dir.join("onto"), format!("{}\n", self.onto))?;

        let todo = format_todo(&self.todo, repo)?;
        std::fs::write(dir.join("todo"), todo)?;

        let done = format_todo(&self.done, repo)?;
        std::fs::write(dir.join("done"), done)?;

        match self.in_progress {
            Some(oid) => std::fs::write(dir.join("current"), format!("{}\n", oid))?,
            None => {
                let _ = std::fs::remove_file(dir.join("current"));
            }
        }

        // `mode` distinguishes revert from cherry-pick; absent file ⇒
        // cherry-pick (back-compat for older sequencer state).
        if self.revert {
            std::fs::write(dir.join("mode"), "revert\n")?;
        } else {
            let _ = std::fs::remove_file(dir.join("mode"));
        }
        Ok(())
    }

    /// Read all state files back. Errors with `NoState` if the directory is
    /// absent, `Corrupt` if any file is malformed.
    pub fn load(repo: &Repository) -> Result<Self, SequencerError> {
        let dir = Self::dir(repo);
        if !dir.is_dir() {
            return Err(SequencerError::NoState);
        }
        let hk = repo.hash_kind();

        let head_branch_raw = read_trim(&dir.join("head-name"))?;
        let head_branch = FullName::new(head_branch_raw.clone())
            .map_err(|e| SequencerError::Corrupt(format!("head-name {head_branch_raw:?}: {e}")))?;
        let orig_head_raw = read_trim(&dir.join("orig-head"))?;
        let orig_head = ObjectId::parse_hex(hk, &orig_head_raw)
            .map_err(|e| SequencerError::Corrupt(format!("orig-head: {e}")))?;
        let onto_raw = read_trim(&dir.join("onto"))?;
        let onto = ObjectId::parse_hex(hk, &onto_raw)
            .map_err(|e| SequencerError::Corrupt(format!("onto: {e}")))?;

        let todo_text = match std::fs::read_to_string(dir.join("todo")) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(e.into()),
        };
        let todo = parse_todo(&todo_text, hk)?;
        let done_text = match std::fs::read_to_string(dir.join("done")) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(e.into()),
        };
        let done = parse_todo(&done_text, hk)?;

        let in_progress = match std::fs::read_to_string(dir.join("current")) {
            Ok(s) => Some(
                ObjectId::parse_hex(hk, s.trim())
                    .map_err(|e| SequencerError::Corrupt(format!("current: {e}")))?,
            ),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(e.into()),
        };

        let revert = match std::fs::read_to_string(dir.join("mode")) {
            Ok(s) => s.trim() == "revert",
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
            Err(e) => return Err(e.into()),
        };

        Ok(Self {
            head_branch,
            orig_head,
            onto,
            todo,
            done,
            in_progress,
            revert,
        })
    }

    /// True iff a sequencer dir exists.
    pub fn exists(repo: &Repository) -> bool {
        Self::dir(repo).is_dir()
    }

    /// Remove the state directory (after `--abort` or successful completion).
    pub fn cleanup(repo: &Repository) -> Result<(), SequencerError> {
        let dir = Self::dir(repo);
        if dir.exists() {
            std::fs::remove_dir_all(&dir)?;
        }
        // Also tidy up the CHERRY_PICK_HEAD / REVERT_HEAD / MERGE_MSG markers if present.
        let _ = std::fs::remove_file(repo.gitdir().join("CHERRY_PICK_HEAD"));
        let _ = std::fs::remove_file(repo.gitdir().join("REVERT_HEAD"));
        let _ = std::fs::remove_file(repo.gitdir().join("MERGE_MSG"));
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// abort / cont
// ---------------------------------------------------------------------------

/// `--abort`: restore HEAD to `orig_head`, clear state.
pub fn abort(repo: &Repository) -> Result<(), SequencerError> {
    let state = State::load(repo)?;

    // Update the branch ref back to orig_head (Any expected old-value: the
    // current ref might be the original or anything we wrote during the
    // sequence, so we don't enforce a particular old value).
    let mut tx = repo.refs().transaction();
    tx.update(
        &state.head_branch,
        ExpectedOldValue::Any,
        NewValue::Direct(state.orig_head),
        ReflogMessage::from("sequencer: aborting".to_string()),
    )?;
    tx.commit()?;

    // Reset workdir + index to orig_head's tree (force=true to nuke any
    // half-merged state the user might have).
    let orig_tree = read_commit_tree(repo, state.orig_head)?;
    let unpack_opts = UnpackOpts {
        force: true,
        keep_extra: false,
        update_workdir: true,
        update_index: true,
    };
    checkout_tree(repo, orig_tree, &unpack_opts)?;

    State::cleanup(repo)?;
    Ok(())
}

/// `--continue`: pick up where we left off. Caller asserts conflicts are
/// resolved (we don't verify; that's git's behavior too). Re-runs from `todo`.
///
/// If `in_progress` is set, we treat it as committed already (the user just
/// ran `commit` to finalize the previously-conflicted apply) — caller is
/// responsible for that. We then drain `todo`.
pub fn cont(repo: &Repository) -> Result<ContinueOutcome, SequencerError> {
    let mut state = State::load(repo)?;

    // If we were mid-conflict, fold that commit into `done` (the user
    // presumably resolved + committed before invoking --continue).
    if let Some(commit) = state.in_progress.take() {
        state.done.push(commit);
        state.save(repo)?;
    }

    let opts = ApplyOpts {
        revert: state.revert,
        ..ApplyOpts::default()
    };
    while let Some(next) = state.todo.first().copied() {
        match apply_commit(repo, next, &opts)? {
            ApplyOutcome::Done { .. } => {
                state.todo.remove(0);
                state.done.push(next);
                state.save(repo)?;
            }
            ApplyOutcome::Empty => {
                // Skip empty commits silently (matches `--allow-empty=drop`).
                state.todo.remove(0);
                state.save(repo)?;
            }
            ApplyOutcome::Conflicted { offending_paths } => {
                state.in_progress = Some(next);
                state.todo.remove(0);
                state.save(repo)?;
                return Ok(ContinueOutcome::Conflicted {
                    commit: next,
                    offending_paths,
                });
            }
        }
    }

    State::cleanup(repo)?;
    Ok(ContinueOutcome::Done)
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

fn current_head(repo: &Repository) -> Result<(FullName, ObjectId), SequencerError> {
    let head_name = FullName::new("HEAD").map_err(|e| SequencerError::Corrupt(e.to_string()))?;
    let head_ref = repo
        .refs()
        .read(&head_name)?
        .ok_or_else(|| SequencerError::Corrupt("HEAD missing".into()))?;
    let branch_name = match head_ref.target {
        RefTarget::Symbolic(b) => b,
        RefTarget::Direct(_) => return Err(SequencerError::DetachedHead),
    };
    let oid = match repo.refs().read(&branch_name)? {
        Some(r) => match r.target {
            RefTarget::Direct(o) => o,
            RefTarget::Symbolic(_) => {
                return Err(SequencerError::Corrupt(format!(
                    "{branch_name} resolves to symbolic ref"
                )));
            }
        },
        None => {
            return Err(SequencerError::Corrupt(format!(
                "{branch_name} does not exist (no initial commit?)"
            )));
        }
    };
    Ok((branch_name, oid))
}

fn read_commit(repo: &Repository, oid: ObjectId) -> Result<Commit, SequencerError> {
    let obj = repo
        .odb()
        .read(&oid)
        .map_err(|_| SequencerError::NoSuchCommit(oid))?;
    if obj.kind != ObjectKind::Commit {
        return Err(SequencerError::Corrupt(format!(
            "{oid} is not a commit (kind={:?})",
            obj.kind
        )));
    }
    Ok(Commit::parse(&obj.data, repo.hash_kind())?)
}

fn read_commit_tree(repo: &Repository, oid: ObjectId) -> Result<ObjectId, SequencerError> {
    Ok(read_commit(repo, oid)?.tree)
}

fn collect_conflicted_paths(outcome: &MergeOutcome) -> Vec<Vec<u8>> {
    outcome
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
        .map(|p| p.path.clone())
        .collect()
}

fn materialize_conflicted_workdir(
    repo: &Repository,
    outcome: &MergeOutcome,
) -> Result<(), SequencerError> {
    for p in &outcome.paths {
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
            let obj = repo.odb().read(&oid)?;
            let abs = repo
                .workdir()
                .join(bytes_to_path_checked(&p.path, "merge-write")?);
            if let Some(parent) = abs.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&abs, &obj.data)?;
        }
    }
    Ok(())
}

fn write_cherry_pick_head(repo: &Repository, oid: ObjectId) -> Result<(), SequencerError> {
    let path = repo.gitdir().join("CHERRY_PICK_HEAD");
    std::fs::write(&path, format!("{oid}\n"))?;
    Ok(())
}

fn write_revert_head(repo: &Repository, oid: ObjectId) -> Result<(), SequencerError> {
    let path = repo.gitdir().join("REVERT_HEAD");
    std::fs::write(&path, format!("{oid}\n"))?;
    Ok(())
}

/// Canonical revert commit message — matches `git revert`'s default exactly:
/// `Revert "<first-line>"\n\nThis reverts commit <oid>.\n`.
fn canonical_revert_message(reverted_oid: ObjectId, original_message: &[u8]) -> Vec<u8> {
    let title = first_line(original_message);
    format!("Revert \"{title}\"\n\nThis reverts commit {reverted_oid}.\n").into_bytes()
}

/// Empty-tree oid — used as the "before" tree when reverting a root commit
/// (the inverse direction takes us back to nothing).
fn empty_tree_oid() -> ObjectId {
    // SHA-1 of the empty tree object. Same constant git uses.
    ObjectId::parse_hex(
        crate::hash::HashKind::Sha1,
        "4b825dc642cb6eb9a060e54bf8d69288fbee4904",
    )
    .expect("empty-tree oid is a valid 40-char hex string")
}

fn write_merge_msg(repo: &Repository, message: &[u8]) -> Result<(), SequencerError> {
    let path = repo.gitdir().join("MERGE_MSG");
    let mut body = message.to_vec();
    if !body.ends_with(b"\n") {
        body.push(b'\n');
    }
    std::fs::write(&path, &body)?;
    Ok(())
}

fn first_line(message: &[u8]) -> String {
    let nl = message
        .iter()
        .position(|&b| b == b'\n')
        .unwrap_or(message.len());
    String::from_utf8_lossy(&message[..nl]).into_owned()
}

fn read_trim(path: &std::path::Path) -> Result<String, SequencerError> {
    let bytes = std::fs::read(path).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => {
            SequencerError::Corrupt(format!("missing file {}", path.display()))
        }
        _ => SequencerError::Io(e),
    })?;
    let s = std::str::from_utf8(&bytes)
        .map_err(|e| SequencerError::Corrupt(format!("non-UTF8 in {}: {e}", path.display())))?
        .trim()
        .to_string();
    if s.is_empty() {
        return Err(SequencerError::Corrupt(format!(
            "empty file {}",
            path.display()
        )));
    }
    Ok(s)
}

fn format_todo(oids: &[ObjectId], repo: &Repository) -> Result<String, SequencerError> {
    let mut out = String::new();
    for oid in oids {
        let subject = match read_commit(repo, *oid) {
            Ok(c) => first_line(&c.message),
            Err(_) => String::new(),
        };
        // Match the git todo-file convention: `pick <oid> <subject>\n`.
        out.push_str("pick ");
        out.push_str(&oid.to_string());
        if !subject.is_empty() {
            out.push(' ');
            out.push_str(&subject);
        }
        out.push('\n');
    }
    Ok(out)
}

fn parse_todo(text: &str, hk: crate::hash::HashKind) -> Result<Vec<ObjectId>, SequencerError> {
    let mut out = Vec::new();
    for (lineno, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // Expected form: `pick <oid> [<subject>]`. Be a little permissive about
        // the command — accept lone `<oid>` as well.
        let rest = line.strip_prefix("pick ").unwrap_or(line);
        let oid_str = rest.split_whitespace().next().ok_or_else(|| {
            SequencerError::Corrupt(format!("todo line {} has no oid: {raw:?}", lineno + 1))
        })?;
        let oid = ObjectId::parse_hex(hk, oid_str)
            .map_err(|e| SequencerError::Corrupt(format!("todo line {}: {e}", lineno + 1)))?;
        out.push(oid);
    }
    Ok(out)
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
        // Best-effort lossy decode retained for display-only paths.
        // Mutating callsites should use `bytes_to_path_checked` so
        // non-UTF-8 names refuse explicitly instead of getting silently
        // mangled by U+FFFD substitution.
        std::path::PathBuf::from(String::from_utf8_lossy(b).into_owned())
    }
}

/// Strict variant of [`bytes_to_path`] for sequencer-driven workdir writes.
/// Identical to `bytes_to_path` on Unix; on Windows / other non-Unix
/// platforms it refuses non-UTF-8 names by returning
/// `UnpackError::PathEncodingError` (propagated via
/// `SequencerError::Unpack`). Matches the policy in
/// [`crate::unpack_trees::bytes_to_relpath_checked`].
fn bytes_to_path_checked(b: &[u8], op: &str) -> Result<std::path::PathBuf, SequencerError> {
    #[cfg(unix)]
    {
        let _ = op;
        Ok(bytes_to_path(b))
    }
    #[cfg(not(unix))]
    {
        match std::str::from_utf8(b) {
            Ok(s) => Ok(std::path::PathBuf::from(s)),
            Err(_) => Err(SequencerError::Unpack(
                crate::unpack_trees::UnpackError::PathEncodingError {
                    bytes: b.to_vec(),
                    op: op.to_string(),
                },
            )),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hash::HashKind;
    use crate::identity::{Signature, Time};
    use crate::object::RawObject;
    use crate::tree::{FileMode, Tree, TreeEntry};
    use std::collections::BTreeMap;
    use std::path::Path;
    use std::process::Command;
    use tempfile::TempDir;

    // ---- repo + commit-building scaffolding ----

    fn git_available() -> bool {
        Command::new("git")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    fn make_repo() -> (TempDir, Repository) {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        Command::new("git")
            .args(["init", "-q", "-b", "main", "."])
            .current_dir(dir)
            .output()
            .expect("git init");
        Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(dir)
            .output()
            .ok();
        Command::new("git")
            .args(["config", "user.email", "test@example.com"])
            .current_dir(dir)
            .output()
            .ok();
        let repo = Repository::discover(dir).unwrap();
        (tmp, repo)
    }

    /// Build a flat tree from `path -> content` strings.
    fn make_tree(repo: &Repository, files: &[(&str, &str)]) -> ObjectId {
        // Group entries by leading directory components — but since none of
        // our test paths have slashes here, build a flat tree directly.
        let mut blobs: BTreeMap<Vec<u8>, (FileMode, ObjectId)> = BTreeMap::new();
        for (path, content) in files {
            let oid = repo
                .odb()
                .write(&RawObject::new(
                    ObjectKind::Blob,
                    content.as_bytes().to_vec(),
                ))
                .unwrap();
            blobs.insert(path.as_bytes().to_vec(), (FileMode::Regular, oid));
        }
        if blobs.is_empty() {
            let empty = Tree {
                entries: Vec::new(),
            };
            return repo.odb().write(&empty.to_object()).unwrap();
        }
        // Flat tree: every path is a leaf at the root.
        let mut entries: Vec<TreeEntry> = blobs
            .into_iter()
            .map(|(name, (mode, oid))| TreeEntry { mode, name, oid })
            .collect();
        Tree::sort_entries(&mut entries);
        let tree = Tree::new(entries);
        repo.odb().write(&tree.to_object()).unwrap()
    }

    fn fake_sig(name: &str, secs: i64) -> Signature {
        Signature::new(name, format!("{name}@example.test"), Time::new(secs, 0))
    }

    fn make_commit(
        repo: &Repository,
        tree: ObjectId,
        parents: Vec<ObjectId>,
        author_name: &str,
        secs: i64,
        message: &str,
    ) -> ObjectId {
        let mut msg = message.as_bytes().to_vec();
        if !msg.ends_with(b"\n") {
            msg.push(b'\n');
        }
        let commit = Commit {
            tree,
            parents,
            author: fake_sig(author_name, secs),
            committer: fake_sig(author_name, secs),
            message: msg,
            encoding: None,
            gpgsig: None,
        };
        repo.odb().write(&commit.to_object()).unwrap()
    }

    /// Set the current branch ref (main by default) to `oid`. Also writes the
    /// workdir to match the commit's tree so subsequent `apply_commit` calls
    /// pass the "HEAD's tree" lookup correctly.
    fn point_main_at(repo: &Repository, oid: ObjectId) {
        let main = FullName::new("refs/heads/main").unwrap();
        let mut tx = repo.refs().transaction();
        tx.update(
            &main,
            ExpectedOldValue::Any,
            NewValue::Direct(oid),
            ReflogMessage::from("test: point main"),
        )
        .unwrap();
        tx.commit().unwrap();

        // Ensure HEAD -> refs/heads/main.
        let head = FullName::new("HEAD").unwrap();
        let mut tx = repo.refs().transaction();
        tx.update(
            &head,
            ExpectedOldValue::Any,
            NewValue::Symbolic(main.clone()),
            ReflogMessage::none(),
        )
        .unwrap();
        tx.commit().unwrap();

        // Materialize the tree to workdir+index. `force=true` so we don't choke
        // on whatever the fresh test repo has lying around.
        let tree = read_commit_tree(repo, oid).unwrap();
        let opts = UnpackOpts {
            force: true,
            keep_extra: false,
            update_workdir: true,
            update_index: true,
        };
        checkout_tree(repo, tree, &opts).unwrap();
    }

    fn read_head_oid(repo: &Repository) -> ObjectId {
        let main = FullName::new("refs/heads/main").unwrap();
        match repo.refs().read(&main).unwrap().unwrap().target {
            RefTarget::Direct(o) => o,
            RefTarget::Symbolic(_) => panic!("main resolves to symbolic"),
        }
    }

    // ---- Test 1: apply_commit clean ----

    /// Two histories diverged; cherry-pick one onto the other → clean.
    /// base ──A──> C1 (adds a.txt)
    ///         └──> C2 (adds b.txt)   ← we're on this
    /// Cherry-picking C1 onto C2 should land cleanly with both files present.
    #[test]
    fn apply_commit_clean_disjoint() {
        let (_tmp, repo) = make_repo();

        let base_tree = make_tree(&repo, &[("base.txt", "shared\n")]);
        let base = make_commit(&repo, base_tree, vec![], "Author", 1000, "base");

        let c1_tree = make_tree(&repo, &[("base.txt", "shared\n"), ("a.txt", "from C1\n")]);
        let c1 = make_commit(&repo, c1_tree, vec![base], "AuthorC1", 1100, "add a.txt");

        let c2_tree = make_tree(&repo, &[("base.txt", "shared\n"), ("b.txt", "from C2\n")]);
        let c2 = make_commit(&repo, c2_tree, vec![base], "Author", 1200, "add b.txt");

        point_main_at(&repo, c2);

        let outcome = apply_commit(&repo, c1, &ApplyOpts::default()).unwrap();
        let new_oid = match outcome {
            ApplyOutcome::Done { new_commit } => new_commit,
            other => panic!("expected Done, got {other:?}"),
        };

        // HEAD advanced.
        assert_eq!(read_head_oid(&repo), new_oid);

        // The new commit's tree contains both a.txt and b.txt.
        let new = read_commit(&repo, new_oid).unwrap();
        let merged = flatten_tree_for_test(&repo, new.tree);
        assert!(merged.iter().any(|(p, _)| p == b"a.txt"));
        assert!(merged.iter().any(|(p, _)| p == b"b.txt"));

        // The parent is c2 (HEAD-at-time-of-apply), NOT c1's old parent.
        assert_eq!(new.parents, vec![c2]);

        // Workdir has both files.
        assert!(repo.workdir().join("a.txt").exists());
        assert!(repo.workdir().join("b.txt").exists());
    }

    // ---- Test 2: apply_commit with content conflict ----

    /// Both commits modify the same line of foo.txt differently → conflict.
    #[test]
    fn apply_commit_content_conflict() {
        let (_tmp, repo) = make_repo();

        let base_tree = make_tree(&repo, &[("foo.txt", "line\n")]);
        let base = make_commit(&repo, base_tree, vec![], "A", 1000, "base");

        let c1_tree = make_tree(&repo, &[("foo.txt", "C1 line\n")]);
        let c1 = make_commit(&repo, c1_tree, vec![base], "A", 1100, "C1");

        let c2_tree = make_tree(&repo, &[("foo.txt", "C2 line\n")]);
        let c2 = make_commit(&repo, c2_tree, vec![base], "A", 1200, "C2");

        point_main_at(&repo, c2);

        let outcome = apply_commit(&repo, c1, &ApplyOpts::default()).unwrap();
        match outcome {
            ApplyOutcome::Conflicted { offending_paths } => {
                assert_eq!(offending_paths, vec![b"foo.txt".to_vec()]);
            }
            other => panic!("expected Conflicted, got {other:?}"),
        }
        // HEAD did NOT advance.
        assert_eq!(read_head_oid(&repo), c2);

        // CHERRY_PICK_HEAD records the commit we tried to apply.
        let chp = std::fs::read_to_string(repo.gitdir().join("CHERRY_PICK_HEAD")).unwrap();
        assert_eq!(chp.trim(), c1.to_string());

        // MERGE_MSG contains C1's message.
        let mmsg = std::fs::read_to_string(repo.gitdir().join("MERGE_MSG")).unwrap();
        assert!(mmsg.contains("C1"));
    }

    // ---- Test 3: apply_commit empty (already-present change) ----

    /// Cherry-picking a commit whose diff is already in HEAD should be Empty.
    #[test]
    fn apply_commit_empty_when_already_present() {
        let (_tmp, repo) = make_repo();

        let base_tree = make_tree(&repo, &[("a.txt", "x\n")]);
        let base = make_commit(&repo, base_tree, vec![], "A", 1000, "base");

        // C1 changes a.txt to "y\n".
        let c1_tree = make_tree(&repo, &[("a.txt", "y\n")]);
        let c1 = make_commit(&repo, c1_tree, vec![base], "A", 1100, "C1");

        // C2 ALSO changes a.txt to "y\n" (identical effect).
        let c2 = make_commit(&repo, c1_tree, vec![base], "A", 1200, "C2 same change");

        point_main_at(&repo, c2);

        let outcome = apply_commit(&repo, c1, &ApplyOpts::default()).unwrap();
        assert!(matches!(outcome, ApplyOutcome::Empty));
        // HEAD still at c2.
        assert_eq!(read_head_oid(&repo), c2);
    }

    // ---- Test 4: apply_commit preserves author ----

    #[test]
    fn apply_commit_preserves_author() {
        let (_tmp, repo) = make_repo();

        let base_tree = make_tree(&repo, &[("a.txt", "a\n")]);
        let base = make_commit(&repo, base_tree, vec![], "Base", 1000, "base");

        let c1_tree = make_tree(&repo, &[("a.txt", "a\n"), ("b.txt", "B\n")]);
        let c1 = make_commit(
            &repo,
            c1_tree,
            vec![base],
            "Originator",
            1100,
            "C1 by Originator",
        );

        let c2_tree = make_tree(&repo, &[("a.txt", "a\n"), ("c.txt", "C\n")]);
        let c2 = make_commit(&repo, c2_tree, vec![base], "Other", 1200, "C2");

        point_main_at(&repo, c2);

        // Set GIT_COMMITTER env so we can verify committer != author.
        let prev_name = std::env::var("GIT_COMMITTER_NAME").ok();
        let prev_email = std::env::var("GIT_COMMITTER_EMAIL").ok();
        let prev_date = std::env::var("GIT_COMMITTER_DATE").ok();
        std::env::set_var("GIT_COMMITTER_NAME", "Committer");
        std::env::set_var("GIT_COMMITTER_EMAIL", "committer@example.test");
        std::env::set_var("GIT_COMMITTER_DATE", "1700000000 +0000");
        // For preserve_author=false branch we'd need GIT_AUTHOR_*, but we're
        // testing the default (preserve_author=true).

        let outcome = apply_commit(&repo, c1, &ApplyOpts::default()).unwrap();
        let new_oid = match outcome {
            ApplyOutcome::Done { new_commit } => new_commit,
            other => panic!("expected Done, got {other:?}"),
        };
        let new = read_commit(&repo, new_oid).unwrap();
        assert_eq!(new.author.name, "Originator");
        assert_eq!(new.author.email, "Originator@example.test");
        assert_eq!(new.committer.name, "Committer");
        assert_eq!(new.committer.email, "committer@example.test");

        // Restore env.
        match prev_name {
            Some(v) => std::env::set_var("GIT_COMMITTER_NAME", v),
            None => std::env::remove_var("GIT_COMMITTER_NAME"),
        }
        match prev_email {
            Some(v) => std::env::set_var("GIT_COMMITTER_EMAIL", v),
            None => std::env::remove_var("GIT_COMMITTER_EMAIL"),
        }
        match prev_date {
            Some(v) => std::env::set_var("GIT_COMMITTER_DATE", v),
            None => std::env::remove_var("GIT_COMMITTER_DATE"),
        }
    }

    // ---- Test 5: apply_commit on root commit (no parents) ----

    /// C is a root commit (parents=[]) → base must be empty tree.
    /// HEAD has some unrelated file; cherry-picking C should add C's new file.
    #[test]
    fn apply_commit_root_commit() {
        let (_tmp, repo) = make_repo();

        // Set up "ours" history.
        let ours_tree = make_tree(&repo, &[("ours.txt", "ours content\n")]);
        let ours = make_commit(&repo, ours_tree, vec![], "Ours", 1000, "ours head");
        point_main_at(&repo, ours);

        // A separate root commit, completely unrelated.
        let root_tree = make_tree(&repo, &[("root.txt", "root content\n")]);
        let root = make_commit(&repo, root_tree, vec![], "Rooter", 500, "the root");

        // Apply the root commit on top of ours. Base = empty tree, ours = ours_tree, theirs = root_tree.
        // The 3-way merge of (empty, ours, root) is: take ours (since ours has ours.txt
        // and base had nothing) and take theirs (since root.txt is in theirs only).
        let outcome = apply_commit(&repo, root, &ApplyOpts::default()).unwrap();
        let new_oid = match outcome {
            ApplyOutcome::Done { new_commit } => new_commit,
            other => panic!("expected Done, got {other:?}"),
        };

        let new = read_commit(&repo, new_oid).unwrap();
        let merged = flatten_tree_for_test(&repo, new.tree);
        let paths: Vec<&[u8]> = merged.iter().map(|(p, _)| p.as_slice()).collect();
        assert!(paths.contains(&b"ours.txt".as_ref()));
        assert!(paths.contains(&b"root.txt".as_ref()));
    }

    // ---- Test 6: State round-trip ----

    #[test]
    fn state_round_trip() {
        let (_tmp, repo) = make_repo();

        // Build a few real commits so save() can read their messages for the
        // todo's subject column.
        let base_tree = make_tree(&repo, &[("x", "x\n")]);
        let base = make_commit(&repo, base_tree, vec![], "A", 1000, "base");
        let c1 = make_commit(&repo, base_tree, vec![base], "A", 1100, "first pick");
        let c2 = make_commit(&repo, base_tree, vec![base], "A", 1200, "second pick");
        let c3 = make_commit(&repo, base_tree, vec![base], "A", 1300, "third pick");

        let state = State {
            head_branch: FullName::new("refs/heads/feature").unwrap(),
            orig_head: base,
            onto: c3,
            todo: vec![c1, c2],
            done: vec![c3],
            in_progress: Some(c1),
            revert: false,
        };
        state.save(&repo).unwrap();

        // Verify on-disk format briefly.
        let dir = State::dir(&repo);
        let head_name = std::fs::read_to_string(dir.join("head-name")).unwrap();
        assert_eq!(head_name.trim(), "refs/heads/feature");
        let todo_text = std::fs::read_to_string(dir.join("todo")).unwrap();
        assert!(todo_text.starts_with("pick "));
        assert!(todo_text.contains("first pick"));
        assert!(todo_text.contains("second pick"));

        let loaded = State::load(&repo).unwrap();
        assert_eq!(loaded, state);
    }

    // ---- Test 7: abort restores HEAD ----

    #[test]
    fn abort_restores_head() {
        let (_tmp, repo) = make_repo();

        let base_tree = make_tree(&repo, &[("a.txt", "a\n")]);
        let base = make_commit(&repo, base_tree, vec![], "A", 1000, "base");
        let advanced_tree = make_tree(&repo, &[("a.txt", "b\n")]);
        let advanced = make_commit(&repo, advanced_tree, vec![base], "A", 1100, "advanced");

        // Put HEAD at "advanced", then write state claiming the original was "base".
        point_main_at(&repo, advanced);
        let state = State {
            head_branch: FullName::new("refs/heads/main").unwrap(),
            orig_head: base,
            onto: base,
            todo: Vec::new(),
            done: Vec::new(),
            in_progress: None,
            revert: false,
        };
        state.save(&repo).unwrap();
        // Also drop a stale CHERRY_PICK_HEAD so we can verify abort clears it.
        std::fs::write(repo.gitdir().join("CHERRY_PICK_HEAD"), "deadbeef\n").unwrap();

        abort(&repo).unwrap();
        assert_eq!(read_head_oid(&repo), base);
        assert!(!repo.gitdir().join("CHERRY_PICK_HEAD").exists());
        assert!(!State::exists(&repo));
        // Workdir matches base.
        let a_path = repo.workdir().join("a.txt");
        assert_eq!(std::fs::read(&a_path).unwrap(), b"a\n");
    }

    // ---- Test 8: cleanup removes the state dir ----

    #[test]
    fn cleanup_removes_state() {
        let (_tmp, repo) = make_repo();
        // Build dummy commits for state.save to find.
        let tree = make_tree(&repo, &[("x", "x\n")]);
        let c = make_commit(&repo, tree, vec![], "A", 1000, "c");

        let state = State {
            head_branch: FullName::new("refs/heads/main").unwrap(),
            orig_head: c,
            onto: c,
            todo: vec![],
            done: vec![],
            in_progress: None,
            revert: false,
        };
        state.save(&repo).unwrap();
        assert!(State::exists(&repo));

        // Also drop the conflict markers.
        std::fs::write(repo.gitdir().join("CHERRY_PICK_HEAD"), "x\n").unwrap();
        std::fs::write(repo.gitdir().join("MERGE_MSG"), "x\n").unwrap();

        State::cleanup(&repo).unwrap();
        assert!(!State::exists(&repo));
        assert!(!repo.gitdir().join("CHERRY_PICK_HEAD").exists());
        assert!(!repo.gitdir().join("MERGE_MSG").exists());
    }

    // ---- Additional tests ----

    /// load() with no state directory → NoState.
    #[test]
    fn load_no_state_errors() {
        let (_tmp, repo) = make_repo();
        let err = State::load(&repo).unwrap_err();
        assert!(matches!(err, SequencerError::NoState));
    }

    /// State::save preserves None in_progress correctly (no `current` file).
    #[test]
    fn state_save_without_in_progress() {
        let (_tmp, repo) = make_repo();
        let tree = make_tree(&repo, &[("x", "x\n")]);
        let c = make_commit(&repo, tree, vec![], "A", 1000, "c");
        let state = State {
            head_branch: FullName::new("refs/heads/main").unwrap(),
            orig_head: c,
            onto: c,
            todo: vec![],
            done: vec![],
            in_progress: None,
            revert: false,
        };
        state.save(&repo).unwrap();
        let dir = State::dir(&repo);
        assert!(!dir.join("current").exists());
        let loaded = State::load(&repo).unwrap();
        assert_eq!(loaded.in_progress, None);
    }

    /// Detached HEAD → apply_commit returns DetachedHead.
    #[test]
    fn detached_head_errors() {
        let (_tmp, repo) = make_repo();

        let base_tree = make_tree(&repo, &[("a.txt", "a\n")]);
        let base = make_commit(&repo, base_tree, vec![], "A", 1000, "base");
        point_main_at(&repo, base);

        // Now detach HEAD by overwriting it to point directly at the commit.
        let head = FullName::new("HEAD").unwrap();
        let mut tx = repo.refs().transaction();
        tx.update(
            &head,
            ExpectedOldValue::Any,
            NewValue::Direct(base),
            ReflogMessage::from("detach"),
        )
        .unwrap();
        tx.commit().unwrap();

        let err = apply_commit(&repo, base, &ApplyOpts::default()).unwrap_err();
        assert!(matches!(err, SequencerError::DetachedHead));
    }

    /// Cross-verify with system git: cherry-pick produces the same tree.
    #[test]
    fn cross_verify_clean_pick_against_git() {
        if !git_available() {
            eprintln!("skip: no git");
            return;
        }
        let (_tmp, repo) = make_repo();
        // Use git to build base/c1/c2 so the layouts match exactly.
        let dir = repo.workdir().to_path_buf();
        std::fs::write(dir.join("a.txt"), "a\n").unwrap();
        git_cmd(&dir, &["add", "."]);
        git_cmd(&dir, &["commit", "-q", "-m", "base"]);
        let base_oid = git_rev_parse(&dir, "HEAD");

        git_cmd(&dir, &["checkout", "-q", "-b", "feature"]);
        std::fs::write(dir.join("a.txt"), "a\nfeature line\n").unwrap();
        git_cmd(&dir, &["commit", "-am", "feature work", "-q"]);
        let feature_oid = git_rev_parse(&dir, "HEAD");

        git_cmd(&dir, &["checkout", "-q", "main"]);
        std::fs::write(dir.join("b.txt"), "b\n").unwrap();
        git_cmd(&dir, &["add", "b.txt"]);
        git_cmd(&dir, &["commit", "-m", "main: add b", "-q"]);
        let main_oid = git_rev_parse(&dir, "HEAD");

        // Now use our apply_commit to pick `feature` onto `main`.
        let feature = ObjectId::parse_hex(repo.hash_kind(), &feature_oid).unwrap();
        let _main_oid_obj = ObjectId::parse_hex(repo.hash_kind(), &main_oid).unwrap();
        let _base_oid_obj = ObjectId::parse_hex(repo.hash_kind(), &base_oid).unwrap();

        // We're currently on `main` (just from `checkout main`). Re-discover.
        let repo = Repository::discover(&dir).unwrap();
        let outcome = apply_commit(&repo, feature, &ApplyOpts::default()).unwrap();
        let new_oid = match outcome {
            ApplyOutcome::Done { new_commit } => new_commit,
            other => panic!("expected Done, got {other:?}"),
        };
        let new = read_commit(&repo, new_oid).unwrap();
        // The merged tree should have both b.txt and the modified a.txt.
        let merged = flatten_tree_for_test(&repo, new.tree);
        let paths: Vec<&[u8]> = merged.iter().map(|(p, _)| p.as_slice()).collect();
        assert!(paths.contains(&b"a.txt".as_ref()));
        assert!(paths.contains(&b"b.txt".as_ref()));
        let a_oid = merged.iter().find(|(p, _)| p == b"a.txt").unwrap().1;
        let a_blob = repo.odb().read(&a_oid).unwrap();
        assert_eq!(a_blob.data, b"a\nfeature line\n");
    }

    /// cont() with no state → NoState.
    #[test]
    fn cont_no_state_errors() {
        let (_tmp, repo) = make_repo();
        let err = cont(&repo).unwrap_err();
        assert!(matches!(err, SequencerError::NoState));
    }

    /// cont() drains a todo of multiple commits cleanly.
    #[test]
    fn cont_drains_todo_clean() {
        let (_tmp, repo) = make_repo();
        let base_tree = make_tree(&repo, &[("a.txt", "a\n")]);
        let base = make_commit(&repo, base_tree, vec![], "A", 1000, "base");
        point_main_at(&repo, base);

        // Two cherry-pick candidates: each adds a different file.
        let p1_tree = make_tree(&repo, &[("a.txt", "a\n"), ("p1.txt", "1\n")]);
        let p1 = make_commit(&repo, p1_tree, vec![base], "A", 1100, "p1");
        let p2_tree = make_tree(&repo, &[("a.txt", "a\n"), ("p2.txt", "2\n")]);
        let p2 = make_commit(&repo, p2_tree, vec![base], "A", 1200, "p2");

        let state = State {
            head_branch: FullName::new("refs/heads/main").unwrap(),
            orig_head: base,
            onto: base,
            todo: vec![p1, p2],
            done: vec![],
            in_progress: None,
            revert: false,
        };
        state.save(&repo).unwrap();

        let outcome = cont(&repo).unwrap();
        assert!(matches!(outcome, ContinueOutcome::Done));
        // Both commits applied: workdir has p1.txt and p2.txt.
        assert!(repo.workdir().join("p1.txt").exists());
        assert!(repo.workdir().join("p2.txt").exists());
        // State dir cleaned up.
        assert!(!State::exists(&repo));
    }

    /// cont() stops at conflict, leaving state with in_progress = the
    /// failing commit and the todo drained up to it.
    #[test]
    fn cont_stops_at_conflict() {
        let (_tmp, repo) = make_repo();
        let base_tree = make_tree(&repo, &[("f.txt", "x\n")]);
        let base = make_commit(&repo, base_tree, vec![], "A", 1000, "base");
        point_main_at(&repo, base);

        // p1: cleanly add p1.txt
        let p1_tree = make_tree(&repo, &[("f.txt", "x\n"), ("p1.txt", "1\n")]);
        let p1 = make_commit(&repo, p1_tree, vec![base], "A", 1100, "p1");

        // p2: change f.txt to "y"
        let p2_tree = make_tree(&repo, &[("f.txt", "y\n")]);
        let p2 = make_commit(&repo, p2_tree, vec![base], "A", 1200, "p2");

        // p3: also change f.txt but to "z" — will conflict because our HEAD
        // (after p2) has "y\n", base for p3 has "x\n", theirs has "z\n".
        // The 3-way merge of base="x\n", ours="y\n", theirs="z\n" conflicts.
        let p3_tree = make_tree(&repo, &[("f.txt", "z\n")]);
        let p3 = make_commit(&repo, p3_tree, vec![base], "A", 1300, "p3");

        let state = State {
            head_branch: FullName::new("refs/heads/main").unwrap(),
            orig_head: base,
            onto: base,
            todo: vec![p1, p2, p3],
            done: vec![],
            in_progress: None,
            revert: false,
        };
        state.save(&repo).unwrap();

        let outcome = cont(&repo).unwrap();
        match outcome {
            ContinueOutcome::Conflicted {
                commit,
                offending_paths,
            } => {
                assert_eq!(commit, p3);
                assert_eq!(offending_paths, vec![b"f.txt".to_vec()]);
            }
            other => panic!("expected Conflicted, got {other:?}"),
        }
        // State still present.
        assert!(State::exists(&repo));
        let saved = State::load(&repo).unwrap();
        assert_eq!(saved.in_progress, Some(p3));
        assert_eq!(saved.done, vec![p1, p2]);
        assert!(saved.todo.is_empty());
    }

    /// First-line message helper handles a message with a single line.
    #[test]
    fn first_line_single() {
        assert_eq!(first_line(b"hello"), "hello");
        assert_eq!(first_line(b"hello\nworld"), "hello");
        assert_eq!(first_line(b""), "");
    }

    /// parse_todo accepts both `pick <oid>` and lone `<oid>` lines.
    #[test]
    fn parse_todo_accepts_variants() {
        let hk = HashKind::Sha1;
        let oid = "1111111111111111111111111111111111111111";
        let text = format!("pick {oid} a subject\n{oid}\n# a comment\n\n");
        let v = parse_todo(&text, hk).unwrap();
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].to_string(), oid);
        assert_eq!(v[1].to_string(), oid);
    }

    /// parse_todo errors on garbage.
    #[test]
    fn parse_todo_rejects_garbage() {
        let hk = HashKind::Sha1;
        let err = parse_todo("pick notavalidoid\n", hk).unwrap_err();
        assert!(matches!(err, SequencerError::Corrupt(_)));
    }

    /// override_message replaces the commit's text.
    #[test]
    fn apply_commit_override_message() {
        let (_tmp, repo) = make_repo();
        let base_tree = make_tree(&repo, &[("a.txt", "a\n")]);
        let base = make_commit(&repo, base_tree, vec![], "A", 1000, "base");
        let pick_tree = make_tree(&repo, &[("a.txt", "a\n"), ("p.txt", "p\n")]);
        let pick = make_commit(&repo, pick_tree, vec![base], "A", 1100, "original subject");
        point_main_at(&repo, base);

        let opts = ApplyOpts {
            override_message: Some("custom subject".into()),
            ..ApplyOpts::default()
        };
        let outcome = apply_commit(&repo, pick, &opts).unwrap();
        let new_oid = match outcome {
            ApplyOutcome::Done { new_commit } => new_commit,
            other => panic!("got {other:?}"),
        };
        let c = read_commit(&repo, new_oid).unwrap();
        assert!(c.message.starts_with(b"custom subject"));
    }

    // ---- helpers ----

    fn flatten_tree_for_test(repo: &Repository, tree_oid: ObjectId) -> Vec<(Vec<u8>, ObjectId)> {
        let mut out = Vec::new();
        flatten_inner(repo, &tree_oid, &mut Vec::new(), &mut out);
        out
    }

    fn flatten_inner(
        repo: &Repository,
        tree_oid: &ObjectId,
        prefix: &mut Vec<u8>,
        out: &mut Vec<(Vec<u8>, ObjectId)>,
    ) {
        let raw = repo.odb().read(tree_oid).unwrap();
        if raw.kind != ObjectKind::Tree {
            return;
        }
        let tree = Tree::parse(&raw.data, repo.hash_kind()).unwrap();
        for entry in &tree.entries {
            let saved = prefix.len();
            if !prefix.is_empty() {
                prefix.push(b'/');
            }
            prefix.extend_from_slice(&entry.name);
            if entry.mode.is_tree() {
                flatten_inner(repo, &entry.oid, prefix, out);
            } else {
                out.push((prefix.clone(), entry.oid));
            }
            prefix.truncate(saved);
        }
    }

    fn git_cmd(dir: &Path, args: &[&str]) {
        let out = Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.com")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.com")
            .env("GIT_AUTHOR_DATE", "1700000000 +0000")
            .env("GIT_COMMITTER_DATE", "1700000000 +0000")
            .output()
            .expect("git");
        if !out.status.success() {
            panic!(
                "git {:?} failed: {}",
                args,
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }

    fn git_rev_parse(dir: &Path, rev: &str) -> String {
        let out = Command::new("git")
            .args(["rev-parse", rev])
            .current_dir(dir)
            .output()
            .expect("rev-parse");
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    }
}
