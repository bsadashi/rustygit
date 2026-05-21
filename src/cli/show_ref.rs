//! `rustygit show-ref` — list refs.
//!
//! Subset implemented in M2:
//!  - default: show all refs (under `refs/`) with their resolved oid
//!  - `--head`: include `HEAD`
//!  - `--heads`, `--tags`: filter to those subtrees
//!  - bare arg: filter by suffix match (e.g. `master` matches `refs/heads/master`)
//!
//! Out of scope: `-d`/`--dereference` (peel annotated tags), `-s`/`--hash`,
//! `--abbrev`, `--exclude-existing`, `--verify`. We can add these in M2 if
//! needed by callers, but they're not on the M2 critical path.

use std::io::{self, Write};

use clap::Args;

use crate::hash::ObjectId;
use crate::refs::{RefStore, RefTarget, Reference};
use crate::repo::Repository;

#[derive(Debug, Args)]
pub struct ShowRefArgs {
    /// Include the HEAD ref in the listing.
    #[arg(long = "head")]
    pub head: bool,

    /// Show refs under refs/heads/.
    #[arg(long = "heads")]
    pub heads: bool,

    /// Show refs under refs/tags/.
    #[arg(long = "tags")]
    pub tags: bool,

    /// Filter by suffix match.
    #[arg(value_name = "PATTERNS")]
    pub patterns: Vec<String>,
}

pub fn run(args: ShowRefArgs) -> io::Result<i32> {
    let repo = Repository::discover_from_cwd().map_err(io_err)?;
    let mut found = false;

    let stdout = io::stdout();
    let mut out = stdout.lock();

    if args.head {
        if let Some(r) = repo.refs().read(&parse("HEAD")?).map_err(io_err)? {
            if let Some(oid) = direct_oid(repo.refs(), &r).map_err(io_err)? {
                if matches_filter(&args, &r) {
                    writeln!(out, "{oid} HEAD")?;
                    found = true;
                }
            }
        }
    }

    let prefix = if args.heads && !args.tags {
        Some("refs/heads/")
    } else if args.tags && !args.heads {
        Some("refs/tags/")
    } else {
        Some("refs/")
    };

    let mut refs: Vec<Reference> = repo
        .refs()
        .iter(prefix)
        .filter_map(Result::ok)
        .filter(|r| matches_filter(&args, r))
        .collect();
    refs.sort_by(|a, b| a.name.as_str().cmp(b.name.as_str()));

    for r in refs {
        if let Some(oid) = direct_oid(repo.refs(), &r).map_err(io_err)? {
            writeln!(out, "{oid} {}", r.name)?;
            found = true;
        }
    }

    if !found && !args.patterns.is_empty() {
        return Ok(1);
    }
    Ok(0)
}

fn matches_filter(args: &ShowRefArgs, r: &Reference) -> bool {
    if args.patterns.is_empty() {
        return true;
    }
    args.patterns
        .iter()
        .any(|p| r.name.as_str() == p || r.name.as_str().ends_with(&format!("/{p}")))
}

fn direct_oid(
    store: &dyn RefStore,
    r: &Reference,
) -> Result<Option<ObjectId>, crate::refs::RefError> {
    match &r.target {
        RefTarget::Direct(o) => Ok(Some(*o)),
        RefTarget::Symbolic(_) => Ok(RefTarget::resolve(store, &r.name)?.map(|(_, o)| o)),
    }
}

fn parse(name: &str) -> io::Result<crate::refs::FullName> {
    crate::refs::FullName::new(name)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, format!("{e}")))
}

fn io_err<E: std::fmt::Display>(e: E) -> io::Error {
    io::Error::other(format!("{e}"))
}
