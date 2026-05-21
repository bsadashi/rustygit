//! `rustygit replace` — partial implementation.
//!
//! Status (NON_GOALS.md Batch E): `--list` works; create/delete/edit return
//! 128 with a "not implemented" message.
//!
//! The `replace` mechanism lives in the `refs/replace/<original-oid>`
//! namespace — each ref points at a replacement object for the original.
//! Reading the namespace just means iterating the refs and stripping the
//! prefix; that's cheap and useful (`git replace --list` is the common
//! debugging affordance, e.g. "what replacements does this repo have?").
//!
//! Creating replacements requires more thought — we'd need to validate
//! that source and target objects have the same type, build a graft, and
//! handle `--edit` (spawn `$EDITOR` on a tree/commit dump). Deferred.

use std::io;

use clap::Args;

use crate::refs::{RefTarget, Reference};
use crate::repo::Repository;

#[derive(Debug, Args)]
pub struct ReplaceArgs {
    /// List existing replacement refs.
    #[arg(short = 'l', long = "list")]
    pub list: bool,

    /// Pattern restricting which replacements to list (`*` glob).
    #[arg(value_name = "PATTERN", required = false)]
    pub pattern: Option<String>,

    /// Delete the named replacement(s). Not yet supported.
    #[arg(short = 'd', long = "delete")]
    pub delete: bool,

    /// Edit the replacement. Not yet supported.
    #[arg(short = 'e', long = "edit")]
    pub edit: bool,

    /// Force a replacement that changes the object's type. Not yet supported.
    #[arg(short = 'f', long = "force")]
    pub force: bool,

    /// Graft a commit's parents. Not yet supported.
    #[arg(short = 'g', long = "graft")]
    pub graft: bool,

    /// Trailing OBJECT [REPLACEMENT] positional args.
    #[arg(value_name = "OBJECTS", trailing_var_arg = true)]
    pub objects: Vec<String>,
}

pub fn run(args: ReplaceArgs) -> io::Result<i32> {
    if args.delete {
        return delete_replacements(&args.objects);
    }
    if args.edit {
        eprintln!("rustygit: replace -e/--edit (spawn $EDITOR on the object dump) is deferred.");
        return Ok(128);
    }
    if args.graft {
        return graft(&args.objects);
    }
    if !args.list && args.objects.len() == 2 {
        return create_replacement(&args.objects[0], &args.objects[1], args.force);
    }
    if !args.list && !args.objects.is_empty() {
        eprintln!("rustygit: 'replace <original> <replacement>' takes exactly two args.");
        return Ok(129);
    }

    list_replacements(args.pattern.as_deref())
}

fn create_replacement(original: &str, replacement: &str, force: bool) -> io::Result<i32> {
    let repo = Repository::discover_from_cwd().map_err(io_err)?;
    let orig_oid = crate::revparse::resolve(repo.refs(), repo.odb(), original).map_err(io_err)?;
    let repl_oid =
        crate::revparse::resolve(repo.refs(), repo.odb(), replacement).map_err(io_err)?;
    // Validate that the two objects have the same type unless --force.
    if !force {
        let (orig_kind, _) = repo.odb().read_header(&orig_oid).map_err(io_err)?;
        let (repl_kind, _) = repo.odb().read_header(&repl_oid).map_err(io_err)?;
        if orig_kind != repl_kind {
            eprintln!(
                "rustygit: replace: object type mismatch ({orig_kind} vs {repl_kind}); pass -f to force"
            );
            return Ok(128);
        }
    }
    let ref_name =
        crate::refs::FullName::new(format!("refs/replace/{orig_oid}")).map_err(io_err)?;
    let mut tx = repo.refs().transaction();
    tx.update(
        &ref_name,
        if force {
            crate::refs::ExpectedOldValue::Any
        } else {
            crate::refs::ExpectedOldValue::Missing
        },
        crate::refs::NewValue::Direct(repl_oid),
        crate::refs::ReflogMessage::from(format!("replace: {orig_oid} -> {repl_oid}")),
    )
    .map_err(io_err)?;
    tx.commit().map_err(io_err)?;
    Ok(0)
}

fn delete_replacements(objects: &[String]) -> io::Result<i32> {
    if objects.is_empty() {
        eprintln!("rustygit: replace -d requires <original>...");
        return Ok(129);
    }
    let repo = Repository::discover_from_cwd().map_err(io_err)?;
    let mut any_error = false;
    for arg in objects {
        let oid = match crate::revparse::resolve(repo.refs(), repo.odb(), arg) {
            Ok(o) => o,
            Err(_) => {
                // Maybe the user passed a literal `refs/replace/<oid>` suffix.
                match crate::hash::ObjectId::parse_hex(repo.hash_kind(), arg) {
                    Ok(o) => o,
                    Err(e) => {
                        eprintln!("rustygit: replace: bad object {arg:?}: {e}");
                        any_error = true;
                        continue;
                    }
                }
            }
        };
        let name = crate::refs::FullName::new(format!("refs/replace/{oid}")).map_err(io_err)?;
        let mut tx = repo.refs().transaction();
        if let Err(e) = tx
            .delete(&name, crate::refs::ExpectedOldValue::Any)
            .and_then(|()| tx.commit())
        {
            eprintln!("rustygit: replace -d {arg}: {e}");
            any_error = true;
        }
    }
    Ok(if any_error { 1 } else { 0 })
}

