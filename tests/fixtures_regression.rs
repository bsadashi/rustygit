//! Snapshot fixture regression suite (NON_GOALS C3).
//!
//! Hand-built tiny fixture repos under `tests/fixtures/canonical/`
//! exercise read-only commands (`log`, `show`, `diff`, `status`,
//! `cat-file`, `ls-tree`, `rev-parse`, `show-ref`, `for-each-ref`,
//! `branch`) against golden output files. These guard against
//! rustygit-version regressions independently of upstream git's
//! version (the C1 multi-version matrix does that part).
//!
//! ## How it works
//!
//! For each fixture (`01-linear`, `02-branched`, ...):
//!
//! 1. Run the fixture's `build.sh` in a tempdir to materialize the
//!    `.git` directory. The script uses pinned author/committer
//!    identities and dates so object ids are byte-identical run-to-run.
//! 2. For each `(label, argv)` pair in `OPS_*`, run rustygit and
//!    compare stdout bytes against `golden/<label>.txt`.
//!
//! ## Regenerating goldens
//!
//! After a deliberate behavior change:
//!
//! ```text
//! GOLDEN_REGEN=1 cargo test --test fixtures_regression
//! ```
//!
//! That writes the observed output back to disk. Always review the
//! diff before committing.
//!
//! ## Constraints
//!
//! - Skipped silently if system `git` is missing (we don't ship it).
//! - Skipped if `bash` is missing (we don't ship that either; rare
//!   on dev boxes but worth a guard for minimal containers).

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;

use assert_cmd::Command as AssertCmd;
use common::has_system_git;

/// One operation we cross-check: a label that becomes the golden
/// filename, plus the argv to pass to rustygit.
struct Op {
    label: &'static str,
    argv: &'static [&'static str],
}

// Per-fixture op sets. Different fixtures have different interesting
// surface area (a linear repo has no branches; a tagged repo has tag
// refs); keeping the sets per-fixture avoids golden files of the
// "(none)" variety.

const OPS_LINEAR: &[Op] = &[
    Op {
        label: "log-oneline",
        argv: &["log", "--oneline"],
    },
    Op {
        label: "rev-parse-head",
        argv: &["rev-parse", "HEAD"],
    },
    Op {
        label: "ls-tree-head",
        argv: &["ls-tree", "HEAD"],
    },
    Op {
        label: "ls-tree-r-head",
        argv: &["ls-tree", "-r", "HEAD"],
    },
    Op {
        label: "cat-file-t-head",
        argv: &["cat-file", "-t", "HEAD"],
    },
    Op {
        label: "status-porcelain",
        argv: &["status", "--porcelain"],
    },
];

const OPS_BRANCHED: &[Op] = &[
    Op {
        label: "log-oneline",
        argv: &["log", "--oneline"],
    },
    Op {
        label: "rev-parse-head",
        argv: &["rev-parse", "HEAD"],
    },
    Op {
        label: "rev-parse-feature",
        argv: &["rev-parse", "feature"],
    },
    Op {
        label: "ls-tree-head",
        argv: &["ls-tree", "HEAD"],
    },
    Op {
        label: "show-ref",
        argv: &["show-ref"],
    },
    Op {
        label: "branch-list",
        argv: &["branch", "--list"],
    },
    Op {
        label: "status-porcelain",
        argv: &["status", "--porcelain"],
    },
];

const OPS_MERGED: &[Op] = &[
    Op {
        label: "log-oneline",
        argv: &["log", "--oneline"],
    },
    Op {
        label: "rev-parse-head",
        argv: &["rev-parse", "HEAD"],
    },
    Op {
        label: "rev-parse-head-tree",
        argv: &["rev-parse", "HEAD^{tree}"],
    },
    Op {
        label: "ls-tree-r-head",
        argv: &["ls-tree", "-r", "HEAD"],
    },
    Op {
        label: "show-ref",
        argv: &["show-ref"],
    },
    Op {
        label: "branch-list",
        argv: &["branch", "--list"],
    },
];

const OPS_TAGGED: &[Op] = &[
    Op {
        label: "log-oneline",
        argv: &["log", "--oneline"],
    },
    Op {
        label: "rev-parse-head",
        argv: &["rev-parse", "HEAD"],
    },
    Op {
        label: "show-ref",
        argv: &["show-ref"],
    },
    Op {
        label: "for-each-ref",
        argv: &["for-each-ref"],
    },
];

const OPS_DELETED: &[Op] = &[
    Op {
        label: "log-oneline",
        argv: &["log", "--oneline"],
    },
    Op {
        label: "ls-tree-head",
        argv: &["ls-tree", "HEAD"],
    },
    Op {
        label: "ls-tree-r-head",
        argv: &["ls-tree", "-r", "HEAD"],
    },
    Op {
        label: "status-porcelain",
        argv: &["status", "--porcelain"],
    },
    Op {
        label: "cat-file-t-head",
        argv: &["cat-file", "-t", "HEAD"],
    },
];

