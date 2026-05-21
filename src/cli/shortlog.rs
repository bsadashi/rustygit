//! `rustygit shortlog` — group commits by author and print a summary.
//!
//! Default output:
//! ```text
//! <Author Name> (N):
//!       <subject 1>
//!       <subject 2>
//!       ...
//!
//! <Other Author> (M):
//!       ...
//! ```
//!
//! Flags:
//!   * `-n` / `--numbered` — sort authors by descending commit count.
//!   * `-s` / `--summary` — suppress subject lines (`N\t<Author>`).
//!   * `-e` / `--email` — include `<email>` in author lines.
//!   * `-c <revision>` — start commits to count from. Defaults to HEAD.

use std::collections::HashMap;
use std::io::{self, Write};

use clap::Args;

use crate::commit::Commit;
use crate::hash::ObjectId;
use crate::repo::Repository;
use crate::revparse::resolve;

#[derive(Debug, Args)]
pub struct ShortlogArgs {
    /// Sort by descending commit count.
    #[arg(short = 'n', long = "numbered")]
    pub numbered: bool,
    /// Print count-and-author summary only (suppress subjects).
    #[arg(short = 's', long = "summary")]
    pub summary: bool,
    /// Include each author's email in the heading line.
    #[arg(short = 'e', long = "email")]
    pub email: bool,
    /// One or more revisions to start the walk from. Defaults to HEAD.
    #[arg(value_name = "REVISION")]
    pub revisions: Vec<String>,
}

pub fn run(args: ShortlogArgs) -> io::Result<i32> {
    let repo = Repository::discover_from_cwd().map_err(io_err)?;

    let starts: Vec<String> = if args.revisions.is_empty() {
        vec!["HEAD".to_string()]
    } else {
        args.revisions.clone()
    };

    // Resolve every start, then walk the union of reachable commits.
    let mut start_oids: Vec<ObjectId> = Vec::new();
    for rev in &starts {
        match resolve(repo.refs(), repo.odb(), rev) {
            Ok(o) => start_oids.push(o),
            Err(e) => {
                eprintln!("rustygit: shortlog: bad revision {rev:?}: {e}");
                return Ok(128);
            }
        }
    }

    let groups = group_by_author(&repo, &start_oids)?;
    print_groups(&groups, &args)?;
    Ok(0)
}

// `Group` was the original explicit shape; we now use a HashMap directly
// for simplicity. Kept the doc comment here for future readers.
//
// (per-author bucket: key = name or "name <email>", value = subject list)

fn group_by_author(
    repo: &Repository,
    starts: &[ObjectId],
) -> io::Result<HashMap<(String, String), Vec<String>>> {
    let mut seen: std::collections::HashSet<ObjectId> = std::collections::HashSet::new();
    let mut stack: Vec<ObjectId> = starts.to_vec();
    let mut out: HashMap<(String, String), Vec<String>> = HashMap::new();
    while let Some(oid) = stack.pop() {
        if !seen.insert(oid) {
            continue;
        }
        let raw = repo.odb().read(&oid).map_err(io_err)?;
        if raw.kind != crate::object::ObjectKind::Commit {
            continue;
        }
        let commit = Commit::parse(&raw.data, repo.hash_kind()).map_err(io_err)?;
        let subject = first_line(&commit.message);
        let key = (commit.author.name.clone(), commit.author.email.clone());
        out.entry(key).or_default().push(subject);
        for p in &commit.parents {
            stack.push(*p);
        }
    }
    Ok(out)
}

fn print_groups(
    groups: &HashMap<(String, String), Vec<String>>,
    args: &ShortlogArgs,
) -> io::Result<()> {
    let mut entries: Vec<(&(String, String), &Vec<String>)> = groups.iter().collect();
    if args.numbered {
        entries.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then_with(|| a.0 .0.cmp(&b.0 .0)));
    } else {
        entries.sort_by(|a, b| a.0 .0.cmp(&b.0 .0));
    }

    let stdout = io::stdout();
    let mut out = stdout.lock();
    let mut first = true;
    for ((name, email), subjects) in entries {
        let header = if args.email {
            format!("{name} <{email}>")
        } else {
            name.clone()
        };
        if args.summary {
            writeln!(out, "{:>6}\t{}", subjects.len(), header)?;
            continue;
        }
        if !first {
            writeln!(out)?;
        }
        first = false;
        writeln!(out, "{header} ({}):", subjects.len())?;
        for subject in subjects {
            writeln!(out, "      {subject}")?;
        }
    }
    Ok(())
}

fn first_line(message: &[u8]) -> String {
    let nl = message
        .iter()
        .position(|&b| b == b'\n')
        .unwrap_or(message.len());
    String::from_utf8_lossy(&message[..nl]).into_owned()
}

fn io_err<E: std::fmt::Display>(e: E) -> io::Error {
    io::Error::other(format!("{e}"))
}
