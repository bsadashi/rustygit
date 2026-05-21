//! `rustygit add -p` — interactive hunk staging.
//!
//! Orchestrator. For each file with worktree-vs-index changes, compute the
//! per-file unified diff, parse the hunks, and walk them one at a time
//! prompting the user `y/n/q/a/d/?/s`. After all hunks for a file are
//! processed, rebuild the to-be-staged content by applying ONLY the chosen
//! hunks to the index version, hash it as a blob, and upsert into the index.
//!
//! Subset shipped here:
//!   y - stage this hunk
//!   n - skip this hunk
//!   q - quit; skip remaining hunks
//!   a - stage this and all later hunks in this file
//!   d - skip this and all later hunks in this file
//!   s - split the hunk into smaller hunks
//!   ? - print help
//!
//! Not shipped (documented as not yet supported): `e` (manual edit), `g`
//! (go to numbered hunk), `j`/`J` (skip but mark undecided), `/` (search by
//! regex). These are full power-user features deferred per POLISH.md item 7.
//!
//! from git/add-patch.c::patch_update_file

use std::io::{self, BufRead, Write};

use crate::add_patch::{self, Hunk};
use crate::diff::{self as diff_engine, DiffPair, DiffStatus};
use crate::hash::ObjectId;
use crate::index::{Index, IndexEntry};
use crate::object::{ObjectKind, RawObject};
use crate::repo::Repository;
use crate::xdiff::{unified_diff, UnifiedDiffOpts};

/// Entry point invoked from `cli::add::run` when `-p` is set.
pub fn run(repo: &Repository) -> io::Result<i32> {
    let stdin = io::stdin();
    let stdin_lock = stdin.lock();
    let stdout = io::stdout();
    let stdout_lock = stdout.lock();
    let stderr = io::stderr();
    let stderr_lock = stderr.lock();

    let mut session = Session {
        stdin: Box::new(stdin_lock),
        stdout: Box::new(stdout_lock),
        stderr: Box::new(stderr_lock),
    };
    run_with_io(repo, &mut session)
}

/// Same as `run` but with injectable IO — used by integration tests.
pub fn run_with_io(repo: &Repository, session: &mut Session<'_>) -> io::Result<i32> {
    let mut index = Index::read(repo).map_err(io_err)?;
    let candidates = collect_candidates(repo, &index).map_err(io_err)?;

    if candidates.is_empty() {
        // Match git: silent no-op when nothing to patch.
        return Ok(0);
    }

    for cand in &candidates {
        match cand.status {
            DiffStatus::Modified | DiffStatus::TypeChanged | DiffStatus::ModeChanged => {
                // Process modified files via the hunk-walk path.
                if let Some(action) = process_file(repo, &mut index, cand, session)? {
                    if matches!(action, Action::Quit) {
                        break;
                    }
                }
            }
            DiffStatus::Deleted => {
                // Worktree-deleted files: prompt yes/no/quit to stage the
                // deletion as a single "hunk". Subset implementation.
                if let Some(action) = prompt_deletion(repo, &mut index, cand, session)? {
                    if matches!(action, Action::Quit) {
                        break;
                    }
                }
            }
            DiffStatus::Added => {
                // `add -p` only walks tracked-path changes; untracked files
                // are handled by `git add -N <path>` first. We skip them
                // silently to match git's behavior.
            }
        }
    }

    index.sort();
    index.write(repo).map_err(io_err)?;
    Ok(0)
}

/// IO abstraction so tests can drive the prompt loop synchronously.
pub struct Session<'a> {
    pub stdin: Box<dyn BufRead + 'a>,
    pub stdout: Box<dyn Write + 'a>,
    pub stderr: Box<dyn Write + 'a>,
}

/// What the prompt loop decided about one file. Bubbled up so the outer
/// loop can short-circuit on `q`.
enum Action {
    Continue,
    Quit,
}

