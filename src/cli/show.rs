//! `rustygit show <rev>` — print one object plus, for commits, the diff
//! against its first parent.
//!
//! Behavior matches `git show`'s default for each object kind:
//!
//! * **commit** — medium-format commit header (same as `git log -1`) followed
//!   by a blank line and the diff against the first parent. For a root commit
//!   (no parents) the diff is against the empty tree, which means every file
//!   shows as "new file". For merge commits we print the header only and a
//!   note that combined-diff output is deferred (matches the spirit of
//!   `git show --no-patch` for merges; the full `--cc`/`-m` output is its own
//!   subsystem).
//! * **tag** — annotated-tag header (tag/object/type/tagger + message) and
//!   then recursively shows the tagged object.
//! * **tree** — `ls-tree`-style listing of the tree.
//! * **blob** — raw bytes to stdout.

use std::io::{self, Write};

use clap::Args;

use crate::commit::Commit;
use crate::config::Config;
use crate::diff;
use crate::hash::ObjectId;
use crate::object::ObjectKind;
use crate::repo::Repository;
use crate::revparse::resolve;
use crate::tree::Tree;

#[derive(Debug, Args)]
pub struct ShowArgs {
    /// Object to show. Defaults to HEAD. Multiple objects are shown in order
    /// (matching `git show <a> <b> <c>`); separated by a blank line each.
    #[arg(value_name = "OBJECT", default_value = "HEAD")]
    pub objects: Vec<String>,
}

pub fn run(args: ShowArgs) -> io::Result<i32> {
    let repo = Repository::discover_from_cwd().map_err(io_err)?;
    let cfg = Config::from_repo_dir(repo.gitdir()).unwrap_or_else(|_| Config::empty());
    let mut out = crate::cli::pager::open(&cfg, false)?;

    // Default-value handling: clap fills `objects` with ["HEAD"] when none is
    // given, but if the user passes flags + an explicit list we use that. We
    // separate multiple objects with a single blank line, like git does.
    let objects = if args.objects.is_empty() {
        vec!["HEAD".to_string()]
    } else {
        args.objects
    };

    for (i, name) in objects.iter().enumerate() {
        if out.stopped() {
            break;
        }
        if i > 0 {
            writeln!(out)?;
        }
        let oid = match resolve(repo.refs(), repo.odb(), name) {
            Ok(o) => o,
            Err(e) => {
                eprintln!("rustygit: show: {e}");
                return Ok(128);
            }
        };
        if let Some(code) = show_object(&repo, oid, &mut out, 0)? {
            return Ok(code);
        }
    }
    Ok(0)
}

/// Maximum tag → tag → ... chain depth we'll follow before giving up. A
/// safety bound against pathological repos (or actively malicious ones) so
/// `show` can't be turned into a stack-overflow vector. 10 is generous —
/// real-world annotated-tag chains are essentially never longer than 1.
const MAX_TAG_DEPTH: u32 = 10;

/// Show one object. Returns `Some(exit_code)` if we want to bail (only on
/// real errors); `None` for "successfully showed, keep going."
///
/// `depth` tracks recursion through annotated-tag chains; we refuse past
/// [`MAX_TAG_DEPTH`] so a malicious tag → tag → ... chain can't stack-
/// overflow the process.
fn show_object<W: Write>(
    repo: &Repository,
    oid: ObjectId,
    out: &mut W,
    depth: u32,
) -> io::Result<Option<i32>> {
    let raw = match repo.odb().read(&oid) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("rustygit: show: {e}");
            return Ok(Some(128));
        }
    };
    match raw.kind {
        ObjectKind::Commit => {
            let commit = Commit::parse(&raw.data, repo.hash_kind()).map_err(io_err)?;
            print_commit_header(out, &oid, &commit)?;
            // Diff against the first parent. Root commits diff against the
            // empty tree (i.e. every file is "new file"). Merge commits print
            // the header only — combined-diff output is its own subsystem.
            if commit.parents.len() <= 1 {
                writeln!(out)?;
                let target_tree = commit.tree;
                if let Some(parent_oid) = commit.parents.first().copied() {
                    let parent = repo.odb().read(&parent_oid).map_err(io_err)?;
                    let parent_commit =
                        Commit::parse(&parent.data, repo.hash_kind()).map_err(io_err)?;
                    diff::diff_two_trees(repo, parent_commit.tree, target_tree, out)?;
                } else {
                    // Root commit: diff against the empty tree by walking the
                    // target tree as all-Added entries.
                    diff_against_empty(repo, target_tree, out)?;
                }
            }
            Ok(None)
        }
        ObjectKind::Tag => {
            if depth >= MAX_TAG_DEPTH {
                eprintln!(
                    "rustygit: show: tag chain exceeded depth {MAX_TAG_DEPTH} at {oid}; \
                     refusing to recurse further"
                );
                return Ok(Some(128));
            }
            let (target_oid, _target_kind) = print_tag_and_get_target(repo, &raw.data, out)?;
            writeln!(out)?;
            // Recurse to show the underlying object — git's `show <tag>`
            // always peels through annotated tags regardless of the tagged
            // object's kind.
            show_object(repo, target_oid, out, depth + 1)
        }
        ObjectKind::Tree => {
            let tree = Tree::parse(&raw.data, repo.hash_kind()).map_err(io_err)?;
            writeln!(out, "tree {oid}")?;
            writeln!(out)?;
            for ent in &tree.entries {
                let name = String::from_utf8_lossy(&ent.name);
                let suffix = if ent.mode.is_tree() { "/" } else { "" };
                writeln!(out, "{name}{suffix}")?;
            }
            Ok(None)
        }
        ObjectKind::Blob => {
            out.write_all(&raw.data)?;
            Ok(None)
        }
    }
}

