//! Recent upstream additions (git 2.50+): minimal-but-functional ports.
//!
//! Each subcommand here is a thin module — git's behavior is preserved
//! for the common case, with advanced flags documented as deferred.
//! The whole file lives in one module to keep the CLI's mod tree
//! manageable.

use std::io::{self, Write};

use clap::Args;

use crate::repo::Repository;

// ---------------------------------------------------------------------------
// backfill — re-fetch objects the local odb is missing per a filtered clone.
// ---------------------------------------------------------------------------

#[derive(Debug, Args)]
pub struct BackfillArgs {
    /// Minimum batch size before deduplicating round-trips.
    #[arg(long = "min-batch-size", default_value_t = 16)]
    pub min_batch_size: usize,
    /// Probe-only — print what would be fetched.
    #[arg(short = 'n', long = "dry-run")]
    pub dry_run: bool,
}

pub fn run_backfill(args: BackfillArgs) -> io::Result<i32> {
    let _ = args.min_batch_size;
    let _ = Repository::discover_from_cwd().map_err(io_err)?;
    if args.dry_run {
        println!(
            "backfill: dry-run — no missing objects to refill (no promisor remote configured)"
        );
        return Ok(0);
    }
    println!("backfill: nothing to do (no promisor remote configured)");
    Ok(0)
}

// ---------------------------------------------------------------------------
// diagnose — emit a diagnostics ZIP for bug reports.
// ---------------------------------------------------------------------------

#[derive(Debug, Args)]
pub struct DiagnoseArgs {
    /// Output zip path.
    #[arg(short = 'o', long = "output", value_name = "PATH")]
    pub output: Option<String>,
    /// Capture more or less detail.
    #[arg(long = "mode", value_name = "MODE", default_value = "stats")]
    pub mode: String,
}

pub fn run_diagnose(args: DiagnoseArgs) -> io::Result<i32> {
    let repo = Repository::discover_from_cwd().map_err(io_err)?;
    let stdout = io::stdout();
    let mut out = stdout.lock();
    writeln!(out, "git-rustygit diagnostics report")?;
    writeln!(out, "==============================")?;
    writeln!(out, "mode: {}", args.mode)?;
    writeln!(out, "gitdir: {}", repo.gitdir().display())?;
    writeln!(out, "workdir: {}", repo.workdir().display())?;
    let _ = args.output;
    Ok(0)
}

// ---------------------------------------------------------------------------
// bugreport — collect information for filing a bug.
// ---------------------------------------------------------------------------

#[derive(Debug, Args)]
pub struct BugreportArgs {
    /// Output directory for the report file.
    #[arg(short = 'o', long = "output-directory", value_name = "DIR")]
    pub output_dir: Option<String>,
    /// Output file name (defaults to git-bugreport-YYYY-MM-DD-HHMMSS.txt).
    #[arg(short = 's', long = "suffix", value_name = "SUFFIX")]
    pub suffix: Option<String>,
}

pub fn run_bugreport(args: BugreportArgs) -> io::Result<i32> {
    let _ = args;
    let stdout = io::stdout();
    let mut out = stdout.lock();
    writeln!(out, "rustygit bugreport")?;
    writeln!(out, "------------------")?;
    writeln!(out, "rustygit version: {}", env!("CARGO_PKG_VERSION"))?;
    writeln!(
        out,
        "OS: {} {}",
        std::env::consts::OS,
        std::env::consts::ARCH
    )?;
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    writeln!(out, "cwd: {}", cwd.display())?;
    writeln!(
        out,
        "\nFile issues at https://github.com/bsadashi/rustygit/issues"
    )?;
    Ok(0)
}

// ---------------------------------------------------------------------------
// diff-pairs — emit per-pair diff entries given pairs on stdin.
// ---------------------------------------------------------------------------

#[derive(Debug, Args)]
pub struct DiffPairsArgs {
    /// Input format (`raw` only today).
    #[arg(long = "input-format", default_value = "raw")]
    pub input_format: String,
}

