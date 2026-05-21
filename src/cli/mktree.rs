//! `rustygit mktree` — build a tree object from `ls-tree`-format input
//! on stdin, write it to the ODB, print the resulting oid.
//!
//! Input lines: `<mode> SP <type> SP <oid> TAB <name>\n`.
//! `<type>` is one of `blob`, `tree`, or `commit` (for gitlinks).
//! Modes must be one of `100644`, `100755`, `120000`, `040000`, `160000`.
//!
//! With `-z`, lines are NUL-terminated instead of newline-terminated
//! (matches `git ls-tree -z` output).

use std::io::{self, Read};

use clap::Args;

use crate::hash::ObjectId;
use crate::repo::Repository;
use crate::tree::{FileMode, Tree, TreeEntry};

#[derive(Debug, Args)]
pub struct MktreeArgs {
    /// Lines are NUL-terminated instead of LF-terminated.
    #[arg(short = 'z')]
    pub nul: bool,
    /// Treat referenced subtrees that aren't in the odb as still-valid
    /// (we always allow this; matches git's --missing).
    #[arg(long = "missing")]
    pub missing: bool,
    /// Batch mode — repeat until empty line / EOF, print each tree's oid.
    #[arg(long = "batch")]
    pub batch: bool,
}

pub fn run(args: MktreeArgs) -> io::Result<i32> {
    let repo = Repository::discover_from_cwd().map_err(io_err)?;
    let mut buf = Vec::new();
    io::stdin().read_to_end(&mut buf)?;

    if args.batch {
        // In batch mode, split on blank lines (or NUL records pair).
        let separator: &[u8] = if args.nul { b"\0\0" } else { b"\n\n" };
        for chunk in buf
            .split(|&b| b == b'\n')
            .collect::<Vec<_>>()
            .split(|l| l.is_empty())
        {
            let _ = separator;
            if chunk.is_empty() {
                continue;
            }
            let mut text = chunk.join(&b'\n');
            text.push(b'\n');
            let oid = mktree_from_lines(&repo, &text, args.nul)?;
            println!("{oid}");
        }
        return Ok(0);
    }

    let oid = mktree_from_lines(&repo, &buf, args.nul)?;
    println!("{oid}");
    Ok(0)
}

fn mktree_from_lines(repo: &Repository, buf: &[u8], nul: bool) -> io::Result<ObjectId> {
    let sep: u8 = if nul { 0 } else { b'\n' };
    let mut entries: Vec<TreeEntry> = Vec::new();
    for line in buf.split(|&b| b == sep) {
        if line.is_empty() {
            continue;
        }
        let entry = parse_line(line)?;
        entries.push(entry);
    }
    // Trees must be sorted by name byte-wise; user input may not be.
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    // Deduplicate exact dupes — keep the last.
    entries.dedup_by(|a, b| a.name == b.name);

    let tree = Tree::new(entries);
    let raw = tree.to_object();
    repo.odb().write(&raw).map_err(io_err)
}

fn parse_line(line: &[u8]) -> io::Result<TreeEntry> {
    // `<mode> SP <type> SP <oid> TAB <name>`
    let space1 = line
        .iter()
        .position(|&b| b == b' ')
        .ok_or_else(|| io::Error::other("mktree: missing space after mode"))?;
    let after_mode = &line[space1 + 1..];
    let space2 = after_mode
        .iter()
        .position(|&b| b == b' ')
        .ok_or_else(|| io::Error::other("mktree: missing space after type"))?;
    let tab = after_mode[space2 + 1..]
        .iter()
        .position(|&b| b == b'\t')
        .ok_or_else(|| io::Error::other("mktree: missing tab before name"))?;

    let mode_str = std::str::from_utf8(&line[..space1])
        .map_err(|_| io::Error::other("mktree: bad mode utf8"))?;
    let type_str = std::str::from_utf8(&after_mode[..space2])
        .map_err(|_| io::Error::other("mktree: bad type utf8"))?;
    let oid_str = std::str::from_utf8(&after_mode[space2 + 1..space2 + 1 + tab])
        .map_err(|_| io::Error::other("mktree: bad oid utf8"))?;
    let name = after_mode[space2 + 1 + tab + 1..].to_vec();

    let mode = parse_mode(mode_str)?;
    // Validate type matches mode (mostly sanity).
    let expected_type = match mode {
        FileMode::Tree => "tree",
        FileMode::Gitlink => "commit",
        _ => "blob",
    };
    if type_str != expected_type {
        return Err(io::Error::other(format!(
            "mktree: type {type_str} does not match mode {mode_str}"
        )));
    }
    let oid = ObjectId::parse_hex(crate::hash::HashKind::Sha1, oid_str).map_err(io_err)?;
    Ok(TreeEntry { mode, name, oid })
}

fn parse_mode(s: &str) -> io::Result<FileMode> {
    match s {
        "100644" => Ok(FileMode::Regular),
        "100755" => Ok(FileMode::Executable),
        "120000" => Ok(FileMode::Symlink),
        "040000" | "40000" => Ok(FileMode::Tree),
        "160000" => Ok(FileMode::Gitlink),
        other => Err(io::Error::other(format!("mktree: unknown mode {other}"))),
    }
}

fn io_err<E: std::fmt::Display>(e: E) -> io::Error {
    io::Error::other(format!("{e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_blob_line() {
        let line = b"100644 blob deadbeefdeadbeefdeadbeefdeadbeefdeadbeef\thello.txt";
        let entry = parse_line(line).unwrap();
        assert_eq!(entry.mode, FileMode::Regular);
        assert_eq!(entry.name, b"hello.txt");
    }

    #[test]
    fn parses_subtree_line() {
        let line = b"040000 tree deadbeefdeadbeefdeadbeefdeadbeefdeadbeef\tsubdir";
        let entry = parse_line(line).unwrap();
        assert_eq!(entry.mode, FileMode::Tree);
        assert_eq!(entry.name, b"subdir");
    }

    #[test]
    fn rejects_mode_type_mismatch() {
        let line = b"100644 tree deadbeefdeadbeefdeadbeefdeadbeefdeadbeef\tx";
        assert!(parse_line(line).is_err());
    }
}
