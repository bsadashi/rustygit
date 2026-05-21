//! `rustygit mktag` — plumbing: read a tag body from stdin, validate it
//! against git's well-formedness rules, write it as a `tag` object to the
//! ODB, print the resulting oid to stdout.
//!
//! Validation matches `git mktag --no-strict`: the headers must contain
//! `object`, `type`, `tag`, and `tagger` (in that order, no extras between
//! them), and the message must follow after a blank line. We don't run the
//! `--strict` checks (which additionally require the message to be a clean
//! single paragraph) because the strict form is rarely useful in scripts.

use std::io::{self, Read};

use clap::Args;

use crate::repo::Repository;
use crate::tag::Tag;

#[derive(Debug, Args)]
pub struct MktagArgs {}

pub fn run(_args: MktagArgs) -> io::Result<i32> {
    let repo = Repository::discover_from_cwd().map_err(io_err)?;

    let mut body = Vec::new();
    io::stdin().read_to_end(&mut body)?;

    // Parse to validate.
    let _tag = match Tag::parse(&body, repo.hash_kind()) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("rustygit: mktag: malformed tag: {e}");
            return Ok(128);
        }
    };

    // Write the bytes verbatim — we preserve byte-for-byte input fidelity
    // so the resulting oid matches `git mktag`'s output exactly.
    let raw = crate::object::RawObject::new(crate::object::ObjectKind::Tag, body);
    let oid = repo.odb().write(&raw).map_err(io_err)?;
    println!("{oid}");
    Ok(0)
}

fn io_err<E: std::fmt::Display>(e: E) -> io::Error {
    io::Error::other(format!("{e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser, Debug)]
    struct Wrap {
        #[command(flatten)]
        args: MktagArgs,
    }

    #[test]
    fn parses_no_args() {
        let _w = Wrap::try_parse_from(["test"]).unwrap();
    }
}
