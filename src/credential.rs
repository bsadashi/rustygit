//! Git credential helper protocol (M12).
//!
//! When an HTTPS request returns 401, we need a username/password for the
//! remote. Git's mechanism for this is the *credential helper protocol*:
//! a side-channel where an external program (osxkeychain, libsecret, the
//! plaintext `store` helper, a custom shell script, …) is invoked with
//! action `get` / `store` / `erase`, fed a `key=value\n` description on
//! stdin terminated by a blank line, and (for `get`) is expected to print
//! a similarly-shaped response that includes `username=…` and
//! `password=…` lines.
//!
//! Spec: `git-credential(1)` and `gitcredentials(7)` (see
//! `Documentation/git-credential.adoc` and `gitcredentials.adoc` in the
//! git source).
//!
//! Resolution of a `credential.helper` config value to an actual command
//! follows three rules (per `gitcredentials.adoc` "CUSTOM HELPERS"):
//!
//!   1. If the helper string begins with `!`, everything after the `!` is
//!      a shell snippet (executed via `sh -c …`).
//!   2. Otherwise, if the helper string starts with an absolute path, the
//!      verbatim string becomes the command.
//!   3. Otherwise, the prefix `git credential-` is prepended (so `foo`
//!      becomes `git credential-foo`).
//!
//! M12 scope:
//! - read `credential.helper` from a single [`Config`] (the repo's local
//!   config); multi-helper support and user-/system-level config merging
//!   are TODOs.
//! - the TTY-prompt fallback shells out to `stty -echo` / `stty echo` to
//!   read a password silently; not bulletproof but matches git's Unix
//!   behavior when `getpass` isn't available.
//! - we do *not* implement `store` / `erase` actions yet — just `get`.
//!   Those are needed when we want to persist a freshly-prompted
//!   credential, which is a separate milestone.

use std::io::{self, Read, Write};
use std::process::{Command, Stdio};

use thiserror::Error;

use crate::config::{Config, ConfigError};

/// A credential lookup context. Mirrors git's documented attribute set —
/// `protocol`, `host`, `path`, optional pre-filled `username`. We do not
/// model the more exotic attributes (`authtype`, `state[]`, `wwwauth[]`,
/// …) in M12; the helpers we care about (osxkeychain / libsecret / store
/// / a custom shell snippet) don't require them for the basic
/// HTTPS-Basic-auth flow.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CredentialRequest {
    pub protocol: Option<String>,
    pub host: Option<String>,
    pub path: Option<String>,
    /// Sometimes pre-filled by the URL (`https://alice@host/...`).
    pub username: Option<String>,
}

/// The user/password we want from a helper or terminal prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialResponse {
    pub username: String,
    pub password: String,
}

impl CredentialRequest {
    /// Build a request from an HTTPS URL like
    /// `https://alice@github.com/foo/bar.git`.
    ///
    /// Path keeps the leading-slash stripped (git's helpers expect e.g.
    /// `foo/bar.git`, not `/foo/bar.git`); a trailing `.git` is preserved
    /// because the path is opaque to us — the server may or may not
    /// require it.
    pub fn from_url(url: &str) -> Self {
        let mut req = CredentialRequest::default();

        // Split scheme.
        let (scheme, rest) = match url.find("://") {
            Some(i) => (&url[..i], &url[i + 3..]),
            None => return req, // no scheme — can't do much
        };
        req.protocol = Some(scheme.to_string());

        // The authority is everything up to the first `/`, `?`, or `#`.
        let auth_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
        let authority = &rest[..auth_end];
        let path = &rest[auth_end..];

        // userinfo@host:port — split on the *last* `@` so passwords with
        // an `@` in them don't confuse us (RFC 3986 actually disallows
        // unencoded `@` in userinfo, but we're lenient).
        let (userinfo, host) = match authority.rfind('@') {
            Some(i) => (Some(&authority[..i]), &authority[i + 1..]),
            None => (None, authority),
        };
        if let Some(ui) = userinfo {
            // userinfo = username[:password] — we only care about the
            // username here; the password (if any) is not used for the
            // helper request.
            let name = match ui.find(':') {
                Some(j) => &ui[..j],
                None => ui,
            };
            if !name.is_empty() {
                req.username = Some(name.to_string());
            }
        }

        // The host field in the credential protocol *includes* the port
        // if one was specified (per the spec — `example.com:8088`).
        req.host = Some(host.to_string());

        // Strip leading `/` from the path. Empty path → None (don't send
        // a `path=` line).
        let path_trimmed = path.trim_start_matches('/');
        // Also strip query/fragment if present.
        let path_clean = path_trimmed
            .split(['?', '#'])
            .next()
            .unwrap_or(path_trimmed);
        if !path_clean.is_empty() {
            req.path = Some(path_clean.to_string());
        }

        req
    }

