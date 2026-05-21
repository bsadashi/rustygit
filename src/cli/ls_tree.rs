//! `rustygit ls-tree` — list the contents of a tree object.
//!
//! Supports the most-used flags: `-r` (recurse into subtrees), `-t` (show tree
//! entries even when recursing), `-d` (only directories), `--name-only`.
//! Output format mirrors `git ls-tree`: `<mode> <type> <oid>\t<name>\n`, with
//! mode left-padded to 6 chars (so `40000` prints as `040000`).

use std::io;

use clap::Args;

use crate::hash::{HashKind, ObjectId};
use crate::object::ObjectKind;
use crate::odb::PrefixMatch;
use crate::repo::Repository;
use crate::tree::{FileMode, Tree};

#[derive(Debug, Args)]
pub struct LsTreeArgs {
    /// Recurse into sub-trees.
    #[arg(short = 'r')]
    pub recurse: bool,

    /// When recursing, also show tree entries.
    #[arg(short = 't')]
    pub show_trees_in_recursion: bool,

    /// Only show directory entries.
    #[arg(short = 'd', conflicts_with_all = ["recurse", "show_trees_in_recursion"])]
    pub dirs_only: bool,

    /// Show only file names.
    #[arg(long = "name-only", visible_alias = "name-status")]
    pub name_only: bool,

    /// The tree-ish to list.
    #[arg(value_name = "TREE-ISH")]
    pub tree_ish: String,

    /// Optional path filter (M1 honors only the first leading-component prefix).
    #[arg(value_name = "PATH")]
    pub paths: Vec<String>,
}

pub fn run(args: LsTreeArgs) -> io::Result<i32> {
    let repo = Repository::discover_from_cwd().map_err(|e| io::Error::other(format!("{e}")))?;
    let hash_kind = repo.hash_kind();

    let oid = resolve_treeish(&repo, &args.tree_ish, hash_kind)?;
    let stdout = io::stdout();
    let mut stdout = stdout.lock();
    walk(&repo, &oid, Vec::new(), &args, hash_kind, &mut stdout, true)?;
    Ok(0)
}

fn resolve_treeish(repo: &Repository, s: &str, hash_kind: HashKind) -> io::Result<ObjectId> {
    // Try the full revparse pipeline first — handles ref names (HEAD,
    // refs/heads/...), oid prefixes, and `HEAD^{tree}`-style peels.
    if let Ok(o) = crate::revparse::resolve(repo.refs(), repo.odb(), s) {
        return Ok(o);
    }
    if let Ok(o) = ObjectId::parse_hex(hash_kind, s) {
        return Ok(o);
    }
    match repo
        .odb()
        .resolve_prefix(s)
        .map_err(|e| io::Error::other(format!("{e}")))?
    {
        PrefixMatch::Found(o) => Ok(o),
        PrefixMatch::None => Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("not a valid tree-ish: {s}"),
        )),
        PrefixMatch::Ambiguous(_) => Err(io::Error::other(format!("ambiguous tree-ish: {s}"))),
    }
}

fn walk(
    repo: &Repository,
    oid: &ObjectId,
    prefix: Vec<u8>,
    args: &LsTreeArgs,
    hash_kind: HashKind,
    out: &mut dyn io::Write,
    is_root: bool,
) -> io::Result<()> {
    let obj = repo
        .odb()
        .read(oid)
        .map_err(|e| io::Error::other(format!("{e}")))?;
    let tree = match obj.kind {
        ObjectKind::Tree => Tree::parse(&obj.data, hash_kind)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("{e}")))?,
        ObjectKind::Commit => {
            // Walk through to the tree.
            let body = std::str::from_utf8(&obj.data).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidData, "non-utf8 commit object")
            })?;
            let tree_line = body.lines().next().unwrap_or("");
            let tree_oid = tree_line.strip_prefix("tree ").ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "commit missing tree line")
            })?;
            let tree_oid = ObjectId::parse_hex(hash_kind, tree_oid)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("{e}")))?;
            return walk(repo, &tree_oid, prefix, args, hash_kind, out, is_root);
        }
        other => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("not a tree: {other}"),
            ));
        }
    };

    for entry in tree.entries {
        let mut full = prefix.clone();
        if !full.is_empty() {
            full.push(b'/');
        }
        full.extend_from_slice(&entry.name);

        let is_subtree = entry.mode.is_tree();
        let should_show = if args.dirs_only {
            is_subtree
        } else if args.recurse && is_subtree {
            // When recursing, by default we don't print subtree entries
            // themselves — only their leaves. `-t` re-enables them.
            args.show_trees_in_recursion
        } else {
            true
        };

        if should_show {
            print_entry(out, args, &entry.mode, &entry.oid, &full)?;
        }

        if args.recurse && is_subtree {
            walk(repo, &entry.oid, full, args, hash_kind, out, false)?;
        }
    }

    let _ = is_root;
    Ok(())
}

fn print_entry(
    out: &mut dyn io::Write,
    args: &LsTreeArgs,
    mode: &FileMode,
    oid: &ObjectId,
    full_name: &[u8],
) -> io::Result<()> {
    if args.name_only {
        out.write_all(full_name)?;
        out.write_all(b"\n")?;
        return Ok(());
    }
    let mode_str = mode.as_octal();
    let padded = if mode_str.len() == 5 {
        format!("0{mode_str}")
    } else {
        mode_str.to_string()
    };
    write!(out, "{padded} {} {}\t", mode.object_kind(), oid)?;
    out.write_all(full_name)?;
    out.write_all(b"\n")?;
    Ok(())
}
