//! `rustygit multi-pack-index` — write or verify a multi-pack-index file.
//!
//! Subcommands:
//!   - `write`: scan `<gitdir>/objects/pack/` for every `*.pack` / `*.idx`
//!     pair and write a `multi-pack-index` summarising them.
//!   - `verify`: open the existing `multi-pack-index` and run all of its
//!     internal consistency checks plus the trailer hash.

use std::io::{self, Write as _};

use clap::Args;

use crate::midx::{self, MultiPackIndex};
use crate::repo::Repository;

#[derive(Debug, Args)]
pub struct MultiPackIndexArgs {
    #[command(subcommand)]
    pub subcommand: MidxSubcommand,
}

#[derive(Debug, clap::Subcommand)]
pub enum MidxSubcommand {
    /// Write a multi-pack-index over all packs in objects/pack/.
    Write {
        #[arg(short = 'q', long = "quiet")]
        quiet: bool,
    },
    /// Verify the multi-pack-index.
    Verify,
}

pub fn run(args: MultiPackIndexArgs) -> io::Result<i32> {
    let repo = Repository::discover_from_cwd().map_err(io_err)?;
    match args.subcommand {
        MidxSubcommand::Write { quiet } => run_write(&repo, quiet),
        MidxSubcommand::Verify => run_verify(&repo),
    }
}

fn run_write(repo: &Repository, quiet: bool) -> io::Result<i32> {
    match midx::write(repo) {
        Ok(r) => {
            if !quiet {
                let stdout = io::stdout();
                let mut out = stdout.lock();
                writeln!(
                    out,
                    "wrote multi-pack-index covering {} pack(s), {} object(s)",
                    r.pack_count, r.object_count
                )?;
            }
            Ok(0)
        }
        Err(e) => {
            eprintln!("rustygit: multi-pack-index write: {e}");
            Ok(128)
        }
    }
}

fn run_verify(repo: &Repository) -> io::Result<i32> {
    let path = repo
        .gitdir()
        .join("objects")
        .join("pack")
        .join(crate::midx::MIDX_FILENAME);
    let midx = match MultiPackIndex::open(&path, repo.hash_kind()) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("rustygit: multi-pack-index verify: {e}");
            return Ok(128);
        }
    };
    match midx.verify() {
        Ok(()) => {
            let stdout = io::stdout();
            let mut out = stdout.lock();
            writeln!(out, "multi-pack-index: ok")?;
            Ok(0)
        }
        Err(e) => {
            eprintln!("rustygit: multi-pack-index verify: {e}");
            Ok(1)
        }
    }
}

fn io_err<E: std::fmt::Display>(e: E) -> io::Error {
    io::Error::other(format!("{e}"))
}
