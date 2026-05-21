//! `rustygit bug-report` — bundle environment context for a bug report.
//!
//! Reporters paste the output of this command into a GitHub issue. The
//! payload is engineered to be:
//!
//! 1. **Self-contained.** Everything a triager would otherwise have to ask
//!    for in a back-and-forth: rustygit version + target triple, OS, the
//!    upstream `git --version` (so we know which side of the
//!    rustygit/git A/B comparison the reporter is on), `rustygit doctor`
//!    output, and the relevant subset of process environment variables.
//! 2. **Safe to paste in public.** A [`redact_secrets`] pass strips PATs,
//!    GitLab tokens, basic-auth credentials embedded in URLs, and bare
//!    long hex strings (which can be either object ids — harmless — or
//!    SSH-key fragments — not harmless; we err on the side of redaction).
//!    Environment variables whose name contains `PASSWORD` / `TOKEN` /
//!    `AUTH` are reported as `<set>` / `<unset>` without their value.
//! 3. **Predictable in size.** Single page of stdout. We don't dump the
//!    whole environment, don't recursively walk the repo, don't include
//!    the full reflog — only the last 10 SUBCOMMAND NAMES (not full
//!    argv) from an opt-in history log.
//!
//! The history log is wired separately by `dispatch`: when
//! `rustygit.history.enabled = true`, each subcommand appends one line
//! (the subcommand name only) to
//! `$XDG_DATA_HOME/rustygit/history.log` — see [`history_log_path`]. If
//! the user hasn't opted in, [`recent_subcommands`] returns `None` and
//! the bundle says `<history disabled or empty>`.

use std::io::{self, Write};
use std::path::PathBuf;
use std::process::Command;

use clap::Args;

/// `rustygit bug-report` argv. No flags today; reserved for future
/// `--no-doctor` / `--no-env` toggles without forcing a SemVer bump.
#[derive(Debug, Args)]
pub struct BugReportArgs {}

/// Top-level entry point invoked from [`crate::cli::dispatch`].
///
/// Builds the report into a `String`, runs it through [`redact_secrets`]
/// once at the end (so a secret can't sneak through by living in one
/// section but being missed by a section-local sanitizer), then prints
/// the result. Exits 0 — bug-report failing is itself a bug worth
/// reporting, and we shouldn't make that harder.
pub fn run(_args: BugReportArgs) -> io::Result<i32> {
    let bundle = build_report();
    let redacted = redact_secrets(&bundle);
    let stdout = io::stdout();
    let mut out = stdout.lock();
    out.write_all(redacted.as_bytes())?;
    Ok(0)
}

/// Compose the full report. Pure function — easy to unit-test by
/// inspecting the returned string. Each section is delimited by a
/// `=== ... ===` header so reporters can collapse sections in editor view.
fn build_report() -> String {
    let mut s = String::with_capacity(4096);
    s.push_str("=== rustygit bug-report ===\n");
    s.push_str(&format!(
        "rustygit version: {}\n",
        env!("CARGO_PKG_VERSION")
    ));
    s.push_str(&format!(
        "Platform: {} {} (family: {})\n",
        std::env::consts::OS,
        std::env::consts::ARCH,
        std::env::consts::FAMILY
    ));
    s.push_str(&format!("OS details: {}\n", os_details()));
    s.push_str(&format!("git --version: {}\n", git_version()));
    s.push('\n');

    s.push_str("=== rustygit doctor ===\n");
    s.push_str(&doctor_output());
    s.push('\n');

    s.push_str("=== environment ===\n");
    s.push_str(&env_block());
    s.push('\n');

    s.push_str("=== recent subcommands ===\n");
    match recent_subcommands(10) {
        Some(lines) if !lines.is_empty() => {
            for line in lines {
                s.push_str(&line);
                s.push('\n');
            }
        }
        _ => s.push_str("<history disabled or empty>\n"),
    }

    s
}