/// Build the (sorted) list of `DiffPair`s describing the worktree-vs-index
/// delta. Mirrors the path-discovery half of `cli::diff_files::run`.
fn collect_candidates(repo: &Repository, index: &Index) -> Result<Vec<DiffPair>, io::Error> {
    let a_entries = diff_engine::flatten_index(index);
    let b_entries = diff_engine::flatten_workdir_against_index(repo, index)
        .map_err(|e| io::Error::other(format!("{e}")))?;
    Ok(diff_engine::diff_entries(&a_entries, &b_entries))
}

/// Walk the hunks for one modified file, prompt for each, and stage the
/// chosen subset by upserting a fresh blob into the index.
fn process_file(
    repo: &Repository,
    index: &mut Index,
    pair: &DiffPair,
    session: &mut Session<'_>,
) -> io::Result<Option<Action>> {
    let a = pair
        .a
        .as_ref()
        .expect("Modified/TypeChanged/ModeChanged pair has both sides");
    let b = pair
        .b
        .as_ref()
        .expect("Modified/TypeChanged/ModeChanged pair has both sides");
    let path_display = display_path(&a.path);

    // Read both sides of the content.
    let base_bytes = read_blob_bytes(repo, &a.oid)?;
    let work_bytes = read_blob_bytes(repo, &b.oid)?;

    // Compute the diff and parse into hunks.
    let mut diff_bytes: Vec<u8> = Vec::new();
    unified_diff(
        &base_bytes,
        &work_bytes,
        &UnifiedDiffOpts::default(),
        &mut diff_bytes,
    )
    .map_err(|e| io::Error::other(format!("{e}")))?;
    let parsed_hunks = match add_patch::parse_hunks_from_diff(&diff_bytes) {
        Ok(h) => h,
        Err(e) => {
            writeln!(
                session.stderr,
                "rustygit add -p: {path_display}: failed to parse diff: {e}"
            )?;
            return Ok(Some(Action::Continue));
        }
    };

    if parsed_hunks.is_empty() {
        // Mode-only change with identical content — no hunks to prompt for.
        // Stage the mode flip directly.
        upsert_with_content(repo, index, pair, &work_bytes)?;
        return Ok(Some(Action::Continue));
    }

    // Print a per-file header. git emits a `diff --git` / `--- a/` / `+++ b/`
    // banner; we keep ours minimal but informative.
    writeln!(
        session.stdout,
        "diff --git a/{path_display} b/{path_display}"
    )?;
    writeln!(session.stdout, "--- a/{path_display}")?;
    writeln!(session.stdout, "+++ b/{path_display}")?;

    // Working list of hunks: split_hunk re-adds entries when the user splits.
    let mut hunks: Vec<Hunk> = parsed_hunks;
    let mut chosen: Vec<bool> = vec![false; hunks.len()];

    let mut i: usize = 0;
    let mut quit = false;
    while i < hunks.len() {
        // Print this hunk and prompt.
        let formatted = add_patch::format_hunk(&hunks[i]);
        session.stdout.write_all(&formatted)?;
        session.stdout.flush()?;

        let n_hunks = hunks.len();
        let hunk_num = i + 1;

        // Result of this prompt round, in terms of how the outer loop should
        // advance i.
        let mut advance = true;

        // git uses `%s` to inject "(N/M)" only when there is more than one
        // hunk; we always include the counter for clarity.
        loop {
            write!(
                session.stderr,
                "({hunk_num}/{n_hunks}) Stage this hunk [y,n,q,a,d,s,?]? "
            )?;
            session.stderr.flush()?;
            let line = match read_line(&mut session.stdin)? {
                Some(l) => l,
                None => {
                    // EOF on stdin — treat like `q` (matches git's behavior).
                    quit = true;
                    break;
                }
            };
            let ch = line.trim().chars().next().unwrap_or('\0');
            match ch {
                'y' => {
                    chosen[i] = true;
                    break;
                }
                'n' => {
                    chosen[i] = false;
                    break;
                }
                'q' => {
                    quit = true;
                    break;
                }
                'a' => {
                    chosen[i] = true;
                    for c in chosen.iter_mut().skip(i + 1) {
                        *c = true;
                    }
                    i = hunks.len(); // jump past all remaining
                    advance = false; // already at end
                    break;
                }
                'd' => {
                    chosen[i] = false;
                    for c in chosen.iter_mut().skip(i + 1) {
                        *c = false;
                    }
                    i = hunks.len();
                    advance = false;
                    break;
                }
                's' => {
                    let sub = add_patch::split_hunk(&hunks[i]);
                    if sub.len() <= 1 {
                        writeln!(session.stderr, "Sorry, cannot split this hunk")?;
                        continue;
                    }
                    let split_count = sub.len();
                    // Replace hunks[i] with `sub` in-place; update `chosen`.
                    hunks.splice(i..=i, sub);
                    let new_chosen: Vec<bool> = vec![false; split_count];
                    chosen.splice(i..=i, new_chosen);
                    writeln!(session.stderr, "Split into {split_count} hunks.")?;
                    // Stay at the same `i` and reprompt for the freshly-split
                    // first sub-hunk.
                    advance = false;
                    break;
                }
                '?' => {
                    print_help(session)?;
                    continue;
                }
                'j' | 'J' => {
                    // Leave undecided — move to the next hunk without
                    // flipping `chosen[i]`. `J` is "leave undecided AND
                    // jump to next undecided hunk" upstream; for our
                    // purposes both behave identically.
                    break;
                }
                'g' => {
                    // `g <N>` — jump to hunk N. With no argument, the
                    // upstream behavior is to list every hunk; we just
                    // re-prompt for the number.
                    let rest = line.trim()[1..].trim();
                    let target: Option<usize> = if rest.is_empty() {
                        write!(session.stderr, "go to which hunk? ")?;
                        session.stderr.flush()?;
                        read_line(&mut session.stdin)?.and_then(|s| s.trim().parse::<usize>().ok())
                    } else {
                        rest.parse::<usize>().ok()
                    };
                    if let Some(n) = target {
                        if n >= 1 && n <= hunks.len() {
                            i = n - 1;
                            advance = false;
                            break;
                        }
                    }
                    writeln!(
                        session.stderr,
                        "Sorry, only {} hunks available.",
                        hunks.len()
                    )?;
                    continue;
                }
                '/' => {
                    // `/<pattern>` — skip forward to the next hunk whose
                    // body contains <pattern> (substring match).
                    let pat = line.trim()[1..].trim().to_string();
                    if pat.is_empty() {
                        writeln!(session.stderr, "Search pattern required after `/`.")?;
                        continue;
                    }
                    let pat_bytes = pat.as_bytes();
                    let found =
                        (i + 1..hunks.len()).find(|&j| hunk_body_contains(&hunks[j], pat_bytes));
                    if let Some(j) = found {
                        i = j;
                        advance = false;
                        break;
                    }
                    writeln!(session.stderr, "No hunk matches the given pattern.")?;
                    continue;
                }
                'e' => {
                    // Edit the hunk in $EDITOR. Write the formatted hunk
                    // bytes to a temp file, spawn the editor, re-parse.
                    let formatted = add_patch::format_hunk(&hunks[i]);
                    let tmp = std::env::temp_dir().join(format!(
                        "rustygit-addp-{}-{}.diff",
                        std::process::id(),
                        i
                    ));
                    std::fs::write(&tmp, &formatted)?;
                    let editor = crate::cli::var::pick_editor(&crate::config::Config::empty());
                    let status = std::process::Command::new("sh")
                        .arg("-c")
                        .arg(format!("{editor} {}", tmp.display()))
                        .status();
                    if status.is_err() || !status.as_ref().is_ok_and(|s| s.success()) {
                        writeln!(
                            session.stderr,
                            "Editor exited with non-zero status; keeping original hunk."
                        )?;
                        let _ = std::fs::remove_file(&tmp);
                        continue;
                    }
                    let edited = std::fs::read(&tmp)?;
                    let _ = std::fs::remove_file(&tmp);
                    match add_patch::parse_hunks_from_diff(&edited) {
                        Ok(mut parsed) if !parsed.is_empty() => {
                            // Replace hunks[i] with the parsed list.
                            let new_count = parsed.len();
                            let replacement: Vec<bool> = vec![true; new_count];
                            hunks.splice(i..=i, parsed.drain(..));
                            chosen.splice(i..=i, replacement);
                            advance = false;
                            break;
                        }
                        _ => {
                            writeln!(
                                session.stderr,
                                "Edited hunk did not parse; keeping original."
                            )?;
                            continue;
                        }
                    }
                }
                _ => {
                    print_help(session)?;
                    continue;
                }
            }
        }

        if quit {
            break;
        }
        if advance {
            i += 1;
        }
    }

    // Apply chosen hunks to base.
    let chosen_refs: Vec<&Hunk> = hunks
        .iter()
        .zip(chosen.iter().copied())
        .filter_map(|(h, c)| if c { Some(h) } else { None })
        .collect();

    if !chosen_refs.is_empty() {
        let new_content = add_patch::apply_hunks_to_base(&base_bytes, &chosen_refs);
        upsert_with_content(repo, index, pair, &new_content)?;
    }

    if quit {
        Ok(Some(Action::Quit))
    } else {
        Ok(Some(Action::Continue))
    }
}

