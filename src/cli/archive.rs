//! `rustygit archive` — create a tar archive of a tree-ish.
//!
//! Subset: `--format=tar` (default; zip is deferred), `--prefix=`,
//! output to `<file>` (`-o`) or stdout (default).
//!
//! Tar format: standard ustar headers (512-byte blocks, padded). No
//! compression — pipe through `gzip`/`xz`/etc. externally.

use std::io::{self, Write};

use clap::Args;

use crate::hash::ObjectId;
use crate::object::ObjectKind;
use crate::repo::Repository;
use crate::tree::{FileMode, Tree};

#[derive(Debug, Args)]
pub struct ArchiveArgs {
    /// Archive format. Only `tar` is supported today.
    #[arg(long = "format", default_value = "tar")]
    pub format: String,
    /// Path prefix to prepend to every entry inside the archive.
    #[arg(long = "prefix", default_value = "")]
    pub prefix: String,
    /// Write to this file instead of stdout.
    #[arg(short = 'o', long = "output", value_name = "FILE")]
    pub output: Option<String>,
    /// Tree-ish to archive.
    #[arg(value_name = "TREE-ISH", required = true)]
    pub treeish: String,
}

pub fn run(args: ArchiveArgs) -> io::Result<i32> {
    if args.format != "tar" {
        eprintln!(
            "rustygit: archive: --format={:?} not supported; only `tar` today",
            args.format
        );
        return Ok(128);
    }
    let repo = Repository::discover_from_cwd().map_err(io_err)?;
    let oid = crate::revparse::resolve(repo.refs(), repo.odb(), &args.treeish).map_err(io_err)?;
    let tree_oid = peel_to_tree(&repo, oid)?;

    let mut tar = TarBuilder::new();
    walk_tree(&repo, tree_oid, &args.prefix, &mut tar)?;

    let bytes = tar.finalize();

    if let Some(path) = &args.output {
        std::fs::write(path, &bytes)?;
    } else {
        let stdout = io::stdout();
        let mut out = stdout.lock();
        out.write_all(&bytes)?;
    }
    Ok(0)
}

fn peel_to_tree(repo: &Repository, oid: ObjectId) -> io::Result<ObjectId> {
    let raw = repo.odb().read(&oid).map_err(io_err)?;
    match raw.kind {
        ObjectKind::Tree => Ok(oid),
        ObjectKind::Commit => {
            let commit =
                crate::commit::Commit::parse(&raw.data, repo.hash_kind()).map_err(io_err)?;
            Ok(commit.tree)
        }
        ObjectKind::Tag => {
            let tag = crate::tag::Tag::parse(&raw.data, repo.hash_kind()).map_err(io_err)?;
            peel_to_tree(repo, tag.object)
        }
        _ => Err(io::Error::other(format!("archive: {oid} is not tree-ish"))),
    }
}

fn walk_tree(
    repo: &Repository,
    tree_oid: ObjectId,
    prefix: &str,
    tar: &mut TarBuilder,
) -> io::Result<()> {
    let raw = repo.odb().read(&tree_oid).map_err(io_err)?;
    let tree = Tree::parse(&raw.data, repo.hash_kind()).map_err(io_err)?;
    for entry in &tree.entries {
        let name = std::str::from_utf8(&entry.name)
            .map_err(|_| io::Error::other("archive: non-utf8 path"))?;
        let full_path = if prefix.is_empty() {
            name.to_string()
        } else if prefix.ends_with('/') {
            format!("{prefix}{name}")
        } else {
            format!("{prefix}/{name}")
        };
        match entry.mode {
            FileMode::Tree => {
                let dir_path = format!("{full_path}/");
                tar.add_dir(&dir_path);
                walk_tree(repo, entry.oid, &full_path, tar)?;
            }
            FileMode::Regular | FileMode::Executable => {
                let blob = repo.odb().read(&entry.oid).map_err(io_err)?;
                let mode = if entry.mode == FileMode::Executable {
                    0o755
                } else {
                    0o644
                };
                tar.add_file(&full_path, mode, &blob.data);
            }
            FileMode::Symlink => {
                let blob = repo.odb().read(&entry.oid).map_err(io_err)?;
                let target = std::str::from_utf8(&blob.data)
                    .map_err(|_| io::Error::other("archive: non-utf8 symlink target"))?;
                tar.add_symlink(&full_path, target);
            }
            FileMode::Gitlink => {
                // Submodules are not archived.
            }
        }
    }
    Ok(())
}

