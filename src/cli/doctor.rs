//! `rustygit doctor` — a repo health-check that surfaces problems the user
//! probably wants to know about before they bite them.
//!
//! Categories of check (each gets one row in the report):
//!
//! 1. **Stale lockfiles** — `*.lock` older than the [`STALE_LOCK_HINT_SECS`]
//!    threshold under `.git/`. Same scope as `prune-locks`.
//! 2. **Orphan checkout shadow dirs** — `checkout.tmp.*` left behind by a
//!    crashed transactional checkout.
//! 3. **Rollback recovery dirs** — `checkout.recover.*` directories holding
//!    originals from a failed rollback. These are recoverable user data,
//!    flagged with high priority.
//! 4. **Index version** — reads `.git/index` and reports the on-disk
//!    version (informational only; useful for the "did we silently
//!    downgrade?" diagnostic).
//! 5. **HEAD resolvability** — `HEAD` resolves to a commit (handles the
//!    unborn-branch case gracefully).
//!
//! What this is NOT: a full `git fsck`. rustygit has a separate `fsck`
//! subcommand for object-graph integrity. `doctor` is for the
//! "operational" health questions a user is likely to ask first.

use std::io;
use std::path::PathBuf;

use clap::Args;

use crate::index::Index;
use crate::lockfile::STALE_LOCK_HINT_SECS;
use crate::refs::{FullName, RefTarget};
use crate::repo::Repository;

#[derive(Debug, Args)]
pub struct DoctorArgs {
    /// Exit non-zero (1) if any issues are reported. Default: always 0.
    #[arg(long = "fail-on-issues")]
    pub fail_on_issues: bool,

    /// Override the staleness threshold (seconds) for `*.lock` and
    /// `checkout.tmp.*` discovery. Default: 1 hour.
    #[arg(long = "older-than", value_name = "SECONDS")]
    pub older_than: Option<u64>,

    /// Run a battery of read-only operations against both rustygit and the
    /// system `git` binary, and report any divergence. Useful as a
    /// pre-migration sanity check or when filing a bug report — "does
    /// rustygit match upstream git on this specific repo?".
    #[arg(long = "vs-git", conflicts_with = "import_config")]
    pub vs_git: bool,

    /// Read the user's layered config (`~/.gitconfig` + XDG + local + ...) and
    /// report which keys rustygit actually honors today. Helpful when
    /// switching from upstream git — surfaces config that will be silently
    /// ignored. Does NOT modify any file.
    #[arg(long = "import-config", conflicts_with = "vs_git")]
    pub import_config: bool,
}

pub fn run(args: DoctorArgs) -> io::Result<i32> {
    // The three operating modes are mutually exclusive (enforced by clap).
    if args.vs_git {
        return run_vs_git();
    }
    if args.import_config {
        return run_import_config();
    }
    run_health_checks(args)
}

fn run_health_checks(args: DoctorArgs) -> io::Result<i32> {
    let repo = Repository::discover_from_cwd().map_err(io_err)?;
    let threshold = args.older_than.unwrap_or(STALE_LOCK_HINT_SECS);

    let mut issues = 0usize;
    println!("rustygit doctor — repo: {}", repo.workdir().display());
    println!("  gitdir:    {}", repo.gitdir().display());
    println!("  commondir: {}", repo.commondir().display());
    println!();

    issues += check_stale_locks(&repo, threshold);
    issues += check_orphan_shadow_dirs(&repo, threshold);
    issues += check_recovery_dirs(&repo);
    check_index_version(&repo);
    check_head_resolvable(&repo);

    println!();
    if issues == 0 {
        println!("OK: no issues found");
        Ok(0)
    } else {
        println!("{issues} issue(s) found");
        if args.fail_on_issues {
            Ok(1)
        } else {
            Ok(0)
        }
    }
}

// ---------------------------------------------------------------------------
// --vs-git mode (B6, part 1)
// ---------------------------------------------------------------------------

/// One row of the cross-binary comparison table.
struct CrossOp {
    /// User-visible name in the report.
    label: &'static str,
    /// argv tail passed to BOTH `rustygit` and `git`. Both binaries see
    /// the same args; we don't try to be clever about flag differences
    /// here — pick ops where the surface is identical.
    args: &'static [&'static str],
}

