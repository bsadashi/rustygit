//! `rustygit notes` — porcelain for git notes.
//!
//! Subcommands shipped: `list` (default), `add`, `show`, `append`, `copy`,
//! `remove`, `edit`, `prune`. Unimplemented: `merge` (multi-strategy notes
//! merge is its own subsystem; deferred).
//!
//! All subcommands accept `--ref <name>` to target a non-default notes ref
//! (e.g. `--ref reviewers` → `refs/notes/reviewers`). The default ref is
//! `refs/notes/commits`, overridable by `GIT_NOTES_REF` env or
//! `core.notesRef` config.

use std::io;

use clap::{Args, Subcommand};

use crate::config::Config;
use crate::hash::ObjectId;
use crate::notes::{self, pick_editor, write_note_blob, NotesTree};
use crate::refs::FullName;
use crate::repo::Repository;
use crate::revparse::resolve;
use crate::signing::GpgSigner;

#[derive(Debug, Args)]
pub struct NotesArgs {
    /// Operate on a different notes ref (short form `reviewers` or full
    /// `refs/notes/reviewers`).
    #[arg(long = "ref", value_name = "REF", global = true)]
    pub ref_name: Option<String>,

    #[command(subcommand)]
    pub command: Option<NotesCommand>,

    /// Positional args when no subcommand is given (defaults to `list`).
    /// This mirrors `git notes [<object>]`.
    #[arg(value_name = "OBJECT", trailing_var_arg = true)]
    pub positional: Vec<String>,
}

#[derive(Debug, Subcommand)]
pub enum NotesCommand {
    /// List notes — prints `<note-oid> <object-oid>` lines, or just the note
    /// for one object if one is given.
    List {
        #[arg(value_name = "OBJECT")]
        object: Option<String>,
    },
    /// Add a note to an object.
    Add {
        /// Overwrite an existing note.
        #[arg(short = 'f', long = "force")]
        force: bool,
        /// Message body. Repeating concatenates with a blank-line separator.
        #[arg(short = 'm', value_name = "MESSAGE")]
        messages: Vec<String>,
        /// Read message body from this file.
        #[arg(short = 'F', long = "file", value_name = "PATH")]
        file: Option<String>,
        /// Allow an empty note.
        #[arg(long = "allow-empty")]
        allow_empty: bool,
        #[arg(value_name = "OBJECT")]
        object: Option<String>,
    },
    /// Show the note on an object.
    Show {
        #[arg(value_name = "OBJECT")]
        object: Option<String>,
    },
    /// Append text to the note on an object.
    Append {
        #[arg(short = 'm', value_name = "MESSAGE")]
        messages: Vec<String>,
        #[arg(short = 'F', long = "file", value_name = "PATH")]
        file: Option<String>,
        #[arg(long = "allow-empty")]
        allow_empty: bool,
        #[arg(value_name = "OBJECT")]
        object: Option<String>,
    },
    /// Copy the note from one object to another.
    Copy {
        #[arg(short = 'f', long = "force")]
        force: bool,
        #[arg(value_name = "FROM_OBJECT")]
        from: String,
        #[arg(value_name = "TO_OBJECT")]
        to: Option<String>,
    },
    /// Remove the note on one or more objects.
    Remove {
        /// Don't complain when an object has no note.
        #[arg(long = "ignore-missing")]
        ignore_missing: bool,
        #[arg(value_name = "OBJECT")]
        objects: Vec<String>,
    },
    /// Edit the note on an object in $EDITOR.
    Edit {
        #[arg(value_name = "OBJECT")]
        object: Option<String>,
    },
    /// Drop notes whose target object no longer exists.
    Prune {
        /// Don't actually drop; just print what would be removed.
        #[arg(short = 'n', long = "dry-run")]
        dry_run: bool,
        /// Print the targets being removed.
        #[arg(short = 'v', long = "verbose")]
        verbose: bool,
    },
    /// Merge another notes ref into this one.
    Merge {
        /// Strategy: union (default), ours, theirs.
        #[arg(short = 's', long = "strategy", default_value = "union")]
        strategy: String,
        /// The remote notes ref to merge from (full ref name).
        #[arg(value_name = "REMOTE_REF")]
        remote: String,
    },
}

