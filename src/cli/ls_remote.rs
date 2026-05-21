//! `rustygit ls-remote <url>` — print the server's refs without writing
//! anything to disk.
//!
//! Format matches `git ls-remote`: `<oid> TAB <refname>`, one per line. The
//! `--heads` / `--tags` filters limit the namespace; positional `<pattern>`
//! args do fnmatch-style suffix matching against the ref name (we replicate
//! git's "matches the tail" semantics with [`crate::wildmatch`]).
//!
//! No repository discovery — `ls-remote` works in any directory. The only
//! requirement is the URL.

use std::io;

use clap::Args;

use crate::config::Config;
use crate::repo::Repository;
use crate::transport::protocol_v2::{self, CapabilityAdvertisement};
use crate::transport::{connect_upload_pack_with_config, Connection};

#[derive(Debug, Args)]
pub struct LsRemoteArgs {
    /// Limit to refs under `refs/heads/`.
    #[arg(long = "heads")]
    pub heads: bool,

    /// Limit to refs under `refs/tags/`.
    #[arg(long = "tags")]
    pub tags: bool,

    /// Repository URL.
    #[arg(value_name = "REPOSITORY")]
    pub repository: String,

    /// Optional ref-name patterns. If empty, every ref (within the namespace
    /// filter) is shown. Patterns match the ref name with a tail/glob match
    /// matching `git ls-remote`'s heuristics.
    #[arg(value_name = "PATTERN")]
    pub patterns: Vec<String>,
}

pub fn run(args: LsRemoteArgs) -> io::Result<i32> {
    // Load config so `[url "..."] insteadOf = ...` rewrites apply. `ls-remote`
    // can run outside a repo, in which case we read only the global/XDG
    // layers (`Config::from_repo_dir` on a non-repo dir is equivalent —
    // local config is just missing). Failures here demote to "no rewrites":
    // the user's command shouldn't fail because their `~/.gitconfig` is
    // broken, that's what the layered loader already logs about.
    let cfg = match Repository::discover_from_cwd() {
        Ok(repo) => Config::from_repo_dir(repo.commondir()).unwrap_or_default(),
        Err(_) => match std::env::current_dir() {
            Ok(cwd) => Config::from_repo_dir(&cwd).unwrap_or_default(),
            Err(_) => Config::empty(),
        },
    };

    let mut conn = match connect_upload_pack_with_config(&args.repository, &cfg) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("fatal: {e}");
            return Ok(128);
        }
    };

    let cap_pkts = match conn.discover_capabilities() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("fatal: {e}");
            return Ok(128);
        }
    };

    let cap = match CapabilityAdvertisement::parse(&cap_pkts) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("fatal: {e}");
            return Ok(128);
        }
    };

    if !cap.supports("ls-refs") {
        eprintln!("fatal: server doesn't advertise ls-refs");
        return Ok(128);
    }

    // Build the ref-prefix list. Default (no flags) matches git: HEAD,
    // refs/heads/, refs/tags/.
    let prefixes: Vec<&str> = match (args.heads, args.tags) {
        (true, false) => vec!["refs/heads/"],
        (false, true) => vec!["refs/tags/"],
        (true, true) => vec!["refs/heads/", "refs/tags/"],
        (false, false) => vec!["HEAD", "refs/heads/", "refs/tags/"],
    };

    let refs = match protocol_v2::ls_refs(&mut conn, &prefixes, cap.object_format) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("fatal: {e}");
            return Ok(128);
        }
    };

    let stdout = io::stdout();
    let mut out = stdout.lock();

    for r in &refs {
        if !ref_matches_patterns(&r.name, &args.patterns) {
            continue;
        }
        use std::io::Write;
        // Tab separator — matches `git ls-remote`.
        writeln!(out, "{}\t{}", r.oid, r.name)?;
        // Annotated tags also emit a `<peeled>\t<refname>^{}` line.
        if let Some(peeled) = &r.peeled {
            writeln!(out, "{peeled}\t{}^{{}}", r.name)?;
        }
    }
    Ok(0)
}

/// True if any of `patterns` matches `name`. Empty patterns list = "match all".
fn ref_matches_patterns(name: &str, patterns: &[String]) -> bool {
    if patterns.is_empty() {
        return true;
    }
    for p in patterns {
        if matches_tail(name, p) {
            return true;
        }
    }
    false
}

/// Best-effort port of `git ls-remote`'s matcher.
///
/// Real git's rule: a pattern matches if (a) the ref name ends with `/` +
/// pattern, (b) the ref name *equals* the pattern, or (c) the ref name
/// matches the pattern under `fnmatch(FNM_PATHNAME)` semantics. We
/// approximate (c) with a plain glob via `crate::wildmatch` to avoid
/// implementing a separate matcher here.
fn matches_tail(ref_name: &str, pattern: &str) -> bool {
    if ref_name == pattern {
        return true;
    }
    if let Some(rest) = ref_name.strip_suffix(pattern) {
        if rest.ends_with('/') {
            return true;
        }
    }
    // Wildcard match: fall through to wildmatch only if the pattern actually
    // contains a wildcard char, so plain-name patterns don't surprise users
    // with unintended globbing.
    if pattern.contains('*') || pattern.contains('?') || pattern.contains('[') {
        // wildmatch operates on byte slices; pathname mode treats `/` literally.
        return crate::wildmatch::wildmatch(
            pattern.as_bytes(),
            ref_name.as_bytes(),
            crate::wildmatch::WM_PATHNAME,
        );
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Debug, Parser)]
    struct Wrap {
        #[command(flatten)]
        args: LsRemoteArgs,
    }

    #[test]
    fn parses_minimal() {
        let w = Wrap::try_parse_from(["x", "https://example.com/r.git"]).unwrap();
        assert_eq!(w.args.repository, "https://example.com/r.git");
        assert!(!w.args.heads);
        assert!(!w.args.tags);
        assert!(w.args.patterns.is_empty());
    }

    #[test]
    fn parses_flags_and_patterns() {
        let w = Wrap::try_parse_from([
            "x",
            "--heads",
            "https://example.com/r.git",
            "main",
            "feature/*",
        ])
        .unwrap();
        assert!(w.args.heads);
        assert_eq!(w.args.repository, "https://example.com/r.git");
        assert_eq!(w.args.patterns, vec!["main", "feature/*"]);
    }

    #[test]
    fn pattern_exact_match() {
        assert!(ref_matches_patterns(
            "refs/heads/main",
            &["refs/heads/main".to_string()]
        ));
    }

    #[test]
    fn pattern_tail_match() {
        // "main" matches "refs/heads/main" via the trailing-slash heuristic.
        assert!(ref_matches_patterns(
            "refs/heads/main",
            &["main".to_string()]
        ));
        // But "ain" should NOT match — the prefix needs to end at a slash.
        assert!(!ref_matches_patterns(
            "refs/heads/main",
            &["ain".to_string()]
        ));
    }

    #[test]
    fn pattern_empty_matches_everything() {
        assert!(ref_matches_patterns("refs/heads/main", &[]));
    }
}
