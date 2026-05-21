//! Gitignore-style pattern matching.
//!
//! This module implements `.gitignore` semantics on top of [`crate::wildmatch`].
//! The data model has three layers:
//!
//! - [`IgnorePattern`]: a single compiled `.gitignore` line (negation,
//!   anchoring, dir-only flag, and the glob pattern itself).
//! - A `Vec<IgnorePattern>`: one parsed `.gitignore` file. Last-matching-pattern
//!   wins within a file (gitignore spec).
//! - [`IgnoreStack`]: the stack of `.gitignore` files seen on the way down the
//!   directory tree. Closer-to-the-file wins between files.
//!
//! Notes / deferrals:
//! - We do NOT yet handle the rule that "a parent directory being excluded
//!   prevents re-including files inside it". That requires the caller to walk
//!   directories and stop descending into excluded ones, which is an outer-loop
//!   concern. See `TODO(M4+)` below.
//! - `core.excludesFile` and `$GIT_DIR/info/exclude` are not loaded here; the
//!   caller is expected to feed their contents via `push_file`.

use crate::wildmatch::{wildmatch, WM_PATHNAME};

/// One compiled gitignore pattern.
///
/// Built from a single line via [`IgnorePattern::parse`]. The compiled form
/// remembers whether the pattern was anchored (had a slash before its end),
/// whether it was a negation (`!`), whether it was directory-only (`/` at the
/// end), and the source directory the pattern was anchored against.
#[derive(Debug, Clone)]
pub struct IgnorePattern {
    /// The glob pattern, with `**/` prepended internally for unanchored
    /// patterns so `wildmatch` with PATHNAME flag does the right thing.
    pattern: Vec<u8>,
    /// True when the pattern matched a directory only (trailing `/`).
    dir_only: bool,
    /// True when the pattern was negated (`!` prefix).
    negated: bool,
    /// Directory the pattern is anchored to, expressed relative to the repo
    /// root. For an anchored pattern, the path under match must lie under
    /// `source_dir`. Empty bytes mean "repo root".
    source_dir: Vec<u8>,
}

/// Outcome of a single-pattern match.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchResult {
    /// Pattern was a negation (`!foo`) and it matched — the file should be
    /// re-included.
    Include,
    /// Pattern matched and was not a negation — the file is ignored.
    Exclude,
}

impl IgnorePattern {
    /// Parse a single `.gitignore` line. Returns `None` for blank or comment
    /// lines (which the caller should skip rather than error on).
    ///
    /// `source_dir` is the directory containing the `.gitignore` file,
    /// expressed relative to the repo root (e.g. `b"src"` or `b""` for root).
    pub fn parse(line: &[u8], source_dir: &[u8]) -> Option<Self> {
        // Strip CR (in case of CRLF line endings) and trailing whitespace
        // (unless escaped). The escape handling here is approximate but
        // matches the common case of `\ ` for trailing space.
        let mut line = strip_trailing_unescaped_whitespace(line);
        if line.ends_with(b"\r") {
            line = &line[..line.len() - 1];
        }
        if line.is_empty() {
            return None;
        }
        if line[0] == b'#' {
            // `\#` was already handled in trim; a `#` in column 0 here is a
            // real comment.
            return None;
        }

        let mut idx = 0usize;
        let mut negated = false;
        if line[idx] == b'!' {
            negated = true;
            idx += 1;
            if idx >= line.len() {
                return None;
            }
        } else if line.starts_with(b"\\!") || line.starts_with(b"\\#") {
            // Escaped first char — drop the leading backslash.
            idx += 1;
        }

        let mut body = line[idx..].to_vec();

        // Trailing `/` => directory-only, then strip the trailing slash for
        // the match itself.
        let dir_only = body.last().copied() == Some(b'/');
        if dir_only {
            body.pop();
        }
        if body.is_empty() {
            return None;
        }

        // A leading `/` anchors to source_dir; we strip it because anchoring
        // is communicated by `is_anchored` below.
        let leading_slash = body[0] == b'/';
        if leading_slash {
            body.remove(0);
        }
        if body.is_empty() {
            return None;
        }

        // Determine anchoring: any `/` in the middle of the pattern (not
        // counting the now-stripped leading or trailing one) means the
        // pattern is anchored to source_dir. Otherwise the pattern matches
        // at any depth — we simulate by prepending `**/`.
        let has_internal_slash = body[..body.len().saturating_sub(0)].contains(&b'/');
        let anchored = leading_slash || has_internal_slash;

        let pattern = if anchored {
            body
        } else {
            let mut p = Vec::with_capacity(body.len() + 3);
            p.extend_from_slice(b"**/");
            p.extend_from_slice(&body);
            p
        };

        Some(IgnorePattern {
            pattern,
            dir_only,
            negated,
            source_dir: source_dir.to_vec(),
        })
    }

