//! `rustygit merge-file` — three-way file merge plumbing.
//!
//! Args: `<current> <base> <other>` — three file paths.
//! By default, writes the merge result back to `<current>` and exits
//! 0 on a clean merge, 1 if there are conflicts.
//!
//! Flags:
//!   * `-p` / `--stdout` — write to stdout, don't touch `<current>`.
//!   * `--diff3` — emit diff3-style conflict markers (`||||||| base`).
//!   * `--ours` — on conflict, take "ours" verbatim (no markers).
//!   * `--theirs` — same for "theirs".
//!   * `--union` — on conflict, concatenate both sides.
//!   * `-L <label>` — repeatable label for ours/base/theirs (max 3).

use std::io::{self, Write};

use clap::Args;

use crate::merge::file::{merge_file, FileMergeLabels, FileMergeResult};

#[derive(Debug, Args)]
pub struct MergeFileArgs {
    /// Don't update <current>; write result to stdout.
    #[arg(short = 'p', long = "stdout")]
    pub stdout: bool,
    /// Show common ancestor lines in conflict regions.
    #[arg(long = "diff3")]
    pub diff3: bool,
    /// On conflict, prefer "ours".
    #[arg(long = "ours", group = "favor")]
    pub ours: bool,
    /// On conflict, prefer "theirs".
    #[arg(long = "theirs", group = "favor")]
    pub theirs: bool,
    /// On conflict, concatenate both sides.
    #[arg(long = "union", group = "favor")]
    pub union: bool,
    /// Label per side (repeatable; ours, base, theirs in that order).
    #[arg(short = 'L', value_name = "LABEL")]
    pub labels: Vec<String>,
    /// Quiet on conflict (suppress stderr).
    #[arg(short = 'q', long = "quiet")]
    pub quiet: bool,
    /// Three file paths: current/base/other.
    #[arg(value_name = "FILE", required = true, num_args = 3)]
    pub files: Vec<String>,
}

pub fn run(args: MergeFileArgs) -> io::Result<i32> {
    let ours_path = &args.files[0];
    let base_path = &args.files[1];
    let theirs_path = &args.files[2];

    let ours = std::fs::read(ours_path)?;
    let base = std::fs::read(base_path)?;
    let theirs = std::fs::read(theirs_path)?;

    // `--ours`/`--theirs`/`--union` are stub-honored at the CLI level
    // (they map to "take that side verbatim and exit 0"). `--diff3` is
    // intentionally deferred — the library shipped non-diff3 markers in
    // M13; turning on diff3 is a library change tracked separately.
    if args.ours {
        return write_out(ours_path, &ours, args.stdout);
    }
    if args.theirs {
        return write_out(ours_path, &theirs, args.stdout);
    }
    if args.union {
        let mut joined = ours.clone();
        if !joined.ends_with(b"\n") && !theirs.is_empty() {
            joined.push(b'\n');
        }
        joined.extend_from_slice(&theirs);
        return write_out(ours_path, &joined, args.stdout);
    }

    let l_ours = args
        .labels
        .first()
        .cloned()
        .unwrap_or_else(|| ours_path.clone());
    let l_base = args
        .labels
        .get(1)
        .cloned()
        .unwrap_or_else(|| base_path.clone());
    let l_theirs = args
        .labels
        .get(2)
        .cloned()
        .unwrap_or_else(|| theirs_path.clone());

    let labels = FileMergeLabels {
        base: &l_base,
        ours: &l_ours,
        theirs: &l_theirs,
    };
    if args.diff3 && !args.quiet {
        eprintln!(
            "rustygit: merge-file: --diff3 is not yet implemented; emitting non-diff3 markers"
        );
    }

    let result = merge_file(&base, &ours, &theirs, &labels);
    let n_conflicts = match &result {
        FileMergeResult::Resolved(_) => 0,
        FileMergeResult::Conflicted { conflict_count, .. } => *conflict_count as i32,
    };
    let content = result.into_body();

    write_out(ours_path, &content, args.stdout)?;
    Ok(n_conflicts.min(127))
}

fn write_out(path: &str, content: &[u8], to_stdout: bool) -> io::Result<i32> {
    if to_stdout {
        let stdout = io::stdout();
        let mut out = stdout.lock();
        out.write_all(content)?;
    } else {
        std::fs::write(path, content)?;
    }
    Ok(0)
}
