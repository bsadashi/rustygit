//! `rustygit log` — walk the commit ancestor chain and print each commit.
//!
//! M3 scope: starts at HEAD (or a given rev), follows first-parent only,
//! formats in `--pretty=medium` style (the git default for `log`):
//!
//! ```text
//! commit <oid>
//! Author: <name> <<email>>
//! Date:   <human-readable>
//!
//!     <message lines indented by 4 spaces>
//! ```
//!
//! Out of scope: `--graph`, multi-parent merge traversal, date filters,
//! `--oneline`, `--pretty=format:...`, file filters, `--all`. M5/M6 handle
//! these as the corresponding subsystems land.

use std::io::{self, Write};

use clap::Args;

use crate::commit::Commit;
use crate::config::Config;
use crate::hash::ObjectId;
use crate::repo::Repository;
use crate::revparse::resolve;

#[derive(Debug, Args)]
pub struct LogArgs {
    /// Limit output to the first N commits.
    #[arg(short = 'n', long = "max-count", value_name = "N")]
    pub max: Option<usize>,

    /// One-line summary instead of medium format. Implies `--abbrev-commit`.
    #[arg(long = "oneline")]
    pub oneline: bool,

    /// Abbreviate object IDs in the output to this many hex chars.
    /// Default: 7 with `--oneline`, full-width otherwise.
    #[arg(long = "abbrev", value_name = "N")]
    pub abbrev: Option<usize>,

    /// Explicitly enable oid abbreviation (without forcing the oneline format).
    #[arg(long = "abbrev-commit")]
    pub abbrev_commit: bool,

    /// Show the patch (unified diff) for each commit.
    #[arg(short = 'p', long = "patch")]
    pub patch: bool,

    /// Only show commits whose message contains <pattern> (substring).
    #[arg(long = "grep", value_name = "PATTERN")]
    pub grep: Option<String>,

    /// Only show commits whose author matches <pattern> (substring).
    #[arg(long = "author", value_name = "PATTERN")]
    pub author: Option<String>,

    /// Only show commits whose committer matches <pattern> (substring).
    #[arg(long = "committer", value_name = "PATTERN")]
    pub committer: Option<String>,

    /// Optional starting revision (defaults to HEAD).
    #[arg(value_name = "REV", default_value = "HEAD")]
    pub start: String,
}

pub fn run(args: LogArgs) -> io::Result<i32> {
    let repo = Repository::discover_from_cwd().map_err(io_err)?;
    let mut cur = match resolve(repo.refs(), repo.odb(), &args.start) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("rustygit: log: {e}");
            return Ok(128);
        }
    };

    // Resolve the abbreviation width. `--oneline` and `--abbrev-commit` both
    // turn it on; `--abbrev=<n>` overrides the width. Default is 7 chars
    // (matching git's `core.abbrev = 7` default).
    let abbrev_active = args.oneline || args.abbrev_commit || args.abbrev.is_some();
    let abbrev_width = args.abbrev.unwrap_or(7);
    let oid_render = |o: &ObjectId| -> String {
        if abbrev_active {
            o.short_hex(abbrev_width)
        } else {
            o.to_string()
        }
    };

    let cfg = Config::from_repo_dir(repo.gitdir()).unwrap_or_else(|_| Config::empty());
    let mut out = crate::cli::pager::open(&cfg, false)?;

    let mut printed = 0usize;
    loop {
        if out.stopped() {
            // Pager closed (user pressed `q`). Stop emitting.
            break;
        }
        if let Some(limit) = args.max {
            if printed >= limit {
                break;
            }
        }
        let obj = repo.odb().read(&cur).map_err(io_err)?;
        if obj.kind != crate::object::ObjectKind::Commit {
            eprintln!("rustygit: log: {cur} is not a commit");
            return Ok(128);
        }
        let commit = Commit::parse(&obj.data, repo.hash_kind()).map_err(io_err)?;

        // Filters: --grep / --author / --committer.
        let msg_str = String::from_utf8_lossy(&commit.message);
        if let Some(pat) = &args.grep {
            if !msg_str.contains(pat.as_str()) {
                match commit.parents.first().copied() {
                    Some(p) => {
                        cur = p;
                        continue;
                    }
                    None => break,
                }
            }
        }
        if let Some(pat) = &args.author {
            if !commit.author.name.contains(pat) && !commit.author.email.contains(pat) {
                match commit.parents.first().copied() {
                    Some(p) => {
                        cur = p;
                        continue;
                    }
                    None => break,
                }
            }
        }
        if let Some(pat) = &args.committer {
            if !commit.committer.name.contains(pat) && !commit.committer.email.contains(pat) {
                match commit.parents.first().copied() {
                    Some(p) => {
                        cur = p;
                        continue;
                    }
                    None => break,
                }
            }
        }

        // git log separates commits with a blank line — emit it BEFORE each
        // commit after the first so the last commit's output doesn't have a
        // trailing blank line.
        if printed > 0 && !args.oneline {
            writeln!(out)?;
        }

        if args.oneline {
            print_oneline(&mut out, &oid_render(&cur), &commit)?;
        } else {
            print_medium(
                &mut out,
                &oid_render(&cur),
                &commit,
                abbrev_active,
                abbrev_width,
            )?;
        }

        // -p / --patch: emit the diff between this commit and its first
        // parent (or against the empty tree for a root commit).
        if args.patch {
            let parent_tree = match commit.parents.first().copied() {
                Some(p) => {
                    let praw = repo.odb().read(&p).map_err(io_err)?;
                    let pc = Commit::parse(&praw.data, repo.hash_kind()).map_err(io_err)?;
                    pc.tree
                }
                None => empty_tree_oid(),
            };
            writeln!(out)?;
            crate::diff::diff_two_trees(&repo, parent_tree, commit.tree, &mut out)
                .map_err(io_err)?;
        }
        printed += 1;

        // Shallow-clone awareness: if `cur` is at the shallow boundary, its
        // parent commit isn't present in the odb — git silently stops the
        // walk here; we do the same.
        if repo.is_shallow_boundary(&cur) {
            break;
        }

        match commit.parents.first().copied() {
            Some(p) => {
                cur = p;
            }
            None => break,
        }
    }
    Ok(0)
}