pub fn run(args: NotesArgs) -> io::Result<i32> {
    let repo = Repository::discover_from_cwd().map_err(io_err)?;
    let config = Config::from_repo_dir(repo.gitdir()).map_err(io_err)?;
    let notes_ref = notes::resolve_notes_ref(args.ref_name.as_deref(), &config).map_err(io_err)?;

    // `notes` with no subcommand defaults to `list`. `notes <object>` is also
    // equivalent to `notes list <object>`.
    let cmd = args.command.unwrap_or_else(|| NotesCommand::List {
        object: args.positional.first().cloned(),
    });

    match cmd {
        NotesCommand::List { object } => run_list(&repo, &notes_ref, object.as_deref()),
        NotesCommand::Add {
            force,
            messages,
            file,
            allow_empty,
            object,
        } => run_add(
            &repo,
            &notes_ref,
            object.as_deref(),
            force,
            &messages,
            file.as_deref(),
            allow_empty,
        ),
        NotesCommand::Show { object } => run_show(&repo, &notes_ref, object.as_deref()),
        NotesCommand::Append {
            messages,
            file,
            allow_empty,
            object,
        } => run_append(
            &repo,
            &notes_ref,
            object.as_deref(),
            &messages,
            file.as_deref(),
            allow_empty,
        ),
        NotesCommand::Copy { force, from, to } => {
            run_copy(&repo, &notes_ref, &from, to.as_deref(), force)
        }
        NotesCommand::Remove {
            ignore_missing,
            objects,
        } => run_remove(&repo, &notes_ref, &objects, ignore_missing),
        NotesCommand::Edit { object } => run_edit(&repo, &notes_ref, object.as_deref()),
        NotesCommand::Prune { dry_run, verbose } => run_prune(&repo, &notes_ref, dry_run, verbose),
        NotesCommand::Merge { strategy, remote } => {
            run_merge(&repo, &notes_ref, &remote, &strategy)
        }
    }
}

/// Three-way merge of two notes refs into the local one.
///
/// Strategies supported:
///   * `union` (default) — keep both notes when both sides have one
///     (concatenated with a blank line); otherwise take the side that
///     has a note.
///   * `ours` — preserve every local note; only add notes for objects
///     the local ref doesn't already have.
///   * `theirs` — overwrite local notes with remote notes whenever both
///     sides have one.
fn run_merge(
    repo: &Repository,
    local_ref: &FullName,
    remote_ref_spec: &str,
    strategy: &str,
) -> io::Result<i32> {
    let remote_full = FullName::new(if remote_ref_spec.contains('/') {
        remote_ref_spec.to_string()
    } else {
        format!("refs/notes/{remote_ref_spec}")
    })
    .map_err(io_err)?;

    let local_tree = notes::NotesTree::open(repo, local_ref).map_err(io_err)?;
    let remote_tree = notes::NotesTree::open(repo, &remote_full).map_err(io_err)?;

    let mut merged = local_tree.clone();
    for (target, remote_note_oid) in remote_tree.iter() {
        let remote_note_oid = *remote_note_oid;
        let target = *target;
        let remote_blob = repo.odb().read(&remote_note_oid).map_err(io_err)?;
        match merged.get(&target) {
            None => {
                merged.set(target, remote_note_oid);
            }
            Some(local_note_oid) => match strategy {
                "ours" => { /* keep local */ }
                "theirs" => {
                    merged.set(target, remote_note_oid);
                }
                _ => {
                    // union: append remote to local (blank line separator),
                    // then write a fresh blob and point at it.
                    let local_raw = repo.odb().read(&local_note_oid).map_err(io_err)?;
                    if local_raw.data != remote_blob.data {
                        let mut joined = local_raw.data.clone();
                        if !joined.ends_with(b"\n") {
                            joined.push(b'\n');
                        }
                        joined.push(b'\n');
                        joined.extend_from_slice(&remote_blob.data);
                        let merged_blob =
                            crate::object::RawObject::new(crate::object::ObjectKind::Blob, joined);
                        let new_oid = repo.odb().write(&merged_blob).map_err(io_err)?;
                        merged.set(target, new_oid);
                    }
                }
            },
        }
    }

    let config = Config::from_repo_dir(repo.gitdir()).map_err(io_err)?;
    let signer = crate::signing::GpgSigner::from_config(&config);
    let _ = merged
        .commit(
            repo,
            &format!("Notes merge from {remote_ref_spec} (strategy: {strategy})"),
            Some(&signer),
        )
        .map_err(io_err)?;
    Ok(0)
}

fn run_list(repo: &Repository, notes_ref: &FullName, object: Option<&str>) -> io::Result<i32> {
    let tree = NotesTree::open(repo, notes_ref).map_err(io_err)?;
    let stdout = io::stdout();
    let mut out = stdout.lock();
    use std::io::Write;

    if let Some(spec) = object {
        let target = resolve_object(repo, spec)?;
        match tree.get(&target) {
            Some(note) => writeln!(out, "{note}")?,
            None => {
                eprintln!("rustygit: no note found for object {} ({})", target, spec);
                return Ok(1);
            }
        }
        return Ok(0);
    }

    // Sort by target oid so output is deterministic and matches what git does.
    let mut pairs: Vec<(ObjectId, ObjectId)> = tree.iter().map(|(t, n)| (*t, *n)).collect();
    pairs.sort_by_key(|a| a.0);
    for (target, note) in pairs {
        writeln!(out, "{note} {target}")?;
    }
    Ok(0)
}

