//! Glob pattern matching, port of git's `wildmatch.c`.
//!
//! This module implements the byte-level glob matcher that powers gitignore,
//! pathspec, and refspec matching. It is intentionally a faithful port of the
//! C reference at `wildmatch.c` (Rich $alz, 1986; Wayne Davison's `**` and
//! per-component-`*` extensions). The algorithm is recursive backtracking with
//! two abort codes that propagate up through `**` boundaries — this mirrors
//! the C and is plenty fast for the patterns and paths we'll see in practice.
//!
//! Public API is a single function `wildmatch(pattern, text, flags) -> bool`.
//! Both `pattern` and `text` are byte slices because git paths are not
//! guaranteed to be UTF-8 on Linux (ADR A0).

/// Case-insensitive matching for ASCII letters.
pub const WM_CASEFOLD: u32 = 1;

/// Pathname mode: `*` does not cross `/`, `?` does not match `/`, and `**`
/// becomes meaningful (matches across `/` boundaries).
pub const WM_PATHNAME: u32 = 2;

/// Bitflags-shaped wrapper for callers that prefer a typed flags value over a
/// raw `u32`. Internally we just shuttle the bits through to `wildmatch`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WildmatchFlags(u32);

impl WildmatchFlags {
    pub const fn new() -> Self {
        Self(0)
    }
    pub const fn casefold(mut self) -> Self {
        self.0 |= WM_CASEFOLD;
        self
    }
    pub const fn pathname(mut self) -> Self {
        self.0 |= WM_PATHNAME;
        self
    }
    pub const fn bits(self) -> u32 {
        self.0
    }
    pub const fn from_bits(bits: u32) -> Self {
        Self(bits)
    }
}

// Internal return values, mirroring the C constants. We keep them as a
// dedicated enum (rather than the C `int` codes) so the recursion is
// type-safe; the public API collapses everything that isn't `Match` to false.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WmResult {
    Match,
    NoMatch,
    /// Bail out completely — the pattern can never match this text.
    AbortAll,
    /// Bail out to the enclosing `**` so it can extend its match across a `/`.
    AbortToStarStar,
}

#[inline]
fn to_lower_ascii(c: u8) -> u8 {
    if c.is_ascii_uppercase() {
        c + 32
    } else {
        c
    }
}

#[inline]
fn fold(c: u8, flags: u32) -> u8 {
    if flags & WM_CASEFOLD != 0 {
        to_lower_ascii(c)
    } else {
        c
    }
}

#[inline]
fn is_glob_special(c: u8) -> bool {
    matches!(c, b'*' | b'?' | b'[' | b'\\')
}

/// Match `pattern` against `text` with the given flag set.
///
/// Returns `true` only when the pattern matches the entire text. With
/// `WM_PATHNAME`, `*` and `?` do not cross `/` and `**` segments may. With
/// `WM_CASEFOLD`, ASCII letters compare case-insensitively (non-ASCII bytes
/// are compared verbatim, matching the C behavior on a default locale).
pub fn wildmatch(pattern: &[u8], text: &[u8], flags: u32) -> bool {
    matches!(dowild(pattern, 0, text, 0, flags), WmResult::Match)
}