    /// Encode for stdin → helper. Per `git-credential(1)`:
    ///
    /// ```text
    /// protocol=https
    /// host=example.com
    /// path=foo.git
    ///
    /// ```
    ///
    /// (Trailing blank line is required.) Keys are emitted in a fixed
    /// order so the output is deterministic / testable.
    pub fn encode(&self) -> String {
        let mut s = String::new();
        if let Some(p) = &self.protocol {
            s.push_str("protocol=");
            s.push_str(p);
            s.push('\n');
        }
        if let Some(h) = &self.host {
            s.push_str("host=");
            s.push_str(h);
            s.push('\n');
        }
        if let Some(p) = &self.path {
            s.push_str("path=");
            s.push_str(p);
            s.push('\n');
        }
        if let Some(u) = &self.username {
            s.push_str("username=");
            s.push_str(u);
            s.push('\n');
        }
        // Terminating blank line — without this, helpers will hang
        // waiting for more input.
        s.push('\n');
        s
    }

    /// Merge a partial helper response into this request. Used between
    /// helpers: if helper A returned `username=alice` but no password,
    /// helper B should be told the username so it can find a matching
    /// stored password.
    fn merge_partial(&mut self, text: &str) {
        for line in text.lines() {
            if line.is_empty() {
                break;
            }
            let Some((k, v)) = line.split_once('=') else {
                continue;
            };
            match k {
                "protocol" => self.protocol = Some(v.to_string()),
                "host" => self.host = Some(v.to_string()),
                "path" => self.path = Some(v.to_string()),
                "username" => self.username = Some(v.to_string()),
                _ => {} // ignore unknown / password / etc.
            }
        }
    }
}

impl CredentialResponse {
    /// Decode stdout from a helper's `get` action. Returns `None` if
    /// either `username` or `password` is missing — that's a signal to
    /// the caller that the helper had no record for this request and we
    /// should fall through to the next helper (or the prompt).
    pub fn decode(text: &str) -> Option<Self> {
        let mut username: Option<String> = None;
        let mut password: Option<String> = None;
        for line in text.lines() {
            if line.is_empty() {
                // Per spec, list of attributes is terminated by a blank
                // line. Anything after this is junk we ignore.
                break;
            }
            let Some((k, v)) = line.split_once('=') else {
                continue;
            };
            match k {
                "username" => username = Some(v.to_string()),
                "password" => password = Some(v.to_string()),
                _ => {} // ignore unrecognised attributes
            }
        }
        Some(CredentialResponse {
            username: username?,
            password: password?,
        })
    }
}

/// Resolve a `credential.helper` config value to an actual command
/// vector. First element is the executable; rest are arguments.
///
/// Rules (from `gitcredentials(7)` "CUSTOM HELPERS"):
///
///   1. Leading `!` → shell command. We return `["sh", "-c", "<snippet>"]`
///      so any quoting / `$VAR` / pipes the user wrote in the snippet
///      work as they expect.
///   2. Absolute path (starts with `/`) → verbatim, split shell-style.
///      We do a simple whitespace split since handling `'...'` quoting
///      properly belongs in a real shell-words parser; M12 documents
///      this limitation.
///   3. Otherwise → `["git", "credential-<name>", <rest...>]`.
///
/// The action argument (`get` / `store` / `erase`) is *not* added here —
/// the caller appends it.
pub fn resolve_helper(helper: &str) -> Vec<String> {
    let trimmed = helper.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }

    if let Some(snippet) = trimmed.strip_prefix('!') {
        // Shell snippet — defer all parsing to sh.
        return vec!["sh".to_string(), "-c".to_string(), snippet.to_string()];
    }

    if trimmed.starts_with('/') {
        // Absolute path. Naive whitespace split for the trailing args.
        // For now this is "good enough"; quoting/escaping would need a
        // shellwords-style parser.
        let parts: Vec<String> = trimmed.split_whitespace().map(|s| s.to_string()).collect();
        return parts;
    }

    // Default: `git credential-<name> [args...]`.
    let mut parts = trimmed.split_whitespace();
    let name = parts.next().unwrap_or("");
    let mut cmd = vec!["git".to_string(), format!("credential-{name}")];
    for arg in parts {
        cmd.push(arg.to_string());
    }
    cmd
}