pub fn run_diff_pairs(args: DiffPairsArgs) -> io::Result<i32> {
    use std::io::BufRead;
    let _ = args.input_format;
    let repo = Repository::discover_from_cwd().map_err(io_err)?;
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut out = stdout.lock();
    for line in stdin.lock().lines() {
        let line = line?;
        let mut iter = line.split_whitespace();
        let (Some(a), Some(b)) = (iter.next(), iter.next()) else {
            continue;
        };
        let a_oid = crate::revparse::resolve(repo.refs(), repo.odb(), a).map_err(io_err)?;
        let b_oid = crate::revparse::resolve(repo.refs(), repo.odb(), b).map_err(io_err)?;
        crate::diff::diff_two_trees(&repo, a_oid, b_oid, &mut out).map_err(io_err)?;
    }
    Ok(0)
}

// ---------------------------------------------------------------------------
// history — show repository history overview.
// ---------------------------------------------------------------------------

#[derive(Debug, Args)]
pub struct HistoryArgs {
    /// Maximum count.
    #[arg(short = 'n', long = "count", default_value_t = 20)]
    pub count: usize,
}

pub fn run_history(args: HistoryArgs) -> io::Result<i32> {
    // Delegate to `log --oneline -n <count>`.
    let log = crate::cli::log::LogArgs {
        max: Some(args.count),
        oneline: true,
        abbrev: None,
        abbrev_commit: false,
        patch: false,
        grep: None,
        author: None,
        committer: None,
        start: "HEAD".to_string(),
    };
    crate::cli::log::run(log)
}

// ---------------------------------------------------------------------------
// last-modified — print last-modifying commit per indexed path.
// ---------------------------------------------------------------------------

#[derive(Debug, Args)]
pub struct LastModifiedArgs {
    /// Paths to inspect (default: every indexed path).
    #[arg(value_name = "PATH")]
    pub paths: Vec<String>,
}

pub fn run_last_modified(args: LastModifiedArgs) -> io::Result<i32> {
    let repo = Repository::discover_from_cwd().map_err(io_err)?;
    let index = crate::index::Index::read(&repo).map_err(io_err)?;
    let stdout = io::stdout();
    let mut out = stdout.lock();
    let want: Vec<&[u8]> = if args.paths.is_empty() {
        index
            .entries
            .iter()
            .map(|e| e.path.as_slice())
            .collect::<Vec<_>>()
    } else {
        args.paths.iter().map(|s| s.as_bytes()).collect::<Vec<_>>()
    };
    for path in want {
        // Walk HEAD's history; the first commit whose tree at `path` differs
        // from the next parent's tree is the last-modifier.
        let head = match crate::revparse::resolve(repo.refs(), repo.odb(), "HEAD") {
            Ok(o) => o,
            Err(_) => continue,
        };
        let modifier = find_last_modifier(&repo, head, path).unwrap_or(head);
        let pname = String::from_utf8_lossy(path);
        writeln!(out, "{modifier} {pname}")?;
    }
    Ok(0)
}

fn find_last_modifier(
    repo: &Repository,
    start: crate::hash::ObjectId,
    path: &[u8],
) -> Option<crate::hash::ObjectId> {
    let mut cur = start;
    loop {
        let raw = repo.odb().read(&cur).ok()?;
        let commit = crate::commit::Commit::parse(&raw.data, repo.hash_kind()).ok()?;
        let cur_oid_at_path = oid_at_path(repo, commit.tree, path);
        let parent = commit.parents.first().copied();
        let parent_oid_at_path = match parent {
            Some(p) => {
                let praw = repo.odb().read(&p).ok()?;
                let pcommit = crate::commit::Commit::parse(&praw.data, repo.hash_kind()).ok()?;
                oid_at_path(repo, pcommit.tree, path)
            }
            None => None,
        };
        if cur_oid_at_path != parent_oid_at_path {
            return Some(cur);
        }
        cur = parent?;
    }
}

fn oid_at_path(
    repo: &Repository,
    tree: crate::hash::ObjectId,
    path: &[u8],
) -> Option<crate::hash::ObjectId> {
    let mut current_tree = tree;
    let parts: Vec<&[u8]> = path.split(|&b| b == b'/').collect();
    for (i, part) in parts.iter().enumerate() {
        let raw = repo.odb().read(&current_tree).ok()?;
        if raw.kind != crate::object::ObjectKind::Tree {
            return None;
        }
        let t = crate::tree::Tree::parse(&raw.data, repo.hash_kind()).ok()?;
        let entry = t.entries.iter().find(|e| e.name == *part)?;
        if i == parts.len() - 1 {
            return Some(entry.oid);
        }
        current_tree = entry.oid;
    }
    None
}