fn print_oneline(out: &mut dyn Write, oid_str: &str, c: &Commit) -> io::Result<()> {
    let summary = first_line(&c.message);
    writeln!(out, "{oid_str} {summary}")
}

fn print_medium(
    out: &mut dyn Write,
    oid_str: &str,
    c: &Commit,
    abbrev_active: bool,
    abbrev_width: usize,
) -> io::Result<()> {
    writeln!(out, "commit {oid_str}")?;
    if c.parents.len() > 1 {
        // Merge line always abbreviates; default to 7 unless caller asked for
        // a different width via --abbrev=<n>.
        let merge_width = if abbrev_active { abbrev_width } else { 7 };
        let merges: Vec<String> = c.parents.iter().map(|p| p.short_hex(merge_width)).collect();
        writeln!(out, "Merge: {}", merges.join(" "))?;
    }
    writeln!(out, "Author: {} <{}>", c.author.name, c.author.email)?;
    writeln!(out, "Date:   {}", format_date(&c.author.when))?;
    writeln!(out)?;
    for line in extract_message_body(&c.message) {
        if line.is_empty() {
            writeln!(out)?;
        } else {
            writeln!(out, "    {line}")?;
        }
    }
    Ok(())
}

/// Empty-tree oid (same constant `git` uses) — used as the "before"
/// tree when --patch encounters a root commit.
fn empty_tree_oid() -> ObjectId {
    ObjectId::parse_hex(
        crate::hash::HashKind::Sha1,
        "4b825dc642cb6eb9a060e54bf8d69288fbee4904",
    )
    .expect("empty-tree oid is a valid 40-char hex string")
}

fn first_line(msg: &[u8]) -> String {
    let s = String::from_utf8_lossy(msg);
    s.lines().next().unwrap_or("").to_string()
}

fn extract_message_body(msg: &[u8]) -> Vec<String> {
    let s = String::from_utf8_lossy(msg);
    let trimmed = s.trim_end_matches('\n');
    trimmed.lines().map(|l| l.to_string()).collect()
}

/// Public re-export so `show` (and any other porcelain that wants `git log`-
/// shaped dates) can share the same formatter.
pub(crate) fn format_date_for_show(t: &crate::identity::Time) -> String {
    format_date(t)
}

/// Format a Time as the git-compatible human date: e.g. `Mon Jan 1 00:00:00 2026 +0000`.
/// We don't have a calendar lib (no chrono per ADR A10), so shell out to `date`
/// for the day-of-week and month name. Falls back to ISO-8601 on failure.
///
/// Note: git omits the leading space `%e` produces for single-digit days, so
/// after shelling out we collapse the `<month>  <d>` double-space into single.
fn format_date(t: &crate::identity::Time) -> String {
    use std::process::Command;
    let out = Command::new("date")
        .args(["-r", &t.seconds.to_string(), "+%a %b %e %H:%M:%S %Y"])
        .env("TZ", offset_tz_env(t.offset_minutes))
        .output();
    if let Ok(o) = out {
        if o.status.success() {
            if let Ok(s) = std::str::from_utf8(&o.stdout) {
                let raw = s.trim();
                // Collapse "<month>  <day>" -> "<month> <day>" to match git.
                let normalized = collapse_double_space(raw);
                let sign = if t.offset_minutes < 0 { '-' } else { '+' };
                let abs = t.offset_minutes.unsigned_abs();
                return format!("{normalized} {sign}{:02}{:02}", abs / 60, abs % 60);
            }
        }
    }
    let sign = if t.offset_minutes < 0 { '-' } else { '+' };
    let abs = t.offset_minutes.unsigned_abs();
    format!("{} {sign}{:02}{:02}", t.seconds, abs / 60, abs % 60)
}

fn collapse_double_space(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_space = false;
    for c in s.chars() {
        if c == ' ' {
            if !prev_space {
                out.push(' ');
            }
            prev_space = true;
        } else {
            out.push(c);
            prev_space = false;
        }
    }
    out
}

/// Build a TZ env value like `UTC-5:30` so `date -r <secs>` displays the time
/// in the offset stored in the signature, regardless of the system's TZ.
fn offset_tz_env(offset_min: i32) -> String {
    // POSIX TZ inverts the sign: TZ=`UTC-5:30` means UTC+5:30 (silly POSIX).
    let inv = -offset_min;
    let sign = if inv < 0 { '-' } else { '+' };
    let abs = inv.unsigned_abs();
    format!("UTC{sign}{}:{:02}", abs / 60, abs % 60)
}

fn io_err<E: std::fmt::Display>(e: E) -> io::Error {
    io::Error::other(format!("{e}"))
}