    /// Returns whether this pattern was a negation. Useful when a caller wants
    /// to short-circuit the stack walk.
    pub fn is_negation(&self) -> bool {
        self.negated
    }

    /// Match a path against this pattern.
    ///
    /// `path` is the path being tested, relative to the repo root (no
    /// leading slash). `is_dir` toggles the `dir-only` semantic — a pattern
    /// like `temp/` only fires when `is_dir == true`.
    pub fn matches(&self, path: &[u8], is_dir: bool) -> Option<MatchResult> {
        if self.dir_only && !is_dir {
            return None;
        }
        // The path being matched against the pattern is the slice of `path`
        // that lies under `source_dir`. If the file is outside source_dir,
        // the pattern simply doesn't apply.
        let candidate = strip_dir_prefix(path, &self.source_dir)?;
        if wildmatch(&self.pattern, candidate, WM_PATHNAME) {
            Some(if self.negated {
                MatchResult::Include
            } else {
                MatchResult::Exclude
            })
        } else {
            None
        }
    }
}

/// Strip leading directory `prefix` (with `/` separator) from `path`, if it
/// matches. Returns `None` when `path` doesn't lie under `prefix`.
///
/// `prefix` of `b""` always matches and returns `path` unchanged.
fn strip_dir_prefix<'a>(path: &'a [u8], prefix: &[u8]) -> Option<&'a [u8]> {
    if prefix.is_empty() {
        return Some(path);
    }
    if path.len() < prefix.len() {
        return None;
    }
    if &path[..prefix.len()] != prefix {
        return None;
    }
    // Either the path is exactly the prefix, or the next byte must be `/`.
    if path.len() == prefix.len() {
        return Some(&path[prefix.len()..]);
    }
    if path[prefix.len()] != b'/' {
        return None;
    }
    Some(&path[prefix.len() + 1..])
}

/// Strip trailing whitespace from `line`, but preserve any whitespace that was
/// escaped with a backslash. Approximate: we just look for unescaped trailing
/// runs of space/tab.
fn strip_trailing_unescaped_whitespace(line: &[u8]) -> &[u8] {
    let mut end = line.len();
    while end > 0 {
        let c = line[end - 1];
        if c != b' ' && c != b'\t' {
            break;
        }
        // Count preceding backslashes; an odd count means the whitespace is
        // escaped and should be kept.
        let mut bs = 0usize;
        let mut k = end - 1;
        while k > 0 && line[k - 1] == b'\\' {
            bs += 1;
            k -= 1;
        }
        if bs % 2 == 1 {
            break;
        }
        end -= 1;
    }
    &line[..end]
}

/// A stack of compiled `.gitignore` files.
///
/// Layers are pushed in the order they apply, from least specific (root) to
/// most specific (deeper directory). Within a single file, the last matching
/// pattern wins. Across files, the closest-to-the-target file wins, which we
/// implement by walking the stack from top (deepest) down.
#[derive(Debug, Default, Clone)]
pub struct IgnoreStack {
    /// One inner `Vec<IgnorePattern>` per `.gitignore` file. Earlier entries
    /// are higher in the tree (less specific); later entries are deeper.
    pub layers: Vec<Vec<IgnorePattern>>,
}

impl IgnoreStack {
    pub fn empty() -> Self {
        Self { layers: Vec::new() }
    }

    /// Parse a `.gitignore` file and push its compiled patterns onto the stack.
    ///
    /// `file_contents` is the raw bytes of the file; `source_dir` is the
    /// directory containing it (relative to the repo root, no leading slash;
    /// use `b""` for the root `.gitignore`).
    pub fn push_file(&mut self, file_contents: &[u8], source_dir: &[u8]) {
        let mut layer = Vec::new();
        for line in file_contents.split(|&b| b == b'\n') {
            if let Some(p) = IgnorePattern::parse(line, source_dir) {
                layer.push(p);
            }
        }
        self.layers.push(layer);
    }

    /// Pop the top (deepest) layer. Used by walkers that interleave directory
    /// descent with per-directory `.gitignore` files.
    pub fn pop_layer(&mut self) -> Option<Vec<IgnorePattern>> {
        self.layers.pop()
    }

    /// Current number of stacked layers — useful for "push N, then pop N
    /// when leaving this dir" callers.
    pub fn depth(&self) -> usize {
        self.layers.len()
    }