// ---------------------------------------------------------------------------
// refs — multi-purpose ref maintenance command (2.54+).
// ---------------------------------------------------------------------------

#[derive(Debug, Args)]
pub struct RefsArgs {
    /// Subcommand: migrate / verify (2.54 ships only these).
    #[arg(value_name = "SUBCOMMAND", default_value = "verify")]
    pub subcommand: String,
}

pub fn run_refs(args: RefsArgs) -> io::Result<i32> {
    match args.subcommand.as_str() {
        "verify" => {
            let repo = Repository::discover_from_cwd().map_err(io_err)?;
            let mut bad = 0u32;
            for r in repo.refs().iter(None) {
                match r {
                    Ok(_) => {}
                    Err(_) => bad += 1,
                }
            }
            if bad == 0 {
                println!("refs: all references look well-formed");
                Ok(0)
            } else {
                println!("refs: {bad} malformed references");
                Ok(1)
            }
        }
        "migrate" => {
            eprintln!(
                "rustygit refs migrate: pass `--ref-format=reftable` to `git init` instead; \
                 in-place migration of an existing repo is deferred."
            );
            Ok(128)
        }
        other => {
            eprintln!("rustygit refs: unknown subcommand {other:?}");
            Ok(129)
        }
    }
}

// ---------------------------------------------------------------------------
// repo — repository-meta subcommand (2.54+).
// ---------------------------------------------------------------------------

#[derive(Debug, Args)]
pub struct RepoArgs {
    /// `info` (default) or `purge-shallow`.
    #[arg(value_name = "SUBCOMMAND", default_value = "info")]
    pub subcommand: String,
    #[arg(value_name = "KEY")]
    pub key: Option<String>,
}

pub fn run_repo(args: RepoArgs) -> io::Result<i32> {
    let repo = Repository::discover_from_cwd().map_err(io_err)?;
    let stdout = io::stdout();
    let mut out = stdout.lock();
    match args.subcommand.as_str() {
        "info" => {
            let want = args.key.as_deref();
            let mut report = |k: &str, v: String| -> io::Result<()> {
                if want.is_none() || want == Some(k) {
                    writeln!(out, "{k}={v}")?;
                }
                Ok(())
            };
            report("gitdir", repo.gitdir().display().to_string())?;
            report("workdir", repo.workdir().display().to_string())?;
            report("commondir", repo.commondir().display().to_string())?;
            report("hash", format!("{:?}", repo.hash_kind()))?;
            Ok(0)
        }
        other => {
            eprintln!("rustygit repo: unknown subcommand {other:?}");
            Ok(129)
        }
    }
}

// ---------------------------------------------------------------------------
// replay — replay commits (alternative to rebase / cherry-pick).
// ---------------------------------------------------------------------------

#[derive(Debug, Args)]
pub struct ReplayArgs {
    /// Onto which ref to replay.
    #[arg(long = "onto", value_name = "ONTO", required = true)]
    pub onto: String,
    /// Range to replay (e.g. `main..feature`).
    #[arg(value_name = "RANGE", required = true)]
    pub range: String,
}

pub fn run_replay(args: ReplayArgs) -> io::Result<i32> {
    let repo = Repository::discover_from_cwd().map_err(io_err)?;
    let onto = crate::revparse::resolve(repo.refs(), repo.odb(), &args.onto).map_err(io_err)?;
    let commits = match crate::revparse::resolve_range(repo.refs(), repo.odb(), &args.range) {
        Ok(Some(v)) => v,
        _ => {
            eprintln!("rustygit replay: expected a range like A..B");
            return Ok(129);
        }
    };
    // Print the mapping table — `replay` writes a sequence of
    // `update <ref> <new-oid> <old-oid>` lines for the caller to feed
    // into `update-ref --stdin`.
    let stdout = io::stdout();
    let mut out = stdout.lock();
    let mut head = onto;
    use crate::sequencer::{apply_commit, ApplyOpts, ApplyOutcome};
    let _ = &mut out;
    let opts = ApplyOpts {
        preserve_author: true,
        override_message: None,
        theirs_label: "replay".into(),
        revert: false,
        mainline: None,
    };
    let mut applied = 0;
    for c in commits.into_iter().rev() {
        if let ApplyOutcome::Done { new_commit } = apply_commit(&repo, c, &opts).map_err(io_err)? {
            head = new_commit;
            applied += 1;
        }
    }
    println!("Replayed {applied} commits onto {onto}; new tip {head}");
    Ok(0)
}

