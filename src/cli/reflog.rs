//! `rustygit reflog` — display reflog entries.
//!
//! Reads `.git/logs/<refname>` (one entry per line) and prints them in
//! newest-first order, matching the default `git reflog show` format:
//!
//! ```text
//! <short-oid> <ref>@{<n>}: <message>
//! ```
//!
//! On-disk format (the same one our `refs::reflog::append` writes):
//!
//! ```text
//! <old-oid> <new-oid> <committer-name> <<committer-email>> <unix-secs> <±HHMM>\t<message>\n
//! ```
//!
//! Where index `n` counts BACKWARD from the most-recent entry: the newest line
//! is `@{0}`, the one before is `@{1}`, etc.
//!
//! Subcommands implemented in M14: `show` (the default). `expire`, `delete`,
//! and `exists` follow in a later milestone.

use std::io::{self, Write};

use clap::Args;

use crate::config::Config;
use crate::refs::FullName;
use crate::repo::Repository;

#[derive(Debug, Args)]
pub struct ReflogArgs {
    /// Subcommand. M14 only supports "show" (the default).
    #[arg(value_name = "SUBCOMMAND", default_value = "show")]
    pub subcommand: String,
    /// Ref to read the reflog for. Defaults to HEAD.
    #[arg(value_name = "REF", default_value = "HEAD")]
    pub refname: String,
}

pub fn run(args: ReflogArgs) -> io::Result<i32> {
    let (subcmd, refname) = normalize_args(&args);
    let repo = Repository::discover_from_cwd().map_err(|e| io::Error::other(format!("{e}")))?;

    match subcmd.as_str() {
        "show" => show(&repo, &refname),
        "expire" => expire(&repo, &refname),
        "delete" => delete(&repo, &refname),
        "exists" => exists(&repo, &refname),
        other => {
            eprintln!("rustygit: reflog: unsupported subcommand '{other}'");
            Ok(129)
        }
    }
}

fn show(repo: &Repository, refname: &str) -> io::Result<i32> {
    let full = FullName::new(refname.to_string()).map_err(|e| io::Error::other(format!("{e}")))?;
    let log_path = repo.gitdir().join("logs").join(full.loose_path_relative());
    let bytes = match std::fs::read(&log_path) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e),
    };
    let entries = parse_reflog(&bytes);
    let cfg = Config::from_repo_dir(repo.gitdir()).unwrap_or_else(|_| Config::empty());
    let mut out = crate::cli::pager::open(&cfg, false)?;
    print_entries(&mut out, &entries, refname)?;
    Ok(0)
}

/// Truncate the reflog file in place. Without --expire-time support
/// (deferred), this drops every entry. Matches `git reflog expire
/// --all`'s nuke-everything behavior when given no time constraint.
fn expire(repo: &Repository, refname: &str) -> io::Result<i32> {
    // Special-case "--all" — clear every reflog under logs/.
    if refname == "--all" {
        let logs = repo.gitdir().join("logs");
        if logs.is_dir() {
            walk_and_truncate(&logs)?;
        }
        return Ok(0);
    }
    let full = FullName::new(refname.to_string()).map_err(|e| io::Error::other(format!("{e}")))?;
    let log_path = repo.gitdir().join("logs").join(full.loose_path_relative());
    if log_path.is_file() {
        std::fs::write(&log_path, b"")?;
    }
    Ok(0)
}

fn walk_and_truncate(dir: &std::path::Path) -> io::Result<()> {
    for entry in std::fs::read_dir(dir)?.flatten() {
        let p = entry.path();
        if p.is_dir() {
            walk_and_truncate(&p)?;
        } else if p.is_file() {
            std::fs::write(&p, b"")?;
        }
    }
    Ok(())
}