/// Read-only ops chosen for byte-equality on a typical clean repo. We
/// avoid `log` without limits (huge output, plus our medium-format
/// renderer has known tail-line differences) and `diff` of HEAD against
/// HEAD (which can vary if there's a dirty index). Each op is fast and
/// deterministic.
const VS_GIT_OPS: &[CrossOp] = &[
    CrossOp {
        label: "rev-parse HEAD",
        args: &["rev-parse", "HEAD"],
    },
    CrossOp {
        label: "cat-file -t HEAD",
        args: &["cat-file", "-t", "HEAD"],
    },
    CrossOp {
        label: "log --oneline -n 5",
        args: &["log", "--oneline", "-n", "5"],
    },
    CrossOp {
        label: "ls-tree HEAD",
        args: &["ls-tree", "HEAD"],
    },
    CrossOp {
        label: "show-ref",
        args: &["show-ref"],
    },
    CrossOp {
        label: "for-each-ref",
        args: &["for-each-ref"],
    },
    CrossOp {
        label: "status --porcelain",
        args: &["status", "--porcelain"],
    },
    CrossOp {
        label: "rev-list --count HEAD",
        args: &["rev-list", "--count", "HEAD"],
    },
];

fn run_vs_git() -> io::Result<i32> {
    // Find a rustygit binary to spawn. Re-exec our own argv[0] — that's the
    // process invoking us, which IS rustygit. (We can't just say "rustygit"
    // because $PATH may or may not have it during local development.)
    let rg_path: PathBuf = std::env::current_exe()?;
    let cwd = std::env::current_dir()?;

    println!(
        "rustygit doctor --vs-git — comparing against system 'git' on {}",
        cwd.display()
    );
    let git_version = std::process::Command::new("git").arg("--version").output();
    match &git_version {
        Ok(out) if out.status.success() => {
            print!("  system git: {}", String::from_utf8_lossy(&out.stdout));
        }
        _ => {
            println!("  system git: NOT FOUND on PATH — cannot run comparison");
            return Ok(2);
        }
    }
    println!("  rustygit:   {}", rg_path.display());
    println!();

    let mut divergences = 0usize;
    let mut ok_count = 0usize;
    for op in VS_GIT_OPS {
        match compare_op(&rg_path, op, &cwd) {
            CompareResult::Match => {
                println!("  [ok]    {}", op.label);
                ok_count += 1;
            }
            CompareResult::Differ { reason, snippet } => {
                println!("  [DIFF]  {}: {reason}", op.label);
                for line in snippet.lines().take(8) {
                    println!("           {line}");
                }
                divergences += 1;
            }
            CompareResult::SkippedNoRepo => {
                println!("  [skip]  {} (not in a repo)", op.label);
            }
            CompareResult::SkippedSpawnError(e) => {
                println!("  [skip]  {} (spawn failed: {e})", op.label);
            }
        }
    }
    println!();
    println!(
        "{ok_count}/{} matches, {divergences} divergence(s)",
        VS_GIT_OPS.len()
    );
    Ok(if divergences == 0 { 0 } else { 1 })
}

enum CompareResult {
    Match,
    Differ { reason: String, snippet: String },
    SkippedNoRepo,
    SkippedSpawnError(String),
}

fn compare_op(rg_path: &std::path::Path, op: &CrossOp, cwd: &std::path::Path) -> CompareResult {
    let rg = std::process::Command::new(rg_path)
        .args(op.args)
        .current_dir(cwd)
        .env_remove("GIT_PAGER") // no pagers under capture
        .env("PAGER", "cat")
        .output();
    let g = std::process::Command::new("git")
        .args(op.args)
        .current_dir(cwd)
        .env_remove("GIT_PAGER")
        .env("PAGER", "cat")
        .output();

    let (rg, g) = match (rg, g) {
        (Ok(a), Ok(b)) => (a, b),
        (Err(e), _) | (_, Err(e)) => return CompareResult::SkippedSpawnError(e.to_string()),
    };

    // Heuristic: if BOTH binaries fail with similar stderr, we're probably
    // outside a repo. Skip rather than report a divergence.
    if !rg.status.success() && !g.status.success() {
        let rs = String::from_utf8_lossy(&rg.stderr).to_lowercase();
        let gs = String::from_utf8_lossy(&g.stderr).to_lowercase();
        if (rs.contains("not a") && rs.contains("repo"))
            || (gs.contains("not a") && gs.contains("repo"))
        {
            return CompareResult::SkippedNoRepo;
        }
    }

    if rg.status.code() != g.status.code() {
        return CompareResult::Differ {
            reason: format!(
                "exit codes differ — rustygit={:?} git={:?}",
                rg.status.code(),
                g.status.code()
            ),
            snippet: format!(
                "rg stderr: {}\ngit stderr: {}",
                String::from_utf8_lossy(&rg.stderr).trim(),
                String::from_utf8_lossy(&g.stderr).trim()
            ),
        };
    }
    if rg.stdout != g.stdout {
        return CompareResult::Differ {
            reason: "stdout bytes differ".into(),
            snippet: unified_snippet(&rg.stdout, &g.stdout),
        };
    }
    CompareResult::Match
}

