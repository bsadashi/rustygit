//! NON_GOALS A3 — `[url "<base>"] insteadOf / pushInsteadOf` rewrites.
//!
//! These tests exercise `rustygit::transport::rewrite_url` against
//! synthetic `Config` values to confirm:
//!   1. an empty config is a pass-through (no allocation, no surprises),
//!   2. a single matching `insteadOf` rewrites both fetch + push,
//!   3. longest-prefix match wins among multiple `insteadOf` candidates,
//!   4. `pushInsteadOf` only fires when `for_push = true`,
//!   5. `pushInsteadOf` takes precedence over `insteadOf` for push but not
//!      for fetch.
//!
//! These are integration-test shaped (live in `tests/`) because the
//! whole point of A3 is that the public API in `lib.rs` is stable and
//! callable from outside the crate.

use rustygit::config::Config;
use rustygit::transport::rewrite_url;

#[test]
fn empty_config_passes_url_through_unchanged() {
    let cfg = Config::empty();
    let out = rewrite_url("git@github.com:owner/repo.git", &cfg, false);
    assert_eq!(out, "git@github.com:owner/repo.git");
    // And the same for push — no rewrite means no rewrite.
    let out = rewrite_url("git@github.com:owner/repo.git", &cfg, true);
    assert_eq!(out, "git@github.com:owner/repo.git");
}

#[test]
fn exact_prefix_match_substitutes_base() {
    // The single most-common case: `git@github.com:` -> `https://github.com/`.
    let cfg = Config::parse_str(
        "[url \"https://github.com/\"]\n\
         \tinsteadOf = git@github.com:\n",
    )
    .unwrap();
    let out = rewrite_url("git@github.com:owner/repo.git", &cfg, false);
    assert_eq!(out, "https://github.com/owner/repo.git");
}

#[test]
fn longest_prefix_wins_when_multiple_match() {
    // Two `insteadOf` rules whose patterns are both prefixes of the URL
    // we're rewriting. The MORE specific (longer) one must win — git's
    // rule, and the only sensible one if you've got, say, a global
    // `git@` → `https://example.org/` and a per-host
    // `git@github.com:` → `https://github.com/`.
    let cfg = Config::parse_str(
        "[url \"https://example.org/\"]\n\
         \tinsteadOf = git@\n\
         [url \"https://github.com/\"]\n\
         \tinsteadOf = git@github.com:\n",
    )
    .unwrap();
    let out = rewrite_url("git@github.com:foo/bar.git", &cfg, false);
    assert_eq!(out, "https://github.com/foo/bar.git");
}

#[test]
fn push_insteadof_only_fires_for_push_not_fetch() {
    // Only pushInsteadOf is set. A fetch URL must be unchanged; a push
    // URL gets rewritten.
    let cfg = Config::parse_str(
        "[url \"ssh://git@gitlab.com/\"]\n\
         \tpushInsteadOf = https://gitlab.com/\n",
    )
    .unwrap();
    // Fetch: pass-through (pushInsteadOf doesn't apply to fetch).
    let fetch = rewrite_url("https://gitlab.com/group/repo.git", &cfg, false);
    assert_eq!(fetch, "https://gitlab.com/group/repo.git");
    // Push: rewritten.
    let push = rewrite_url("https://gitlab.com/group/repo.git", &cfg, true);
    assert_eq!(push, "ssh://git@gitlab.com/group/repo.git");
}

#[test]
fn push_insteadof_takes_precedence_over_insteadof_for_push() {
    // Both `insteadOf` and `pushInsteadOf` set, with different `<base>`
    // URLs. Fetch must use `insteadOf`'s base; push must use
    // `pushInsteadOf`'s base.
    let cfg = Config::parse_str(
        "[url \"https://gitlab.com/\"]\n\
         \tinsteadOf = git@gitlab.com:\n\
         [url \"ssh://git@gitlab.com/\"]\n\
         \tpushInsteadOf = git@gitlab.com:\n",
    )
    .unwrap();
    // Fetch → https://
    let fetch = rewrite_url("git@gitlab.com:group/repo.git", &cfg, false);
    assert_eq!(fetch, "https://gitlab.com/group/repo.git");
    // Push → ssh://
    let push = rewrite_url("git@gitlab.com:group/repo.git", &cfg, true);
    assert_eq!(push, "ssh://git@gitlab.com/group/repo.git");
}

#[test]
fn rewrite_is_case_sensitive_on_url_prefix() {
    // The pattern is `git@github.com:` (lowercase). A URL with an
    // upper-cased host must NOT match — git is case-sensitive on the
    // URL prefix (the hostname casing matters because SSH URLs aren't
    // URL-spec hostnames, they're scp-form remotes).
    let cfg = Config::parse_str(
        "[url \"https://github.com/\"]\n\
         \tinsteadOf = git@github.com:\n",
    )
    .unwrap();
    let out = rewrite_url("git@GitHub.com:owner/repo.git", &cfg, false);
    assert_eq!(out, "git@GitHub.com:owner/repo.git");
}

#[test]
fn no_match_returns_borrowed_no_allocation() {
    use std::borrow::Cow;
    let cfg = Config::parse_str(
        "[url \"https://github.com/\"]\n\
         \tinsteadOf = git@github.com:\n",
    )
    .unwrap();
    // A URL that doesn't share the prefix with any pattern.
    let out = rewrite_url("https://example.org/repo.git", &cfg, false);
    assert!(matches!(out, Cow::Borrowed(_)));
    assert_eq!(out, "https://example.org/repo.git");
}