fn run_add(
    repo: &Repository,
    notes_ref: &FullName,
    object: Option<&str>,
    force: bool,
    messages: &[String],
    file: Option<&str>,
    allow_empty: bool,
) -> io::Result<i32> {
    let target = resolve_object(repo, object.unwrap_or("HEAD"))?;
    let mut tree = NotesTree::open(repo, notes_ref).map_err(io_err)?;

    if tree.get(&target).is_some() && !force {
        eprintln!("rustygit: notes: object {target} already has a note (use -f to overwrite)");
        return Ok(1);
    }

    let body = build_message_body(messages, file)?;
    if body.is_empty() && !allow_empty {
        eprintln!("rustygit: notes: refusing to add an empty note (use --allow-empty)");
        return Ok(1);
    }

    let blob = write_note_blob(repo, &body).map_err(io_err)?;
    tree.set(target, blob);
    commit_with_message(repo, tree, "Notes added by 'git notes add'")?;
    Ok(0)
}

fn run_show(repo: &Repository, notes_ref: &FullName, object: Option<&str>) -> io::Result<i32> {
    let target = resolve_object(repo, object.unwrap_or("HEAD"))?;
    let tree = NotesTree::open(repo, notes_ref).map_err(io_err)?;
    let Some(blob) = tree.get(&target) else {
        eprintln!("rustygit: notes: no note found for object {target}");
        return Ok(1);
    };
    let obj = repo.odb().read(&blob).map_err(io_err)?;
    let stdout = io::stdout();
    let mut out = stdout.lock();
    use std::io::Write;
    out.write_all(&obj.data)?;
    Ok(0)
}

fn run_append(
    repo: &Repository,
    notes_ref: &FullName,
    object: Option<&str>,
    messages: &[String],
    file: Option<&str>,
    allow_empty: bool,
) -> io::Result<i32> {
    let target = resolve_object(repo, object.unwrap_or("HEAD"))?;
    let mut tree = NotesTree::open(repo, notes_ref).map_err(io_err)?;
    let existing = tree.read_note(repo.odb(), &target).map_err(io_err)?;

    let added = build_message_body(messages, file)?;
    if added.is_empty() && !allow_empty {
        // When there's nothing new to add and the user didn't ask for empty,
        // it's a no-op. Match git's behavior — no error.
        return Ok(0);
    }

    let mut combined = match existing {
        Some(prev) if !prev.is_empty() => {
            let mut v = prev;
            if !v.ends_with(b"\n") {
                v.push(b'\n');
            }
            // Blank line separator between old and new content.
            v.push(b'\n');
            v.extend_from_slice(&added);
            v
        }
        _ => added,
    };
    if !combined.is_empty() && !combined.ends_with(b"\n") {
        combined.push(b'\n');
    }

    let blob = write_note_blob(repo, &combined).map_err(io_err)?;
    tree.set(target, blob);
    commit_with_message(repo, tree, "Notes added by 'git notes append'")?;
    Ok(0)
}

fn run_copy(
    repo: &Repository,
    notes_ref: &FullName,
    from: &str,
    to: Option<&str>,
    force: bool,
) -> io::Result<i32> {
    let from_target = resolve_object(repo, from)?;
    let to_target = resolve_object(repo, to.unwrap_or("HEAD"))?;
    let mut tree = NotesTree::open(repo, notes_ref).map_err(io_err)?;

    let Some(source_note) = tree.get(&from_target) else {
        eprintln!("rustygit: notes: no note to copy from object {from_target}");
        return Ok(1);
    };
    if tree.get(&to_target).is_some() && !force {
        eprintln!("rustygit: notes: object {to_target} already has a note (use -f to overwrite)");
        return Ok(1);
    }
    tree.set(to_target, source_note);
    commit_with_message(repo, tree, "Notes added by 'git notes copy'")?;
    Ok(0)
}

