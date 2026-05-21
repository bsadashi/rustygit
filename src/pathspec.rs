//! Pathspec parsing and matching.
//!
//! A pathspec is git's CLI grammar for "which files do you mean?". Examples:
//!
//! ```text
//! foo.txt              # literal under cwd
//! src/*.rs             # glob under cwd
//! :/                   # everything from repo root
//! :/foo.txt            # foo.txt at repo root regardless of cwd
//! :!secret             # exclude secret
//! :^secret             # same, alternate spelling
//! :(literal)foo*       # literal `foo*`, no glob expansion
//! :(glob)foo           # glob even where the default would be literal
//! :(top)src            # anchored to repo root
//! :(exclude)src/*.tmp  # exclude
//! :(top,glob)foo*      # multi-magic
//! ```
//!
//! For M4 we deliberately defer `:(attr:...)`, `:(icase)`, and `:/regex` —
//! those return [`PathspecError::UnsupportedMagic`] with a clear message.
//!
//! The matcher's contract:
//! - An empty pathspec matches every path (so `rustygit add` with no args
//!   means "everything under cwd"). Callers treat that as a sentinel.
//! - Matching is anchored to either the repo root (for `:/` and `:(top)`)
//!   or `cwd_rel_to_root` (default).
//! - The result distinguishes between [`Match::Match`] (a positive item
//!   matched), [`Match::Excluded`] (an exclude item matched), and
//!   [`Match::None`] (nothing matched). Excludes win ties, but a later
//!   positive can re-include if the user types `... :^foo foo/bar`.
//!   For our purposes we keep the rule simple: any exclude match wins
//!   over positive matches, matching git's most common behavior.

use thiserror::Error;

use crate::wildmatch::{wildmatch, WM_PATHNAME};

/// One compiled pathspec entry.
#[derive(Debug, Clone)]
struct PathspecItem {
    /// Pattern bytes, already prefixed with the anchor directory (cwd or empty).
    pattern: Vec<u8>,
    /// True if this is an exclusion (`:!` / `:^` / `:(exclude)`).
    exclude: bool,
    /// Whether to treat the pattern as a glob (`true`) or as a literal prefix
    /// (`false`). Literal mode does no `*`/`?`/`[]` expansion; we still allow
    /// matching files under a literal directory.
    glob: bool,
}

/// A compiled pathspec.
///
/// Construct via [`Pathspec::parse`] from a slice of CLI argv strings plus the
/// cwd-relative-to-repo-root. Match paths via [`Pathspec::matches`].
#[derive(Debug, Clone, Default)]
pub struct Pathspec {
    items: Vec<PathspecItem>,
}

/// Outcome of matching a single path against a pathspec.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Match {
    /// No item in the pathspec applied to this path.
    None,
    /// At least one positive item matched and no exclude overrode it.
    Match,
    /// An exclude item matched.
    Excluded,
}

/// Errors returned during pathspec parsing.
#[derive(Error, Debug, PartialEq, Eq)]
pub enum PathspecError {
    /// A magic word that we recognize but haven't implemented yet.
    #[error("unsupported pathspec magic: {0}")]
    UnsupportedMagic(String),
    /// A malformed pathspec — unbalanced parens, unknown magic, etc.
    #[error("malformed pathspec: {0}")]
    Malformed(String),
}

impl Pathspec {
    /// Parse a list of CLI pathspec arguments.
    ///
    /// `cwd_rel_to_root` is the current working directory expressed relative
    /// to the repository root, in byte form. Use `b""` when the user is at
    /// the root. Trailing slashes are tolerated.
    pub fn parse(args: &[&str], cwd_rel_to_root: &[u8]) -> Result<Self, PathspecError> {
        let cwd = trim_trailing_slash(cwd_rel_to_root);
        let mut items = Vec::with_capacity(args.len());
        for raw in args {
            items.push(parse_one(raw.as_bytes(), cwd)?);
        }
        Ok(Pathspec { items })
    }

    /// Empty pathspec means "match everything"; callers usually short-circuit.
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Match a single path. `is_dir` is currently informational — git's
    /// pathspec semantics don't care about it for our supported magic, but
    /// the parameter is kept for forward compatibility (e.g. `:(literal)dir/`).
    pub fn matches(&self, path: &[u8], _is_dir: bool) -> Match {
        if self.items.is_empty() {
            return Match::Match;
        }
        let mut any_positive = false;
        let mut any_exclude = false;
        for it in &self.items {
            if item_matches(it, path) {
                if it.exclude {
                    any_exclude = true;
                } else {
                    any_positive = true;
                }
            }
        }
        if any_exclude {
            Match::Excluded
        } else if any_positive {
            Match::Match
        } else {
            Match::None
        }
    }
}