/// Recursive matcher. `p_start` is the byte offset into `pattern` where the
/// current match attempt begins; `t_start` is the same for `text`. Working in
/// offsets keeps slice lifetimes simple when we recurse for `**`.
fn dowild(pattern: &[u8], p_start: usize, text: &[u8], t_start: usize, flags: u32) -> WmResult {
    let mut p = p_start;
    let mut t = t_start;
    let pattern_origin = 0usize; // start of the entire pattern, used by `**` boundary check

    while p < pattern.len() {
        let mut p_ch = pattern[p];
        // If text is exhausted but the pattern char isn't `*`, no chance.
        if t >= text.len() && p_ch != b'*' {
            return WmResult::AbortAll;
        }
        // Read the current text char (may be one-past-end when matching `*`).
        let mut t_ch = if t < text.len() { text[t] } else { 0 };
        // Apply casefold to both sides up front; it costs nothing and keeps
        // the rest of the logic readable.
        if flags & WM_CASEFOLD != 0 {
            t_ch = to_lower_ascii(t_ch);
            p_ch = to_lower_ascii(p_ch);
        }

        match p_ch {
            b'\\' => {
                // Literal escape: the next pattern byte matches as-is.
                p += 1;
                if p >= pattern.len() {
                    // Trailing backslash = invalid pattern, never matches.
                    return WmResult::AbortAll;
                }
                let lit = fold(pattern[p], flags);
                if t_ch != lit {
                    return WmResult::NoMatch;
                }
                p += 1;
                t += 1;
            }
            b'?' => {
                if flags & WM_PATHNAME != 0 && t_ch == b'/' {
                    return WmResult::NoMatch;
                }
                p += 1;
                t += 1;
            }
            b'*' => {
                // Look for consecutive `*`s to detect `**`.
                let star_run_start = p;
                p += 1;
                let mut double_star = false;
                if p < pattern.len() && pattern[p] == b'*' {
                    double_star = true;
                    while p < pattern.len() && pattern[p] == b'*' {
                        p += 1;
                    }
                }

                let match_slash;
                if flags & WM_PATHNAME == 0 {
                    // Without WM_PATHNAME, `*` is `**` — slashes are matched.
                    match_slash = true;
                } else if double_star {
                    // `**` is meaningful only at a path-component boundary:
                    // start-of-pattern or after a `/`, AND followed by `/` or
                    // end-of-pattern. Otherwise it degrades to a regular `*`.
                    let prev_is_boundary =
                        star_run_start == pattern_origin || pattern[star_run_start - 1] == b'/';
                    let next_is_boundary = p >= pattern.len() || pattern[p] == b'/';
                    if prev_is_boundary && next_is_boundary {
                        // Special case for `<dir>/**/<rest>`: the `**` may match
                        // zero directories. Try the rest with current text first.
                        if p < pattern.len() && pattern[p] == b'/' {
                            if let WmResult::Match = dowild(pattern, p + 1, text, t, flags) {
                                return WmResult::Match;
                            }
                        }
                        match_slash = true;
                    } else {
                        match_slash = false;
                    }
                } else {
                    match_slash = false;
                }

                if p >= pattern.len() {
                    // Trailing star: `**` matches everything; lone `*` rejects
                    // any remaining slash.
                    if !match_slash && text[t..].contains(&b'/') {
                        return WmResult::AbortToStarStar;
                    }
                    return WmResult::Match;
                } else if !match_slash && pattern[p] == b'/' {
                    // `*/` means: skip up to the next slash in text, then let
                    // the outer loop consume it.
                    let slash_off = match memchr(b'/', &text[t..]) {
                        Some(o) => o,
                        None => return WmResult::AbortAll,
                    };
                    t += slash_off;
                    // Don't advance p; the next loop iteration will consume the
                    // `/` character match against pattern[p].
                    continue;
                }

                // General star: try every possible split.
                loop {
                    if t >= text.len() {
                        break;
                    }
                    // Fast-forward when the next pattern char is a literal.
                    if !is_glob_special(pattern[p]) {
                        let target = fold(pattern[p], flags);
                        while t < text.len() {
                            let mut tc = text[t];
                            if flags & WM_CASEFOLD != 0 {
                                tc = to_lower_ascii(tc);
                            }
                            if !match_slash && tc == b'/' {
                                break;
                            }
                            if tc == target {
                                break;
                            }
                            t += 1;
                        }
                        if t >= text.len() {
                            return if match_slash {
                                WmResult::AbortAll
                            } else {
                                WmResult::AbortToStarStar
                            };
                        }
                        let mut tc_now = text[t];
                        if flags & WM_CASEFOLD != 0 {
                            tc_now = to_lower_ascii(tc_now);
                        }
                        if tc_now != target {
                            // Hit a slash before we found our literal.
                            if !match_slash {
                                return WmResult::AbortToStarStar;
                            } else {
                                return WmResult::AbortAll;
                            }
                        }
                    }
                    // Recurse: try matching the rest of the pattern at position t.
                    match dowild(pattern, p, text, t, flags) {
                        WmResult::NoMatch => {
                            if !match_slash && t < text.len() && text[t] == b'/' {
                                return WmResult::AbortToStarStar;
                            }
                        }
                        WmResult::AbortToStarStar if match_slash => {
                            // Outer `**` can keep going; we caught it.
                        }
                        other => return other,
                    }
                    t += 1;
                }
                return WmResult::AbortAll;
            }
            b'[' => {
                // Character class. Consume up through the matching `]`.
                let class_start = p;
                p += 1;
                if p >= pattern.len() {
                    return WmResult::AbortAll;
                }
                let mut first = pattern[p];
                if first == b'^' {
                    first = b'!';
                }
                let negated = first == b'!';
                if negated {
                    p += 1;
                    if p >= pattern.len() {
                        return WmResult::AbortAll;
                    }
                }
                let mut prev_ch: Option<u8> = None;
                let mut matched = false;
                loop {
                    if p >= pattern.len() {
                        return WmResult::AbortAll;
                    }
                    let mut c = pattern[p];
                    if c == b']' && p != class_start + 1 + negated as usize {
                        // End of class.
                        break;
                    }
                    if c == b'\\' {
                        p += 1;
                        if p >= pattern.len() {
                            return WmResult::AbortAll;
                        }
                        c = pattern[p];
                        let folded = fold(c, flags);
                        if t_ch == folded {
                            matched = true;
                        }
                        prev_ch = Some(folded);
                        p += 1;
                        continue;
                    }
                    if c == b'-'
                        && prev_ch.is_some()
                        && p + 1 < pattern.len()
                        && pattern[p + 1] != b']'
                    {
                        // Range.
                        p += 1;
                        let mut hi = pattern[p];
                        if hi == b'\\' {
                            p += 1;
                            if p >= pattern.len() {
                                return WmResult::AbortAll;
                            }
                            hi = pattern[p];
                        }
                        let lo = prev_ch.unwrap();
                        let hi_f = fold(hi, flags);
                        if t_ch >= lo && t_ch <= hi_f {
                            matched = true;
                        }
                        // Casefold widening: if t_ch is uppercase and the range
                        // covers its lowercase, count it as matched. (The C
                        // version checks the inverse, since p is already folded.)
                        prev_ch = None;
                        p += 1;
                        continue;
                    }
                    let folded = fold(c, flags);
                    if t_ch == folded {
                        matched = true;
                    }
                    prev_ch = Some(folded);
                    p += 1;
                }
                // p currently points at `]`.
                if matched == negated || (flags & WM_PATHNAME != 0 && t_ch == b'/') {
                    return WmResult::NoMatch;
                }
                p += 1;
                t += 1;
            }
            other => {
                // Literal byte (already folded above into p_ch).
                let _ = other;
                if t_ch != p_ch {
                    return WmResult::NoMatch;
                }
                p += 1;
                t += 1;
            }
        }
    }

    // Pattern exhausted; we match iff text is also exhausted.
    if t >= text.len() {
        WmResult::Match
    } else {
        WmResult::NoMatch
    }
}

