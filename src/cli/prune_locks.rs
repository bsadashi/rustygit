//! `rustygit prune-locks` — clean up stale `*.lock` files left behind by a
//! crashed earlier process.
//!
//! Why this is a separate subcommand and not auto-cleanup at acquire time:
//! a `*.lock` file is a real exclusion primitive against concurrent writers.
//! At acquire time we can't safely distinguish "another process is mid-
//! write" from "a process crashed an hour ago" — silently deleting a peer's
//! in-progress lock is data corruption. Instead, the user runs this command
//! explicitly when they know no other rustygit / git process is running.
//!
//! Discovery scope (mirrors git's own crash recovery):
//! * `<gitdir>/index.lock`
//! * `<gitdir>/HEAD.lock`
//! * `<gitdir>/config.lock`
//! * `<gitdir>/packed-refs.lock`
//! * `<gitdir>/refs/**/*.lock`
//! * `<gitdir>/logs/**/*.lock`
//! * `<gitdir>/checkout.tmp.*/` (shadow dirs from transactional checkout)
//! * `<gitdir>/checkout.recover.*/` (these we WARN about but never auto-
//!   delete — they hold originals that rolled-back checkout couldn't
//!   restore. Print the path; let the user inspect.)

use std::io;
use std::path::{Path, PathBuf};

use clap::Args;

use crate::lockfile::STALE_LOCK_HINT_SECS;
use crate::repo::Repository;

#[derive(Debug, Args)]
pub struct PruneLocksArgs {
    /// Show what would be removed without actually removing anything.
    #[arg(short = 'n', long = "dry-run")]
    pub dry_run: bool,

    /// Remove every `*.lock` found, regardless of age. Use only when you
    /// know no other rustygit/git process is running against this repo.
    #[arg(long = "force")]
    pub force: bool,

    /// Override the default age threshold (in seconds). Locks newer than
    /// this are left alone unless `--force`.
    #[arg(long = "older-than", value_name = "SECONDS")]
    pub older_than: Option<u64>,

    /// Verbose listing of every lock found, including non-stale ones.
    #[arg(short = 'v', long = "verbose")]
    pub verbose: bool,
}

pub fn run(args: PruneLocksArgs) -> io::Result<i32> {
    let repo = Repository::discover_from_cwd().map_err(io_err)?;
    let threshold = args.older_than.unwrap_or(STALE_LOCK_HINT_SECS);

    let mut found = Vec::new();
    collect_locks(repo.gitdir(), &mut found);

    let mut removed = 0usize;
    let mut kept = 0usize;
    let mut recoveries = Vec::new();
    for entry in &found {
        if entry.is_recovery_dir {
            recoveries.push(entry.path.clone());
            continue;
        }
        let stale = args.force || age_seconds(&entry.path).unwrap_or(0) >= threshold;
        if stale {
            if args.dry_run {
                println!("would remove: {}", entry.path.display());
                removed += 1;
            } else {
                let res = if entry.is_dir {
                    std::fs::remove_dir_all(&entry.path)
                } else {
                    std::fs::remove_file(&entry.path)
                };
                match res {
                    Ok(()) => {
                        if args.verbose {
                            println!("removed: {}", entry.path.display());
                        }
                        removed += 1;
                    }
                    Err(e) => {
                        eprintln!(
                            "rustygit: prune-locks: cannot remove {}: {e}",
                            entry.path.display()
                        );
                    }
                }
            }
        } else {
            kept += 1;
            if args.verbose {
                let age = age_seconds(&entry.path).unwrap_or(0);
                println!("keep ({age}s old): {}", entry.path.display());
            }
        }
    }

    for r in &recoveries {
        eprintln!(
            "rustygit: prune-locks: recovery directory {} preserves originals from a failed \
             rollback — inspect contents and move them back by hand, then remove the directory",
            r.display()
        );
    }

    println!(
        "rustygit: prune-locks: {removed} removed, {kept} kept, {} recovery dir(s)",
        recoveries.len()
    );
    Ok(0)
}

