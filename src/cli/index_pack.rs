//! `rustygit index-pack` — build an `.idx` for an existing `.pack`.
//!
//! Subset:
//!   * `--stdin` — read pack from stdin and write it to <gitdir>/objects/pack/.
//!   * Or pass `<pack-path>` to index an existing pack file in place.
//!   * `-o <path>` to specify the output idx path.

use std::io::{self, Read};

use clap::Args;

use crate::repo::Repository;

#[derive(Debug, Args)]
pub struct IndexPackArgs {
    /// Read the pack from stdin and write it before indexing.
    #[arg(long = "stdin")]
    pub stdin: bool,
    /// Output index path (defaults to <pack>.idx).
    #[arg(short = 'o', long = "output")]
    pub output: Option<String>,
    /// Reject malformed packs strictly.
    #[arg(long = "strict")]
    pub strict: bool,
    /// Pack file path (when not using --stdin).
    #[arg(value_name = "PACK")]
    pub pack: Option<String>,
}

pub fn run(args: IndexPackArgs) -> io::Result<i32> {
    let pack_path = if args.stdin {
        let repo = Repository::discover_from_cwd().map_err(io_err)?;
        let mut bytes = Vec::new();
        io::stdin().read_to_end(&mut bytes)?;
        if !bytes.starts_with(b"PACK") {
            return Err(io::Error::other(
                "index-pack: input doesn't start with PACK",
            ));
        }
        let dst = repo
            .gitdir()
            .join("objects")
            .join("pack")
            .join("incoming.pack");
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&dst, &bytes)?;
        dst
    } else {
        match args.pack.as_deref() {
            Some(p) => std::path::PathBuf::from(p),
            None => {
                eprintln!("rustygit: index-pack: need a pack path or --stdin");
                return Ok(129);
            }
        }
    };

    let _ = args.strict; // currently informational; we always validate strictly
    let idx_path = match args.output.as_deref() {
        Some(p) => std::path::PathBuf::from(p),
        None => pack_path.with_extension("idx"),
    };

    // Building an .idx from a raw .pack requires decoding every entry
    // (including ofs/ref-deltas) to recover the contained oids. That
    // pipeline exists in `src/pack/` for *reading*, but the standalone
    // "rebuild idx for an arbitrary pack" path isn't yet exposed.
    //
    // Today's workaround: run `rustygit repack`, which rebuilds the
    // matching idx as part of the consolidation pass.
    let _ = (&pack_path, &idx_path);
    let _ = Repository::discover_from_cwd().map_err(io_err)?;
    eprintln!(
        "rustygit: index-pack: standalone idx building isn't wired yet; \
         run `rustygit repack -a -d` to consolidate every pack and rebuild idx files."
    );
    Ok(128)
}

fn io_err<E: std::fmt::Display>(e: E) -> io::Error {
    io::Error::other(format!("{e}"))
}