/// Resolve an OS version string. Unix gets `uname -srm`; Windows gets
/// `cmd /c ver`; anything else falls back to a static label. We never
/// fail — a missing `uname` just falls back to the os/arch we already
/// have from `std::env::consts`.
fn os_details() -> String {
    #[cfg(unix)]
    {
        if let Ok(out) = Command::new("uname").arg("-srm").output() {
            if out.status.success() {
                return String::from_utf8_lossy(&out.stdout).trim().to_string();
            }
        }
        format!("{} (uname unavailable)", std::env::consts::OS)
    }
    #[cfg(windows)]
    {
        if let Ok(out) = Command::new("cmd").args(["/c", "ver"]).output() {
            if out.status.success() {
                return String::from_utf8_lossy(&out.stdout).trim().to_string();
            }
        }
        format!("{} (ver unavailable)", std::env::consts::OS)
    }
    #[cfg(not(any(unix, windows)))]
    {
        format!("{} (no platform-specific probe)", std::env::consts::OS)
    }
}

/// Best-effort upstream-git version. We use `PATH`-resolved `git`, not
/// `GIT_EXEC_PATH`, because the question we're answering is "what `git`
/// would the user hit if they typed `git --version` right now?".
fn git_version() -> String {
    match Command::new("git").arg("--version").output() {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout).trim().to_string(),
        Ok(out) => format!(
            "git present but errored ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ),
        Err(_) => "not found".to_string(),
    }
}

/// Run `rustygit doctor` as a subprocess (instead of refactoring
/// `doctor::run` to take a writer). Rationale: doctor's output is
/// already designed for human reading and won't change between
/// in-process and subprocess calls; spawning a child also gives us the
/// "what doctor would print right now" behavior, including any future
/// I/O-bound checks that wouldn't be safe to do in-line during a
/// panic-handler-adjacent code path.
///
/// If we're not inside a repo, doctor exits non-zero and prints to
/// stderr. We capture and report both states. Bug-reporters who aren't
/// inside a repo are still legitimate (e.g. reporting a `rustygit init`
/// crash), so we don't fail the bundle on doctor failure.
fn doctor_output() -> String {
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => return format!("<cannot resolve current_exe: {e}>\n"),
    };
    match Command::new(&exe).arg("doctor").output() {
        Ok(out) => {
            let mut s = String::new();
            if !out.stdout.is_empty() {
                s.push_str(&String::from_utf8_lossy(&out.stdout));
                if !s.ends_with('\n') {
                    s.push('\n');
                }
            }
            if !out.status.success() {
                s.push_str(&format!("<doctor exited {}>\n", out.status));
                if !out.stderr.is_empty() {
                    s.push_str("<stderr:>\n");
                    s.push_str(&String::from_utf8_lossy(&out.stderr));
                    if !s.ends_with('\n') {
                        s.push('\n');
                    }
                }
            }
            s
        }
        Err(e) => format!("<cannot spawn doctor: {e}>\n"),
    }
}

/// Names of env vars whose VALUE is safe to include verbatim. Anything
/// not on this list and not in the GIT_* / XDG_* / LC_* families gets
/// skipped entirely. The split between SAFE_VARS and the prefix-matched
/// family vars is deliberate — `LANG` is on the explicit list,
/// `LC_MESSAGES` matches the `LC_*` prefix walk.
const SAFE_VARS: &[&str] = &[
    "LANG",
    "TERM",
    "PAGER",
    "EDITOR",
    "VISUAL",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "XDG_CACHE_HOME",
    "SHELL",
];

/// Substrings that, if present in an env var name, mean we report the
/// var as `<set>` / `<unset>` without printing its value. Belt-and-
/// suspenders alongside `redact_secrets`: a `GIT_ASKPASS=mytoken`
/// shouldn't even reach the redaction layer.
const SENSITIVE_SUBSTRINGS: &[&str] = &["PASSWORD", "TOKEN", "AUTH", "SECRET", "KEY", "CREDENTIAL"];

/// Explicit list of token-bearing GIT_* vars. We list them by exact name
/// because `GIT_ASKPASS` doesn't trip any of the substring rules above.
const TOKEN_BEARING_GIT_VARS: &[&str] = &[
    "GIT_ASKPASS",
    "GIT_SSH_COMMAND",
    "GIT_SSH",
    "GIT_PROXY_COMMAND",
    "GIT_HTTP_USER_AGENT",
];

