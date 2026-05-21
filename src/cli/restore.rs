//! `rustygit restore` — restore working-tree (or index) entries from a
//! source tree.
//!
//! Forms supported in M6:
//! - `restore <path>...`: restore each path's content in the working tree
//!   from the index.
//! - `restore --staged <path>...`: restore each path's index entry from
//!   `HEAD`'s tree (or `--source` when supplied).
//! - `restore --source=<rev> <path>...`: restore the working tree from the
//!   given rev (and the index too if `--staged` is also set).
//!
//! Restore never moves HEAD and never updates the branch ref. There's no
//! reflog entry to write at this layer.

use std::io::{self, Write};
use std::path::PathBuf;

use clap::Args;

use crate::cli::checkout::{io_err, peel_to_tree, print_conflicts};
use crate::index::Index;
use crate::object::ObjectKind;
use crate::refs::{FullName, RefTarget};
use crate::repo::Repository;
use crate::revparse::resolve;
use crate::unpack_trees::{self, UnpackError, UnpackOpts};

#[derive(Debug, Args)]
pub struct RestoreArgs {
    /// Restore the index instead of (or in addition to) the working tree.
    #[arg(long = "staged")]
    pub staged: bool,

    /// Restore from this rev (default: HEAD for `--staged`, the index for the
    /// working tree).
    #[arg(long = "source", value_name = "REV")]
    pub source: Option<String>,

    /// Files to restore.
    #[arg(value_name = "PATH", required = true)]
    pub paths: Vec<PathBuf>,
}

pub fn run(args: RestoreArgs) -> io::Result<i32> {
    let repo = Repository::discover_from_cwd().map_err(io_err)?;

    // Convert PathBuf paths to repo-relative byte paths. We don't yet do
    // proper pathspec normalization (M7+); use literal bytes.
    let paths: Vec<Vec<u8>> = args.paths.iter().map(|p| pathbuf_to_bytes(p)).collect();

    // Decide where the workdir / index restoration sources come from.
    // Three cases:
    //   --staged                  : index <- HEAD-tree
    //   <path>...                 : workdir <- index
    //   --source=<rev>            : workdir <- rev-tree
    //   --staged --source=<rev>   : index <- rev-tree
    //   --source=<rev> (no flag)  : workdir <- rev-tree (same as 3)
    //
    // When both --staged and a workdir flag would apply we currently support
    // only the documented combos above. A bare `--source` without `--staged`
    // restores the workdir only (matching git default).

    let stage_source = if args.staged {
        Some(match &args.source {
            Some(rev) => resolve_tree(&repo, rev)?,
            None => match resolve_head_tree(&repo)? {
                Some(t) => t,
                None => {
                    eprintln!("error: --staged requires HEAD or --source on a fresh repo");
                    return Ok(1);
                }
            },
        })
    } else {
        None
    };

    // Workdir source.
    if !args.staged {
        // No --staged: restore workdir.
        let source_tree = match &args.source {
            Some(rev) => Some(resolve_tree(&repo, rev)?),
            None => None, // restore from index, see below
        };

        match source_tree {
            Some(tree) => {
                let opts = UnpackOpts {
                    force: false,
                    keep_extra: false,
                    update_index: false,
                    update_workdir: true,
                };
                if let Err(e) = unpack_trees::checkout_tree_for_paths(&repo, tree, &paths, &opts) {
                    return handle_unpack_err(e);
                }
            }
            None => {
                // workdir <- index. We don't go through the unpack-trees
                // engine for this since the index already has the content;
                // we just re-materialize the blob bytes for each path.
                if let Err(code) = restore_workdir_from_index(&repo, &paths)? {
                    return Ok(code);
                }
            }
        }
    } else {
        // --staged: only the index is touched.
        let tree = stage_source.expect("set above when staged is true");
        let opts = UnpackOpts {
            force: false,
            keep_extra: false,
            update_index: true,
            update_workdir: false,
        };
        if let Err(e) = unpack_trees::checkout_tree_for_paths(&repo, tree, &paths, &opts) {
            return handle_unpack_err(e);
        }
    }

    Ok(0)
}

fn handle_unpack_err(e: UnpackError) -> io::Result<i32> {
    match e {
        UnpackError::Conflicts(conflicts) => {
            print_conflicts("checkout", &conflicts);
            Ok(1)
        }
        other => Err(io_err(other)),
    }
}

fn resolve_tree(repo: &Repository, rev: &str) -> io::Result<crate::hash::ObjectId> {
    let oid = resolve(repo.refs(), repo.odb(), rev).map_err(io_err)?;
    peel_to_tree(repo, oid)
}

