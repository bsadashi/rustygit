//! `rustygit add` — stage paths into the index.
//!
//! M3 scope: literal paths and directory recursion. Each path is hashed as a
//! blob (writing into the loose object store) and an `IndexEntry` is upserted
//! into the index with a stat snapshot of the working file.
//!
//! Out of scope for M3: pathspec magic prefixes, `.gitignore`, `--patch`,
//! `--update`, `--all`, `--intent-to-add`, mode probing for symlinks vs files
//! beyond the basic POSIX bit. M4 lands gitignore + pathspec.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use clap::Args;

use crate::index::{Index, IndexEntry};
use crate::object::{ObjectKind, RawObject};
use crate::repo::Repository;
use crate::tree::FileMode;

#[derive(Debug, Args)]
pub struct AddArgs {
    /// Paths to stage. Directories are recursed. Optional when `--patch` is set.
    #[arg(value_name = "PATH")]
    pub paths: Vec<PathBuf>,

    /// Allow updating index entries that match the on-disk stat without
    /// re-hashing — currently always on (we always re-hash). Reserved for
    /// future use.
    #[arg(long)]
    pub refresh: bool,

    /// Interactively choose hunks of patch between the index and the working
    /// tree and add them to the index. Subset implementation: y/n/q/a/d/s/?.
    /// See POLISH.md item 7.
    #[arg(short = 'p', long = "patch")]
    pub patch: bool,
}

pub fn run(args: AddArgs) -> io::Result<i32> {
    let repo = Repository::discover_from_cwd().map_err(io_err)?;

    if args.patch {
        // -p ignores PATH arguments today (subset). git treats them as a
        // pathspec filter, which we don't yet implement here.
        return crate::cli::add_patch::run(&repo);
    }

    let mut index = Index::read(&repo).map_err(io_err)?;

    if args.paths.is_empty() {
        eprintln!("rustygit add: no paths given");
        return Ok(129);
    }

    for input in &args.paths {
        let abs = repo
            .workdir()
            .join(input)
            .canonicalize()
            .or_else(|_| input.canonicalize())
            .unwrap_or_else(|_| repo.workdir().join(input));
        if abs.is_dir() {
            walk_dir(&repo, &abs, &mut index)?;
        } else if abs.is_file() || is_symlink(&abs) {
            stage_one(&repo, &abs, &mut index)?;
        } else {
            eprintln!(
                "rustygit: pathspec '{}' did not match any files",
                input.display()
            );
            return Ok(128);
        }
    }

    index.sort();
    index.write(&repo).map_err(io_err)?;
    Ok(0)
}

fn walk_dir(repo: &Repository, dir: &Path, index: &mut Index) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let name = entry.file_name();
        // Skip the .git directory itself; never index it.
        if name == ".git" {
            continue;
        }
        let ft = entry.file_type()?;
        if ft.is_dir() {
            walk_dir(repo, &path, index)?;
        } else if ft.is_file() || ft.is_symlink() {
            stage_one(repo, &path, index)?;
        }
    }
    Ok(())
}

