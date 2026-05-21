//! Compatibility corpus harness (NON_GOALS C2).
//!
//! Clones a curated set of public repos with system `git`, then runs a
//! fixed sequence of deterministic read-only operations through both
//! `rustygit` and `git`, byte-comparing stdout. Non-zero exit code on any
//! divergence so CI can gate.
//!
//! ## Why a standalone binary
//!
//! - We need the comparison to happen *outside* the test harness because
//!   running 4 large public repos × 10 ops takes minutes-to-hours and
//!   `cargo test` is the wrong tool for long-running matrices.
//! - Keeping the harness out of the main workspace stops `cargo deny`
//!   from auditing `toml`/`serde`, which would be irrelevant noise: the
//!   shipped binary doesn't link them.
//!
//! ## Invocation
//!
//! ```text
//! cargo run --manifest-path xtask-compat-corpus/Cargo.toml -- \
//!   [corpus.toml] [target/corpus] [target/release/rustygit]
//! ```
//!
//! All three positional args have sensible defaults so local quick-runs
//! and CI both work.

use std::path::Path;
use std::process::{Command, ExitCode};

use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct Corpus {
    #[serde(default)]
    repo: Vec<RepoSpec>,
    #[serde(default)]
    op: Vec<OpSpec>,
}

#[derive(Debug, Deserialize, Clone)]
struct RepoSpec {
    name: String,
    url: String,
    #[serde(default)]
    shallow: bool,
    #[serde(default)]
    depth: Option<u32>,
}

#[derive(Debug, Deserialize, Clone)]
struct OpSpec {
    label: String,
    argv: Vec<String>,
}

#[derive(Debug)]
struct OpResult {
    repo: String,
    // `op` label is consumed by `print_summary` and the per-line print
    // in `main()`; we keep it on the struct so future aggregations
    // (e.g. group-by-op rather than group-by-repo) don't need to thread
    // it through a second channel.
    #[allow(dead_code)]
    op: String,
    pass: bool,
    rusty_exit: i32,
    git_exit: i32,
    diverged_bytes: usize,
}

fn main() -> ExitCode {
    let corpus_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "corpus.toml".to_string());
    let workdir = std::env::args()
        .nth(2)
        .unwrap_or_else(|| "target/corpus".to_string());
    let rustygit_bin = std::env::args()
        .nth(3)
        .unwrap_or_else(|| "target/release/rustygit".to_string());

    let rustygit_bin = match std::fs::canonicalize(&rustygit_bin) {
        Ok(p) => p,
        Err(e) => {
            eprintln!(
                "fatal: cannot find rustygit binary at {rustygit_bin}: {e}\n\
                 hint: run `cargo build --release` first."
            );
            return ExitCode::from(2);
        }
    };

    let corpus_text = match std::fs::read_to_string(&corpus_path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("fatal: cannot read corpus at {corpus_path}: {e}");
            return ExitCode::from(2);
        }
    };
    let corpus: Corpus = match toml::from_str(&corpus_text) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("fatal: malformed corpus.toml: {e}");
            return ExitCode::from(2);
        }
    };

    if let Err(e) = std::fs::create_dir_all(&workdir) {
        eprintln!("fatal: cannot create workdir {workdir}: {e}");
        return ExitCode::from(2);
    }
    let workdir = match std::fs::canonicalize(&workdir) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("fatal: cannot canonicalize workdir {workdir}: {e}");
            return ExitCode::from(2);
        }
    };

    println!("== rustygit compatibility corpus ==");
    println!("rustygit: {}", rustygit_bin.display());
    println!("workdir:  {}", workdir.display());
    println!(
        "repos:    {}, ops: {}",
        corpus.repo.len(),
        corpus.op.len()
    );
    println!();

    let mut results: Vec<OpResult> = Vec::new();
    for repo in &corpus.repo {
        let repo_dir = workdir.join(&repo.name);
        if let Err(e) = ensure_cloned(repo, &repo_dir) {
            eprintln!("[{}] clone failed: {e}", repo.name);
            results.push(OpResult {
                repo: repo.name.clone(),
                op: "(clone)".to_string(),
                pass: false,
                rusty_exit: -1,
                git_exit: -1,
                diverged_bytes: 0,
            });
            continue;
        }
        for op in &corpus.op {
            let r = run_op(&rustygit_bin, &repo_dir, op);
            let pass_marker = if r.pass { "PASS" } else { "FAIL" };
            println!(
                "[{}] {:24} {} (rusty={} git={} diff={}b)",
                repo.name, op.label, pass_marker, r.rusty_exit, r.git_exit, r.diverged_bytes
            );
            results.push(r);
        }
    }

    println!();
    print_summary(&results);

    let failures = results.iter().filter(|r| !r.pass).count();
    if failures == 0 {
        println!("\nAll {} comparisons passed.", results.len());
        ExitCode::SUCCESS
    } else {
        println!(
            "\n{} of {} comparisons failed. See *.diff files in {}.",
            failures,
            results.len(),
            workdir.display()
        );
        ExitCode::FAILURE
    }
}

