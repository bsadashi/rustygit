//! `rustygit fsck` — verify the integrity of the object database.
//!
//! Walks every loose object (and, with `--full`, every packed object),
//! reports broken links, missing objects, dangling objects, and bad hashes.
//!
//! Exit code:
//! - 0  → clean (no missing/broken/bad-hash issues; dangling is informational).
//! - 1  → one or more errors detected.

use std::io;

use clap::Args;

use crate::fsck::{fsck, FsckOpts};
use crate::repo::Repository;

#[derive(Debug, Args)]
pub struct FsckArgs {
    /// Include dangling (unreachable) objects in the report.
    #[arg(long = "dangling", default_value_t = true)]
    pub dangling: bool,

    /// Hide the "dangling" report. Mirrors `git fsck --no-dangling`.
    #[arg(long = "no-dangling", action = clap::ArgAction::SetTrue)]
    pub no_dangling: bool,

    /// Verify all objects, including those in packs (default).
    #[arg(long = "full", default_value_t = true)]
    pub full: bool,

    /// Verify connectivity only; don't re-hash every object.
    #[arg(long = "connectivity-only")]
    pub connectivity_only: bool,
}

pub fn run(args: FsckArgs) -> io::Result<i32> {
    let repo = Repository::discover_from_cwd().map_err(io_err)?;
    let opts = FsckOpts {
        full: args.full,
        connectivity_only: args.connectivity_only,
        dangling: args.dangling && !args.no_dangling,
    };
    let report = match fsck(&repo, &opts) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("rustygit: fsck: {e}");
            return Ok(128);
        }
    };

    // Print git-style summary lines.
    // We don't actually progress-bar; the counter lines match git's quiet
    // form. Suppress entirely if we'd otherwise have nothing to say.
    println!("Checking object directories: 100% (256/256), done.");
    println!(
        "Checking objects: 100% ({n}/{n}), done.",
        n = report.object_count
    );

    // Bad hashes first — they're the worst kind of corruption.
    for oid in &report.bad_hashes {
        // We don't know the kind reliably for a bad-hash object, but git's
        // wording uses "broken link" style; we follow that for a tampered file.
        println!("error: hash mismatch {oid}");
    }

    // Broken links. Format: `broken link from <from-kind> <from-oid> to <to-kind> <to-oid>`.
    // We don't know the target's kind cheaply (it's missing!) so we
    // describe it via the relationship reason.
    for link in &report.broken_links {
        println!(
            "broken link from {} {} to {}",
            link.from_kind, link.from, link.to
        );
    }

    // Missing objects (each unique oid).
    for oid in &report.missing {
        println!("missing {oid}");
    }

    // Dangling (unreachable from any ref).
    if opts.dangling {
        for oid in &report.dangling {
            // We need each object's kind to print git-style. Read it; ignore
            // failures (oid came from a successful enumeration).
            let kind = match repo.odb().read(oid) {
                Ok(obj) => obj.kind.to_string(),
                Err(_) => "object".to_string(),
            };
            println!("dangling {kind} {oid}");
        }
    }

    Ok(if report.has_errors() { 1 } else { 0 })
}

fn io_err<E: std::fmt::Display>(e: E) -> io::Error {
    io::Error::other(format!("{e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Debug, Parser)]
    struct Wrap {
        #[command(flatten)]
        args: FsckArgs,
    }

    #[test]
    fn parses_defaults() {
        let w = Wrap::try_parse_from(["x"]).unwrap();
        assert!(w.args.full);
        assert!(w.args.dangling);
        assert!(!w.args.connectivity_only);
        assert!(!w.args.no_dangling);
    }

    #[test]
    fn parses_connectivity_only() {
        let w = Wrap::try_parse_from(["x", "--connectivity-only"]).unwrap();
        assert!(w.args.connectivity_only);
    }

    #[test]
    fn parses_no_dangling() {
        let w = Wrap::try_parse_from(["x", "--no-dangling"]).unwrap();
        assert!(w.args.no_dangling);
    }
}
