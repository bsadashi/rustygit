//! `rustygit cat-file` — print object contents/type/size given an object id.
//!
//! Supports the four primary modes: `-t`, `-s`, `-p`, and `-e`. The `--batch`
//! and `--batch-check` modes (used heavily by GUIs and `git fsck`) arrive in M3
//! when they have something interesting to chew on.

use std::io::{self, Write};

use clap::Args;

use crate::object::ObjectKind;
use crate::odb::PrefixMatch;
use crate::repo::Repository;
use crate::tree::Tree;

#[derive(Debug, Args)]
pub struct CatFileArgs {
    /// Print the object's type.
    #[arg(short = 't', conflicts_with_all = ["size", "pretty", "exists", "batch", "batch_check", "batch_all_objects"])]
    pub type_: bool,

    /// Print the object's uncompressed size in bytes.
    #[arg(short = 's', conflicts_with_all = ["type_", "pretty", "exists", "batch", "batch_check", "batch_all_objects"])]
    pub size: bool,

    /// Pretty-print the object's contents according to its type.
    #[arg(short = 'p', conflicts_with_all = ["type_", "size", "exists", "batch", "batch_check", "batch_all_objects"])]
    pub pretty: bool,

    /// Exit successfully if the object exists; otherwise exit with non-zero.
    #[arg(short = 'e', conflicts_with_all = ["type_", "size", "pretty", "batch", "batch_check", "batch_all_objects"])]
    pub exists: bool,

    /// Stream-mode: read oids on stdin, write
    /// `<oid> <type> <size>\n<content>\n` for each.
    #[arg(long = "batch")]
    pub batch: bool,

    /// Like --batch but write only `<oid> <type> <size>\n` (no body).
    #[arg(long = "batch-check")]
    pub batch_check: bool,

    /// Emit `<oid> <type> <size>` for every object in the odb (paired with --batch-check).
    #[arg(long = "batch-all-objects")]
    pub batch_all_objects: bool,

    #[arg(value_name = "OBJECT")]
    pub object: Option<String>,
}

pub fn run(args: CatFileArgs) -> io::Result<i32> {
    let repo = Repository::discover_from_cwd().map_err(|e| io::Error::other(format!("{e}")))?;

    // Batch modes: stream-process oids from stdin (or all-objects).
    if args.batch || args.batch_check {
        return run_batch(&repo, args.batch_check, args.batch_all_objects);
    }

    let object = match args.object.as_deref() {
        Some(o) => o,
        None => {
            eprintln!("rustygit: cat-file: <object> required (or use --batch/--batch-check)");
            return Ok(129);
        }
    };

    // Try the full revparse pipeline first — this handles ref names (HEAD,
    // refs/heads/...), oid prefixes, and suffix walks (HEAD^, HEAD~3,
    // HEAD^{tree}). Fall back to raw oid parsing for anything else.
    let oid = match crate::revparse::resolve(repo.refs(), repo.odb(), object) {
        Ok(o) => o,
        Err(_) => match repo.odb().resolve_prefix(object).map_err(io_err)? {
            PrefixMatch::Found(o) => o,
            PrefixMatch::None => {
                if let Ok(o) = crate::hash::ObjectId::parse_hex(repo.hash_kind(), object) {
                    o
                } else {
                    eprintln!("rustygit: not a valid object name {}", object);
                    return Ok(128);
                }
            }
            PrefixMatch::Ambiguous(c) => {
                eprintln!(
                    "rustygit: ambiguous object: {} candidates for {}",
                    c.len(),
                    object
                );
                return Ok(128);
            }
        },
    };

    if args.exists {
        return Ok(if repo.odb().contains(&oid).map_err(io_err)? {
            0
        } else {
            1
        });
    }

    if args.type_ || args.size {
        let (kind, size) = repo.odb().read_header(&oid).map_err(io_err)?;
        if args.type_ {
            println!("{kind}");
        } else {
            println!("{size}");
        }
        return Ok(0);
    }

    let obj = repo.odb().read(&oid).map_err(io_err)?;

    if args.pretty {
        return pretty_print(&obj, repo.hash_kind()).map(|_| 0);
    }

    Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        "cat-file: must specify one of -t, -s, -p, -e",
    ))
}

