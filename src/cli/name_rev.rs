//! `rustygit name-rev` — find a symbolic name for an oid.
//!
//! Algorithm: starting from every tip ref (branches and tags), walk
//! commits towards parents and label them with `<tip-name>~<distance>`.
//! For an input oid, return the shortest such label that reaches it.
//!
//! Output: `<oid> <name>` per input. With `--name-only`, just `<name>`.
//! With `--stdin`, read oids from stdin instead of argv.

use std::collections::HashMap;
use std::io::{self, BufRead, Write};

use clap::Args;

use crate::commit::Commit;
use crate::hash::ObjectId;
use crate::refs::RefTarget;
use crate::repo::Repository;

#[derive(Debug, Args)]
pub struct NameRevArgs {
    /// Only print the name, not `<oid> <name>`.
    #[arg(long = "name-only")]
    pub name_only: bool,
    /// Restrict labels to refs/tags/.
    #[arg(long = "tags")]
    pub tags: bool,
    /// Read oids from stdin (one per line).
    #[arg(long = "stdin")]
    pub stdin: bool,
    /// `undefined` if no name can be found (matches git).
    #[arg(long = "no-undefined")]
    pub no_undefined: bool,
    /// One or more revisions to name.
    #[arg(value_name = "COMMIT")]
    pub commits: Vec<String>,
}

pub fn run(args: NameRevArgs) -> io::Result<i32> {
    let repo = Repository::discover_from_cwd().map_err(io_err)?;
    let label_map = build_label_map(&repo, args.tags)?;

    // Resolve inputs.
    let mut inputs: Vec<ObjectId> = Vec::new();
    let mut hex_inputs: Vec<String> = Vec::new();
    if args.stdin {
        let stdin = io::stdin();
        for line in stdin.lock().lines() {
            let line = line?;
            let trimmed = line.trim().to_string();
            if trimmed.is_empty() {
                continue;
            }
            match crate::revparse::resolve(repo.refs(), repo.odb(), &trimmed) {
                Ok(o) => {
                    inputs.push(o);
                    hex_inputs.push(trimmed);
                }
                Err(e) => {
                    eprintln!("rustygit: name-rev: bad revision {trimmed:?}: {e}");
                    return Ok(128);
                }
            }
        }
    } else {
        for r in &args.commits {
            match crate::revparse::resolve(repo.refs(), repo.odb(), r) {
                Ok(o) => {
                    inputs.push(o);
                    hex_inputs.push(r.clone());
                }
                Err(e) => {
                    eprintln!("rustygit: name-rev: bad revision {r:?}: {e}");
                    return Ok(128);
                }
            }
        }
    }

    let stdout = io::stdout();
    let mut out = stdout.lock();
    for (i, oid) in inputs.iter().enumerate() {
        let name = label_map
            .get(oid)
            .cloned()
            .unwrap_or_else(|| "undefined".to_string());
        if args.no_undefined && name == "undefined" {
            return Ok(1);
        }
        if args.name_only {
            writeln!(out, "{name}")?;
        } else {
            writeln!(out, "{} {name}", hex_inputs[i])?;
        }
    }
    Ok(0)
}

/// Walk from each ref tip, labeling reachable commits with the
/// shortest `<tip-name>~<distance>` label.
fn build_label_map(repo: &Repository, tags_only: bool) -> io::Result<HashMap<ObjectId, String>> {
    let mut labels: HashMap<ObjectId, (String, usize)> = HashMap::new();
    let prefix = if tags_only { Some("refs/tags/") } else { None };
    let mut tips: Vec<(String, ObjectId)> = Vec::new();
    for r in repo.refs().iter(prefix) {
        let r = r.map_err(io_err)?;
        if let RefTarget::Direct(oid) = r.target {
            let name = r.name.as_str();
            let short = name
                .strip_prefix("refs/heads/")
                .or_else(|| name.strip_prefix("refs/tags/"))
                .or_else(|| name.strip_prefix("refs/remotes/"))
                .unwrap_or(name);
            tips.push((short.to_string(), oid));
        }
    }
    // Prefer tags first, then branches, in name order — stable across runs.
    tips.sort();
    for (tip_name, tip_oid) in &tips {
        walk_and_label(repo, &mut labels, tip_name, *tip_oid)?;
    }
    Ok(labels.into_iter().map(|(k, (name, _))| (k, name)).collect())
}

fn walk_and_label(
    repo: &Repository,
    labels: &mut HashMap<ObjectId, (String, usize)>,
    tip_name: &str,
    tip_oid: ObjectId,
) -> io::Result<()> {
    use std::collections::VecDeque;
    let mut q: VecDeque<(ObjectId, usize)> = VecDeque::new();
    q.push_back((tip_oid, 0));
    while let Some((oid, dist)) = q.pop_front() {
        // Existing label with shorter distance wins; otherwise replace.
        if let Some((_, prior)) = labels.get(&oid) {
            if *prior <= dist {
                continue;
            }
        }
        let name = if dist == 0 {
            tip_name.to_string()
        } else {
            format!("{tip_name}~{dist}")
        };
        labels.insert(oid, (name, dist));

        // Continue to parents.
        let raw = match repo.odb().read(&oid) {
            Ok(r) => r,
            Err(_) => continue,
        };
        if raw.kind != crate::object::ObjectKind::Commit {
            continue;
        }
        let commit = match Commit::parse(&raw.data, repo.hash_kind()) {
            Ok(c) => c,
            Err(_) => continue,
        };
        for p in &commit.parents {
            q.push_back((*p, dist + 1));
        }
    }
    Ok(())
}

fn io_err<E: std::fmt::Display>(e: E) -> io::Error {
    io::Error::other(format!("{e}"))
}