    /// Returns whether `path` is ignored by the current stack.
    ///
    /// Semantics:
    /// - Closer files override farther ones (we walk top-of-stack first).
    /// - Within a file, the last matching pattern wins (we iterate in reverse).
    /// - A negation (`!foo`) matched within the closest layer that has any
    ///   match wins for that file.
    ///
    /// Returns `false` (not ignored) if no layer matches.
    pub fn is_ignored(&self, path: &[u8], is_dir: bool) -> bool {
        // Walk layers from deepest to shallowest. For each, find the LAST
        // pattern that matches (gitignore: last match wins within a file).
        // The first layer with any match decides.
        for layer in self.layers.iter().rev() {
            let mut last: Option<MatchResult> = None;
            for pat in layer {
                if let Some(r) = pat.matches(path, is_dir) {
                    last = Some(r);
                }
            }
            if let Some(r) = last {
                return matches!(r, MatchResult::Exclude);
            }
        }
        // TODO(M4+): handle the "parent dir excluded prevents re-include"
        // rule. Today the caller must avoid descending into excluded dirs.
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(line: &str, dir: &str) -> Option<IgnorePattern> {
        IgnorePattern::parse(line.as_bytes(), dir.as_bytes())
    }

    fn check(pat: &IgnorePattern, path: &str, is_dir: bool) -> Option<MatchResult> {
        pat.matches(path.as_bytes(), is_dir)
    }

    #[test]
    fn skips_blank_and_comment() {
        assert!(parse("", "").is_none());
        assert!(parse("   ", "").is_none());
        assert!(parse("# a comment", "").is_none());
    }

    #[test]
    fn star_log_at_any_depth() {
        let p = parse("*.log", "").unwrap();
        assert_eq!(check(&p, "foo.log", false), Some(MatchResult::Exclude));
        assert_eq!(check(&p, "sub/foo.log", false), Some(MatchResult::Exclude));
        assert_eq!(check(&p, "a/b/c.log", false), Some(MatchResult::Exclude));
        assert_eq!(check(&p, "foo.txt", false), None);
    }

    #[test]
    fn slash_anchored_only_at_top() {
        let p = parse("/build", "").unwrap();
        assert_eq!(check(&p, "build", false), Some(MatchResult::Exclude));
        assert_eq!(check(&p, "build", true), Some(MatchResult::Exclude));
        assert_eq!(check(&p, "sub/build", false), None);
    }

    #[test]
    fn dir_only_pattern() {
        let p = parse("temp/", "").unwrap();
        assert_eq!(check(&p, "temp", false), None);
        assert_eq!(check(&p, "temp", true), Some(MatchResult::Exclude));
        assert_eq!(check(&p, "sub/temp", true), Some(MatchResult::Exclude));
    }

    #[test]
    fn negation_within_file() {
        let mut stack = IgnoreStack::empty();
        stack.push_file(b"*.log\n!important.log\n", b"");
        assert!(stack.is_ignored(b"foo.log", false));
        assert!(!stack.is_ignored(b"important.log", false));
        assert!(stack.is_ignored(b"sub/foo.log", false));
    }

    #[test]
    fn embedded_slash_anchored() {
        // `doc/frotz` matches doc/frotz, NOT a/doc/frotz
        let p = parse("doc/frotz", "").unwrap();
        assert_eq!(check(&p, "doc/frotz", true), Some(MatchResult::Exclude));
        assert_eq!(check(&p, "a/doc/frotz", true), None);
    }

    #[test]
    fn parent_and_child_gitignore_cooperate() {
        let mut stack = IgnoreStack::empty();
        // Parent at repo root: ignore everything ending in .log
        stack.push_file(b"*.log\n", b"");
        // Child in `sub/`: re-include important.log
        stack.push_file(b"!important.log\n", b"sub");
        assert!(stack.is_ignored(b"sub/foo.log", false));
        assert!(!stack.is_ignored(b"sub/important.log", false));
        // Outside sub/, the child layer doesn't apply.
        assert!(stack.is_ignored(b"top.log", false));
    }

    #[test]
    fn comments_and_blank_lines_in_file() {
        let mut stack = IgnoreStack::empty();
        stack.push_file(b"# a comment\n\n*.tmp\n   \n# another\n", b"");
        assert!(stack.is_ignored(b"x.tmp", false));
        assert!(!stack.is_ignored(b"x.txt", false));
    }

    #[test]
    fn child_file_in_subdir_anchors_correctly() {
        // `/foo` inside `sub/.gitignore` matches `sub/foo` only.
        let mut stack = IgnoreStack::empty();
        stack.push_file(b"/foo\n", b"sub");
        assert!(stack.is_ignored(b"sub/foo", false));
        assert!(!stack.is_ignored(b"sub/x/foo", false));
        assert!(!stack.is_ignored(b"foo", false));
    }

    #[test]
    fn double_star_in_pattern() {
        let p = parse("a/**/b", "").unwrap();
        assert_eq!(check(&p, "a/b", true), Some(MatchResult::Exclude));
        assert_eq!(check(&p, "a/x/b", true), Some(MatchResult::Exclude));
        assert_eq!(check(&p, "a/x/y/b", true), Some(MatchResult::Exclude));
        assert_eq!(check(&p, "a/b/x", true), None);
    }

    #[test]
    fn escaped_hash_pattern() {
        // `\#config` should match a file literally named `#config`.
        let p = parse("\\#config", "").unwrap();
        assert_eq!(check(&p, "#config", false), Some(MatchResult::Exclude));
    }
}