/// Minimal ustar tar archive builder.
struct TarBuilder {
    out: Vec<u8>,
}

impl TarBuilder {
    fn new() -> Self {
        Self { out: Vec::new() }
    }

    fn finalize(mut self) -> Vec<u8> {
        // Two empty 512-byte blocks signal end of archive.
        self.out.extend_from_slice(&[0u8; 1024]);
        self.out
    }

    fn add_file(&mut self, path: &str, mode: u32, content: &[u8]) {
        self.write_header(path, mode, content.len() as u64, b'0', "");
        self.out.extend_from_slice(content);
        self.pad_to_block();
    }

    fn add_dir(&mut self, path: &str) {
        // Directories are encoded as type '5' with zero size.
        self.write_header(path, 0o755, 0, b'5', "");
    }

    fn add_symlink(&mut self, path: &str, target: &str) {
        self.write_header(path, 0o777, 0, b'2', target);
    }

    fn pad_to_block(&mut self) {
        let pad = (512 - (self.out.len() % 512)) % 512;
        if pad > 0 {
            self.out.extend_from_slice(&vec![0u8; pad]);
        }
    }

    fn write_header(&mut self, path: &str, mode: u32, size: u64, typeflag: u8, linkname: &str) {
        let mut header = [0u8; 512];
        // name (100), mode (8 octal), uid (8), gid (8), size (12 octal),
        // mtime (12 octal), checksum (8), typeflag (1), linkname (100),
        // magic (6 = "ustar\0"), version (2 = "00"), uname (32), gname
        // (32), devmajor (8), devminor (8), prefix (155).
        let name_bytes = path.as_bytes();
        let (prefix_bytes, name_bytes) = if name_bytes.len() > 100 {
            // Split at the last `/` before position 100.
            let split_at = name_bytes[..100]
                .iter()
                .rposition(|&b| b == b'/')
                .unwrap_or(0);
            (&name_bytes[..split_at], &name_bytes[split_at + 1..])
        } else {
            (&b""[..], name_bytes)
        };
        let max_name = 100.min(name_bytes.len());
        header[..max_name].copy_from_slice(&name_bytes[..max_name]);
        write_octal(&mut header[100..108], mode as u64, 7);
        write_octal(&mut header[108..116], 0, 7); // uid
        write_octal(&mut header[116..124], 0, 7); // gid
        write_octal(&mut header[124..136], size, 11);
        write_octal(&mut header[136..148], 0, 11); // mtime
                                                   // checksum placeholder = 8 spaces
        for b in &mut header[148..156] {
            *b = b' ';
        }
        header[156] = typeflag;
        let link_bytes = linkname.as_bytes();
        let lmax = 100.min(link_bytes.len());
        header[157..157 + lmax].copy_from_slice(&link_bytes[..lmax]);
        header[257..263].copy_from_slice(b"ustar\0");
        header[263..265].copy_from_slice(b"00");
        // prefix
        let pmax = 155.min(prefix_bytes.len());
        header[345..345 + pmax].copy_from_slice(&prefix_bytes[..pmax]);

        // Compute checksum: sum of all bytes treating the checksum field
        // as spaces.
        let chk: u32 = header.iter().map(|&b| b as u32).sum();
        // Replace the checksum field with the actual value (6 octal digits,
        // a NUL, then a space).
        let chk_str = format!("{chk:06o}\0 ");
        header[148..156].copy_from_slice(chk_str.as_bytes());

        self.out.extend_from_slice(&header);
    }
}

fn write_octal(buf: &mut [u8], value: u64, max_digits: usize) {
    let s = format!("{value:0>width$o}", width = max_digits);
    let bytes = s.as_bytes();
    let n = bytes.len().min(max_digits);
    for (i, b) in bytes.iter().take(n).enumerate() {
        buf[i] = *b;
    }
    buf[max_digits] = 0;
}

fn io_err<E: std::fmt::Display>(e: E) -> io::Error {
    io::Error::other(format!("{e}"))
}