fn resolve_head_tree(repo: &Repository) -> io::Result<Option<crate::hash::ObjectId>> {
    let head_name = FullName::new("HEAD").map_err(io_err)?;
    match RefTarget::resolve(repo.refs(), &head_name).map_err(io_err)? {
        Some((_, oid)) => Ok(Some(peel_to_tree(repo, oid)?)),
        None => Ok(None),
    }
}

/// Walk the requested paths, look each one up in the current index, and
/// rewrite the corresponding working-tree file with the blob's content.
///
/// Returns `Ok(Err(code))` to propagate a non-zero exit code without
/// distinguishing between "io error" and "expected failure".
fn restore_workdir_from_index(repo: &Repository, paths: &[Vec<u8>]) -> io::Result<Result<(), i32>> {
    let index = Index::read(repo).map_err(io_err)?;

    // O(N*M) is fine for typical M6 inputs; switch to a BTreeMap if it ever
    // shows up in a flame graph.
    let mut any_missing = false;
    for path in paths {
        let entry = match index
            .entries
            .iter()
            .find(|e| e.path == *path && e.stage == 0)
        {
            Some(e) => e,
            None => {
                eprintln!(
                    "error: pathspec '{}' did not match any file(s) known to rustygit",
                    String::from_utf8_lossy(path)
                );
                any_missing = true;
                continue;
            }
        };

        let blob = repo.odb().read(&entry.oid).map_err(io_err)?;
        if blob.kind != ObjectKind::Blob {
            eprintln!(
                "error: index entry for '{}' is not a blob",
                String::from_utf8_lossy(path)
            );
            any_missing = true;
            continue;
        }

        let rel = match bytes_to_relpath_checked(path) {
            Ok(p) => p,
            Err(e) => return Err(io_err(e)),
        };
        let abs_path = repo.workdir().join(rel);
        if let Some(parent) = abs_path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| io::Error::other(format!("create dir {}: {e}", parent.display())))?;
        }
        // Write atomically-ish: open, truncate, write, sync.
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&abs_path)
            .map_err(|e| io::Error::other(format!("open {}: {e}", abs_path.display())))?;
        f.write_all(&blob.data)?;

        // Preserve executable mode roughly (M6: bits 0o100755 vs 0o100644).
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode_bits: u32 = if entry.mode == 0o100755 { 0o755 } else { 0o644 };
            let _ = std::fs::set_permissions(&abs_path, std::fs::Permissions::from_mode(mode_bits));
        }
    }

    if any_missing {
        Ok(Err(1))
    } else {
        Ok(Ok(()))
    }
}

fn pathbuf_to_bytes(p: &std::path::Path) -> Vec<u8> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        p.as_os_str().as_bytes().to_vec()
    }
    #[cfg(not(unix))]
    {
        p.to_string_lossy().into_owned().into_bytes()
    }
}

fn bytes_to_relpath(b: &[u8]) -> PathBuf {
    #[cfg(unix)]
    {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;
        PathBuf::from(OsStr::from_bytes(b))
    }
    #[cfg(not(unix))]
    {
        PathBuf::from(String::from_utf8_lossy(b).into_owned())
    }
}

/// Strict variant: refuses non-UTF-8 names on non-Unix platforms. Use this
/// for any callsite that writes to the workdir; the lossy `bytes_to_relpath`
/// is fine for display-only consumers.
fn bytes_to_relpath_checked(b: &[u8]) -> Result<PathBuf, crate::unpack_trees::UnpackError> {
    #[cfg(unix)]
    {
        Ok(bytes_to_relpath(b))
    }
    #[cfg(not(unix))]
    {
        match std::str::from_utf8(b) {
            Ok(s) => Ok(PathBuf::from(s)),
            Err(_) => Err(crate::unpack_trees::UnpackError::PathEncodingError {
                bytes: b.to_vec(),
                op: "restore".to_string(),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Debug, Parser)]
    struct Wrap {
        #[command(flatten)]
        args: RestoreArgs,
    }

    #[test]
    fn parses_basic_path() {
        let w = Wrap::try_parse_from(["x", "foo.txt"]).unwrap();
        assert!(!w.args.staged);
        assert!(w.args.source.is_none());
        assert_eq!(w.args.paths, vec![PathBuf::from("foo.txt")]);
    }

    #[test]
    fn parses_staged_with_source() {
        let w = Wrap::try_parse_from(["x", "--staged", "--source=HEAD~1", "foo.txt"]).unwrap();
        assert!(w.args.staged);
        assert_eq!(w.args.source.as_deref(), Some("HEAD~1"));
    }

    #[test]
    fn requires_at_least_one_path() {
        assert!(Wrap::try_parse_from(["x", "--staged"]).is_err());
    }
}