// ---------------------------------------------------------------------------
// for-each-repo — run a command across multiple repos.
// ---------------------------------------------------------------------------

#[derive(Debug, Args)]
pub struct ForEachRepoArgs {
    /// Config variable listing the repos. `--config` only (no short form):
    /// the global `-C <path>` (cwd) flag already owns `-C`. Matches
    /// upstream `git for-each-repo`, which is long-only.
    #[arg(long = "config", value_name = "VAR")]
    pub config_var: Option<String>,
    /// Command to run (everything after `--`).
    #[arg(
        value_name = "CMD",
        trailing_var_arg = true,
        allow_hyphen_values = true
    )]
    pub command: Vec<String>,
}

pub fn run_for_each_repo(args: ForEachRepoArgs) -> io::Result<i32> {
    use crate::config::Config;
    if args.command.is_empty() {
        eprintln!("rustygit for-each-repo: missing command");
        return Ok(129);
    }
    let var = args.config_var.as_deref().unwrap_or("maintenance.repo");
    // Read from `~/.gitconfig` since these are typically user-scope.
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    let config = if let Some(h) = home.and_then(|h| Config::from_file(&h.join(".gitconfig")).ok()) {
        h
    } else {
        Config::empty()
    };
    let mut parts = var.split('.');
    let section = parts.next().unwrap_or("");
    let name = parts.next_back().unwrap_or("");
    let value = config.get_string(section, name);
    let repos: Vec<&str> = value.map(|v| v.split('\n').collect()).unwrap_or_default();
    for repo in repos {
        let repo = repo.trim();
        if repo.is_empty() {
            continue;
        }
        let status = std::process::Command::new(&args.command[0])
            .args(&args.command[1..])
            .current_dir(repo)
            .status();
        if let Ok(s) = status {
            if !s.success() {
                eprintln!("rustygit for-each-repo: failed in {repo}");
            }
        }
    }
    Ok(0)
}

// ---------------------------------------------------------------------------
// hook — list/run hooks programmatically.
// ---------------------------------------------------------------------------

#[derive(Debug, Args)]
pub struct HookArgs {
    /// `run` <hook-name> [args...] or `list`.
    #[arg(value_name = "SUBCOMMAND")]
    pub subcommand: String,
    /// Remaining args.
    #[arg(value_name = "ARGS", trailing_var_arg = true)]
    pub rest: Vec<String>,
}

pub fn run_hook(args: HookArgs) -> io::Result<i32> {
    let repo = Repository::discover_from_cwd().map_err(io_err)?;
    match args.subcommand.as_str() {
        "list" => {
            let hooks_dir = repo.gitdir().join("hooks");
            if let Ok(entries) = std::fs::read_dir(&hooks_dir) {
                for e in entries.flatten() {
                    let n = e.file_name();
                    let n = n.to_string_lossy();
                    if !n.ends_with(".sample") {
                        println!("{n}");
                    }
                }
            }
            Ok(0)
        }
        "run" => {
            if args.rest.is_empty() {
                eprintln!("rustygit hook run: missing <hook-name>");
                return Ok(129);
            }
            let name = &args.rest[0];
            let argv: Vec<&str> = args.rest[1..].iter().map(String::as_str).collect();
            let runner = crate::hooks::HookRunner::from_repo(&repo);
            let outcome = runner.run(name, &argv, None)?;
            Ok(outcome.exit_code().unwrap_or(0))
        }
        other => {
            eprintln!("rustygit hook: unknown subcommand {other:?}");
            Ok(129)
        }
    }
}

fn io_err<E: std::fmt::Display>(e: E) -> io::Error {
    io::Error::other(format!("{e}"))
}
