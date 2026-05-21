//! `rev-parse` resolution engine.
//!
//! Subset implemented in M2:
//!
//! - Full hex object id (40 / 64 chars)
//! - Hex prefix (>= 4 chars), with ambiguity reporting
//! - `HEAD`, `FETCH_HEAD`, `MERGE_HEAD`, `ORIG_HEAD`
//! - Bare ref names (`main`, `origin/main`, `v1.0.0`) — disambiguated by
//!   the standard search order (DWIM rules in git's `revision.c`):
//!     1. `refs/<name>` if it exists
//!     2. `refs/tags/<name>`
//!     3. `refs/heads/<name>`
//!     4. `refs/remotes/<name>`
//!     5. `refs/remotes/<name>/HEAD`
//! - Suffix walks: `^` (first parent), `^N` (Nth parent), `~N` (Nth ancestor),
//!   composable: `HEAD~3^2`.
//! - `<name>^{tree}` to peel a commit (or commit-ish) to its tree.
//!
//! Out of scope for M2: `@{...}` reflog/upstream syntax, `:/regex`, `<name>:<path>`,
//! `^{<type>}` peel for type other than `tree`, range syntax (`..`, `...`),
//! `--default`, `--abbrev-ref`. These arrive when their callers do.

use thiserror::Error;

use crate::hash::ObjectId;
use crate::object::ObjectKind;
use crate::odb::{ObjectDb, OdbError, PrefixMatch};
use crate::refs::{FullName, RefError, RefStore, RefTarget};