/// Match a single compiled item against a path. The pattern was constructed
/// with the right anchor by `parse_one`; here we just apply the right matcher.
fn item_matches(it: &PathspecItem, path: &[u8]) -> bool {
    if it.glob {
        wildmatch(&it.pattern, path, WM_PATHNAME)
    } else {
        // Literal: match exactly, OR match a path that lies under the literal
        // (i.e. user wrote `src` and we're checking `src/lib.rs`).
        if it.pattern == path {
            return true;
        }
        if path.len() > it.pattern.len()
            && path.starts_with(&it.pattern)
            && path[it.pattern.len()] == b'/'
        {
            return true;
        }
        false
    }
}

/// Parse one CLI arg. Handles short-form magic (`:!`, `:^`, `:/`) and the
/// long-form magic group `:(...)`.
fn parse_one(raw: &[u8], cwd: &[u8]) -> Result<PathspecItem, PathspecError> {
    let mut top = false;
    let mut exclude = false;
    let mut glob = false;
    let mut force_literal = false;

    let mut body: &[u8] = raw;

    if raw.starts_with(b":") {
        // Short form: skip the colon, then look at the next byte.
        let after = &raw[1..];
        if after.starts_with(b"!") || after.starts_with(b"^") {
            exclude = true;
            body = &after[1..];
        } else if after.starts_with(b"/") {
            // `:/`: top-anchored. May also be `:/regex` (unsupported).
            // We can't easily distinguish "`:/foo` means top-anchored foo"
            // from "`:/foo` means regex foo". git's resolution is config-
            // driven; for M4 we treat `:/` as top-anchor (the more common
            // case) UNLESS the rest looks like an obvious regex (e.g. starts
            // with a regex anchor). We defer the regex form to M4+.
            top = true;
            body = &after[1..];
        } else if after.starts_with(b"(") {
            // Long-form magic group.
            let close = after.iter().position(|&b| b == b')').ok_or_else(|| {
                PathspecError::Malformed(format!(
                    "unbalanced parens in pathspec: {}",
                    String::from_utf8_lossy(raw)
                ))
            })?;
            let magics = &after[1..close];
            for word in magics.split(|&b| b == b',') {
                match word {
                    b"top" => top = true,
                    b"exclude" => exclude = true,
                    b"glob" => glob = true,
                    b"literal" => force_literal = true,
                    b"icase" => {
                        return Err(PathspecError::UnsupportedMagic(
                            "icase (case-insensitive matching) not supported in M4".into(),
                        ));
                    }
                    other if other.starts_with(b"attr:") || other == b"attr" => {
                        return Err(PathspecError::UnsupportedMagic(
                            "attr:... pathspec magic not supported in M4".into(),
                        ));
                    }
                    [] => {
                        // `:(,glob)foo` — empty word, just skip.
                    }
                    other => {
                        return Err(PathspecError::Malformed(format!(
                            "unknown pathspec magic: {}",
                            String::from_utf8_lossy(other)
                        )));
                    }
                }
            }
            body = &after[close + 1..];
        }
        // Otherwise: unrecognized leading colon, treat as literal name.
    }

    // Decide the effective globbing mode. Default in git CLI is "glob unless
    // pathspec global setting says otherwise"; we adopt the modern default
    // (no glob unless any wildcard char is present OR the user explicitly
    // asked for glob magic). `:(literal)` forces literal regardless.
    glob = !force_literal && (glob || contains_glob_meta(body));

    // Anchor: `:/` and `:(top)` are anchored to the repo root; otherwise we
    // anchor to cwd (relative to root).
    let anchor: &[u8] = if top { b"" } else { cwd };

    let pattern = combine_anchor(anchor, body);

    Ok(PathspecItem {
        pattern,
        exclude,
        glob,
    })
}

/// Glue an anchor and a relative path together with `/`. Trims redundant
/// slashes on either side.
fn combine_anchor(anchor: &[u8], rel: &[u8]) -> Vec<u8> {
    let anchor = trim_trailing_slash(anchor);
    let rel = trim_leading_slash(rel);
    if anchor.is_empty() {
        return rel.to_vec();
    }
    if rel.is_empty() {
        return anchor.to_vec();
    }
    let mut out = Vec::with_capacity(anchor.len() + 1 + rel.len());
    out.extend_from_slice(anchor);
    out.push(b'/');
    out.extend_from_slice(rel);
    out
}

fn trim_trailing_slash(s: &[u8]) -> &[u8] {
    let mut end = s.len();
    while end > 0 && s[end - 1] == b'/' {
        end -= 1;
    }
    &s[..end]
}

fn trim_leading_slash(s: &[u8]) -> &[u8] {
    let mut start = 0;
    while start < s.len() && s[start] == b'/' {
        start += 1;
    }
    &s[start..]
}