/// Returns true if `bash` is on PATH. The build.sh scripts use bash
/// features (`set -euo pipefail`, `[[ ... ]]`, `printf`) that aren't
/// portable to `sh`.
fn has_bash() -> bool {
    Command::new("bash")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Path to the fixtures root (relative to the manifest dir, which is
/// the rustygit repo root for integration tests).
fn fixtures_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("canonical")
}

/// Build the fixture into `dest` using its `build.sh`. Asserts
/// non-zero exit code on failure.
fn build_fixture(name: &str, dest: &Path) {
    let script = fixtures_root().join(name).join("build.sh");
    assert!(
        script.exists(),
        "fixture build script missing: {}",
        script.display()
    );
    let out = Command::new("bash")
        .arg(&script)
        .arg(dest)
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn bash for {name}: {e}"));
    assert!(
        out.status.success(),
        "fixture {name} build.sh failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Run `rustygit <argv...>` from `repo_dir` and return stdout bytes.
fn run_rustygit(repo_dir: &Path, argv: &[&str]) -> Vec<u8> {
    let mut cmd = AssertCmd::cargo_bin("rustygit").unwrap();
    let out = cmd
        .args(argv)
        .current_dir(repo_dir)
        .output()
        .unwrap_or_else(|e| panic!("spawn rustygit {argv:?}: {e}"));
    // We do NOT assert exit-code success here. Some ops legitimately
    // produce empty output with exit 0; others (e.g. cat-file -t)
    // could return non-zero on legitimate divergence. The golden
    // file IS the expected behavior — exit codes get checked
    // implicitly via output bytes.
    out.stdout
}

/// Compare actual bytes to the golden file. With `GOLDEN_REGEN=1`,
/// overwrite the golden and assert nothing.
fn compare_or_regen(fixture: &str, label: &str, actual: &[u8]) {
    let golden_dir = fixtures_root().join(fixture).join("golden");
    if !golden_dir.exists() {
        std::fs::create_dir_all(&golden_dir).expect("create golden dir");
    }
    let golden_path = golden_dir.join(format!("{label}.txt"));

    if std::env::var_os("GOLDEN_REGEN").is_some() {
        std::fs::write(&golden_path, actual)
            .unwrap_or_else(|e| panic!("write golden {}: {e}", golden_path.display()));
        return;
    }

    let expected = match std::fs::read(&golden_path) {
        Ok(b) => b,
        Err(_) => {
            panic!(
                "missing golden file {}\n\
                 hint: GOLDEN_REGEN=1 cargo test --test fixtures_regression",
                golden_path.display()
            );
        }
    };

    if expected != actual {
        // Render a short, line-oriented diff so test logs are useful.
        let exp = String::from_utf8_lossy(&expected);
        let act = String::from_utf8_lossy(actual);
        let mut diff = String::new();
        let exp_lines: Vec<&str> = exp.lines().collect();
        let act_lines: Vec<&str> = act.lines().collect();
        let max = exp_lines.len().max(act_lines.len());
        for i in 0..max {
            let e = exp_lines.get(i).copied().unwrap_or("");
            let a = act_lines.get(i).copied().unwrap_or("");
            if e != a {
                diff.push_str(&format!("- {e}\n+ {a}\n"));
            }
        }
        panic!(
            "fixture {fixture} / {label}: golden mismatch\n\
             expected {} bytes, got {} bytes\n\
             {diff}\n\
             hint: GOLDEN_REGEN=1 cargo test --test fixtures_regression {fixture}",
            expected.len(),
            actual.len(),
        );
    }
}

/// Build the fixture and verify every op.
fn run_fixture(name: &str, ops: &[Op]) {
    if !has_system_git() {
        eprintln!("skipping {name}: system git not available");
        return;
    }
    if !has_bash() {
        eprintln!("skipping {name}: bash not available");
        return;
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    let repo_dir = tmp.path().join(name);
    build_fixture(name, &repo_dir);

    for op in ops {
        let actual = run_rustygit(&repo_dir, op.argv);
        compare_or_regen(name, op.label, &actual);
    }
}

// ---------- per-fixture test entry points ----------

#[test]
fn fixture_01_linear() {
    run_fixture("01-linear", OPS_LINEAR);
}

#[test]
fn fixture_02_branched() {
    run_fixture("02-branched", OPS_BRANCHED);
}

#[test]
fn fixture_03_merged() {
    run_fixture("03-merged", OPS_MERGED);
}

#[test]
fn fixture_04_tagged() {
    run_fixture("04-tagged", OPS_TAGGED);
}

#[test]
fn fixture_05_deleted_files() {
    run_fixture("05-deleted-files", OPS_DELETED);
}

// ---------- meta-tests that don't need git/bash ----------

#[test]
fn fixture_root_exists_and_has_five_fixtures() {
    // Run unconditionally — this guards against an accidental rm -rf
    // of the fixtures directory or a missing build.sh.
    let root = fixtures_root();
    assert!(root.exists(), "fixtures root missing: {}", root.display());
    for name in [
        "01-linear",
        "02-branched",
        "03-merged",
        "04-tagged",
        "05-deleted-files",
    ] {
        let build = root.join(name).join("build.sh");
        assert!(
            build.exists(),
            "fixture {name} is missing build.sh at {}",
            build.display()
        );
    }
}