/// Tiny inline `memchr`. We could pull in the `memchr` crate but keeping the
/// dep-free invariant is worth more than the speedup at our scale.
fn memchr(needle: u8, haystack: &[u8]) -> Option<usize> {
    for (i, &b) in haystack.iter().enumerate() {
        if b == needle {
            return Some(i);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(p: &str, t: &str, flags: u32) -> bool {
        wildmatch(p.as_bytes(), t.as_bytes(), flags)
    }

    // ---------- basic literals ----------

    #[test]
    fn empty_pattern_matches_empty_text() {
        assert!(m("", "", 0));
        assert!(!m("", "x", 0));
        assert!(!m("x", "", 0));
    }

    #[test]
    fn literal_match() {
        assert!(m("foo.rs", "foo.rs", 0));
        assert!(!m("foo.rs", "foo.py", 0));
    }

    // ---------- ? ----------

    #[test]
    fn question_matches_one_char() {
        assert!(m("a?c", "abc", 0));
        assert!(m("a?c", "axc", 0));
        assert!(!m("a?c", "ac", 0));
        assert!(!m("a?c", "abbc", 0));
    }

    #[test]
    fn question_does_not_match_slash_in_pathname() {
        assert!(!m("a?c", "a/c", WM_PATHNAME));
        assert!(m("a?c", "a/c", 0));
    }

    // ---------- * (no PATHNAME) ----------

    #[test]
    fn star_matches_any_chars() {
        assert!(m("*", "abc", 0));
        assert!(m("a*c", "abc", 0));
        assert!(m("a*c", "axxxxc", 0));
        assert!(m("a*c", "ac", 0));
        assert!(!m("a*c", "axb", 0));
    }

    #[test]
    fn star_without_pathname_crosses_slashes() {
        assert!(m("*", "a/b/c", 0));
        assert!(m("a*c", "a/b/c", 0));
    }

    // ---------- * (PATHNAME) ----------

    #[test]
    fn star_with_pathname_does_not_cross_slash() {
        // "*" with PATHNAME should not match strings containing /
        assert!(!m("*", "a/b/c", WM_PATHNAME));
        assert!(m("*", "abc", WM_PATHNAME));
        assert!(!m("foo*bar", "foo/bar", WM_PATHNAME));
        assert!(m("foo*bar", "foozzbar", WM_PATHNAME));
    }

    #[test]
    fn star_slash_consumes_one_directory() {
        // "*/foo" should match exactly one path component before foo.
        assert!(m("*/foo", "bar/foo", WM_PATHNAME));
        assert!(!m("*/foo", "a/b/foo", WM_PATHNAME));
    }

    // ---------- ** ----------

    #[test]
    fn double_star_crosses_slash_only_with_pathname() {
        assert!(m("**", "a/b/c", WM_PATHNAME));
        assert!(m("**", "a/b/c", 0));
    }

    #[test]
    fn leading_double_star_slash() {
        // **/foo matches at any depth (including zero).
        assert!(m("**/foo", "foo", WM_PATHNAME));
        assert!(m("**/foo", "bar/foo", WM_PATHNAME));
        assert!(m("**/foo", "bar/baz/foo", WM_PATHNAME));
        assert!(!m("**/foo", "bar/foo/baz", WM_PATHNAME));
    }

    #[test]
    fn trailing_slash_double_star() {
        assert!(m("foo/**", "foo/bar", WM_PATHNAME));
        assert!(m("foo/**", "foo/bar/baz", WM_PATHNAME));
        assert!(!m("foo/**", "foo", WM_PATHNAME));
        assert!(!m("foo/**", "x/foo/bar", WM_PATHNAME));
    }

    #[test]
    fn middle_double_star() {
        assert!(m("foo/**/bar", "foo/bar", WM_PATHNAME));
        assert!(m("foo/**/bar", "foo/x/bar", WM_PATHNAME));
        assert!(m("foo/**/bar", "foo/x/y/bar", WM_PATHNAME));
        assert!(!m("foo/**/bar", "foo/bar/baz", WM_PATHNAME));
    }

    // ---------- character classes ----------

    #[test]
    fn char_class_simple() {
        assert!(m("[abc]", "a", 0));
        assert!(m("[abc]", "b", 0));
        assert!(!m("[abc]", "d", 0));
    }

    #[test]
    fn char_class_range() {
        assert!(m("[a-z]", "k", 0));
        assert!(!m("[a-z]", "K", 0));
        assert!(m("[A-Z]", "K", 0));
        assert!(m("[0-9]", "5", 0));
    }

    #[test]
    fn char_class_negated() {
        assert!(!m("[!a-z]", "k", 0));
        assert!(m("[!a-z]", "K", 0));
        assert!(m("[^a-z]", "K", 0));
    }

    #[test]
    fn char_class_does_not_match_slash_in_pathname() {
        assert!(!m("[abc/]", "/", WM_PATHNAME));
        assert!(m("[abc/]", "/", 0));
    }

    // ---------- casefold ----------

    #[test]
    fn casefold_makes_case_insensitive() {
        assert!(m("foo", "FOO", WM_CASEFOLD));
        assert!(m("FOO", "foo", WM_CASEFOLD));
        assert!(!m("foo", "FOO", 0));
        assert!(m("[a-z]", "K", WM_CASEFOLD));
    }

    // ---------- escapes ----------

    #[test]
    fn backslash_escapes_special() {
        assert!(m("\\*", "*", 0));
        assert!(!m("\\*", "x", 0));
        assert!(m("\\?", "?", 0));
    }

    // ---------- mixed ----------

    #[test]
    fn deep_glob_combination() {
        // src/**/*.rs
        assert!(m("src/**/*.rs", "src/lib.rs", WM_PATHNAME));
        assert!(m("src/**/*.rs", "src/foo/bar.rs", WM_PATHNAME));
        assert!(m("src/**/*.rs", "src/a/b/c.rs", WM_PATHNAME));
        assert!(!m("src/**/*.rs", "tests/lib.rs", WM_PATHNAME));
        assert!(!m("src/**/*.rs", "src/lib.txt", WM_PATHNAME));
    }
}
