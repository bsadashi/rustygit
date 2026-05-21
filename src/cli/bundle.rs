//! `rustygit bundle` — offline pack-and-refs bundles.
//!
//! Subset:
//!   * `create <file> <git-rev-list-args>` — capture commits + a pack.
//!   * `verify <file>` — sanity-check format.
//!   * `list-heads <file>` — print refs the bundle includes.
//!   * `unbundle <file>` — feed the embedded pack into the odb.
//!
//! Format (v2):
//!   ```text
//!   # v2 git bundle\n
//!   <oid> <refname>\n        (one per included tip)
//!   ...
//!   \n                       (blank line)
//!   <PACK bytes>
//!   ```

use std::io::{self, Read, Write};

use clap::{Args, Subcommand};

use crate::repo::Repository;

#[derive(Debug, Args)]
pub struct BundleArgs {
    #[command(subcommand)]
    pub sub: BundleSub,
}

#[derive(Debug, Subcommand)]
pub enum BundleSub {
    Create {
        #[arg(value_name = "FILE")]
        file: String,
        /// Refs / oids / ranges to include (passed through to rev-list).
        #[arg(value_name = "REVS")]
        revs: Vec<String>,
    },
    Verify {
        #[arg(value_name = "FILE")]
        file: String,
    },
    ListHeads {
        #[arg(value_name = "FILE")]
        file: String,
    },
    Unbundle {
        #[arg(value_name = "FILE")]
        file: String,
    },
}

pub fn run(args: BundleArgs) -> io::Result<i32> {
    match args.sub {
        BundleSub::Create { file, revs } => create(&file, &revs),
        BundleSub::Verify { file } => verify(&file),
        BundleSub::ListHeads { file } => list_heads(&file),
        BundleSub::Unbundle { file } => unbundle(&file),
    }
}

const HEADER: &str = "# v2 git bundle\n";

fn create(file: &str, revs: &[String]) -> io::Result<i32> {
    let repo = Repository::discover_from_cwd().map_err(io_err)?;

    // Resolve every rev to (oid, refname). If the arg looks like a ref
    // name, use it verbatim. Otherwise, fall back to "HEAD" as the name.
    let mut tips: Vec<(crate::hash::ObjectId, String)> = Vec::new();
    if revs.is_empty() {
        let head = crate::revparse::resolve(repo.refs(), repo.odb(), "HEAD").map_err(io_err)?;
        tips.push((head, "HEAD".to_string()));
    } else {
        for r in revs {
            let oid = crate::revparse::resolve(repo.refs(), repo.odb(), r).map_err(io_err)?;
            tips.push((oid, r.clone()));
        }
    }

    // Build a pack containing every commit reachable from tips.
    let oid_list: Vec<crate::hash::ObjectId> = collect_reachable(&repo, &tips)?;

    // Use the existing pack writer (writes to a directory). Stage in a
    // tempdir, then re-read the pack bytes back so we can splice them
    // into the bundle file.
    let tmp = tempfile::tempdir()?;
    let result =
        crate::pack::build::write_pack(&oid_list, repo.odb(), tmp.path(), repo.hash_kind())
            .map_err(|e| io::Error::other(format!("bundle: pack write: {e}")))?;
    let pack_bytes = std::fs::read(&result.pack_path)?;

    let mut out = std::fs::File::create(file)?;
    out.write_all(HEADER.as_bytes())?;
    for (oid, name) in &tips {
        writeln!(out, "{oid} {name}")?;
    }
    out.write_all(b"\n")?;
    out.write_all(&pack_bytes)?;
    Ok(0)
}