fn stage_one(repo: &Repository, abs: &Path, index: &mut Index) -> io::Result<()> {
    let workdir = repo.workdir();
    let rel = match abs.strip_prefix(workdir) {
        Ok(r) => r,
        Err(_) => {
            eprintln!("rustygit: '{}' is outside the working tree", abs.display());
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "outside worktree",
            ));
        }
    };
    let rel_bytes = path_to_index_bytes(rel);

    let metadata = fs::symlink_metadata(abs)?;
    let mode = derive_mode(&metadata);

    // Read content. For symlinks, the blob payload is the link target.
    let mut payload = if metadata.file_type().is_symlink() {
        fs::read_link(abs)?
            .as_os_str()
            .to_string_lossy()
            .into_owned()
            .into_bytes()
    } else {
        fs::read(abs)?
    };

    // `core.autocrlf` (true | input): normalize CRLF → LF on text blobs
    // before hashing so the index OID matches what an `add` on a Unix box
    // would produce. The text-blob heuristic mirrors upstream git: "no
    // NUL byte in the first 8000 bytes". Symlink targets are never
    // line-end-converted (their payload is a path, not text in the
    // CRLF-sense). NON_GOALS A10.
    if mode != FileMode::Symlink
        && repo
            .core_autocrlf()
            .map(|m| m.normalizes_on_add())
            .unwrap_or(false)
        && crate::config::is_text_blob(&payload)
    {
        let normalized = crate::config::normalize_crlf_to_lf(&payload);
        if let std::borrow::Cow::Owned(v) = normalized {
            payload = v;
        }
    }

    let blob = RawObject::new(ObjectKind::Blob, payload);
    let oid = repo
        .odb()
        .write(&blob)
        .map_err(|e| io::Error::other(format!("{e}")))?;

    let stat = stat_for_entry(&metadata);
    let entry = IndexEntry {
        ctime_s: stat.ctime_s,
        ctime_n: stat.ctime_n,
        mtime_s: stat.mtime_s,
        mtime_n: stat.mtime_n,
        dev: stat.dev,
        ino: stat.ino,
        mode: mode.to_index_mode(),
        uid: stat.uid,
        gid: stat.gid,
        size: stat.size,
        oid,
        flags: encode_flags(rel_bytes.len(), 0, false, false),
        path: rel_bytes,
        stage: 0,
        assume_valid: false,
        extended: false,
        extended_flags: 0,
    };
    index.upsert(entry);
    Ok(())
}

fn derive_mode(meta: &fs::Metadata) -> FileMode {
    let ft = meta.file_type();
    if ft.is_symlink() {
        return FileMode::Symlink;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o111 != 0 {
            return FileMode::Executable;
        }
    }
    FileMode::Regular
}

struct StatBits {
    ctime_s: u32,
    ctime_n: u32,
    mtime_s: u32,
    mtime_n: u32,
    dev: u32,
    ino: u32,
    uid: u32,
    gid: u32,
    size: u32,
}

fn stat_for_entry(meta: &fs::Metadata) -> StatBits {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        StatBits {
            ctime_s: meta.ctime() as u32,
            ctime_n: meta.ctime_nsec() as u32,
            mtime_s: meta.mtime() as u32,
            mtime_n: meta.mtime_nsec() as u32,
            dev: meta.dev() as u32,
            ino: meta.ino() as u32,
            uid: meta.uid(),
            gid: meta.gid(),
            size: meta.size().min(u32::MAX as u64) as u32,
        }
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        StatBits {
            ctime_s: 0,
            ctime_n: 0,
            mtime_s: 0,
            mtime_n: 0,
            dev: 0,
            ino: 0,
            uid: 0,
            gid: 0,
            size: 0,
        }
    }
}

fn encode_flags(name_len: usize, stage: u8, assume_valid: bool, extended: bool) -> u16 {
    let mut f: u16 = (name_len.min(0x0FFF) as u16) & 0x0FFF;
    f |= ((stage as u16) & 0x3) << 12;
    if extended {
        f |= 0x4000;
    }
    if assume_valid {
        f |= 0x8000;
    }
    f
}

#[cfg(unix)]
fn is_symlink(p: &Path) -> bool {
    fs::symlink_metadata(p)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_symlink(_p: &Path) -> bool {
    false
}

fn path_to_index_bytes(rel: &Path) -> Vec<u8> {
    // Index paths use forward slashes regardless of OS. `win_paths::to_index`
    // is identity on Unix and `\` → `/` on Windows.
    let s = rel.to_string_lossy();
    crate::cli::win_paths::to_index(&s).into_bytes()
}

fn io_err<E: std::fmt::Display>(e: E) -> io::Error {
    io::Error::other(format!("{e}"))
}