/// Produce a tiny side-by-side excerpt of the first ~6 differing lines,
/// just enough for the user to recognize what changed without flooding
/// stdout. We don't try to produce a real unified diff here — `rustygit
/// diff --no-index` exists if the user wants more.
fn unified_snippet(rg: &[u8], g: &[u8]) -> String {
    let rg_s = String::from_utf8_lossy(rg);
    let g_s = String::from_utf8_lossy(g);
    let mut out = String::new();
    let mut showed = 0;
    for (a, b) in rg_s.lines().zip(g_s.lines()) {
        if showed >= 6 {
            break;
        }
        if a != b {
            out.push_str(&format!("-{a}\n+{b}\n"));
            showed += 1;
        }
    }
    if out.is_empty() {
        out.push_str("(line counts differ but contents match where they line up)");
    }
    out
}

// ---------------------------------------------------------------------------
// --import-config mode (B6, part 2)
// ---------------------------------------------------------------------------

/// The catalog of config keys rustygit honors today. Each entry is
/// `("section.[subsection.]name", "what we do with it")`. Built-up by
/// hand because the alternative — grepping the source for `get_string`
/// callers — is too noisy.
///
/// Subsection-wildcards are spelled `<sub>` (e.g. `alias.<name>`,
/// `url.<base>.insteadof`). The check ignores the wildcard segment.
const HONORED_KEYS: &[(&str, &str)] = &[
    ("user.name", "commit author/committer"),
    ("user.email", "commit author/committer"),
    ("user.signingkey", "GPG signing key id"),
    (
        "core.repositoryformatversion",
        "repo format version (must be 0/1)",
    ),
    ("core.filemode", "honor +x bit in index"),
    ("core.bare", "bare-repo detection"),
    ("core.symlinks", "follow vs refuse symlinks (A10)"),
    ("core.ignorecase", "case-insensitive FS detection"),
    ("core.precomposeunicode", "macOS NFD→NFC (config-only)"),
    ("core.autocrlf", "CRLF↔LF conversion (A10)"),
    ("core.pager", "pager program (A4)"),
    ("core.notesref", "default notes ref"),
    ("core.hookspath", "alternative hooks dir"),
    ("core.compression", "zlib compression level"),
    ("commit.gpgsign", "sign commits by default (Batch F)"),
    ("commit.template", "default commit message template"),
    ("tag.gpgsign", "sign tags by default"),
    ("gpg.program", "gpg binary path"),
    ("alias.<name>", "subcommand aliases (A1)"),
    ("url.<base>.insteadof", "URL rewrite for fetch/push (A3)"),
    ("url.<base>.pushinsteadof", "URL rewrite for push only (A3)"),
    ("extensions.objectformat", "sha1 vs sha256"),
    ("extensions.refstorage", "files vs reftable"),
    ("remote.<name>.url", "remote location"),
    ("remote.<name>.fetch", "default refspecs"),
    ("branch.<name>.remote", "upstream remote"),
    ("branch.<name>.merge", "upstream branch"),
    ("color.ui", "color output mode (auto/always/never)"),
    ("gc.auto", "loose-object threshold for auto-gc"),
    ("gc.rerereresolved", "rerere entry expiry (resolved)"),
    ("gc.rerereunresolved", "rerere entry expiry (unresolved)"),
    ("rustygit.beta.acknowledged", "silence the beta banner"),
    ("rustygit.history.enabled", "opt-in subcommand history.log"),
];