/// High-level entry point: read configured helpers from `config`, try
/// each in order; on miss, prompt the terminal.
///
/// In M12 we only consult `credential.helper` (single value, last-write
/// wins via [`Config::get_string`]) — multi-value support is a TODO when
/// the Config layer exposes it.
pub fn fill_credentials(
    request: &CredentialRequest,
    config: &Config,
) -> Result<CredentialResponse, CredentialError> {
    // Work on a mutable copy so partial helper responses can refine the
    // next helper's input.
    let mut req = request.clone();

    if let Some(helper_str) = config.get_string("credential", "helper") {
        let helper_str = helper_str.trim();
        // Per spec: empty-string `credential.helper` resets the list.
        // For M12 with a single helper value, an empty string means
        // "no helper configured".
        if !helper_str.is_empty() {
            match try_helper(helper_str, &req)? {
                HelperOutcome::Full(resp) => return Ok(resp),
                HelperOutcome::Partial(text) => {
                    req.merge_partial(&text);
                }
                HelperOutcome::Nothing => {}
            }
        }
    }

    // Fall through to the terminal prompt.
    if !is_stdin_tty() {
        return Err(CredentialError::NoCredentials);
    }

    let username = match &req.username {
        Some(u) => u.clone(),
        None => prompt_line("Username: ")?,
    };
    let password = prompt_password("Password: ")?;
    Ok(CredentialResponse { username, password })
}

/// Run a single helper's `get` action and classify the result.
enum HelperOutcome {
    /// Helper returned both `username=` and `password=`.
    Full(CredentialResponse),
    /// Helper returned at least one attribute but not the full pair.
    /// The raw stdout is kept so the caller can merge it into the
    /// request for the next helper.
    Partial(String),
    /// Helper produced no output (or exited non-zero) — try the next
    /// helper / fall back to the prompt.
    Nothing,
}

fn try_helper(
    helper_str: &str,
    request: &CredentialRequest,
) -> Result<HelperOutcome, CredentialError> {
    let argv = resolve_helper(helper_str);
    if argv.is_empty() {
        return Ok(HelperOutcome::Nothing);
    }

    let mut cmd = Command::new(&argv[0]);
    for a in &argv[1..] {
        cmd.arg(a);
    }
    cmd.arg("get")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            // Helper executable not found / not executable. Per git's
            // behaviour, this is non-fatal — just log and move on.
            eprintln!("rustygit: credential helper '{helper_str}' could not be started: {e}");
            return Ok(HelperOutcome::Nothing);
        }
    };

    let encoded = request.encode();
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(encoded.as_bytes())?;
        // Closing stdin (via drop) signals EOF to the helper.
    }

    let mut stdout = String::new();
    if let Some(mut out) = child.stdout.take() {
        out.read_to_string(&mut stdout)?;
    }
    let mut stderr = String::new();
    if let Some(mut err) = child.stderr.take() {
        err.read_to_string(&mut stderr)?;
    }

    let status = child.wait()?;
    if !status.success() {
        // Non-zero exit — print helper's stderr to ours and move on.
        // We don't surface this as an error because git itself doesn't:
        // a helper that exits non-zero just means "I have no credentials
        // for this", not "the whole operation should fail".
        if !stderr.trim().is_empty() {
            eprintln!(
                "rustygit: credential helper '{helper_str}': {}",
                stderr.trim()
            );
        }
        return Ok(HelperOutcome::Nothing);
    }

    match CredentialResponse::decode(&stdout) {
        Some(resp) => Ok(HelperOutcome::Full(resp)),
        None => {
            if stdout.trim().is_empty() {
                Ok(HelperOutcome::Nothing)
            } else {
                Ok(HelperOutcome::Partial(stdout))
            }
        }
    }
}