/// Build the env-var section. Walks `std::env::vars_os` once, sorts the
/// output by name for determinism (so two `bug-report` runs from the
/// same shell produce identical bundles), and applies the
/// safe-vs-token classification to each match.
fn env_block() -> String {
    let mut rows: Vec<(String, String)> = Vec::new();
    for (k, v) in std::env::vars_os() {
        let key = k.to_string_lossy().to_string();
        if !is_interesting(&key) {
            continue;
        }
        let value = if is_token_bearing(&key) {
            "<set>".to_string()
        } else if SAFE_VARS.contains(&key.as_str()) {
            v.to_string_lossy().to_string()
        } else {
            // GIT_* / XDG_* / LC_* that aren't on SAFE_VARS and don't
            // trip the token classifier. Show the value — these are
            // things like `GIT_DIR`, `GIT_AUTHOR_NAME`, `XDG_RUNTIME_DIR`.
            v.to_string_lossy().to_string()
        };
        rows.push((key, value));
    }
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    let mut s = String::new();
    if rows.is_empty() {
        s.push_str("<no relevant env vars set>\n");
        return s;
    }
    for (k, v) in rows {
        s.push_str(&format!("{k}={v}\n"));
    }
    s
}

/// Whether an env var name belongs in the report at all. Be inclusive
/// of `GIT_*` (the whole point of bug-report is to debug rustygit
/// behavior, which is driven by `GIT_*` overrides) and family-prefixed
/// locale vars; restrict everything else to the SAFE_VARS allow-list.
fn is_interesting(name: &str) -> bool {
    if SAFE_VARS.contains(&name) {
        return true;
    }
    name.starts_with("GIT_") || name.starts_with("XDG_") || name.starts_with("LC_")
}

fn is_token_bearing(name: &str) -> bool {
    if TOKEN_BEARING_GIT_VARS.contains(&name) {
        return true;
    }
    for sub in SENSITIVE_SUBSTRINGS {
        if name.contains(sub) {
            return true;
        }
    }
    false
}

/// Path of the opt-in subcommand history log. Honors `XDG_DATA_HOME`
/// when set; otherwise falls back to `$HOME/.local/share` per the XDG
/// base-directory spec. Returns `None` only when neither is set, which
/// is exceedingly rare (we'd be running outside any reasonable login
/// session).
pub fn history_log_path() -> Option<PathBuf> {
    let base: PathBuf = if let Some(x) = std::env::var_os("XDG_DATA_HOME") {
        let p = PathBuf::from(x);
        if p.is_absolute() {
            p
        } else {
            // Per spec, relative XDG_DATA_HOME is to be ignored.
            let home = std::env::var_os("HOME")?;
            PathBuf::from(home).join(".local/share")
        }
    } else {
        let home = std::env::var_os("HOME")?;
        PathBuf::from(home).join(".local/share")
    };
    Some(base.join("rustygit").join("history.log"))
}

/// Read the last `n` lines of the history log. Returns `None` when the
/// file doesn't exist (the common case — user hasn't opted in).
/// Returns `Some(empty)` if the file exists but is empty (a slightly
/// different signal: history is enabled but no commands have run yet).
fn recent_subcommands(n: usize) -> Option<Vec<String>> {
    let path = history_log_path()?;
    let content = std::fs::read_to_string(&path).ok()?;
    let mut lines: Vec<String> = content
        .lines()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if lines.len() > n {
        let start = lines.len() - n;
        lines.drain(..start);
    }
    Some(lines)
}

/// Strip likely-secret substrings from `input`. Hand-rolled (no regex
/// dep) — every pattern here is a fixed prefix + a tail of bounded-
/// alphabet characters, so a single linear scan is enough.
///
/// Patterns handled, in order:
///
/// 1. **GitHub PAT** — `ghp_` + 36+ `[A-Za-z0-9]`. Modern GitHub PATs
///    are exactly 36 chars after the prefix; we accept longer to keep
///    forward-compat with format changes.
/// 2. **GitLab PAT** — `glpat-` + 20+ `[A-Za-z0-9_-]`.
/// 3. **Basic auth in URL** — `https://USER:TOKEN@host` →
///    `https://USER:<REDACTED>@host`. We match `://` then a non-`/@`
///    user, `:`, a non-`/@` token, `@`.
/// 4. **Long hex** — 40+ contiguous `[0-9a-fA-F]`. Catches SHA-1 oids
///    (harmless but redacting is fine) and SSH-key fragments
///    (definitely not harmless).
///
/// The four passes compose: a URL-embedded `ghp_…` token is hit by
/// both passes 1 and 3; whichever fires first wins. We accept the
/// minor over-redaction in exchange for simpler logic.
pub fn redact_secrets(input: &str) -> String {
    let mut s = input.to_string();
    s = redact_prefix(&s, "ghp_", 36, is_alnum, "ghp_<REDACTED>");
    s = redact_prefix(
        &s,
        "glpat-",
        20,
        is_alnum_dash_underscore,
        "glpat-<REDACTED>",
    );
    s = redact_basic_auth(&s);
    s = redact_long_hex(&s, 40);
    s
}