struct LockEntry {
    path: PathBuf,
    is_dir: bool,
    is_recovery_dir: bool,
}

fn collect_locks(gitdir: &Path, out: &mut Vec<LockEntry>) {
    // Top-level `*.lock` files.
    if let Ok(rd) = std::fs::read_dir(gitdir) {
        for ent in rd.flatten() {
            let name = ent.file_name();
            let name_s = name.to_string_lossy();
            let path = ent.path();
            if name_s.ends_with(".lock") {
                out.push(LockEntry {
                    path,
                    is_dir: false,
                    is_recovery_dir: false,
                });
            } else if name_s.starts_with("checkout.tmp.") {
                out.push(LockEntry {
                    path,
                    is_dir: true,
                    is_recovery_dir: false,
                });
            } else if name_s.starts_with("checkout.recover.") {
                out.push(LockEntry {
                    path,
                    is_dir: true,
                    is_recovery_dir: true,
                });
            }
        }
    }
    // `refs/**/*.lock` and `logs/**/*.lock`.
    for sub in ["refs", "logs"] {
        walk_locks(&gitdir.join(sub), out);
    }
}

fn walk_locks(root: &Path, out: &mut Vec<LockEntry>) {
    let rd = match std::fs::read_dir(root) {
        Ok(r) => r,
        Err(_) => return,
    };
    for ent in rd.flatten() {
        let path = ent.path();
        let ft = match ent.file_type() {
            Ok(t) => t,
            Err(_) => continue,
        };
        if ft.is_dir() {
            walk_locks(&path, out);
        } else if path
            .extension()
            .and_then(|s| s.to_str())
            .map(|s| s == "lock")
            .unwrap_or(false)
        {
            out.push(LockEntry {
                path,
                is_dir: false,
                is_recovery_dir: false,
            });
        }
    }
}

fn age_seconds(path: &Path) -> Option<u64> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn fake_repo() -> TempDir {
        let dir = TempDir::new().unwrap();
        let gitdir = dir.path().join(".git");
        for sub in ["", "refs", "refs/heads", "refs/tags", "logs", "objects"] {
            std::fs::create_dir_all(gitdir.join(sub)).unwrap();
        }
        std::fs::write(gitdir.join("HEAD"), b"ref: refs/heads/master\n").unwrap();
        std::fs::write(
            gitdir.join("config"),
            b"[core]\n\trepositoryformatversion = 0\n",
        )
        .unwrap();
        dir
    }

    #[test]
    fn collect_finds_locks_and_shadow_dirs() {
        let dir = fake_repo();
        let gitdir = dir.path().join(".git");
        // Lay out: index.lock, refs/heads/master.lock, checkout.tmp.X dir,
        // checkout.recover.X dir, plus a non-lock file we must NOT pick up.
        std::fs::write(gitdir.join("index.lock"), b"stuff").unwrap();
        std::fs::write(gitdir.join("refs/heads/master.lock"), b"x").unwrap();
        std::fs::create_dir_all(gitdir.join("checkout.tmp.123.456")).unwrap();
        std::fs::create_dir_all(gitdir.join("checkout.recover.123.456")).unwrap();
        std::fs::write(gitdir.join("config"), b"already exists").unwrap();

        let mut found = Vec::new();
        collect_locks(&gitdir, &mut found);
        let names: Vec<_> = found
            .iter()
            .map(|e| {
                (
                    e.path.file_name().unwrap().to_string_lossy().to_string(),
                    e.is_dir,
                    e.is_recovery_dir,
                )
            })
            .collect();
        assert!(names.contains(&("index.lock".to_string(), false, false)));
        assert!(names.contains(&("master.lock".to_string(), false, false)));
        assert!(names.contains(&("checkout.tmp.123.456".to_string(), true, false)));
        assert!(names.contains(&("checkout.recover.123.456".to_string(), true, true)));
        assert!(!names.iter().any(|(n, _, _)| n == "config"));
    }
}
