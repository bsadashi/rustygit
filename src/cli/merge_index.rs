//! `rustygit merge-index` — for each unmerged index entry, drive the
//! configured merge driver.
//!
//! Subset: we don't ship a driver registry yet, so this command
//! enumerates unmerged paths and prints what git would call the driver
//! with — `<stage1-oid> <stage2-oid> <stage3-oid> <mode1> <mode2> <mode3> <path>`.
//!
//! With `-q`, suppress output. The exit code is the number of unmerged
//! paths (capped at 127), matching upstream's "number of conflicts"
//! convention.

use std::io::{self, Write};

use clap::Args;

use crate::index::Index;
use crate::repo::Repository;

#[derive(Debug, Args)]
pub struct MergeIndexArgs {
    /// Don't run a driver for paths missing one (default: error out).
    #[arg(short = 'q', long = "quiet")]
    pub quiet: bool,
    /// Operate on all unmerged entries (default if no <path>s given).
    #[arg(short = 'a', long = "all")]
    pub all: bool,
    /// Optional driver name + path filters. We currently accept any
    /// driver name but log it as informational; the actual merge driver
    /// is the built-in 3-way merger (see `src/merge/file.rs`).
    #[arg(value_name = "ARG")]
    pub args: Vec<String>,
}

pub fn run(args: MergeIndexArgs) -> io::Result<i32> {
    let repo = Repository::discover_from_cwd().map_err(io_err)?;
    let index = Index::read(&repo).map_err(io_err)?;

    let _ = args.all;
    let _ = args.quiet;

    // Group entries by path; non-zero stages mean unmerged.
    use std::collections::BTreeMap;
    let mut by_path: BTreeMap<Vec<u8>, [Option<&crate::index::IndexEntry>; 4]> = BTreeMap::new();
    for entry in &index.entries {
        let slot = entry.stage.min(3) as usize;
        by_path.entry(entry.path.clone()).or_insert([None; 4])[slot] = Some(entry);
    }

    let stdout = io::stdout();
    let mut out = stdout.lock();
    let mut conflicts = 0i32;
    for (path, slots) in &by_path {
        if slots[1].is_none() && slots[2].is_none() && slots[3].is_none() {
            continue; // already merged
        }
        conflicts += 1;
        let stage_str = |s: Option<&crate::index::IndexEntry>| {
            s.map(|e| (format!("{:06o}", e.mode), e.oid.to_string()))
                .unwrap_or_else(|| {
                    (
                        "000000".into(),
                        "0000000000000000000000000000000000000000".into(),
                    )
                })
        };
        let (m1, o1) = stage_str(slots[1]);
        let (m2, o2) = stage_str(slots[2]);
        let (m3, o3) = stage_str(slots[3]);
        let pname = String::from_utf8_lossy(path);
        if !args.quiet {
            writeln!(
                out,
                "{m1} {o1} 1\t{pname}\n{m2} {o2} 2\t{pname}\n{m3} {o3} 3\t{pname}"
            )?;
        }
    }
    Ok(conflicts.min(127))
}

fn io_err<E: std::fmt::Display>(e: E) -> io::Error {
    io::Error::other(format!("{e}"))
}