fn is_alnum(c: char) -> bool {
    c.is_ascii_alphanumeric()
}

fn is_alnum_dash_underscore(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '-' || c == '_'
}

/// Find every `<prefix><tail of charset, length >= min>` and replace
/// the whole match (prefix + tail) with `replacement`. Linear scan with
/// no backtracking — pattern is anchored at `prefix`, tail is greedy.
fn redact_prefix(
    input: &str,
    prefix: &str,
    min_tail: usize,
    in_charset: fn(char) -> bool,
    replacement: &str,
) -> String {
    let mut out = String::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if input[i..].starts_with(prefix) {
            let start = i;
            let mut j = i + prefix.len();
            while j < bytes.len() {
                // Safe: ASCII-only charsets means single-byte boundaries.
                let c = bytes[j] as char;
                if !in_charset(c) {
                    break;
                }
                j += 1;
            }
            if j - (start + prefix.len()) >= min_tail {
                out.push_str(replacement);
                i = j;
                continue;
            }
        }
        // Push one char (UTF-8 safe via char_indices).
        if let Some(ch) = input[i..].chars().next() {
            out.push(ch);
            i += ch.len_utf8();
        } else {
            break;
        }
    }
    out
}

/// Redact the password in `scheme://user:pass@host`. We support both
/// `http` and `https` and don't touch anything that isn't preceded by
/// `://` (so a literal `foo:bar@baz` in some unrelated context survives).
fn redact_basic_auth(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // Look for `://` to anchor the URL form.
        if input[i..].starts_with("://") {
            let scheme_end = i + 3;
            // Scan forward through the userinfo, looking for `@` BEFORE
            // any `/` (path) or whitespace.
            let mut colon: Option<usize> = None;
            let mut at: Option<usize> = None;
            let mut k = scheme_end;
            while k < bytes.len() {
                let c = bytes[k] as char;
                if c == '/' || c.is_whitespace() {
                    break;
                }
                if c == ':' && colon.is_none() {
                    colon = Some(k);
                } else if c == '@' {
                    at = Some(k);
                    break;
                }
                k += 1;
            }
            if let (Some(col), Some(att)) = (colon, at) {
                if col + 1 < att {
                    // Emit `://USER:<REDACTED>@`.
                    out.push_str("://");
                    out.push_str(&input[scheme_end..col]);
                    out.push_str(":<REDACTED>");
                    out.push('@');
                    i = att + 1;
                    continue;
                }
            }
        }
        if let Some(ch) = input[i..].chars().next() {
            out.push(ch);
            i += ch.len_utf8();
        } else {
            break;
        }
    }
    out
}