/// Read the content of a blob OID. Workdir blobs are pre-hashed into the
/// odb by `flatten_workdir_against_index`, so this always succeeds for the
/// candidates we produce.
fn read_blob_bytes(repo: &Repository, oid: &ObjectId) -> io::Result<Vec<u8>> {
    let raw = repo
        .odb()
        .read(oid)
        .map_err(|e| io::Error::other(format!("read blob {oid}: {e}")))?;
    if raw.kind != ObjectKind::Blob {
        return Err(io::Error::other(format!(
            "expected blob for {oid}, got {}",
            raw.kind
        )));
    }
    Ok(raw.data)
}

/// Write `content` as a new blob, look up the existing IndexEntry for the
/// path to inherit its mode/stat fields, and replace its oid (also updating
/// the size field). The workfile is intentionally NOT touched — unchosen
/// changes remain in the worktree.
fn upsert_with_content(
    repo: &Repository,
    index: &mut Index,
    pair: &DiffPair,
    content: &[u8],
) -> io::Result<()> {
    let path = &pair.a.as_ref().expect("a-side present").path;
    let blob = RawObject::new(ObjectKind::Blob, content.to_vec());
    let new_oid = repo
        .odb()
        .write(&blob)
        .map_err(|e| io::Error::other(format!("write blob: {e}")))?;

    // Find the existing stage-0 entry and update its oid + size in place.
    let existing = index
        .entries
        .iter()
        .find(|e| e.path == *path && e.stage == 0)
        .cloned();
    let mut entry: IndexEntry = match existing {
        Some(e) => e,
        None => {
            return Err(io::Error::other(format!(
                "index entry missing for {}",
                display_path(path)
            )));
        }
    };
    entry.oid = new_oid;
    entry.size = content.len().min(u32::MAX as usize) as u32;
    // If the b-side reflected a mode change, propagate it.
    if let Some(b) = &pair.b {
        entry.mode = b.mode.to_index_mode();
    }
    index.upsert(entry);
    Ok(())
}

