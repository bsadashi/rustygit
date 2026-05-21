//! `rustygit hash-object` — compute the object id of a file or stdin, optionally
//! writing the resulting object into the loose store.
//!
//! Mirrors `git hash-object`'s common flags: `-t <type>`, `-w`, `--stdin`. The
//! deliberately-skipped flags for M1: `--literally`, `--no-filters`,
//! `--path`, `--stdin-paths`, `--batch`. Those arrive when filters/attrs do.

use std::fs;
use std::io::{self, Read};
use std::path::PathBuf;

use clap::Args;

use crate::object::{ObjectKind, RawObject};
use crate::repo::Repository;

#[derive(Debug, Args)]
pub struct HashObjectArgs {
    /// Object type to assume.
    #[arg(short = 't', long = "type", default_value = "blob", value_parser = parse_kind)]
    pub kind: ObjectKind,

    /// Write the object into the object database (otherwise just print the id).
    #[arg(short = 'w', long)]
    pub write: bool,

    /// Read the object from stdin instead of from a file.
    #[arg(long)]
    pub stdin: bool,

    /// Files to hash (or none, if `--stdin`).
    #[arg(value_name = "FILE")]
    pub files: Vec<PathBuf>,
}

fn parse_kind(s: &str) -> Result<ObjectKind, String> {
    ObjectKind::parse(s).map_err(|e| e.to_string())
}

pub fn run(args: HashObjectArgs) -> io::Result<i32> {
    if args.stdin && !args.files.is_empty() {
        eprintln!("rustygit: hash-object: --stdin is incompatible with file arguments");
        return Ok(129);
    }
    if !args.stdin && args.files.is_empty() {
        eprintln!("rustygit: hash-object: nothing to hash (give a file or --stdin)");
        return Ok(129);
    }

    // For `-w` we need an actual repo; without it we can hash without one.
    let repo = if args.write {
        Some(Repository::discover_from_cwd().map_err(|e| io::Error::other(format!("{e}")))?)
    } else {
        None
    };

    let mut sources: Vec<(String, Vec<u8>)> = Vec::new();
    if args.stdin {
        let mut buf = Vec::new();
        io::stdin().read_to_end(&mut buf)?;
        sources.push(("<stdin>".into(), buf));
    } else {
        for f in &args.files {
            let data = fs::read(f)?;
            sources.push((f.display().to_string(), data));
        }
    }

    for (_label, data) in sources {
        let obj = RawObject::new(args.kind, data);
        let oid = if let Some(r) = &repo {
            r.odb()
                .write(&obj)
                .map_err(|e| io::Error::other(format!("hash-object: {e}")))?
        } else {
            obj.oid(crate::hash::HashKind::Sha1)
        };
        println!("{oid}");
    }
    Ok(0)
}