/// Remove a single reflog entry: `reflog delete <ref>@{N}`. We rewrite
/// the file without that line.
fn delete(repo: &Repository, refname: &str) -> io::Result<i32> {
    let (name, n) = parse_ref_at_n(refname).ok_or_else(|| {
        io::Error::other(format!(
            "reflog: delete expects <ref>@{{N}}; got {refname:?}"
        ))
    })?;
    let full = FullName::new(name).map_err(|e| io::Error::other(format!("{e}")))?;
    let log_path = repo.gitdir().join("logs").join(full.loose_path_relative());
    let text = std::fs::read_to_string(&log_path)?;
    let lines: Vec<&str> = text.lines().collect();
    if n >= lines.len() {
        return Ok(1);
    }
    let last_idx = lines.len() - 1 - n;
    let mut kept: Vec<&str> = lines.clone();
    kept.remove(last_idx);
    let mut out = kept.join("\n");
    if !out.is_empty() {
        out.push('\n');
    }
    std::fs::write(&log_path, out)?;
    Ok(0)
}

/// Exit 0 iff the reflog file exists.
fn exists(repo: &Repository, refname: &str) -> io::Result<i32> {
    let full = FullName::new(refname.to_string()).map_err(|e| io::Error::other(format!("{e}")))?;
    let log_path = repo.gitdir().join("logs").join(full.loose_path_relative());
    Ok(if log_path.is_file() { 0 } else { 1 })
}

fn parse_ref_at_n(s: &str) -> Option<(String, usize)> {
    let at = s.find("@{")?;
    let close = s.find('}')?;
    if close <= at + 2 {
        return None;
    }
    let name = s[..at].to_string();
    let n = s[at + 2..close].parse::<usize>().ok()?;
    Some((name, n))
}

/// Normalize the `(subcommand, refname)` pair. clap gives us a positional
/// default of "show" for `subcommand` but doesn't know that, e.g., `reflog
/// refs/heads/main` means "show refs/heads/main", not "subcommand =
/// refs/heads/main". We do that DWIM here.
fn normalize_args(args: &ReflogArgs) -> (String, String) {
    const KNOWN_SUBCOMMANDS: &[&str] = &["show", "expire", "delete", "exists"];
    if KNOWN_SUBCOMMANDS.contains(&args.subcommand.as_str()) {
        return (args.subcommand.clone(), args.refname.clone());
    }
    // The user wrote `rustygit reflog refs/heads/main`. The positional landed
    // in `subcommand` because of the order. Treat it as the ref name.
    // If `refname` is still the default ("HEAD"), use the unrecognized
    // `subcommand` value as the refname.
    if args.refname == "HEAD" {
        return ("show".into(), args.subcommand.clone());
    }
    // Both are present and `subcommand` is not in the known list — bubble up
    // the unknown subcommand so the user sees a clear error.
    (args.subcommand.clone(), args.refname.clone())
}

/// One parsed entry from the reflog. We expose only the fields the printer
/// cares about (the "new" oid and the message).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReflogEntry {
    pub new_hex: String,
    pub message: String,
}

/// Parse a reflog blob into a Vec of entries in file order (oldest first).
///
/// Format: `<old> <new> <ident> <ts> <offset>\t<msg>\n` per line. We tolerate
/// blank lines (skip them) and lines missing a tab (treat as having an empty
/// message). Lines without a recognizable two-oid prefix are skipped — losing
/// one corrupted entry beats failing the whole listing.
pub(crate) fn parse_reflog(bytes: &[u8]) -> Vec<ReflogEntry> {
    let mut out = Vec::new();
    let text = String::from_utf8_lossy(bytes);
    for raw in text.lines() {
        let line = raw.trim_end_matches('\r');
        if line.is_empty() {
            continue;
        }
        // Split message on the first tab.
        let (head, msg) = match line.split_once('\t') {
            Some((h, m)) => (h, m),
            None => (line, ""),
        };
        // The header is "<old> <new> <name> <<email>> <ts> <offset>". We only
        // need the two oids, so we take the first two whitespace-separated
        // tokens and ignore the rest.
        let mut toks = head.split_whitespace();
        let _old = match toks.next() {
            Some(t) => t,
            None => continue,
        };
        let new_hex = match toks.next() {
            Some(t) => t.to_string(),
            None => continue,
        };
        out.push(ReflogEntry {
            new_hex,
            message: msg.to_string(),
        });
    }
    out
}