#[derive(Error, Debug)]
pub enum RevParseError {
    #[error("not a valid object name: {0}")]
    NotFound(String),
    #[error("ambiguous: {0}")]
    Ambiguous(String),
    #[error("expected commit-ish for ^N/~N suffix on {0}")]
    NotCommitish(String),
    #[error("malformed expression: {0}")]
    Malformed(String),
    #[error(transparent)]
    Refs(#[from] RefError),
    #[error(transparent)]
    Odb(#[from] OdbError),
    #[error(transparent)]
    Hash(#[from] crate::hash::HashError),
}

/// Expand a commit range expression `A..B` (or `A...B`) into the ordered
/// list of commit oids reachable from B but not from A, newest-first
/// (matching `git rev-list A..B`).
///
/// `A...B` (symmetric difference) is accepted for upstream-parity but
/// for the current callers (revert, cherry-pick) we treat it identically
/// to `A..B` — both endpoints exist, so the "merge base or commits not
/// in the intersection" set is empty by construction in linear-chain
/// usage. If callers later need true symmetric difference we'll split.
///
/// Returns `Ok(None)` when `expr` doesn't contain `..` (caller falls back
/// to single-oid resolution). Errors if either side fails to resolve or
/// doesn't peel to a commit.
pub fn resolve_range(
    refs: &dyn RefStore,
    odb: &ObjectDb,
    expr: &str,
) -> Result<Option<Vec<ObjectId>>, RevParseError> {
    let (a, b) = match parse_range(expr) {
        Some(r) => r,
        None => return Ok(None),
    };
    let a_oid = resolve(refs, odb, a)?;
    let b_oid = resolve(refs, odb, b)?;

    // Collect every ancestor of A (inclusive). Used as the "exclude" set
    // when walking from B.
    let a_ancestors = collect_ancestors(odb, a_oid)?;

    // Walk B's ancestors newest-first, stopping at any commit in
    // a_ancestors. The order matches `git rev-list A..B`'s default
    // (no `--reverse`) — most recent first.
    let mut seen: std::collections::HashSet<ObjectId> = std::collections::HashSet::new();
    let mut out: Vec<ObjectId> = Vec::new();
    let mut stack: Vec<ObjectId> = vec![b_oid];
    while let Some(oid) = stack.pop() {
        if a_ancestors.contains(&oid) {
            continue;
        }
        if !seen.insert(oid) {
            continue;
        }
        out.push(oid);
        // Push parents in reverse so the *first* parent is the next pop —
        // gives us a stable, parent-first traversal close to git's.
        let parents = commit_parents(odb, oid)?;
        for p in parents.into_iter().rev() {
            if !seen.contains(&p) {
                stack.push(p);
            }
        }
    }
    Ok(Some(out))
}

/// Detects `A..B` or `A...B` and returns `(A, B)`. Three dots win when
/// present, otherwise two.
fn parse_range(expr: &str) -> Option<(&str, &str)> {
    if let Some((a, b)) = expr.split_once("...") {
        if !a.is_empty() && !b.is_empty() && !a.contains("..") && !b.contains("..") {
            return Some((a, b));
        }
    }
    if let Some((a, b)) = expr.split_once("..") {
        if !a.is_empty() && !b.is_empty() && !a.contains("..") && !b.contains("..") {
            return Some((a, b));
        }
    }
    None
}

/// Read a commit's parent oids without parsing the rest of the commit.
fn commit_parents(odb: &ObjectDb, oid: ObjectId) -> Result<Vec<ObjectId>, RevParseError> {
    let raw = odb.read(&oid)?;
    if raw.kind != ObjectKind::Commit {
        return Err(RevParseError::NotCommitish(oid.to_string()));
    }
    let commit = crate::commit::Commit::parse(&raw.data, oid.kind())
        .map_err(|e| RevParseError::Malformed(e.to_string()))?;
    Ok(commit.parents)
}

/// Collect every ancestor of `start` (inclusive). Used to compute the
/// exclude set for range walks.
fn collect_ancestors(
    odb: &ObjectDb,
    start: ObjectId,
) -> Result<std::collections::HashSet<ObjectId>, RevParseError> {
    let mut set: std::collections::HashSet<ObjectId> = std::collections::HashSet::new();
    let mut stack = vec![start];
    while let Some(oid) = stack.pop() {
        if !set.insert(oid) {
            continue;
        }
        for p in commit_parents(odb, oid)? {
            if !set.contains(&p) {
                stack.push(p);
            }
        }
    }
    Ok(set)
}

/// Resolve a string to an `ObjectId`. Walks any suffix expressions (`^`, `~`,
/// `^{tree}`) using the object database.
pub fn resolve(refs: &dyn RefStore, odb: &ObjectDb, expr: &str) -> Result<ObjectId, RevParseError> {
    if expr.is_empty() {
        return Err(RevParseError::Malformed("empty expression".into()));
    }
    let (base, suffix) = split_base_and_suffix(expr);
    let mut oid = resolve_base(refs, odb, base)?;
    let mut rest = suffix;
    while !rest.is_empty() {
        let (op, advanced) = parse_one_suffix(rest)?;
        oid = apply_suffix(odb, &oid, op)?;
        rest = advanced;
    }
    Ok(oid)
}

/// Pure ref / oid resolution with no suffix handling. Useful for
/// `update-ref`, `show-ref`, etc.
pub fn resolve_ref_or_oid(
    refs: &dyn RefStore,
    odb: &ObjectDb,
    expr: &str,
) -> Result<ObjectId, RevParseError> {
    resolve_base(refs, odb, expr)
}

fn split_base_and_suffix(expr: &str) -> (&str, &str) {
    // Find the first occurrence of '^' or '~' in the expression. Everything
    // before it is the base; everything from there on is suffix-territory.
    if let Some(idx) = expr.find(['^', '~']) {
        let (base, suffix) = expr.split_at(idx);
        if base.is_empty() {
            // Bare suffix doesn't make sense; treat the whole thing as a ref name
            // (`@^` style is not in M2 scope).
            return (expr, "");
        }
        (base, suffix)
    } else {
        (expr, "")
    }
}

fn resolve_base(
    refs: &dyn RefStore,
    odb: &ObjectDb,
    base: &str,
) -> Result<ObjectId, RevParseError> {
    // 1. Full or partial hex.
    if base.len() >= 4 && base.chars().all(|c| c.is_ascii_hexdigit()) {
        match odb.resolve_prefix(base)? {
            PrefixMatch::Found(oid) => return Ok(oid),
            PrefixMatch::Ambiguous(c) => {
                if !c.is_empty() {
                    return Err(RevParseError::Ambiguous(format!(
                        "{base} ({} candidates)",
                        c.len()
                    )));
                }
            }
            PrefixMatch::None => { /* fall through to ref search */ }
        }
    }

    // 2. DWIM ref lookup.
    let candidates = ["refs/", "refs/tags/", "refs/heads/", "refs/remotes/"];
    // Pseudo-refs are always exact: HEAD, FETCH_HEAD, etc.
    if let Ok(name) = FullName::new(base) {
        if let Some(r) = refs.read(&name)? {
            return resolve_ref(refs, &r);
        }
    }
    for prefix in candidates {
        let candidate = format!("{prefix}{base}");
        if let Ok(name) = FullName::new(&candidate) {
            if let Some(r) = refs.read(&name)? {
                return resolve_ref(refs, &r);
            }
        }
    }
    let candidate = format!("refs/remotes/{base}/HEAD");
    if let Ok(name) = FullName::new(&candidate) {
        if let Some(r) = refs.read(&name)? {
            return resolve_ref(refs, &r);
        }
    }

    Err(RevParseError::NotFound(base.to_string()))
}

fn resolve_ref(refs: &dyn RefStore, r: &crate::refs::Reference) -> Result<ObjectId, RevParseError> {
    match RefTarget::resolve(refs, &r.name)? {
        Some((_, oid)) => Ok(oid),
        None => Err(RevParseError::NotFound(r.name.to_string())),
    }
}

#[derive(Debug, Clone, Copy)]
enum SuffixOp {
    /// `^N` (1-indexed). `^` alone == `^1`. `^0` peels a tag to its commit.
    Parent(u32),
    /// `~N` — N-th first-parent ancestor. `~` alone == `~1`.
    Ancestor(u32),
    /// `^{tree}` — peel to the tree.
    PeelTree,
}

fn parse_one_suffix(s: &str) -> Result<(SuffixOp, &str), RevParseError> {
    let bytes = s.as_bytes();
    match bytes[0] {
        b'^' => {
            if bytes.len() >= 7 && &bytes[1..7] == b"{tree}" {
                return Ok((SuffixOp::PeelTree, &s[7..]));
            }
            // Read optional digits.
            let (n, rest) = take_decimal(&s[1..]);
            let n = n.unwrap_or(1);
            Ok((SuffixOp::Parent(n), rest))
        }
        b'~' => {
            let (n, rest) = take_decimal(&s[1..]);
            let n = n.unwrap_or(1);
            Ok((SuffixOp::Ancestor(n), rest))
        }
        _ => Err(RevParseError::Malformed(format!("unexpected suffix {s:?}"))),
    }
}

fn take_decimal(s: &str) -> (Option<u32>, &str) {
    let mut end = 0;
    for (i, c) in s.char_indices() {
        if c.is_ascii_digit() {
            end = i + c.len_utf8();
        } else {
            break;
        }
    }
    if end == 0 {
        (None, s)
    } else {
        (s[..end].parse().ok(), &s[end..])
    }
}

fn apply_suffix(odb: &ObjectDb, oid: &ObjectId, op: SuffixOp) -> Result<ObjectId, RevParseError> {
    match op {
        SuffixOp::Parent(n) => {
            let commit = read_commit(odb, oid)?;
            if n == 0 {
                return Ok(commit.commit_oid);
            }
            let idx = (n - 1) as usize;
            commit
                .parents
                .get(idx)
                .copied()
                .ok_or_else(|| RevParseError::Malformed(format!("no parent {n} on {oid}")))
        }
        SuffixOp::Ancestor(n) => {
            let mut cur = *oid;
            for _ in 0..n {
                let c = read_commit(odb, &cur)?;
                cur = *c
                    .parents
                    .first()
                    .ok_or_else(|| RevParseError::Malformed(format!("no first parent on {cur}")))?;
            }
            Ok(cur)
        }
        SuffixOp::PeelTree => {
            let commit = read_commit(odb, oid)?;
            Ok(commit.tree)
        }
    }
}

struct Commit {
    commit_oid: ObjectId,
    tree: ObjectId,
    parents: Vec<ObjectId>,
}

fn read_commit(odb: &ObjectDb, oid: &ObjectId) -> Result<Commit, RevParseError> {
    let obj = odb.read(oid)?;
    let kind = obj.kind;
    let body = std::str::from_utf8(&obj.data)
        .map_err(|_| RevParseError::Malformed(format!("non-utf8 commit/tag header at {oid}")))?;
    match kind {
        ObjectKind::Commit => parse_commit_body(*oid, body),
        ObjectKind::Tag => {
            // Peel one level: read the `object <oid>\n` line and recurse.
            for line in body.lines() {
                if let Some(target) = line.strip_prefix("object ") {
                    let next = ObjectId::parse_hex(odb.hash_kind(), target.trim())?;
                    return read_commit(odb, &next);
                }
                if line.is_empty() {
                    break;
                }
            }
            Err(RevParseError::Malformed(format!(
                "tag {oid} missing 'object' line"
            )))
        }
        other => Err(RevParseError::NotCommitish(format!("{oid} is a {other}"))),
    }
}

fn parse_commit_body(commit_oid: ObjectId, body: &str) -> Result<Commit, RevParseError> {
    let mut tree = None;
    let mut parents = Vec::new();
    for line in body.lines() {
        if line.is_empty() {
            break;
        }
        if let Some(rest) = line.strip_prefix("tree ") {
            tree = Some(parse_oid_for(commit_oid, rest)?);
        } else if let Some(rest) = line.strip_prefix("parent ") {
            parents.push(parse_oid_for(commit_oid, rest)?);
        }
    }
    Ok(Commit {
        commit_oid,
        tree: tree.ok_or_else(|| {
            RevParseError::Malformed(format!("commit {commit_oid} missing tree line"))
        })?,
        parents,
    })
}

fn parse_oid_for(commit_oid: ObjectId, hex: &str) -> Result<ObjectId, RevParseError> {
    ObjectId::parse_hex(commit_oid.kind(), hex.trim()).map_err(Into::into)
}