/// Sections/patterns rustygit does NOT yet honor, with a short reason. We
/// match against the section name (or `section.<subsection>.key` pattern)
/// to flag keys at scan time.
const KNOWN_UNSUPPORTED: &[(&str, &str)] = &[
    (
        "filter.<name>.",
        "smudge/clean/textconv filters not implemented",
    ),
    ("submodule.", "submodule porcelain deferred"),
    ("lfs.", "LFS not implemented"),
    ("mergetool.", "mergetool helpers deferred"),
    ("difftool.", "difftool helpers deferred"),
    ("color.branch.", "only color.ui (top-level) is honored"),
    ("color.diff.", "only color.ui (top-level) is honored"),
    ("color.status.", "only color.ui (top-level) is honored"),
    ("color.grep.", "only color.ui (top-level) is honored"),
    ("color.interactive.", "only color.ui (top-level) is honored"),
    ("color.decorate.", "only color.ui (top-level) is honored"),
    ("fetch.<...>.", "advanced fetch tuning deferred"),
    ("pack.", "pack-builder tuning options deferred"),
    ("receive.", "server-side; out of rustygit's scope"),
    ("uploadpack.", "server-side; out of rustygit's scope"),
    (
        "interactive.",
        "interactive rebase / add -p tuning deferred",
    ),
    ("rebase.", "interactive-rebase tuning deferred"),
    ("rerere.", "rerere is a stub today"),
];

fn run_import_config() -> io::Result<i32> {
    // The layered loader pulls in system + XDG + global + (local if we're
    // in a repo). Outside a repo, the local read no-ops; the global layer
    // is what users typically populate.
    let gitdir = Repository::discover_from_cwd()
        .map(|r| r.gitdir().to_path_buf())
        .unwrap_or_else(|_| PathBuf::from(".git"));
    let cfg = match crate::config::Config::load_layered(&gitdir) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("rustygit: doctor --import-config: {e}");
            return Ok(128);
        }
    };

    println!("rustygit doctor --import-config — checking your layered config");
    println!();

    let entries = cfg.all_entries();
    let total = entries.len();
    let mut compatible = 0usize;
    let mut deferred: Vec<(String, &'static str)> = Vec::new();
    let mut unknown: Vec<String> = Vec::new();

    for (section, subsection, key) in entries.iter().cloned() {
        let full_key = pretty_key(&section, subsection.as_deref(), &key);
        if is_honored(&section, subsection.as_deref(), &key) {
            compatible += 1;
        } else if let Some(reason) = is_known_unsupported(&section, &key) {
            deferred.push((full_key, reason));
        } else {
            unknown.push(full_key);
        }
    }

    println!("  honored:    {compatible}/{total} key(s) rustygit recognizes from your config");
    if !deferred.is_empty() {
        println!(
            "  deferred:   {} key(s) we know about but don't honor yet:",
            deferred.len()
        );
        for (k, why) in &deferred {
            println!("              - {k} ({why})");
        }
    }
    if !unknown.is_empty() {
        println!("  unknown:    {} key(s) not in rustygit's catalog (may be honored by upstream git, ignored here):", unknown.len());
        for k in &unknown {
            println!("              - {k}");
        }
    }
    println!();
    println!("Note: this is a static catalog — `unknown` does NOT mean the key");
    println!("is broken, only that rustygit doesn't have a documented behavior");
    println!("for it yet. File an issue if you depend on one of the listed");
    println!("deferred keys for your day-to-day workflow.");
    Ok(0)
}

fn pretty_key(section: &str, sub: Option<&str>, key: &str) -> String {
    match sub {
        Some(s) => format!("{section}.{s}.{key}"),
        None => format!("{section}.{key}"),
    }
}

fn is_honored(section: &str, _sub: Option<&str>, key: &str) -> bool {
    // The HONORED_KEYS entries are lowercased; the parser already
    // lowercases sections+keys, so just dot-join + lookup.
    let direct = format!("{section}.{key}");
    if HONORED_KEYS.iter().any(|(k, _)| *k == direct) {
        return true;
    }
    // Subsection-wildcard form: `section.<name>.<key>` → match
    // `section.<wildcard>.<key>` in the catalog.
    let wildcard = format!("{section}.<name>.{key}");
    if HONORED_KEYS.iter().any(|(k, _)| {
        *k == wildcard
            || (k.starts_with(&format!("{section}.<")) && k.ends_with(&format!(".{key}")))
    }) {
        return true;
    }
    // Pure alias.<name> shape: any key under `[alias]` is honored.
    if section == "alias" {
        return true;
    }
    false
}

fn is_known_unsupported(section: &str, key: &str) -> Option<&'static str> {
    let prefix = format!("{section}.");
    let full = format!("{section}.{key}");
    for (pat, reason) in KNOWN_UNSUPPORTED {
        // `pat` is either a `section.` prefix or `section.<...>.key`
        // pattern; we lowercase both sides so case differences don't
        // confuse the match.
        if pat.ends_with('.') && (prefix.starts_with(pat) || full.starts_with(pat)) {
            return Some(reason);
        }
        if full.eq_ignore_ascii_case(pat) {
            return Some(reason);
        }
    }
    None
}

