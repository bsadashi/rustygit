//! Shared test helpers (ADR A8 — comparison test harness).
//!
//! Anything that runs `git` from the system PATH or runs the compiled
//! `rustygit` binary lives here so individual tests stay terse and so we can
//! cleanly skip the suite when `git` isn't available.

#![allow(dead_code)]

use std::path::Path;
use std::process::{Command, Output};

/// Returns true if `git` is on PATH and reports a version. Tests should `skip`
/// (early-return) when this is false rather than fail, since we don't ship git.
pub fn has_system_git() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Run system `git` in the given directory. Panics on failure (we want loud
/// failures during test development).
pub fn git(args: &[&str], cwd: &Path) -> Output {
    let out = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn git: {e}"));
    assert!(
        out.status.success(),
        "git {args:?} failed in {cwd:?}\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    out
}

/// Read every regular file under `root` and return a sorted Vec<(relative-path, contents)>.
/// Used to compare two trees for byte-equality.
pub fn snapshot_dir(root: &Path) -> Vec<(String, Vec<u8>)> {
    let mut out = Vec::new();
    walk(root, root, &mut out);
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

fn walk(root: &Path, cur: &Path, out: &mut Vec<(String, Vec<u8>)>) {
    let entries = match std::fs::read_dir(cur) {
        Ok(e) => e,
        Err(_) => return,
    };
    for ent in entries.flatten() {
        let path = ent.path();
        let ft = match ent.file_type() {
            Ok(t) => t,
            Err(_) => continue,
        };
        if ft.is_dir() {
            walk(root, &path, out);
        } else if ft.is_file() {
            let rel = path
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .to_string();
            let bytes = std::fs::read(&path).unwrap_or_default();
            out.push((rel, bytes));
        }
    }
}

/// List relative paths of all directories under `root`, sorted.
pub fn snapshot_dirs(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    walk_dirs(root, root, &mut out);
    out.sort();
    out
}

fn walk_dirs(root: &Path, cur: &Path, out: &mut Vec<String>) {
    let entries = match std::fs::read_dir(cur) {
        Ok(e) => e,
        Err(_) => return,
    };
    for ent in entries.flatten() {
        let path = ent.path();
        let ft = match ent.file_type() {
            Ok(t) => t,
            Err(_) => continue,
        };
        if ft.is_dir() {
            let rel = path
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .to_string();
            out.push(rel);
            walk_dirs(root, &path, out);
        }
    }
}