/// Single-prompt deletion staging — `D` against the worktree where the user
/// removed the file. Subset of git's `prompt_deletion` flow.
fn prompt_deletion(
    repo: &Repository,
    index: &mut Index,
    pair: &DiffPair,
    session: &mut Session<'_>,
) -> io::Result<Option<Action>> {
    let path = &pair.a.as_ref().expect("Deleted has a-side").path;
    let path_display = display_path(path);
    writeln!(
        session.stdout,
        "diff --git a/{path_display} b/{path_display}"
    )?;
    writeln!(session.stdout, "deleted file mode")?;

    loop {
        write!(
            session.stderr,
            "Stage deletion of {path_display} [y,n,q,a,d,?]? "
        )?;
        session.stderr.flush()?;
        let line = match read_line(&mut session.stdin)? {
            Some(l) => l,
            None => return Ok(Some(Action::Quit)),
        };
        let ch = line.trim().chars().next().unwrap_or('\0');
        match ch {
            'y' | 'a' => {
                index.remove(path);
                // Best-effort odb GC of the now-orphan blob is not needed.
                let _ = repo;
                return Ok(Some(Action::Continue));
            }
            'n' | 'd' => return Ok(Some(Action::Continue)),
            'q' => return Ok(Some(Action::Quit)),
            '?' => {
                print_help(session)?;
                continue;
            }
            _ => {
                print_help(session)?;
                continue;
            }
        }
    }
}