/// Print parsed entries in newest-first order, indexed from 0.
pub(crate) fn print_entries(
    out: &mut dyn Write,
    entries: &[ReflogEntry],
    refname: &str,
) -> io::Result<()> {
    // Walk in reverse so the latest entry is index 0 — git's convention.
    let total = entries.len();
    for (i, entry) in entries.iter().rev().enumerate() {
        let _ = total;
        let short = short_hex(&entry.new_hex, 7);
        writeln!(out, "{short} {refname}@{{{i}}}: {msg}", msg = entry.message)?;
    }
    Ok(())
}

fn short_hex(hex: &str, n: usize) -> String {
    if hex.len() <= n {
        hex.to_string()
    } else {
        hex[..n].to_string()
    }
}

// ----------------------------------------------------------------------------
// Tests
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::Path;
    use std::process::{Command as SysCommand, Output};
    use tempfile::TempDir;

    // ---- harness helpers ----

    fn has_system_git() -> bool {
        SysCommand::new("git")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    fn git(args: &[&str], cwd: &Path) -> Output {
        let out = SysCommand::new("git")
            .args(args)
            .current_dir(cwd)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .env("GIT_AUTHOR_DATE", "1700000000 +0000")
            .env("GIT_COMMITTER_DATE", "1700000000 +0000")
            .output()
            .expect("failed to spawn git");
        assert!(
            out.status.success(),
            "git {args:?} failed in {cwd:?}\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );
        out
    }

    fn rustygit(args: &[&str], cwd: &Path) -> Option<Output> {
        let bin = assert_cmd::Command::cargo_bin("rustygit").ok()?;
        let mut c = bin;
        Some(
            c.args(args)
                .current_dir(cwd)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t")
                .env("GIT_AUTHOR_DATE", "1700000000 +0000")
                .env("GIT_COMMITTER_DATE", "1700000000 +0000")
                .output()
                .unwrap(),
        )
    }

    fn integration_ready(tmp: &Path) -> bool {
        if !has_system_git() {
            return false;
        }
        let out = assert_cmd::Command::cargo_bin("rustygit")
            .ok()
            .map(|mut c| c.arg("--help").current_dir(tmp).output());
        matches!(out, Some(Ok(o)) if o.status.success())
    }

    // ---- pure-function tests ----

    #[test]
    fn parses_typical_three_line_reflog() {
        let bytes = b"\
0000000000000000000000000000000000000000 1111111111111111111111111111111111111111 Test <t@e> 1700000000 +0000\tbranch: create\n\
1111111111111111111111111111111111111111 2222222222222222222222222222222222222222 Test <t@e> 1700000001 +0000\tcommit: c2\n\
2222222222222222222222222222222222222222 3333333333333333333333333333333333333333 Test <t@e> 1700000002 +0000\tcommit: c3\n";
        let entries = parse_reflog(bytes);
        assert_eq!(entries.len(), 3);
        assert_eq!(
            entries[0].new_hex,
            "1111111111111111111111111111111111111111"
        );
        assert_eq!(entries[0].message, "branch: create");
        assert_eq!(entries[2].message, "commit: c3");
    }

    #[test]
    fn parses_skips_blank_lines_and_malformed_lines() {
        let bytes = b"\
\n\
0000000000000000000000000000000000000000 1111111111111111111111111111111111111111 a <a@a> 1 +0000\tone\n\
\n\
not-a-reflog-line\n\
1111111111111111111111111111111111111111 2222222222222222222222222222222222222222 a <a@a> 2 +0000\ttwo\n";
        let entries = parse_reflog(bytes);
        // "not-a-reflog-line" has only one token, so we skip it; blank lines skipped.
        // The "not-a-reflog-line" actually has one token: it'll be treated as "old" but no "new" → skipped.
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].message, "one");
        assert_eq!(entries[1].message, "two");
    }

    #[test]
    fn parses_line_without_tab_has_empty_message() {
        let bytes = b"0000000000000000000000000000000000000000 1111111111111111111111111111111111111111 a <a@a> 1 +0000\n";
        let entries = parse_reflog(bytes);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].message, "");
        assert_eq!(
            entries[0].new_hex,
            "1111111111111111111111111111111111111111"
        );
    }

    #[test]
    fn parses_empty_file_yields_empty_vec() {
        assert!(parse_reflog(b"").is_empty());
    }

    #[test]
    fn print_entries_emits_newest_first_with_index() {
        let entries = vec![
            ReflogEntry {
                new_hex: "1111111111111111111111111111111111111111".into(),
                message: "branch: create".into(),
            },
            ReflogEntry {
                new_hex: "2222222222222222222222222222222222222222".into(),
                message: "commit: c2".into(),
            },
            ReflogEntry {
                new_hex: "3333333333333333333333333333333333333333".into(),
                message: "commit: c3".into(),
            },
        ];
        let mut buf: Vec<u8> = Vec::new();
        print_entries(&mut buf, &entries, "HEAD").unwrap();
        let got = String::from_utf8(buf).unwrap();
        let lines: Vec<&str> = got.lines().collect();
        assert_eq!(lines.len(), 3);
        // Latest entry first, index 0.
        assert_eq!(lines[0], "3333333 HEAD@{0}: commit: c3");
        assert_eq!(lines[1], "2222222 HEAD@{1}: commit: c2");
        assert_eq!(lines[2], "1111111 HEAD@{2}: branch: create");
    }

    #[test]
    fn print_entries_handles_empty_input() {
        let mut buf: Vec<u8> = Vec::new();
        print_entries(&mut buf, &[], "HEAD").unwrap();
        assert!(buf.is_empty());
    }

    #[test]
    fn short_hex_handles_short_and_long() {
        assert_eq!(short_hex("abc", 7), "abc");
        assert_eq!(short_hex("abcdef0123456789", 7), "abcdef0");
    }

    #[test]
    fn args_parse_default_subcommand_and_ref() {
        use clap::Parser;
        #[derive(Debug, Parser)]
        struct Wrap {
            #[command(flatten)]
            args: ReflogArgs,
        }
        let w = Wrap::try_parse_from(["x"]).unwrap();
        assert_eq!(w.args.subcommand, "show");
        assert_eq!(w.args.refname, "HEAD");
    }

    #[test]
    fn args_parse_show_with_ref() {
        use clap::Parser;
        #[derive(Debug, Parser)]
        struct Wrap {
            #[command(flatten)]
            args: ReflogArgs,
        }
        let w = Wrap::try_parse_from(["x", "show", "refs/heads/main"]).unwrap();
        assert_eq!(w.args.subcommand, "show");
        assert_eq!(w.args.refname, "refs/heads/main");
    }

    #[test]
    fn normalize_dwims_bare_refname() {
        // `rustygit reflog refs/heads/main` — clap places the refname into
        // `subcommand`; we should detect it and remap.
        let args = ReflogArgs {
            subcommand: "refs/heads/main".into(),
            refname: "HEAD".into(),
        };
        let (s, r) = normalize_args(&args);
        assert_eq!(s, "show");
        assert_eq!(r, "refs/heads/main");
    }

    #[test]
    fn normalize_preserves_known_subcommand() {
        let args = ReflogArgs {
            subcommand: "show".into(),
            refname: "refs/heads/feature".into(),
        };
        let (s, r) = normalize_args(&args);
        assert_eq!(s, "show");
        assert_eq!(r, "refs/heads/feature");
    }

    // ---- integration tests (rustygit binary) ----

    /// Test #9 in the M14 spec: `reflog` lists HEAD's history including a
    /// commit and a checkout.
    #[test]
    fn reflog_lists_head_history() {
        let tmp = TempDir::new().unwrap();
        if !integration_ready(tmp.path()) {
            return;
        }
        git(&["init", "-q", "-b", "master", "."], tmp.path());
        // 1. First commit (writes HEAD@{1} pointing at the new commit).
        std::fs::write(tmp.path().join("f.txt"), b"v1\n").unwrap();
        git(&["add", "f.txt"], tmp.path());
        git(&["commit", "-q", "-m", "c1"], tmp.path());
        // 2. Create + checkout a new branch (writes HEAD@{0}).
        git(&["checkout", "-q", "-b", "feature"], tmp.path());

        let r = rustygit(&["reflog"], tmp.path()).unwrap();
        assert!(
            r.status.success(),
            "reflog failed: stderr={}",
            String::from_utf8_lossy(&r.stderr)
        );
        let stdout = String::from_utf8_lossy(&r.stdout);
        // Format: "<oid> HEAD@{0}: <msg>\n<oid> HEAD@{1}: <msg>\n..."
        let lines: Vec<&str> = stdout.lines().collect();
        assert!(
            lines.len() >= 2,
            "expected >= 2 reflog lines, got {}: {stdout}",
            lines.len()
        );
        assert!(
            lines[0].contains("HEAD@{0}:"),
            "first line missing HEAD@{{0}}: {stdout}"
        );
        assert!(
            lines[1].contains("HEAD@{1}:"),
            "second line missing HEAD@{{1}}: {stdout}"
        );
    }

    /// Test #10: `reflog refs/heads/main` works for non-HEAD refs.
    #[test]
    fn reflog_works_for_branch_ref() {
        let tmp = TempDir::new().unwrap();
        if !integration_ready(tmp.path()) {
            return;
        }
        git(&["init", "-q", "-b", "master", "."], tmp.path());
        std::fs::write(tmp.path().join("f.txt"), b"v1\n").unwrap();
        git(&["add", "f.txt"], tmp.path());
        git(&["commit", "-q", "-m", "c1"], tmp.path());
        std::fs::write(tmp.path().join("f.txt"), b"v2\n").unwrap();
        git(&["add", "f.txt"], tmp.path());
        git(&["commit", "-q", "-m", "c2"], tmp.path());

        let r = rustygit(&["reflog", "refs/heads/master"], tmp.path()).unwrap();
        assert!(
            r.status.success(),
            "reflog refs/heads/master failed: stderr={}",
            String::from_utf8_lossy(&r.stderr)
        );
        let stdout = String::from_utf8_lossy(&r.stdout);
        // Should list both commits.
        let lines: Vec<&str> = stdout.lines().collect();
        assert!(lines.len() >= 2, "expected >= 2 entries: {stdout}");
        for line in &lines {
            assert!(
                line.contains("refs/heads/master@{"),
                "line missing ref label: {line}"
            );
        }
    }

    #[test]
    fn reflog_missing_log_returns_empty_success() {
        let tmp = TempDir::new().unwrap();
        if !integration_ready(tmp.path()) {
            return;
        }
        git(&["init", "-q", "-b", "master", "."], tmp.path());
        // No commits → .git/logs/HEAD doesn't exist.
        let r = rustygit(&["reflog"], tmp.path()).unwrap();
        assert!(r.status.success());
        assert!(
            r.stdout.is_empty(),
            "expected empty stdout; got: {:?}",
            String::from_utf8_lossy(&r.stdout)
        );
    }

    #[test]
    fn reflog_bare_ref_positional_dwims() {
        // `rustygit reflog refs/heads/master` (no explicit `show`).
        let tmp = TempDir::new().unwrap();
        if !integration_ready(tmp.path()) {
            return;
        }
        git(&["init", "-q", "-b", "master", "."], tmp.path());
        std::fs::write(tmp.path().join("f.txt"), b"v1\n").unwrap();
        git(&["add", "f.txt"], tmp.path());
        git(&["commit", "-q", "-m", "c1"], tmp.path());

        let r = rustygit(&["reflog", "refs/heads/master"], tmp.path()).unwrap();
        assert!(
            r.status.success(),
            "stderr={}",
            String::from_utf8_lossy(&r.stderr)
        );
        let stdout = String::from_utf8_lossy(&r.stdout);
        assert!(stdout.contains("refs/heads/master@{0}:"));
    }
}