fn check_stale_locks(repo: &Repository, threshold: u64) -> usize {
    let mut count = 0usize;
    let mut walk = vec![repo.gitdir().to_path_buf()];
    while let Some(d) = walk.pop() {
        let rd = match std::fs::read_dir(&d) {
            Ok(r) => r,
            Err(_) => continue,
        };
        for ent in rd.flatten() {
            let path = ent.path();
            let ft = match ent.file_type() {
                Ok(t) => t,
                Err(_) => continue,
            };
            if ft.is_dir() {
                // Skip objects/ — that's huge and never holds .lock files
                // worth flagging. Same for hooks/ and info/.
                let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
                if matches!(name, "objects" | "hooks" | "info") {
                    continue;
                }
                walk.push(path);
            } else if path
                .extension()
                .and_then(|s| s.to_str())
                .map(|s| s == "lock")
                .unwrap_or(false)
            {
                let age = age_seconds(&path).unwrap_or(0);
                if age >= threshold {
                    println!("  [stale lock]  {} ({age}s old)", path.display());
                    count += 1;
                }
            }
        }
    }
    if count == 0 {
        println!("  [ok] no stale lockfiles found");
    } else {
        println!("  → fix: rustygit prune-locks");
    }
    count
}

fn check_orphan_shadow_dirs(repo: &Repository, threshold: u64) -> usize {
    let mut count = 0usize;
    let rd = match std::fs::read_dir(repo.gitdir()) {
        Ok(r) => r,
        Err(_) => return 0,
    };
    for ent in rd.flatten() {
        let name = ent.file_name();
        let name_s = name.to_string_lossy();
        if !name_s.starts_with("checkout.tmp.") {
            continue;
        }
        let path = ent.path();
        let age = age_seconds(&path).unwrap_or(0);
        if age >= threshold {
            println!("  [orphan checkout shadow] {} ({age}s old)", path.display());
            count += 1;
        }
    }
    if count == 0 {
        println!("  [ok] no orphan checkout shadow dirs");
    } else {
        println!("  → fix: rustygit prune-locks");
    }
    count
}

fn check_recovery_dirs(repo: &Repository) -> usize {
    let mut count = 0usize;
    let mut paths: Vec<PathBuf> = Vec::new();
    let rd = match std::fs::read_dir(repo.gitdir()) {
        Ok(r) => r,
        Err(_) => return 0,
    };
    for ent in rd.flatten() {
        let name = ent.file_name();
        let name_s = name.to_string_lossy();
        if name_s.starts_with("checkout.recover.") {
            paths.push(ent.path());
        }
    }
    for p in &paths {
        println!(
            "  [⚠ recovery dir] {} — preserves originals from a failed rollback; inspect before deleting",
            p.display()
        );
        count += 1;
    }
    if count == 0 {
        println!("  [ok] no rollback-recovery directories");
    }
    count
}

fn check_index_version(repo: &Repository) {
    match Index::read(repo) {
        Ok(idx) => println!(
            "  [ok] index version v{}, {} entries",
            idx.version,
            idx.entries.len()
        ),
        Err(e) => println!("  [⚠ index] cannot read: {e}"),
    }
}

fn check_head_resolvable(repo: &Repository) {
    let head_name = match FullName::new("HEAD") {
        Ok(n) => n,
        Err(e) => {
            println!("  [⚠ HEAD] invalid name: {e}");
            return;
        }
    };
    match RefTarget::resolve(repo.refs(), &head_name) {
        Ok(Some((target, oid))) => {
            println!("  [ok] HEAD → {} → {}", target, oid.short_hex(7));
        }
        Ok(None) => {
            println!("  [info] HEAD is unborn (no commits yet)");
        }
        Err(e) => {
            println!("  [⚠ HEAD] cannot resolve: {e}");
        }
    }
}

fn age_seconds(path: &std::path::Path) -> Option<u64> {
    let meta = std::fs::metadata(path).ok()?;
    let mtime = meta.modified().ok()?;
    std::time::SystemTime::now()
        .duration_since(mtime)
        .ok()
        .map(|d| d.as_secs())
}

fn io_err<E: std::fmt::Display>(e: E) -> io::Error {
    io::Error::other(format!("{e}"))
}