/// Redact contiguous hex runs of length >= `min`. Anchored on a
/// non-hex/non-alnum char (or string start) so we don't bisect words
/// like `deadbeefcafe` embedded inside a path; the boundary check
/// also prevents redacting just the hex-looking tail of a longer
/// identifier like `commit12345abcdef…`.
fn redact_long_hex(input: &str, min: usize) -> String {
    let mut out = String::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i] as char;
        // Only start a hex run at a boundary (start-of-input or after a
        // non-alphanumeric). This avoids treating tail-of-identifier as
        // a separate hex run.
        let at_boundary = i == 0 || {
            let prev = bytes[i - 1] as char;
            !prev.is_ascii_alphanumeric()
        };
        if at_boundary && c.is_ascii_hexdigit() {
            let mut j = i;
            while j < bytes.len() && (bytes[j] as char).is_ascii_hexdigit() {
                j += 1;
            }
            let after_ok = j == bytes.len() || !(bytes[j] as char).is_ascii_alphanumeric();
            if j - i >= min && after_ok {
                out.push_str("<HEX_REDACTED>");
                i = j;
                continue;
            }
        }
        if let Some(ch) = input[i..].chars().next() {
            out.push(ch);
            i += ch.len_utf8();
        } else {
            break;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_github_pat() {
        let pat = "ghp_aBcDeFgHiJkLmNoPqRsTuVwXyZ0123456789";
        let input = format!("token is {pat} please");
        let out = redact_secrets(&input);
        assert!(
            out.contains("ghp_<REDACTED>"),
            "no redaction marker in: {out}"
        );
        assert!(!out.contains(pat), "raw PAT survived: {out}");
    }

    #[test]
    fn redacts_gitlab_pat() {
        let pat = "glpat-1234567890abcdEFGH_-";
        let input = format!("CI={pat}");
        let out = redact_secrets(&input);
        assert!(out.contains("glpat-<REDACTED>"));
        assert!(!out.contains(pat));
    }

    #[test]
    fn redacts_long_hex_oids() {
        // Exactly 40 hex chars (SHA-1).
        let oid = "deadbeefcafebabe0123456789abcdef01234567";
        let input = format!("commit {oid} is here");
        let out = redact_secrets(&input);
        assert!(
            out.contains("<HEX_REDACTED>"),
            "no hex redaction marker in: {out}"
        );
        assert!(!out.contains(oid));
    }

    #[test]
    fn short_hex_strings_survive() {
        // 39 hex chars: below the threshold, must pass through.
        let short = "deadbeefcafebabe0123456789abcdef0123456";
        let out = redact_secrets(short);
        assert_eq!(out, short, "short hex was wrongly redacted: {out}");
    }

    #[test]
    fn redacts_basic_auth_in_url() {
        let url = "https://alice:s3cret-token@example.com/repo.git";
        let out = redact_secrets(url);
        assert!(
            out.contains("https://alice:<REDACTED>@example.com/repo.git"),
            "expected redaction, got: {out}"
        );
        assert!(!out.contains("s3cret-token"));
    }

    #[test]
    fn url_without_userinfo_unchanged() {
        let url = "https://example.com/repo.git";
        let out = redact_secrets(url);
        assert_eq!(out, url);
    }

    #[test]
    fn ssh_url_with_user_at_host_unchanged() {
        // `git@host` is NOT basic-auth (no colon-separated password).
        // We must not turn `git@github.com` into a redaction.
        let url = "git@github.com:bsadashi/rustygit.git";
        let out = redact_secrets(url);
        assert_eq!(out, url);
    }

    #[test]
    fn redaction_is_composable() {
        // A PAT inside a URL inside a longer line: every pattern fires.
        let input =
            "url=https://x:ghp_aBcDeFgHiJkLmNoPqRsTuVwXyZ0123456789@host.example/r.git path=foo";
        let out = redact_secrets(input);
        // Either basic-auth redaction OR ghp_ redaction must have fired
        // (whichever matched first); the raw token must not survive.
        assert!(!out.contains("ghp_aBcDeFgHiJkLmNoPqRsTuVwXyZ0123456789"));
    }

    #[test]
    fn is_token_bearing_classifier() {
        assert!(is_token_bearing("GITHUB_TOKEN"));
        assert!(is_token_bearing("MY_PASSWORD"));
        assert!(is_token_bearing("GIT_ASKPASS"));
        assert!(is_token_bearing("GIT_SSH_COMMAND"));
        assert!(!is_token_bearing("LANG"));
        assert!(!is_token_bearing("TERM"));
        assert!(!is_token_bearing("GIT_DIR"));
    }

    #[test]
    fn is_interesting_classifier() {
        assert!(is_interesting("LANG"));
        assert!(is_interesting("GIT_DIR"));
        assert!(is_interesting("XDG_CONFIG_HOME"));
        assert!(is_interesting("LC_MESSAGES"));
        assert!(!is_interesting("RANDOM_VAR"));
        assert!(!is_interesting("PATH"));
    }

    #[test]
    fn build_report_contains_required_sections() {
        let bundle = build_report();
        assert!(bundle.contains("=== rustygit bug-report ==="));
        assert!(bundle.contains("rustygit version:"));
        assert!(bundle.contains("Platform:"));
        assert!(bundle.contains("=== environment ==="));
        assert!(bundle.contains("=== recent subcommands ==="));
    }
}
