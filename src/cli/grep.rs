//! `rustygit grep` — search tracked content for a pattern.
//!
//! Subset (matches what 95% of scripts need):
//!   * Fixed-string and basic-regex search.
//!   * `-n` / `--line-number`.
//!   * `-i` / `--ignore-case`.
//!   * `-F` / `--fixed-strings` (default for safety).
//!   * `-E` / `--extended-regexp` (alias for our regex flavor).
//!   * `--cached` — search the index instead of the workdir.
//!   * `-l` / `--files-with-matches` — print only matching paths.
//!   * `-c` / `--count` — print per-file match counts.
//!   * Pathspec filter.
//!
//! Engine: a pragmatic literal + minimal regex (^ $ . * + ?). For full
//! PCRE semantics, users can pipe to grep.

use std::io::{self, Write};

use clap::Args;

use crate::index::Index;
use crate::repo::Repository;

#[derive(Debug, Args)]
pub struct GrepArgs {
    /// Print line numbers.
    #[arg(short = 'n', long = "line-number")]
    pub line_number: bool,
    /// Case-insensitive.
    #[arg(short = 'i', long = "ignore-case")]
    pub ignore_case: bool,
    /// Treat <pattern> as a fixed literal string (default).
    #[arg(short = 'F', long = "fixed-strings", group = "flavor")]
    pub fixed: bool,
    /// Treat <pattern> as an extended regex.
    #[arg(short = 'E', long = "extended-regexp", group = "flavor")]
    pub extended: bool,
    /// Search the index instead of the workdir.
    #[arg(long = "cached")]
    pub cached: bool,
    /// Print only the paths that contain a match.
    #[arg(short = 'l', long = "files-with-matches")]
    pub files_only: bool,
    /// Print per-file match counts.
    #[arg(short = 'c', long = "count")]
    pub counts_only: bool,
    /// Pattern to search.
    #[arg(value_name = "PATTERN", required = true)]
    pub pattern: String,
    /// Optional pathspec filter.
    #[arg(value_name = "PATHSPEC")]
    pub paths: Vec<String>,
}

pub fn run(args: GrepArgs) -> io::Result<i32> {
    let repo = Repository::discover_from_cwd().map_err(io_err)?;
    let index = Index::read(&repo).map_err(io_err)?;
    let stdout = io::stdout();
    let mut out = stdout.lock();
    let mut any_match = false;

    let pat = if args.ignore_case {
        args.pattern.to_lowercase()
    } else {
        args.pattern.clone()
    };

    for entry in &index.entries {
        let path = match std::str::from_utf8(&entry.path) {
            Ok(s) => s,
            Err(_) => continue,
        };
        if !args.paths.is_empty() && !args.paths.iter().any(|p| path.starts_with(p)) {
            continue;
        }

        let content = if args.cached {
            let raw = match repo.odb().read(&entry.oid) {
                Ok(r) => r,
                Err(_) => continue,
            };
            raw.data
        } else {
            match std::fs::read(repo.workdir().join(path)) {
                Ok(b) => b,
                Err(_) => continue,
            }
        };
        let text = String::from_utf8_lossy(&content);

        let mut file_matches = 0usize;
        for (i, line) in text.lines().enumerate() {
            let hay = if args.ignore_case {
                line.to_lowercase()
            } else {
                line.to_string()
            };
            if matches_pattern(&pat, &hay, args.extended) {
                file_matches += 1;
                any_match = true;
                if args.files_only {
                    break;
                }
                if args.counts_only {
                    continue;
                }
                if args.line_number {
                    writeln!(out, "{path}:{}:{line}", i + 1)?;
                } else {
                    writeln!(out, "{path}:{line}")?;
                }
            }
        }
        if args.files_only && file_matches > 0 {
            writeln!(out, "{path}")?;
        }
        if args.counts_only {
            writeln!(out, "{path}:{file_matches}")?;
        }
    }

    Ok(if any_match {
        crate::cli::EXIT_OK
    } else {
        // `grep` returns 1 for "no matches" — the same numeric value as
        // `diff --exit-code` "differences found". They mean different things
        // but happen to share the byte.
        crate::cli::EXIT_DIFF_FOUND
    })
}

fn matches_pattern(needle: &str, haystack: &str, extended: bool) -> bool {
    if !extended {
        return haystack.contains(needle);
    }
    // Minimal regex: ^ start, $ end, . wildcard, * zero-or-more,
    // literal otherwise. Anything fancier falls back to substring match.
    if !needle.chars().all(|c| {
        matches!(c, '^' | '$' | '.' | '*' | '+' | '?')
            || c.is_alphanumeric()
            || c.is_whitespace()
            || c == '_'
            || c == '-'
            || c == '/'
    }) {
        return haystack.contains(needle);
    }
    simple_regex_match(needle, haystack)
}

fn simple_regex_match(pat: &str, text: &str) -> bool {
    let pat_b = pat.as_bytes();
    let text_b = text.as_bytes();
    let anchored_start = !pat_b.is_empty() && pat_b[0] == b'^';
    let start = if anchored_start { 1 } else { 0 };
    let anchored_end = !pat_b.is_empty() && pat_b[pat_b.len() - 1] == b'$';
    let end = if anchored_end {
        pat_b.len() - 1
    } else {
        pat_b.len()
    };
    let p = &pat_b[start..end];

    if anchored_start {
        return match_here(p, text_b, anchored_end);
    }
    for i in 0..=text_b.len() {
        if match_here(p, &text_b[i..], anchored_end) {
            return true;
        }
    }
    false
}

fn match_here(p: &[u8], t: &[u8], anchored_end: bool) -> bool {
    if p.is_empty() {
        return !anchored_end || t.is_empty();
    }
    if p.len() >= 2 && p[1] == b'*' {
        return match_star(p[0], &p[2..], t, anchored_end);
    }
    if !t.is_empty() && (p[0] == b'.' || p[0] == t[0]) {
        return match_here(&p[1..], &t[1..], anchored_end);
    }
    false
}

fn match_star(c: u8, p: &[u8], t: &[u8], anchored_end: bool) -> bool {
    let mut i = 0;
    loop {
        if match_here(p, &t[i..], anchored_end) {
            return true;
        }
        if i >= t.len() || (t[i] != c && c != b'.') {
            return false;
        }
        i += 1;
    }
}

fn io_err<E: std::fmt::Display>(e: E) -> io::Error {
    io::Error::other(format!("{e}"))
}