/// `--batch` / `--batch-check` stream-mode entry point.
fn run_batch(repo: &Repository, batch_check: bool, all_objects: bool) -> io::Result<i32> {
    use std::io::BufRead;
    let stdout = io::stdout();
    let mut out = stdout.lock();

    if all_objects {
        // Walk every reachable loose object and every pack idx.
        let objects_dir = repo.gitdir().join("objects");
        if let Ok(entries) = std::fs::read_dir(&objects_dir) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                let s = name.to_string_lossy();
                if s.len() != 2 || !s.chars().all(|c| c.is_ascii_hexdigit()) {
                    continue;
                }
                if let Ok(inner) = std::fs::read_dir(entry.path()) {
                    for f in inner.flatten() {
                        let fname = f.file_name();
                        let fname_str = fname.to_string_lossy();
                        if fname_str.len() != 38
                            || !fname_str.chars().all(|c| c.is_ascii_hexdigit())
                        {
                            continue;
                        }
                        let hex = format!("{s}{fname_str}");
                        if let Ok(oid) = crate::hash::ObjectId::parse_hex(repo.hash_kind(), &hex) {
                            emit_batch(repo, &mut out, &hex, oid, batch_check)?;
                        }
                    }
                }
            }
        }
        return Ok(0);
    }

    let stdin = io::stdin();
    for line in stdin.lock().lines() {
        let line = line?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let oid = match crate::revparse::resolve(repo.refs(), repo.odb(), trimmed) {
            Ok(o) => o,
            Err(_) => {
                writeln!(out, "{trimmed} missing")?;
                continue;
            }
        };
        emit_batch(repo, &mut out, trimmed, oid, batch_check)?;
    }
    Ok(0)
}

fn emit_batch(
    repo: &Repository,
    out: &mut impl Write,
    label: &str,
    oid: crate::hash::ObjectId,
    batch_check: bool,
) -> io::Result<()> {
    let (kind, size) = match repo.odb().read_header(&oid) {
        Ok(h) => h,
        Err(_) => {
            writeln!(out, "{label} missing")?;
            return Ok(());
        }
    };
    if batch_check {
        writeln!(out, "{oid} {kind} {size}")?;
        return Ok(());
    }
    writeln!(out, "{oid} {kind} {size}")?;
    let raw = repo.odb().read(&oid).map_err(io_err)?;
    out.write_all(&raw.data)?;
    out.write_all(b"\n")?;
    Ok(())
}

fn pretty_print(
    obj: &crate::object::RawObject,
    hash_kind: crate::hash::HashKind,
) -> io::Result<()> {
    let mut stdout = io::stdout().lock();
    match obj.kind {
        ObjectKind::Blob | ObjectKind::Commit | ObjectKind::Tag => {
            stdout.write_all(&obj.data)?;
        }
        ObjectKind::Tree => {
            // Pretty-print: `<mode> <type> <oid>\t<name>\n` per entry.
            let tree = Tree::parse(&obj.data, hash_kind)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("{e}")))?;
            for entry in tree.entries {
                let mode = entry.mode.as_octal();
                let mode_padded = if mode.len() == 5 {
                    format!("0{mode}")
                } else {
                    mode.to_string()
                };
                let kind = entry.mode.object_kind();
                writeln!(
                    stdout,
                    "{mode_padded} {kind} {}\t{}",
                    entry.oid,
                    String::from_utf8_lossy(&entry.name)
                )?;
            }
        }
    }
    Ok(())
}

fn io_err<E: std::fmt::Display>(e: E) -> io::Error {
    io::Error::other(format!("{e}"))
}
