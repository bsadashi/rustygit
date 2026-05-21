//! `rustygit mv` — move/rename a path, keeping the index in sync.
//!
//! M4 scope:
//!   `mv <src> <dst>`           — rename a tracked file
//!   `mv <src>... <dst-dir>`    — move multiple files into an existing directory
//!
//! Out of scope: `--force` clobbering, `--dry-run`, `-k` skip-on-error,
//! `--sparse`, `--verbose`. We refuse to clobber existing destinations.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use clap::Args;

use crate::index::Index;
use crate::repo::Repository;

#[derive(Debug, Args)]
pub struct MvArgs {
    /// Force the move even if destination exists. M4 still refuses; reserved.
    #[arg(short = 'f', long = "force")]
    pub force: bool,

    /// One or more sources, then the destination. The destination is the LAST
    /// argument. If there are >= 3 args, the destination must be a directory.
    #[arg(value_name = "PATH", required = true, num_args = 2..)]
    pub paths: Vec<PathBuf>,
}

pub fn run(args: MvArgs) -> io::Result<i32> {
    let repo = Repository::discover_from_cwd().map_err(io_err)?;
    let mut index = Index::read(&repo).map_err(io_err)?;

    let mut paths = args.paths;
    let dst = paths.pop().expect("required >= 2 args");
    let srcs = paths;

    // If multiple sources, dst must be an existing directory.
    let dst_abs = repo.workdir().join(&dst);
    let dst_is_dir = dst_abs.is_dir();
    if srcs.len() > 1 && !dst_is_dir {
        eprintln!(
            "rustygit: destination '{}' is not a directory",
            dst.display()
        );
        return Ok(128);
    }

    for src in &srcs {
        let src_abs = repo.workdir().join(src);
        if !src_abs.exists() {
            eprintln!("rustygit: bad source: '{}'", src.display());
            return Ok(128);
        }
        let src_rel_bytes = match src_abs
            .canonicalize()
            .ok()
            .and_then(|c| c.strip_prefix(repo.workdir()).map(|r| r.to_path_buf()).ok())
        {
            Some(rel) => path_to_index_bytes(&rel),
            None => path_to_index_bytes(src),
        };

        // Destination path resolves either to dst directly (single move) or
        // dst/<basename> (multi-source move into a directory).
        let dst_path: PathBuf = if dst_is_dir {
            let base = src.file_name().ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "src has no basename")
            })?;
            dst_abs.join(base)
        } else {
            dst_abs.clone()
        };
        let dst_rel_bytes = match dst_path
            .strip_prefix(repo.workdir())
            .map(path_to_index_bytes)
        {
            Ok(b) => b,
            Err(_) => path_to_index_bytes(&dst_path),
        };

        if dst_path.exists() && !args.force {
            eprintln!("rustygit: destination exists: '{}'", dst_path.display());
            return Ok(128);
        }

        // Find the index entry for src.
        let entry = match index.entries.iter().find(|e| e.path == src_rel_bytes) {
            Some(e) => e.clone(),
            None => {
                eprintln!("rustygit: not under version control: '{}'", src.display());
                return Ok(128);
            }
        };

        // Move the file on disk.
        if let Some(parent) = dst_path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::rename(&src_abs, &dst_path)?;

        // Update the index: remove old, add renamed copy.
        index.remove(&src_rel_bytes);
        let mut new_entry = entry;
        new_entry.path = dst_rel_bytes.clone();
        new_entry.flags = re_encode_namelen(new_entry.flags, dst_rel_bytes.len());
        index.upsert(new_entry);
    }

    index.sort();
    index.write(&repo).map_err(io_err)?;
    Ok(0)
}

fn re_encode_namelen(flags: u16, new_name_len: usize) -> u16 {
    let high = flags & !0x0FFF;
    let low = (new_name_len.min(0x0FFF) as u16) & 0x0FFF;
    high | low
}

fn path_to_index_bytes(rel: &Path) -> Vec<u8> {
    // `win_paths::to_index` is identity on Unix and `\` → `/` on Windows.
    let s = rel.to_string_lossy();
    crate::cli::win_paths::to_index(&s).into_bytes()
}

fn io_err<E: std::fmt::Display>(e: E) -> io::Error {
    io::Error::other(format!("{e}"))
}