fn print_help(session: &mut Session<'_>) -> io::Result<()> {
    // Wording mirrors git/add-patch.c::patch_mode_add.help_patch_text plus
    // the lines covering `s` and `?` that git puts in its help_patch_text
    // for hunk-mode prompts (split + help). The deferred actions are
    // documented but flagged.
    writeln!(
        session.stderr,
        "y - stage this hunk\n\
         n - do not stage this hunk\n\
         q - quit; do not stage this hunk or any of the remaining ones\n\
         a - stage this hunk and all later hunks in the file\n\
         d - do not stage this hunk or any of the later hunks in the file\n\
         s - split the current hunk into smaller hunks\n\
         e - manually edit the current hunk (not yet implemented)\n\
         g - select a hunk to go to (not yet implemented)\n\
         j - leave this hunk undecided, see next undecided hunk (not yet implemented)\n\
         J - leave this hunk undecided, see next hunk (not yet implemented)\n\
         / - search for a hunk matching the given regex (not yet implemented)\n\
         ? - print help"
    )
}

/// Read one trimmed line from stdin. Returns `Ok(None)` on EOF.
/// True if any line in the hunk's body contains the byte pattern.
fn hunk_body_contains(h: &add_patch::Hunk, pat: &[u8]) -> bool {
    for line in &h.lines {
        if pat.is_empty() {
            return true;
        }
        if line.content.windows(pat.len()).any(|w| w == pat) {
            return true;
        }
    }
    false
}

fn read_line<R: BufRead + ?Sized>(stdin: &mut R) -> io::Result<Option<String>> {
    let mut buf = String::new();
    let n = stdin.read_line(&mut buf)?;
    if n == 0 {
        return Ok(None);
    }
    Ok(Some(buf))
}

fn display_path(p: &[u8]) -> String {
    String::from_utf8_lossy(p).into_owned()
}

fn io_err<E: std::fmt::Display>(e: E) -> io::Error {
    io::Error::other(format!("{e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// Sanity test: an empty stdin with no candidate diffs should produce
    /// exit code 0 without panicking. We construct an empty in-memory repo.
    #[test]
    fn empty_repo_no_op() {
        let tmp = tempfile::tempdir().unwrap();
        let gitdir = tmp.path().join(".git");
        std::fs::create_dir_all(gitdir.join("objects")).unwrap();
        std::fs::create_dir_all(gitdir.join("refs")).unwrap();
        std::fs::write(gitdir.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        let repo = Repository::open(gitdir).unwrap();

        let stdin_data: &[u8] = b"";
        let mut stdout_buf: Vec<u8> = Vec::new();
        let mut stderr_buf: Vec<u8> = Vec::new();
        let mut session = Session {
            stdin: Box::new(Cursor::new(stdin_data)),
            stdout: Box::new(&mut stdout_buf),
            stderr: Box::new(&mut stderr_buf),
        };
        let code = run_with_io(&repo, &mut session).unwrap();
        assert_eq!(code, 0);
    }
}