fn print_commit_header<W: Write>(out: &mut W, oid: &ObjectId, c: &Commit) -> io::Result<()> {
    writeln!(out, "commit {oid}")?;
    if c.parents.len() > 1 {
        let merges: Vec<String> = c.parents.iter().map(|p| p.short_hex(7)).collect();
        writeln!(out, "Merge: {}", merges.join(" "))?;
    }
    writeln!(out, "Author: {} <{}>", c.author.name, c.author.email)?;
    writeln!(
        out,
        "Date:   {}",
        crate::cli::log::format_date_for_show(&c.author.when)
    )?;
    writeln!(out)?;
    let s = String::from_utf8_lossy(&c.message);
    let trimmed = s.trim_end_matches('\n');
    for line in trimmed.lines() {
        if line.is_empty() {
            writeln!(out)?;
        } else {
            writeln!(out, "    {line}")?;
        }
    }
    Ok(())
}

/// Parse a tag object's body, print the standard `git show <tag>` header
/// (tag/object/type/tagger + blank + message), and return the target oid +
/// kind so the caller can recurse to show the tagged object.
fn print_tag_and_get_target<W: Write>(
    repo: &Repository,
    body: &[u8],
    out: &mut W,
) -> io::Result<(ObjectId, ObjectKind)> {
    // Tag header lines: `object <oid>`, `type <kind>`, `tag <name>`,
    // `tagger <name> <email> <when>`. Then a blank line, then the message.
    let text = std::str::from_utf8(body).map_err(|_| io::Error::other("tag: non-utf8 header"))?;
    let mut lines = text.split('\n');
    let mut object_oid: Option<ObjectId> = None;
    let mut object_kind: Option<ObjectKind> = None;
    let mut tag_name: Option<&str> = None;
    let mut tagger_line: Option<&str> = None;
    for line in lines.by_ref() {
        if line.is_empty() {
            break;
        }
        if let Some(rest) = line.strip_prefix("object ") {
            object_oid = Some(ObjectId::parse_hex(repo.hash_kind(), rest).map_err(io_err)?);
        } else if let Some(rest) = line.strip_prefix("type ") {
            object_kind = Some(match rest {
                "commit" => ObjectKind::Commit,
                "tree" => ObjectKind::Tree,
                "blob" => ObjectKind::Blob,
                "tag" => ObjectKind::Tag,
                other => return Err(io::Error::other(format!("tag: unknown type {other}"))),
            });
        } else if let Some(rest) = line.strip_prefix("tag ") {
            tag_name = Some(rest);
        } else if let Some(rest) = line.strip_prefix("tagger ") {
            tagger_line = Some(rest);
        }
    }
    let object_oid = object_oid.ok_or_else(|| io::Error::other("tag: missing 'object' header"))?;
    let object_kind = object_kind.ok_or_else(|| io::Error::other("tag: missing 'type' header"))?;

    if let Some(name) = tag_name {
        writeln!(out, "tag {name}")?;
    }
    if let Some(t) = tagger_line {
        writeln!(out, "Tagger: {t}")?;
    }
    writeln!(out)?;
    let rest: String = lines.collect::<Vec<_>>().join("\n");
    let trimmed = rest.trim_end_matches('\n');
    for line in trimmed.lines() {
        writeln!(out, "{line}")?;
    }
    Ok((object_oid, object_kind))
}

/// Diff the given tree against the empty tree — every blob shows as a "new
/// file". Used for root commits (no parent).
fn diff_against_empty<W: Write>(
    repo: &Repository,
    tree_oid: ObjectId,
    out: &mut W,
) -> io::Result<()> {
    use crate::diff::{diff_entries, flatten_tree, format};
    let a_entries: Vec<crate::diff::DiffEntry> = Vec::new();
    let b_entries = flatten_tree(repo, &tree_oid).map_err(io_err)?;
    let pairs = diff_entries(&a_entries, &b_entries);
    for pair in &pairs {
        format::format_pair(repo, pair, out)?;
    }
    Ok(())
}

fn io_err<E: std::fmt::Display>(e: E) -> io::Error {
    io::Error::other(format!("{e}"))
}
