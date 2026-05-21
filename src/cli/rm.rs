//! `rustygit rm` — remove paths from the index and (optionally) the workdir.
//!
//! M4 scope: literal paths and `-r` for directories. `--cached` keeps the file
//! on disk and only un-stages it. Without `-f`, refuses to remove a file whose
//! workdir content differs from its index entry (matching git's safety).
//!
//! Out of scope for M4: pathspec magic, `--ignore-unmatch`, `--quiet`,
//! `--sparse`, `--pathspec-from-file`. Those follow when pathspec lands.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use clap::Args;

use crate::index::Index;
use crate::object::{ObjectKind, RawObject};
use crate::repo::Repository;

#[derive(Debug, Args)]
pub struct RmArgs {
    /// Allow recursive removal when a directory is given.
    #[arg(short = 'r', long = "recursive")]
    pub recursive: bool,

    /// Override the safety check that refuses to remove modified files.
    #[arg(short = 'f', long = "force")]
    pub force: bool,

    /// Only un-stage from the index; leave the file on disk.
    #[arg(long = "cached")]
    pub cached: bool,

    /// Paths to remove. Literal only in M4.
    #[arg(value_name = "PATH", required = true)]
    pub paths: Vec<PathBuf>,
}

pub fn run(args: RmArgs) -> io::Result<i32> {
    let repo = Repository::discover_from_cwd().map_err(io_err)?;
    let mut index = Index::read(&repo).map_err(io_err)?;

    let mut to_remove: Vec<Vec<u8>> = Vec::new();

    for input in &args.paths {
        let abs = match repo.workdir().join(input).canonicalize() {
            Ok(a) => a,
            Err(_) => repo.workdir().join(input),
        };
        let rel_bytes = match abs.strip_prefix(repo.workdir()) {
            Ok(rel) => path_to_index_bytes(rel),
            Err(_) => path_to_index_bytes(input),
        };

        // Match this path against indexed entries. Literal: either a single
        // matching entry, or a "directory prefix" if -r.
        let matches: Vec<Vec<u8>> = if args.recursive && abs.is_dir() {
            let prefix = if rel_bytes.is_empty() {
                Vec::new()
            } else {
                let mut p = rel_bytes.clone();
                if !p.ends_with(b"/") {
                    p.push(b'/');
                }
                p
            };
            index
                .entries
                .iter()
                .filter(|e| e.path == rel_bytes || e.path.starts_with(&prefix))
                .map(|e| e.path.clone())
                .collect()
        } else {
            // Exact path
            if index.entries.iter().any(|e| e.path == rel_bytes) {
                vec![rel_bytes.clone()]
            } else {
                eprintln!(
                    "rustygit: pathspec '{}' did not match any files in the index",
                    input.display()
                );
                return Ok(128);
            }
        };

        if matches.is_empty() {
            eprintln!(
                "rustygit: pathspec '{}' did not match any files in the index",
                input.display()
            );
            return Ok(128);
        }

        if !args.force {
            for path_bytes in &matches {
                check_safe_to_remove(&repo, &index, path_bytes, args.cached)?;
            }
        }

        to_remove.extend(matches);
    }

    // Apply removals.
    for path_bytes in &to_remove {
        index.remove(path_bytes);
        if !args.cached {
            let abs = repo.workdir().join(bytes_to_path(path_bytes));
            // Ignore NotFound — file may already be gone.
            match fs::remove_file(&abs) {
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
        }
        println!("rm '{}'", String::from_utf8_lossy(path_bytes));
    }

    index.sort();
    index.write(&repo).map_err(io_err)?;
    Ok(0)
}

fn check_safe_to_remove(
    repo: &Repository,
    index: &Index,
    path_bytes: &[u8],
    cached: bool,
) -> io::Result<()> {
    let entry = match index.entries.iter().find(|e| e.path == path_bytes) {
        Some(e) => e,
        None => return Ok(()),
    };
    if cached {
        // `--cached` removes from index regardless of workdir state.
        return Ok(());
    }
    let abs = repo.workdir().join(bytes_to_path(path_bytes));
    let bytes = match fs::read(&abs) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    let blob = RawObject::new(ObjectKind::Blob, bytes);
    if blob.oid(repo.hash_kind()) != entry.oid {
        eprintln!(
            "rustygit: '{}' has local modifications (use -f to force)",
            String::from_utf8_lossy(path_bytes)
        );
        return Err(io::Error::other("local modifications"));
    }
    Ok(())
}

fn path_to_index_bytes(rel: &Path) -> Vec<u8> {
    // `win_paths::to_index` is identity on Unix and `\` → `/` on Windows.
    let s = rel.to_string_lossy();
    crate::cli::win_paths::to_index(&s).into_bytes()
}

fn bytes_to_path(b: &[u8]) -> PathBuf {
    PathBuf::from(String::from_utf8_lossy(b).into_owned())
}

fn io_err<E: std::fmt::Display>(e: E) -> io::Error {
    io::Error::other(format!("{e}"))
}
