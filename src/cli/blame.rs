//! `rustygit blame` — print per-line authorship for a file.
//!
//! Argv shape (subset of git's):
//!
//! ```text
//! rustygit blame [-C|--follow] [-L <a>,<b>] <file>
//! ```
//!
//! Output mirrors `git blame`'s default porcelain-free shape:
//!
//! ```text
//! <short8> (<author> <YYYY-MM-DD HH:MM:SS> <±HHMM> <line-no>) <content>
//! ```

use std::io::{self, Write};

use clap::Args;

use crate::blame::{blame, format_line, BlameOpts};
use crate::config::Config;
use crate::refs::{FullName, RefTarget};
use crate::repo::Repository;

#[derive(Debug, Args)]
pub struct BlameArgs {
    /// Follow file across renames (analogous to `git blame -C`). We can't
    /// expose `-C` short here because rustygit's global `-C <PATH>` for cwd
    /// override claims that letter.
    #[arg(long = "follow")]
    pub follow: bool,

    /// Restrict output to a 1-based inclusive line range, e.g. `-L 10,20`.
    #[arg(short = 'L', value_name = "RANGE")]
    pub range: Option<String>,

    /// Path of the file to blame, relative to the repository root.
    #[arg(value_name = "FILE")]
    pub path: String,
}

pub fn run(args: BlameArgs) -> io::Result<i32> {
    let repo = Repository::discover_from_cwd().map_err(io_err)?;

    // Resolve HEAD.
    let head = FullName::new("HEAD").map_err(io_err)?;
    let head_oid = match RefTarget::resolve(repo.refs(), &head).map_err(io_err)? {
        Some((_, oid)) => oid,
        None => {
            eprintln!("rustygit: blame: HEAD is unborn");
            return Ok(128);
        }
    };

    let line_range = match args.range.as_deref() {
        None => None,
        Some(s) => match parse_range(s) {
            Ok(r) => Some(r),
            Err(e) => {
                eprintln!("rustygit: blame: bad -L value: {e}");
                return Ok(129);
            }
        },
    };

    let opts = BlameOpts {
        follow_renames: args.follow,
        line_range,
    };

    let path_bytes = args.path.as_bytes().to_vec();
    let lines = match blame(&repo, &path_bytes, head_oid, &opts) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("rustygit: blame: {e}");
            return Ok(128);
        }
    };

    let cfg = Config::from_repo_dir(repo.gitdir()).unwrap_or_else(|_| Config::empty());
    let mut out = crate::cli::pager::open(&cfg, false)?;
    let lineno_width = if let Some(last) = lines.last() {
        last.final_lineno.to_string().len()
    } else {
        1
    };
    for line in &lines {
        if out.stopped() {
            break;
        }
        out.write_all(&format_line(line, lineno_width))?;
    }

    Ok(0)
}

fn parse_range(s: &str) -> Result<(u32, u32), String> {
    // Accept "<a>,<b>". (Git also accepts "<a>,+<n>", "/<regex>/", etc.;
    // out of scope for M16.)
    let (a, b) = s
        .split_once(',')
        .ok_or_else(|| format!("expected '<a>,<b>', got {s:?}"))?;
    let a: u32 = a
        .trim()
        .parse()
        .map_err(|_| format!("not a line number: {a:?}"))?;
    let b: u32 = b
        .trim()
        .parse()
        .map_err(|_| format!("not a line number: {b:?}"))?;
    if a == 0 || b == 0 {
        return Err("line numbers are 1-based".into());
    }
    if a > b {
        return Err(format!("start {a} > end {b}"));
    }
    Ok((a, b))
}

fn io_err<E: std::fmt::Display>(e: E) -> io::Error {
    io::Error::other(format!("{e}"))
}

#[cfg(test)]
mod tests {
    use super::parse_range;

    #[test]
    fn parse_range_basic() {
        assert_eq!(parse_range("1,5").unwrap(), (1, 5));
        assert_eq!(parse_range(" 10 , 20 ").unwrap(), (10, 20));
    }

    #[test]
    fn parse_range_rejects_zero() {
        assert!(parse_range("0,5").is_err());
        assert!(parse_range("1,0").is_err());
    }

    #[test]
    fn parse_range_rejects_inverted() {
        assert!(parse_range("10,1").is_err());
    }

    #[test]
    fn parse_range_rejects_missing_comma() {
        assert!(parse_range("10").is_err());
    }
}