/// Whether stdin appears to be a TTY. We don't link to libc, so we use
/// the `tty` shell command as a portable detector: it exits 0 iff stdin
/// is a terminal. (This is a touch heavier than `isatty(0)` but avoids a
/// dependency.)
fn is_stdin_tty() -> bool {
    Command::new("tty")
        .stdin(Stdio::inherit()) // inherit *our* stdin so tty(1) tests it
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn prompt_line(prompt: &str) -> io::Result<String> {
    eprint!("{prompt}");
    let _ = io::stderr().flush();
    let mut s = String::new();
    io::stdin().read_line(&mut s)?;
    Ok(s.trim_end_matches(['\n', '\r']).to_string())
}

/// Read a line from stdin with terminal echo disabled. On Unix we shell
/// out to `stty` rather than linking to termios. This is best-effort:
/// if `stty` is unavailable the password will simply be echoed (and the
/// user is no worse off than typing into a regular prompt).
fn prompt_password(prompt: &str) -> io::Result<String> {
    eprint!("{prompt}");
    let _ = io::stderr().flush();

    let _ = Command::new("stty").arg("-echo").status();
    let mut s = String::new();
    let r = io::stdin().read_line(&mut s);
    let _ = Command::new("stty").arg("echo").status();
    // Echo was off — the user's Enter didn't render a newline either.
    eprintln!();
    r.map(|_| s.trim_end_matches(['\n', '\r']).to_string())
}

#[derive(Error, Debug)]
pub enum CredentialError {
    #[error("no credentials available (no helper configured and no TTY for prompt)")]
    NoCredentials,
    #[error("credential helper '{helper}' failed: {message}")]
    HelperFailed { helper: String, message: String },
    #[error("io: {0}")]
    Io(#[from] io::Error),
    #[error("malformed helper response: {0}")]
    Malformed(String),
    #[error("config: {0}")]
    Config(#[from] ConfigError),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_url_parses_https() {
        let req = CredentialRequest::from_url("https://github.com/foo/bar");
        assert_eq!(req.protocol.as_deref(), Some("https"));
        assert_eq!(req.host.as_deref(), Some("github.com"));
        assert_eq!(req.path.as_deref(), Some("foo/bar"));
        assert_eq!(req.username, None);
    }

    #[test]
    fn from_url_parses_https_with_dot_git() {
        // `.git` is part of the opaque path; we don't strip it.
        let req = CredentialRequest::from_url("https://github.com/foo/bar.git");
        assert_eq!(req.path.as_deref(), Some("foo/bar.git"));
    }

    #[test]
    fn from_url_handles_prefilled_username() {
        let req = CredentialRequest::from_url("https://user@github.com/foo/bar");
        assert_eq!(req.username.as_deref(), Some("user"));
        assert_eq!(req.host.as_deref(), Some("github.com"));
    }

    #[test]
    fn from_url_handles_user_and_pass_in_url() {
        let req = CredentialRequest::from_url("https://user:secret@github.com/foo/bar");
        // We only extract the username; the password in the URL is left
        // alone (the helper / prompt fills the actual password).
        assert_eq!(req.username.as_deref(), Some("user"));
    }

    #[test]
    fn from_url_includes_port_in_host() {
        let req = CredentialRequest::from_url("https://example.com:8443/foo");
        assert_eq!(req.host.as_deref(), Some("example.com:8443"));
    }

    #[test]
    fn from_url_empty_path_is_none() {
        let req = CredentialRequest::from_url("https://example.com");
        assert_eq!(req.path, None);
        let req = CredentialRequest::from_url("https://example.com/");
        assert_eq!(req.path, None);
    }

    #[test]
    fn from_url_strips_query_and_fragment_from_path() {
        let req = CredentialRequest::from_url("https://example.com/foo?bar=1#frag");
        assert_eq!(req.path.as_deref(), Some("foo"));
    }

    #[test]
    fn encode_format_matches_git_spec() {
        let req = CredentialRequest {
            protocol: Some("https".into()),
            host: Some("example.com".into()),
            path: Some("foo.git".into()),
            username: None,
        };
        let s = req.encode();
        assert_eq!(s, "protocol=https\nhost=example.com\npath=foo.git\n\n");
        // Trailing blank line is mandatory.
        assert!(s.ends_with("\n\n"));
    }

    #[test]
    fn encode_includes_username_when_set() {
        let req = CredentialRequest {
            protocol: Some("https".into()),
            host: Some("github.com".into()),
            path: None,
            username: Some("alice".into()),
        };
        let s = req.encode();
        assert_eq!(s, "protocol=https\nhost=github.com\nusername=alice\n\n");
    }

    #[test]
    fn encode_omits_missing_fields() {
        let req = CredentialRequest::default();
        let s = req.encode();
        // Just the terminating blank line.
        assert_eq!(s, "\n");
    }

    #[test]
    fn decode_reads_username_and_password() {
        let text = "username=alice\npassword=hunter2\n";
        let resp = CredentialResponse::decode(text).expect("decoded");
        assert_eq!(resp.username, "alice");
        assert_eq!(resp.password, "hunter2");
    }

    #[test]
    fn decode_ignores_unknown_keys() {
        let text = "protocol=https\nhost=example.com\nusername=alice\npassword=hunter2\nquit=0\n";
        let resp = CredentialResponse::decode(text).expect("decoded");
        assert_eq!(resp.username, "alice");
        assert_eq!(resp.password, "hunter2");
    }

    #[test]
    fn decode_returns_none_if_username_missing() {
        let text = "password=hunter2\n";
        assert!(CredentialResponse::decode(text).is_none());
    }

    #[test]
    fn decode_returns_none_if_password_missing() {
        let text = "username=alice\n";
        assert!(CredentialResponse::decode(text).is_none());
    }

    #[test]
    fn decode_stops_at_blank_line() {
        // Anything after a blank line should be ignored.
        let text = "username=alice\npassword=hunter2\n\npassword=junk\n";
        let resp = CredentialResponse::decode(text).expect("decoded");
        assert_eq!(resp.password, "hunter2");
    }

    #[test]
    fn decode_value_with_equals_sign() {
        // Split on the *first* `=` — the value can legitimately contain
        // more `=` chars (e.g. base64 padding).
        let text = "username=alice\npassword=ab=cd==\n";
        let resp = CredentialResponse::decode(text).expect("decoded");
        assert_eq!(resp.password, "ab=cd==");
    }

    #[test]
    fn resolve_helper_named_macos() {
        let argv = resolve_helper("osxkeychain");
        assert_eq!(argv, vec!["git", "credential-osxkeychain"]);
    }

    #[test]
    fn resolve_helper_named_store() {
        let argv = resolve_helper("store");
        assert_eq!(argv, vec!["git", "credential-store"]);
    }

    #[test]
    fn resolve_helper_with_args() {
        let argv = resolve_helper("store --file=/tmp/x");
        assert_eq!(argv, vec!["git", "credential-store", "--file=/tmp/x"]);
    }

    #[test]
    fn resolve_helper_shell_snippet() {
        // Leading `!` → executed via `sh -c`.
        let argv = resolve_helper("!my-script --foo");
        assert_eq!(argv, vec!["sh", "-c", "my-script --foo"]);
    }

    #[test]
    fn resolve_helper_absolute_path() {
        let argv = resolve_helper("/abs/path/helper");
        assert_eq!(argv, vec!["/abs/path/helper"]);
    }

    #[test]
    fn resolve_helper_absolute_path_with_args() {
        let argv = resolve_helper("/abs/path/helper --foo bar");
        assert_eq!(argv, vec!["/abs/path/helper", "--foo", "bar"]);
    }

    #[test]
    fn resolve_helper_empty() {
        let argv = resolve_helper("");
        assert!(argv.is_empty());
        let argv = resolve_helper("   ");
        assert!(argv.is_empty());
    }

    #[test]
    fn merge_partial_updates_request() {
        let mut req = CredentialRequest {
            protocol: Some("https".into()),
            host: Some("example.com".into()),
            path: None,
            username: None,
        };
        req.merge_partial("username=alice\nprotocol=https\n");
        assert_eq!(req.username.as_deref(), Some("alice"));
        assert_eq!(req.protocol.as_deref(), Some("https"));
    }

    /// End-to-end: write a small shell script that mimics a helper, point
    /// `credential.helper` at it via a tempdir Config, run
    /// `fill_credentials`, and confirm we get back what the script
    /// printed.
    #[test]
    #[cfg(unix)]
    fn fake_helper_end_to_end() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let script_path = dir.path().join("fake-helper.sh");
        std::fs::write(
            &script_path,
            "#!/bin/sh\n\
             # Read (and discard) stdin so the parent doesn't see a broken pipe.\n\
             cat >/dev/null\n\
             echo username=test\n\
             echo password=secret\n",
        )
        .unwrap();
        let mut perm = std::fs::metadata(&script_path).unwrap().permissions();
        perm.set_mode(0o755);
        std::fs::set_permissions(&script_path, perm).unwrap();

        // Configure helper as the absolute path to the script.
        let cfg_text = format!("[credential]\n\thelper = {}\n", script_path.display());
        let config = Config::parse_str(&cfg_text).unwrap();

        let req = CredentialRequest::from_url("https://example.com/foo/bar.git");
        let resp = fill_credentials(&req, &config).expect("fill_credentials");
        assert_eq!(resp.username, "test");
        assert_eq!(resp.password, "secret");
    }

    /// A helper that exits with non-zero status should not blow up the
    /// caller; without a TTY the result is `NoCredentials`, but the
    /// important assertion is "no panic, no Io error bubbled up".
    #[test]
    #[cfg(unix)]
    fn helper_failure_is_non_fatal() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let script_path = dir.path().join("broken-helper.sh");
        std::fs::write(
            &script_path,
            "#!/bin/sh\ncat >/dev/null\necho 'no creds for you' >&2\nexit 1\n",
        )
        .unwrap();
        let mut perm = std::fs::metadata(&script_path).unwrap().permissions();
        perm.set_mode(0o755);
        std::fs::set_permissions(&script_path, perm).unwrap();

        let cfg_text = format!("[credential]\n\thelper = {}\n", script_path.display());
        let config = Config::parse_str(&cfg_text).unwrap();

        let req = CredentialRequest::from_url("https://example.com/foo/bar.git");
        let result = fill_credentials(&req, &config);
        // We may have a TTY (running interactively) or not (CI). Either
        // is fine: we must NOT see `Io` or `HelperFailed`.
        if let Err(e) = result {
            match e {
                CredentialError::NoCredentials => {}
                other => panic!("unexpected error from broken helper: {other:?}"),
            }
        }
    }

    /// A helper that prints only `username=` (no password) is a partial
    /// response — we don't return success but we also don't panic.
    #[test]
    #[cfg(unix)]
    fn helper_partial_response_falls_through() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let script_path = dir.path().join("partial-helper.sh");
        std::fs::write(
            &script_path,
            "#!/bin/sh\ncat >/dev/null\necho username=alice\n",
        )
        .unwrap();
        let mut perm = std::fs::metadata(&script_path).unwrap().permissions();
        perm.set_mode(0o755);
        std::fs::set_permissions(&script_path, perm).unwrap();

        let cfg_text = format!("[credential]\n\thelper = {}\n", script_path.display());
        let config = Config::parse_str(&cfg_text).unwrap();
        let req = CredentialRequest::from_url("https://example.com/foo");
        let result = fill_credentials(&req, &config);
        // Same TTY caveat as above.
        if let Err(e) = result {
            assert!(matches!(e, CredentialError::NoCredentials));
        }
    }

    #[test]
    fn empty_helper_config_means_no_helper() {
        // An empty-string helper, per spec, means "reset the list" —
        // and in M12's single-helper world that means no helper at all.
        let config = Config::parse_str("[credential]\n\thelper = \n").unwrap();
        let req = CredentialRequest::from_url("https://example.com/foo");
        // Without a TTY we expect NoCredentials; with one the prompt
        // would block. We don't assert on the value, only that we don't
        // crash trying to run an empty command.
        let _ = fill_credentials(&req, &config);
    }
}
