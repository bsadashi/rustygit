//! `rustygit describe` — find the closest tag reachable from a commit
//! and format it as `tag-N-gabc` where N is the number of additional
//! commits beyond the tag and `abc` is the short oid.
//!
//! Flags:
//!   * `--tags`   — consider lightweight tags too (default).
//!   * `--always` — fall back to short oid if no tag is reachable.
//!   * `--abbrev <n>` — abbrev width in oid suffix (default 7).
//!   * `--dirty[=<suffix>]` — append `-dirty` when worktree has changes.

use std::collections::{HashMap, VecDeque};
use std::io;

use clap::Args;

use crate::commit::Commit;
use crate::hash::ObjectId;
use crate::refs::RefTarget;
use crate::repo::Repository;
use crate::revparse::resolve;

#[derive(Debug, Args)]
pub struct DescribeArgs {
    /// Consider lightweight tags (default true; annotated tags are
    /// always considered).
    #[arg(long = "tags")]
    pub tags: bool,
    /// Fall back to a short oid when no tag is reachable.
    #[arg(long = "always")]
    pub always: bool,
    /// Width of the abbreviated oid in the suffix.
    #[arg(long = "abbrev", default_value_t = 7)]
    pub abbrev: usize,
    /// Append `-dirty` (or the given suffix) when the worktree has
    /// uncommitted changes.
    #[arg(long = "dirty", value_name = "SUFFIX", num_args = 0..=1, default_missing_value = "-dirty")]
    pub dirty: Option<String>,
    /// Optional commit-ish; defaults to HEAD.
    #[arg(value_name = "COMMIT")]
    pub commit: Option<String>,
}

pub fn run(args: DescribeArgs) -> io::Result<i32> {
    let repo = Repository::discover_from_cwd().map_err(io_err)?;
    let start_rev = args.commit.as_deref().unwrap_or("HEAD");
    let start_oid = match resolve(repo.refs(), repo.odb(), start_rev) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("rustygit: describe: bad revision {start_rev:?}: {e}");
            return Ok(128);
        }
    };

    // Collect every tag's target oid (peel annotated tags to commits).
    let tag_targets = collect_tag_targets(&repo)?;

    // BFS from start_oid towards parents. The first commit we hit that
    // has a tag wins; track the distance.
    let mut dist: HashMap<ObjectId, usize> = HashMap::new();
    dist.insert(start_oid, 0);
    let mut q: VecDeque<ObjectId> = VecDeque::new();
    q.push_back(start_oid);
    let mut best: Option<(String, usize, ObjectId)> = None; // (tag, dist, target)
    while let Some(oid) = q.pop_front() {
        let d = *dist.get(&oid).unwrap();
        if let Some(name) = tag_targets.get(&oid) {
            // Prefer closer / earlier-discovered tags.
            if best.as_ref().is_none_or(|(_, bd, _)| d < *bd) {
                best = Some((name.clone(), d, oid));
            }
        }
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
            if !dist.contains_key(p) {
                dist.insert(*p, d + 1);
                q.push_back(*p);
            }
        }
    }

    let dirty_suffix = if let Some(suffix) = &args.dirty {
        let report = crate::worktree::status::status(&repo).map_err(io_err)?;
        if report
            .entries
            .iter()
            .any(|e| e.worktree_state != crate::worktree::status::WorktreeState::Untracked)
        {
            suffix.clone()
        } else {
            String::new()
        }
    } else {
        String::new()
    };

    match best {
        Some((tag, d, _target)) => {
            if d == 0 && dirty_suffix.is_empty() {
                println!("{tag}");
            } else {
                let short = start_oid.short_hex(args.abbrev);
                if d == 0 {
                    println!("{tag}{dirty_suffix}");
                } else {
                    println!("{tag}-{d}-g{short}{dirty_suffix}");
                }
            }
            Ok(0)
        }
        None => {
            if args.always {
                println!("{}{dirty_suffix}", start_oid.short_hex(args.abbrev));
                Ok(0)
            } else {
                eprintln!(
                    "rustygit: describe: No tags can describe '{start_rev}'. Pass --always for a fallback."
                );
                Ok(128)
            }
        }
    }
}

fn collect_tag_targets(repo: &Repository) -> io::Result<HashMap<ObjectId, String>> {
    let mut out: HashMap<ObjectId, String> = HashMap::new();
    for r in repo.refs().iter(Some("refs/tags/")) {
        let r = r.map_err(io_err)?;
        let name = r
            .name
            .as_str()
            .strip_prefix("refs/tags/")
            .unwrap_or(r.name.as_str())
            .to_string();
        if let RefTarget::Direct(oid) = r.target {
            // Peel annotated tags to commits.
            let target = peel_to_commit(repo, oid).unwrap_or(oid);
            // Closer-named tag wins on tie via insertion order; we don't
            // currently expose a date-based ranking like git does.
            out.entry(target).or_insert(name);
        }
    }
    Ok(out)
}

fn peel_to_commit(repo: &Repository, oid: ObjectId) -> Option<ObjectId> {
    let mut cur = oid;
    for _ in 0..8 {
        let raw = repo.odb().read(&cur).ok()?;
        match raw.kind {
            crate::object::ObjectKind::Commit => return Some(cur),
            crate::object::ObjectKind::Tag => {
                let tag = crate::tag::Tag::parse(&raw.data, repo.hash_kind()).ok()?;
                cur = tag.object;
            }
            _ => return None,
        }
    }
    None
}

fn io_err<E: std::fmt::Display>(e: E) -> io::Error {
    io::Error::other(format!("{e}"))
}