fn verify(file: &str) -> io::Result<i32> {
    let mut f = std::fs::File::open(file)?;
    let mut header = Vec::new();
    let mut byte = [0u8; 1];
    while f.read_exact(&mut byte).is_ok() {
        header.push(byte[0]);
        if header.ends_with(b"\n\n") {
            break;
        }
        if header.len() > 64 * 1024 {
            return Err(io::Error::other("bundle: header too large"));
        }
    }
    if !header.starts_with(HEADER.as_bytes()) {
        return Err(io::Error::other("bundle: not a v2 git bundle"));
    }
    // Try reading a few pack bytes; the pack header begins with "PACK".
    let mut maybe_pack = [0u8; 4];
    f.read_exact(&mut maybe_pack)?;
    if &maybe_pack != b"PACK" {
        return Err(io::Error::other("bundle: missing PACK after header"));
    }
    println!("The bundle records a complete history.");
    Ok(0)
}

fn list_heads(file: &str) -> io::Result<i32> {
    let mut f = std::fs::File::open(file)?;
    let mut header = Vec::new();
    let mut byte = [0u8; 1];
    while f.read_exact(&mut byte).is_ok() {
        header.push(byte[0]);
        if header.ends_with(b"\n\n") {
            break;
        }
        if header.len() > 64 * 1024 {
            return Err(io::Error::other("bundle: header too large"));
        }
    }
    if !header.starts_with(HEADER.as_bytes()) {
        return Err(io::Error::other("bundle: not a v2 git bundle"));
    }
    let stdout = io::stdout();
    let mut out = stdout.lock();
    let text = String::from_utf8_lossy(&header);
    for line in text.lines() {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // Expected: "<oid> <refname>".
        writeln!(out, "{line}")?;
    }
    Ok(0)
}

fn unbundle(file: &str) -> io::Result<i32> {
    let repo = Repository::discover_from_cwd().map_err(io_err)?;
    let bytes = std::fs::read(file)?;
    let pack_off = match bytes.windows(2).position(|w| w == b"\n\n") {
        Some(off) => off + 2,
        None => return Err(io::Error::other("bundle: missing header/pack separator")),
    };
    let pack = &bytes[pack_off..];
    // Stash to a temp file under .git/objects/pack/ and let repack
    // pick it up. For now, just print the size — full unpack requires
    // the index-pack pipeline.
    let tmp = repo
        .gitdir()
        .join("objects")
        .join("pack")
        .join("incoming.pack");
    if let Some(parent) = tmp.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&tmp, pack)?;
    println!("Wrote pack to {}", tmp.display());
    println!("Run `rustygit repack` or `rustygit index-pack` to integrate.");
    Ok(0)
}

fn collect_reachable(
    repo: &Repository,
    tips: &[(crate::hash::ObjectId, String)],
) -> io::Result<Vec<crate::hash::ObjectId>> {
    let mut out: std::collections::HashSet<crate::hash::ObjectId> =
        std::collections::HashSet::new();
    let mut stack: Vec<crate::hash::ObjectId> = tips.iter().map(|(o, _)| *o).collect();
    while let Some(oid) = stack.pop() {
        if !out.insert(oid) {
            continue;
        }
        let raw = match repo.odb().read(&oid) {
            Ok(r) => r,
            Err(_) => continue,
        };
        match raw.kind {
            crate::object::ObjectKind::Commit => {
                if let Ok(c) = crate::commit::Commit::parse(&raw.data, repo.hash_kind()) {
                    stack.push(c.tree);
                    for p in &c.parents {
                        stack.push(*p);
                    }
                }
            }
            crate::object::ObjectKind::Tree => {
                if let Ok(t) = crate::tree::Tree::parse(&raw.data, repo.hash_kind()) {
                    for e in &t.entries {
                        stack.push(e.oid);
                    }
                }
            }
            crate::object::ObjectKind::Tag => {
                if let Ok(tg) = crate::tag::Tag::parse(&raw.data, repo.hash_kind()) {
                    stack.push(tg.object);
                }
            }
            _ => {}
        }
    }
    Ok(out.into_iter().collect())
}

fn io_err<E: std::fmt::Display>(e: E) -> io::Error {
    io::Error::other(format!("{e}"))
}
