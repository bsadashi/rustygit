//! `rustygit rev-parse` — resolve names/expressions to object ids.
//!
//! Subset implemented in M2:
//!  - `<name>`: resolve to a full oid via DWIM ref search
//!  - `<oid-prefix>`: resolve hex prefix to full oid (>= 4 chars)
//!  - `<name>^N`, `<name>~N`, `<name>^{tree}`: suffix walks
//!  - `--verify`: stricter mode (fail on ambiguity / not found)
//!  - `--abbrev-ref`: when the input is a ref, print the short form instead of oid
//!
//! Multiple revisions may be given; each is printed on its own line.

use std::io;

use clap::Args;

use crate::refs::{FullName, RefTarget};
use crate::repo::Repository;
use crate::revparse::resolve;

#[derive(Debug, Args)]
pub struct RevParseArgs {
    /// Strict resolution: fail if any rev is ambiguous or not found.
    #[arg(long = "verify")]
    pub verify: bool,

    /// If the rev is a ref, print its short name (`main`) instead of the oid.
    #[arg(long = "abbrev-ref")]
    pub abbrev_ref: bool,

    /// Revisions / object names to resolve.
    #[arg(value_name = "REV")]
    pub revs: Vec<String>,
}

pub fn run(args: RevParseArgs) -> io::Result<i32> {
    if args.revs.is_empty() {
        return Ok(0);
    }
    let repo = Repository::discover_from_cwd().map_err(io_err)?;
    let mut had_error = false;
    let stdout = io::stdout();
    let mut out = stdout.lock();
    use std::io::Write as _;

    for rev in &args.revs {
        // --abbrev-ref: try ref lookup first, fall through to normal resolve.
        if args.abbrev_ref {
            if let Ok(name) = FullName::new(rev) {
                if let Some(r) = repo.refs().read(&name).map_err(io_err)? {
                    match &r.target {
                        RefTarget::Symbolic(t) => {
                            writeln!(out, "{}", short_form(t.as_str()))?;
                            continue;
                        }
                        RefTarget::Direct(_) => {
                            writeln!(out, "{}", short_form(name.as_str()))?;
                            continue;
                        }
                    }
                }
            }
        }
        match resolve(repo.refs(), repo.odb(), rev) {
            Ok(oid) => {
                writeln!(out, "{oid}")?;
            }
            Err(e) => {
                if args.verify {
                    eprintln!("rustygit: rev-parse: {e}");
                    return Ok(128);
                }
                eprintln!("rustygit: rev-parse: {e}");
                had_error = true;
            }
        }
    }

    Ok(if had_error { 1 } else { 0 })
}

fn short_form(full: &str) -> String {
    for prefix in ["refs/heads/", "refs/tags/", "refs/remotes/"] {
        if let Some(rest) = full.strip_prefix(prefix) {
            return rest.to_string();
        }
    }
    full.to_string()
}

fn io_err<E: std::fmt::Display>(e: E) -> io::Error {
    io::Error::other(format!("{e}"))
}