fn run_remove(
    repo: &Repository,
    notes_ref: &FullName,
    objects: &[String],
    ignore_missing: bool,
) -> io::Result<i32> {
    let targets: Vec<&str> = if objects.is_empty() {
        vec!["HEAD"]
    } else {
        objects.iter().map(String::as_str).collect()
    };
    let mut tree = NotesTree::open(repo, notes_ref).map_err(io_err)?;
    let mut any_removed = false;
    let mut any_missing = false;
    for t in &targets {
        let oid = resolve_object(repo, t)?;
        if tree.remove(&oid) {
            any_removed = true;
        } else {
            any_missing = true;
            if !ignore_missing {
                eprintln!("rustygit: notes: no note to remove for object {oid}");
            }
        }
    }
    if any_removed {
        commit_with_message(repo, tree, "Notes removed by 'git notes remove'")?;
    }
    if any_missing && !ignore_missing {
        return Ok(1);
    }
    Ok(0)
}

fn run_edit(repo: &Repository, notes_ref: &FullName, object: Option<&str>) -> io::Result<i32> {
    let target = resolve_object(repo, object.unwrap_or("HEAD"))?;
    let mut tree = NotesTree::open(repo, notes_ref).map_err(io_err)?;
    let seed = tree
        .read_note(repo.odb(), &target)
        .map_err(io_err)?
        .unwrap_or_default();

    let config = Config::from_repo_dir(repo.gitdir()).map_err(io_err)?;
    let editor = pick_editor(&config);
    let edited = notes::edit_text(&editor, &seed).map_err(io_err)?;

    // git notes treats an empty edit (or content unchanged after stripping
    // whitespace) as a remove. We match that — empty edit removes the note;
    // if the user typed something, save it.
    let trimmed: Vec<u8> = edited
        .iter()
        .copied()
        .skip_while(|b| b.is_ascii_whitespace())
        .collect();
    let trimmed_back: Vec<u8> = trimmed
        .iter()
        .rev()
        .copied()
        .skip_while(|b| b.is_ascii_whitespace())
        .collect::<Vec<u8>>()
        .into_iter()
        .rev()
        .collect();

    if trimmed_back.is_empty() {
        if tree.remove(&target) {
            commit_with_message(repo, tree, "Notes removed by 'git notes edit'")?;
        }
        return Ok(0);
    }

    let blob = write_note_blob(repo, &edited).map_err(io_err)?;
    tree.set(target, blob);
    commit_with_message(repo, tree, "Notes added by 'git notes edit'")?;
    Ok(0)
}

fn run_prune(
    repo: &Repository,
    notes_ref: &FullName,
    dry_run: bool,
    verbose: bool,
) -> io::Result<i32> {
    let mut tree = NotesTree::open(repo, notes_ref).map_err(io_err)?;
    let mut to_prune: Vec<ObjectId> = Vec::new();
    for (target, _) in tree.iter() {
        if !repo.odb().contains(target).map_err(io_err)? {
            to_prune.push(*target);
        }
    }
    if verbose || dry_run {
        for t in &to_prune {
            println!("{t}");
        }
    }
    if dry_run || to_prune.is_empty() {
        return Ok(0);
    }
    for t in &to_prune {
        tree.remove(t);
    }
    commit_with_message(repo, tree, "Notes removed by 'git notes prune'")?;
    Ok(0)
}

/// Common message-body assembly: `-m` repeated, then `-F <file>`, then
/// concatenated with blank-line separators.
fn build_message_body(messages: &[String], file: Option<&str>) -> io::Result<Vec<u8>> {
    let mut out: Vec<u8> = Vec::new();
    for (i, m) in messages.iter().enumerate() {
        if i > 0 {
            out.extend_from_slice(b"\n\n");
        }
        out.extend_from_slice(m.as_bytes());
    }
    if let Some(path) = file {
        if !out.is_empty() {
            out.extend_from_slice(b"\n\n");
        }
        let bytes = std::fs::read(path)?;
        out.extend_from_slice(&bytes);
    }
    Ok(out)
}

/// Resolve a user-supplied object specifier (`HEAD`, a hex prefix, etc.)
/// to an oid.
fn resolve_object(repo: &Repository, spec: &str) -> io::Result<ObjectId> {
    resolve(repo.refs(), repo.odb(), spec).map_err(io_err)
}

/// Commit the mutated `tree` with `message`. Optionally GPG-sign per
/// `commit.gpgsign` config.
fn commit_with_message(repo: &Repository, tree: NotesTree, message: &str) -> io::Result<()> {
    let config = Config::from_repo_dir(repo.gitdir()).map_err(io_err)?;
    let sign = config.get_bool("commit", "gpgsign").unwrap_or(false);
    let signer = if sign {
        Some(GpgSigner::from_config(&config))
    } else {
        None
    };
    let signer_ref: Option<&dyn crate::signing::Signer> = signer.as_ref().map(|s| s as _);
    tree.commit(repo, message, signer_ref).map_err(io_err)?;
    Ok(())
}

fn io_err<E: std::fmt::Display>(e: E) -> io::Error {
    io::Error::other(format!("{e}"))
}