/// Clone (shallow, if requested) the repo into `repo_dir`. Skip if the
/// dir already looks like a git repo — this lets `actions/cache`
/// short-circuit the slow network step.
fn ensure_cloned(spec: &RepoSpec, repo_dir: &Path) -> Result<(), String> {
    if repo_dir.join(".git").exists() {
        // Already cloned. We could `git fetch` to refresh, but corpus
        // determinism is more important than freshness — re-running on
        // the same cached commit gives byte-identical results both
        // sides.
        return Ok(());
    }
    if let Some(parent) = repo_dir.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("mkdir parent: {e}"))?;
    }

    let mut args: Vec<String> = vec!["clone".to_string()];
    if spec.shallow {
        if let Some(depth) = spec.depth {
            args.push(format!("--depth={depth}"));
        } else {
            args.push("--depth=1".to_string());
        }
    }
    args.push(spec.url.clone());
    args.push(repo_dir.to_string_lossy().into_owned());

    println!("[{}] cloning {} ...", spec.name, spec.url);
    let out = Command::new("git")
        .args(&args)
        .output()
        .map_err(|e| format!("spawn git: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "git clone exited {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    Ok(())
}

/// Run the op against both binaries, byte-compare stdout, dump a diff
/// snippet to `<repo_dir>/<label>.diff` on divergence.
fn run_op(rustygit_bin: &Path, repo_dir: &Path, op: &OpSpec) -> OpResult {
    let argv: Vec<&str> = op.argv.iter().map(|s| s.as_str()).collect();

    let rusty = Command::new(rustygit_bin).args(&argv).current_dir(repo_dir).output();
    let git = Command::new("git").args(&argv).current_dir(repo_dir).output();

    let (rusty_exit, rusty_out) = match rusty {
        Ok(o) => (o.status.code().unwrap_or(-1), o.stdout),
        Err(e) => {
            eprintln!("  rustygit spawn failed: {e}");
            (
                -1,
                format!("(rustygit spawn failed: {e})").into_bytes(),
            )
        }
    };
    let (git_exit, git_out) = match git {
        Ok(o) => (o.status.code().unwrap_or(-1), o.stdout),
        Err(e) => {
            eprintln!("  git spawn failed: {e}");
            (-1, format!("(git spawn failed: {e})").into_bytes())
        }
    };

    let pass = rusty_exit == git_exit && rusty_out == git_out;
    let diverged_bytes = if rusty_out == git_out {
        0
    } else {
        // Cheap measure: count bytes that differ on the longer side.
        let max = rusty_out.len().max(git_out.len());
        let min = rusty_out.len().min(git_out.len());
        let body_diff = rusty_out
            .iter()
            .zip(git_out.iter())
            .filter(|(a, b)| a != b)
            .count();
        body_diff + (max - min)
    };

    if !pass {
        let diff_path = repo_dir.join(format!("{}.diff", op.label));
        if let Err(e) = write_diff(&diff_path, &op.label, &rusty_out, &git_out) {
            eprintln!("  warning: cannot write diff snippet to {}: {e}", diff_path.display());
        }
    }

    OpResult {
        repo: repo_dir
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default(),
        op: op.label.clone(),
        pass,
        rusty_exit,
        git_exit,
        diverged_bytes,
    }
}

/// Write a small unified-diff-flavored snippet: header + first 6
/// differing lines. The goal is to make CI artifacts useful at a glance
/// without dumping multi-megabyte outputs.
fn write_diff(
    diff_path: &Path,
    label: &str,
    rusty: &[u8],
    git: &[u8],
) -> std::io::Result<()> {
    let mut buf: Vec<u8> = Vec::new();
    buf.extend_from_slice(b"# rustygit compatibility corpus diff snippet\n");
    buf.extend_from_slice(format!("# label: {label}\n").as_bytes());
    buf.extend_from_slice(
        format!(
            "# rustygit bytes={} | git bytes={}\n",
            rusty.len(),
            git.len()
        )
        .as_bytes(),
    );
    buf.extend_from_slice(b"# first 6 differing lines (rustygit then git):\n");

    let rusty_lines: Vec<&[u8]> = split_lines(rusty);
    let git_lines: Vec<&[u8]> = split_lines(git);

    let max = rusty_lines.len().max(git_lines.len());
    let mut shown = 0;
    for i in 0..max {
        if shown >= 6 {
            break;
        }
        let r = rusty_lines.get(i).copied().unwrap_or(b"");
        let g = git_lines.get(i).copied().unwrap_or(b"");
        if r != g {
            buf.extend_from_slice(b"- ");
            buf.extend_from_slice(r);
            buf.push(b'\n');
            buf.extend_from_slice(b"+ ");
            buf.extend_from_slice(g);
            buf.push(b'\n');
            shown += 1;
        }
    }
    if shown == 0 {
        buf.extend_from_slice(b"# (lines all matched but byte-streams differ: trailing whitespace or EOL?)\n");
    }

    std::fs::write(diff_path, buf)
}

fn split_lines(bytes: &[u8]) -> Vec<&[u8]> {
    bytes.split(|b| *b == b'\n').collect()
}

fn print_summary(results: &[OpResult]) {
    println!("=== summary ===");
    let mut by_repo: std::collections::BTreeMap<&str, (usize, usize)> =
        std::collections::BTreeMap::new();
    for r in results {
        let e = by_repo.entry(r.repo.as_str()).or_insert((0, 0));
        e.0 += 1;
        if r.pass {
            e.1 += 1;
        }
    }
    for (repo, (total, passed)) in by_repo {
        println!("  {repo:<14} {passed}/{total} ops passed");
    }
}

/// Allow consumers (and future maintainers) to feed in their own
/// corpus from a `&str` — used by the tiny smoke-test below to make
/// sure the parser doesn't rot when corpus.toml is edited.
#[allow(dead_code)]
fn parse_corpus(text: &str) -> Result<Corpus, toml::de::Error> {
    toml::from_str(text)
}

// --------------------------------------------------------------------
// Smoke tests. These only exercise the *parser* — the full harness
// is integration-tested by the nightly workflow (which is where the
// expensive clones happen).
// --------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_minimal() {
        let corpus = parse_corpus(
            r#"
            [[repo]]
            name = "x"
            url = "https://example.invalid/x.git"

            [[op]]
            label = "rp"
            argv = ["rev-parse", "HEAD"]
            "#,
        )
        .unwrap();
        assert_eq!(corpus.repo.len(), 1);
        assert_eq!(corpus.repo[0].name, "x");
        assert!(!corpus.repo[0].shallow);
        assert_eq!(corpus.repo[0].depth, None);
        assert_eq!(corpus.op.len(), 1);
        assert_eq!(corpus.op[0].argv, vec!["rev-parse", "HEAD"]);
    }

    #[test]
    fn parse_shallow_with_depth() {
        let corpus = parse_corpus(
            r#"
            [[repo]]
            name = "x"
            url = "https://example.invalid/x.git"
            shallow = true
            depth = 1234
            "#,
        )
        .unwrap();
        assert!(corpus.repo[0].shallow);
        assert_eq!(corpus.repo[0].depth, Some(1234));
    }

    #[test]
    fn parse_real_corpus() {
        // Sanity-check the file we ship.
        let text = include_str!("../corpus.toml");
        let corpus = parse_corpus(text).expect("corpus.toml parses");
        assert!(!corpus.repo.is_empty(), "corpus has at least one repo");
        assert!(!corpus.op.is_empty(), "corpus has at least one op");
        for op in &corpus.op {
            assert!(!op.argv.is_empty(), "op {} has empty argv", op.label);
        }
    }
}