fn graft(args: &[String]) -> io::Result<i32> {
    if args.is_empty() {
        eprintln!("rustygit: replace --graft <commit> [<parent>...] required");
        return Ok(129);
    }
    let repo = Repository::discover_from_cwd().map_err(io_err)?;
    let target_oid = crate::revparse::resolve(repo.refs(), repo.odb(), &args[0]).map_err(io_err)?;
    let mut parent_oids = Vec::new();
    for p in &args[1..] {
        parent_oids.push(crate::revparse::resolve(repo.refs(), repo.odb(), p).map_err(io_err)?);
    }
    // Read the target commit, rewrite parents, write a new commit, and
    // record refs/replace/<original> → <new oid>.
    let raw = repo.odb().read(&target_oid).map_err(io_err)?;
    let mut commit = crate::commit::Commit::parse(&raw.data, repo.hash_kind()).map_err(io_err)?;
    commit.parents = parent_oids;
    let new_raw = commit.to_object();
    let new_oid = repo.odb().write(&new_raw).map_err(io_err)?;
    let name = crate::refs::FullName::new(format!("refs/replace/{target_oid}")).map_err(io_err)?;
    let mut tx = repo.refs().transaction();
    tx.update(
        &name,
        crate::refs::ExpectedOldValue::Any,
        crate::refs::NewValue::Direct(new_oid),
        crate::refs::ReflogMessage::from(format!("replace --graft {target_oid}")),
    )
    .map_err(io_err)?;
    tx.commit().map_err(io_err)?;
    println!("{new_oid}");
    Ok(0)
}

/// Print each `refs/replace/<oid>` ref with the ref-name stripped to its
/// bare original-oid form, matching `git replace --list` output. When
/// `pattern` is `Some`, filter via fnmatch-style glob (we already have
/// `crate::wildmatch::wildmatch` for that).
fn list_replacements(pattern: Option<&str>) -> io::Result<i32> {
    let repo = Repository::discover_from_cwd().map_err(io_err)?;
    let refs = repo.refs();

    let mut found: Vec<String> = Vec::new();
    // Narrow the iteration to the `refs/replace/` namespace via the prefix
    // hint — the loose+packed backends will optimize accordingly.
    for r in refs.iter(Some("refs/replace/")) {
        let r: Reference = r.map_err(io_err)?;
        let name = r.name.as_str();
        let Some(suffix) = name.strip_prefix("refs/replace/") else {
            continue;
        };
        if let Some(pat) = pattern {
            if !matches_glob(pat, suffix) {
                continue;
            }
        }
        // The ref name's suffix is the ORIGINAL object's oid. The ref's
        // target is the replacement oid. `git replace --list` prints just
        // the original; we follow the same convention.
        let _ = r.target; // we don't print the target; checked for symbolic refs only
        if let RefTarget::Symbolic(_) = &r.target {
            // Symbolic replacement refs are technically allowed but
            // pathological; mention them in stderr so the user knows
            // something weird is going on.
            eprintln!("rustygit: refs/replace/{suffix} is symbolic, skipping");
            continue;
        }
        found.push(suffix.to_string());
    }
    found.sort();
    let stdout = io::stdout();
    let mut out = stdout.lock();
    use std::io::Write;
    for name in &found {
        writeln!(out, "{name}")?;
    }
    Ok(0)
}

/// Fnmatch-style match: `*` matches any run, `?` any single char, literal
/// otherwise. Anchored at both ends so `git replace --list deadbeef*` works.
/// We can't use `crate::wildmatch::wildmatch` directly because its API takes
/// `&[u8]`; for the replace-list case the inputs are guaranteed ASCII hex.
fn matches_glob(pat: &str, s: &str) -> bool {
    crate::wildmatch::wildmatch(pat.as_bytes(), s.as_bytes(), 0)
}

fn io_err<E: std::fmt::Display>(e: E) -> io::Error {
    io::Error::other(format!("{e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_glob_handles_star() {
        assert!(matches_glob("dead*", "deadbeef"));
        assert!(matches_glob("*beef", "deadbeef"));
        assert!(matches_glob("d*f", "deadbeef"));
        assert!(!matches_glob("cafe*", "deadbeef"));
    }

    #[test]
    fn matches_glob_handles_question() {
        assert!(matches_glob("dead????", "deadbeef"));
        assert!(!matches_glob("dead???", "deadbeef")); // one too few
    }

    #[test]
    fn matches_glob_literal() {
        assert!(matches_glob("deadbeef", "deadbeef"));
        assert!(!matches_glob("deadbeef", "deadbeee"));
    }
}