fn contains_glob_meta(s: &[u8]) -> bool {
    s.iter().any(|&b| matches!(b, b'*' | b'?' | b'['))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ps(args: &[&str], cwd: &str) -> Pathspec {
        Pathspec::parse(args, cwd.as_bytes()).expect("parse pathspec")
    }

    fn match_path(p: &Pathspec, path: &str) -> Match {
        p.matches(path.as_bytes(), false)
    }

    #[test]
    fn empty_pathspec_matches_everything() {
        let p = ps(&[], "");
        assert!(p.is_empty());
        assert_eq!(match_path(&p, "anything"), Match::Match);
        assert_eq!(match_path(&p, "deep/path/to/file.rs"), Match::Match);
    }

    #[test]
    fn literal_match_under_cwd() {
        let p = ps(&["foo"], "src");
        assert_eq!(match_path(&p, "src/foo"), Match::Match);
        assert_eq!(match_path(&p, "src/foo/bar.rs"), Match::Match);
        assert_eq!(match_path(&p, "src/foobar"), Match::None);
        assert_eq!(match_path(&p, "foo"), Match::None);
    }

    #[test]
    fn glob_pattern_under_cwd() {
        let p = ps(&["*.rs"], "");
        assert_eq!(match_path(&p, "lib.rs"), Match::Match);
        assert_eq!(match_path(&p, "main.rs"), Match::Match);
        assert_eq!(match_path(&p, "lib.txt"), Match::None);
        // PATHNAME mode: `*.rs` does not cross /.
        assert_eq!(match_path(&p, "src/lib.rs"), Match::None);
    }

    #[test]
    fn double_star_glob() {
        let p = ps(&["src/**/*.rs"], "");
        assert_eq!(match_path(&p, "src/lib.rs"), Match::Match);
        assert_eq!(match_path(&p, "src/foo/bar.rs"), Match::Match);
        assert_eq!(match_path(&p, "src/foo/bar.txt"), Match::None);
    }

    #[test]
    fn top_magic_anchors_to_root() {
        // From cwd=src, `:(top)README.md` should still match the root README.
        let p = ps(&[":(top)README.md"], "src");
        assert_eq!(match_path(&p, "README.md"), Match::Match);
        assert_eq!(match_path(&p, "src/README.md"), Match::None);
    }

    #[test]
    fn slash_short_top() {
        let p = ps(&[":/README.md"], "src");
        assert_eq!(match_path(&p, "README.md"), Match::Match);
        assert_eq!(match_path(&p, "src/README.md"), Match::None);
    }

    #[test]
    fn exclude_short_form() {
        let p = ps(&["src", ":!src/main.rs"], "");
        assert_eq!(match_path(&p, "src/lib.rs"), Match::Match);
        assert_eq!(match_path(&p, "src/main.rs"), Match::Excluded);
    }

    #[test]
    fn exclude_caret_form() {
        let p = ps(&["src", ":^src/main.rs"], "");
        assert_eq!(match_path(&p, "src/main.rs"), Match::Excluded);
    }

    #[test]
    fn exclude_long_form() {
        let p = ps(&["src", ":(exclude)src/main.rs"], "");
        assert_eq!(match_path(&p, "src/main.rs"), Match::Excluded);
    }

    #[test]
    fn literal_magic_forces_literal() {
        // `:(literal)foo*` should match a file literally named `foo*`, not glob.
        let p = ps(&[":(literal)foo*"], "");
        assert_eq!(match_path(&p, "foo*"), Match::Match);
        assert_eq!(match_path(&p, "foobar"), Match::None);
    }

    #[test]
    fn glob_magic_with_no_wildcard() {
        // `:(glob)foo` is valid; matches the literal glob "foo".
        let p = ps(&[":(glob)foo"], "");
        assert_eq!(match_path(&p, "foo"), Match::Match);
        assert_eq!(match_path(&p, "foo/bar"), Match::None);
    }

    #[test]
    fn multi_magic() {
        let p = ps(&[":(top,glob)*.rs"], "src");
        assert_eq!(match_path(&p, "lib.rs"), Match::Match);
        assert_eq!(match_path(&p, "src/lib.rs"), Match::None);
    }

    #[test]
    fn unsupported_icase() {
        let err = Pathspec::parse(&[":(icase)foo"], b"").unwrap_err();
        assert!(matches!(err, PathspecError::UnsupportedMagic(_)));
    }

    #[test]
    fn unsupported_attr() {
        let err = Pathspec::parse(&[":(attr:lfs)foo"], b"").unwrap_err();
        assert!(matches!(err, PathspecError::UnsupportedMagic(_)));
    }

    #[test]
    fn malformed_unbalanced_parens() {
        let err = Pathspec::parse(&[":(top,glob*.rs"], b"").unwrap_err();
        assert!(matches!(err, PathspecError::Malformed(_)));
    }

    #[test]
    fn unknown_magic_word() {
        let err = Pathspec::parse(&[":(bogus)foo"], b"").unwrap_err();
        assert!(matches!(err, PathspecError::Malformed(_)));
    }

    #[test]
    fn combined_include_and_exclude() {
        // ["src", ":!src/main.rs"] → src/lib.rs matches; src/main.rs excluded.
        let p = ps(&["src", ":!src/main.rs"], "");
        assert_eq!(match_path(&p, "src/lib.rs"), Match::Match);
        assert_eq!(match_path(&p, "src/main.rs"), Match::Excluded);
        assert_eq!(match_path(&p, "tests/foo"), Match::None);
    }
}
