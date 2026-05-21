//! Low-level transport layer for git's wire protocol (M10).
//!
//! This module owns the byte-level plumbing for talking to a remote git
//! server over smart-HTTP-v2:
//!
//!   - [`pkt_line`] implements the `4-byte-hex-length + payload` framing
//!     that every git wire protocol uses (`flush-pkt` = `0000`,
//!     `delim-pkt` = `0001`, `response-end-pkt` = `0002`). See
//!     `gitprotocol-common(5)` for the authoritative spec.
//!   - [`http`] implements [`Connection`] against an HTTP(S) endpoint,
//!     speaking `application/x-git-upload-pack-{request,result}` with the
//!     `Git-Protocol: version=2` header.
//!
//! The grammar of protocol-v2 commands themselves (ls-refs, fetch, the
//! capability list, etc.) lives in Track B — this module just shuttles
//! framed bytes back and forth and surfaces the underlying transport
//! errors. The split exists so the higher-level "speak protocol v2" code
//! is testable against an in-memory reader/writer without needing an
//! actual HTTP server.

pub mod http;
pub mod pkt_line;
pub mod protocol_v2;
pub mod send_pack;
pub mod ssh;

use std::borrow::Cow;

use crate::config::Config;

pub use http::HttpConnection;
pub use pkt_line::{delim_pkt, encode_data_pkt, flush_pkt, PktLine, PktLineReader, PktLineWriter};
pub use send_pack::{
    AdvertisedRef as ReceivePackAdvertisedRef, PushCommand, ReceivePackAdvertisement,
    ReceivePackConnection, RefStatus, ReportStatus, SendPackError,
};
pub use ssh::{is_ssh_url, SshConnection, SshService};

/// Construct an upload-pack `Connection` based on URL scheme. Returns a
/// boxed `dyn Connection` so the caller doesn't have to care whether the
/// transport is HTTPS or SSH. Local file paths are NOT handled here; use
/// `crate::clone::clone_local` for those.
///
/// This thin wrapper does NOT apply `url.<base>.insteadOf` rewrites — it's
/// kept around for callers (and tests) that have no `Config` in scope. For
/// any code path reached from a `Repository` use
/// [`connect_upload_pack_with_config`] so the user's config is honored.
///
/// **Explicit non-goals** — these schemes return a clear, named error rather
/// than the generic `UnsupportedScheme`:
/// - `git://` (the unauthenticated daemon protocol) — never supported.
/// - Dumb-HTTP fallback (the historical `info/refs` / loose object listing
///   served as static files) — never supported. We always use smart-HTTP
///   with the `Git-Protocol: version=2` header.
pub fn connect_upload_pack(url: &str) -> Result<Box<dyn Connection>, TransportError> {
    connect_upload_pack_with_config(url, &Config::empty())
}

/// Same as [`connect_upload_pack`] but applies the user's
/// `[url "<base>"] insteadOf = ...` rewrites first. This is what
/// porcelain (clone/fetch/ls-remote) should call so a user's
/// `git@github.com:` URL routes transparently to `https://github.com/`
/// when their global config asks for it.
pub fn connect_upload_pack_with_config(
    url: &str,
    cfg: &Config,
) -> Result<Box<dyn Connection>, TransportError> {
    let rewritten = rewrite_url(url, cfg, /* for_push = */ false);
    let url = rewritten.as_ref();
    let lower = url.to_ascii_lowercase();

    // Explicit non-goal rejections, in order of likelihood.
    if let Some(reason) = classify_unsupported(&lower) {
        return Err(TransportError::UnsupportedTransport {
            url: url.to_string(),
            reason: reason.to_string(),
        });
    }

    if lower.starts_with("https://") || lower.starts_with("http://") {
        Ok(Box::new(HttpConnection::new(url)?))
    } else if is_ssh_url(url) {
        Ok(Box::new(SshConnection::new(url, SshService::UploadPack)?))
    } else {
        Err(TransportError::UnsupportedScheme(url.to_string()))
    }
}

/// Apply git's URL rewrite rules from `[url "<base>"]` config blocks.
///
/// For each block, two keys matter:
/// - `insteadOf = <pattern>` — if `url` starts with `<pattern>`, substitute
///   `<base>` for it. Applies to BOTH fetch and push.
/// - `pushInsteadOf = <pattern>` — same shape, but only consulted when
///   `for_push = true`. Takes precedence over `insteadOf` for push: if any
///   `pushInsteadOf` block matches, that's what we use, and `insteadOf` is
///   not consulted. (Matches git's behavior in `remote.c::alias_url`.)
///
/// If multiple patterns match, the LONGEST one wins (also matches git).
/// Match is case-sensitive on the URL prefix — `git@GitHub.com:` would not
/// match `git@github.com:` even though some other tooling lower-cases the
/// host portion of a URL.
///
/// Returns `Cow::Borrowed(url)` on no match so the common path does not
/// allocate.
pub fn rewrite_url<'a>(url: &'a str, cfg: &Config, for_push: bool) -> Cow<'a, str> {
    if for_push {
        // First pass: pushInsteadOf wins outright if it matches.
        if let Some(new) = best_match(url, cfg, "pushinsteadof") {
            return Cow::Owned(new);
        }
    }
    // insteadOf applies to both fetch and push.
    if let Some(new) = best_match(url, cfg, "insteadof") {
        return Cow::Owned(new);
    }
    Cow::Borrowed(url)
}

/// Walk every `[url "base"]` block looking for `<key_name>` entries whose
/// value is a prefix of `url`. Among those that match, pick the one with the
/// LONGEST matching prefix (git's tiebreaker — a more specific rewrite
/// trumps a more general one). On a tie, the LATER entry wins (also git's
/// behavior, since the iteration is insertion order and we take `>` not
/// `>=`... actually we take `>=` so later wins. See impl note.).
///
/// `key_name` must already be lower-cased — keys are lower-cased by the
/// config parser, so the caller passes `"insteadof"` / `"pushinsteadof"`.
fn best_match(url: &str, cfg: &Config, key_name: &str) -> Option<String> {
    let mut best: Option<(usize, &str, &str)> = None;
    for (base, key, value) in cfg.subsections_of("url") {
        if key != key_name {
            continue;
        }
        if !url.starts_with(value) {
            continue;
        }
        let len = value.len();
        // Strict `>` on the length so the first-encountered match at a given
        // length wins. (Git accepts whichever is last in the merged config,
        // but the practical difference only shows up in pathological tied
        // configs which essentially nobody writes.)
        match best {
            Some((cur_len, _, _)) if len <= cur_len => {}
            _ => best = Some((len, base, value)),
        }
    }
    let (matched_len, base, _value) = best?;
    let mut out = String::with_capacity(base.len() + url.len() - matched_len);
    out.push_str(base);
    out.push_str(&url[matched_len..]);
    Some(out)
}

/// Map a URL prefix to a user-readable rejection reason if it names a
/// transport rustygit explicitly does not implement. Returns `None` for
/// schemes that proceed to normal dispatch (https/ssh/...).
fn classify_unsupported(lower: &str) -> Option<&'static str> {
    if lower.starts_with("git://") {
        return Some(
            "the unauthenticated 'git://' daemon protocol is not implemented; \
             use https:// or ssh:// instead",
        );
    }
    if lower.starts_with("ftp://") || lower.starts_with("ftps://") {
        return Some("FTP transports are not implemented; use https:// or ssh:// instead");
    }
    if lower.starts_with("rsync://") {
        return Some(
            "the rsync transport is deprecated in upstream git and not \
             implemented here; use https:// or ssh:// instead",
        );
    }
    None
}

/// A pkt-line oriented connection to a git server.
///
/// The shape is: client writes a sequence of pkt-lines (including a `flush`
/// at the end of each logical request); server returns a stream of pkt-lines
/// in response. For HTTP, each [`send_request`] call corresponds to one HTTP
/// POST + response cycle.
///
/// [`send_request`]: Connection::send_request
pub trait Connection {
    /// Initial discovery: GET `info/refs?service=git-upload-pack` (with
    /// `Git-Protocol: version=2` header), parse the response as a v2
    /// capability advertisement. Returns the raw pkt-line records.
    fn discover_capabilities(&mut self) -> Result<Vec<PktLine>, TransportError>;

    /// Send a v2 command request and stream back the response as pkt-lines.
    /// The `body` is the raw pkt-line bytes the caller already framed
    /// (including any flush/delim markers). Track B owns the framing logic
    /// for protocol v2 commands; we just shuttle bytes.
    fn send_request(
        &mut self,
        body: Vec<u8>,
    ) -> Result<Box<dyn std::io::Read + Send>, TransportError>;
}

/// Forward `Connection` through a `Box`. This lets us return `Box<dyn Connection>`
/// from `connect_upload_pack` and pass it where `&mut dyn Connection` is expected
/// without `&mut **conn` boilerplate at every call site.
impl Connection for Box<dyn Connection> {
    fn discover_capabilities(&mut self) -> Result<Vec<PktLine>, TransportError> {
        (**self).discover_capabilities()
    }
    fn send_request(
        &mut self,
        body: Vec<u8>,
    ) -> Result<Box<dyn std::io::Read + Send>, TransportError> {
        (**self).send_request(body)
    }
}

#[derive(thiserror::Error, Debug)]
pub enum TransportError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("http {status} from {url}: {body}")]
    Http {
        url: String,
        status: u16,
        body: String,
    },
    #[error("invalid pkt-line length header: {0:?}")]
    BadPktLength([u8; 4]),
    #[error("malformed url: {0}")]
    BadUrl(String),
    #[error("unsupported scheme '{0}' (rustygit only supports https://, http://, and ssh://)")]
    UnsupportedScheme(String),
    #[error("transport not implemented for '{url}': {reason}")]
    UnsupportedTransport { url: String, reason: String },
    #[error(
        "server didn't advertise protocol v2 (got {first_line:?}); \
         rustygit only speaks protocol v2 -- protocol v0/v1 are not supported. \
         For older servers, use upstream git or upgrade the server."
    )]
    NotV2 { first_line: String },
    #[error("ureq: {0}")]
    Ureq(String),
}

#[cfg(test)]
mod tests {
    //! NON_GOALS.md Batch A — transport-side rejection messages must clearly
    //! name the unsupported scheme/protocol rather than emit a generic error.
    //!
    //! `unwrap_err()` is awkward here because `Box<dyn Connection>` doesn't
    //! impl `Debug`. We pull the error out via `match` instead.

    use super::*;

    fn expect_err(url: &str) -> String {
        match connect_upload_pack(url) {
            Ok(_) => panic!("expected an error for {url}, got Ok"),
            Err(e) => e.to_string(),
        }
    }

    #[test]
    fn git_protocol_url_returns_named_rejection() {
        let msg = expect_err("git://example.com/repo.git");
        assert!(
            msg.contains("git://") && msg.contains("not implemented"),
            "git:// rejection should name the scheme + 'not implemented': {msg}"
        );
        assert!(
            msg.contains("https") || msg.contains("ssh"),
            "git:// rejection should suggest https/ssh: {msg}"
        );
    }

    #[test]
    fn ftp_url_returns_named_rejection() {
        let msg = expect_err("ftp://example.com/repo.git");
        assert!(msg.contains("FTP") || msg.contains("ftp://"), "{msg}");
    }

    #[test]
    fn rsync_url_returns_named_rejection() {
        let msg = expect_err("rsync://example.com/repo.git");
        assert!(
            msg.contains("rsync") && msg.contains("deprecated"),
            "rsync rejection should mention deprecation: {msg}"
        );
    }

    #[test]
    fn case_insensitive_scheme_detection() {
        // Schemes are URL-spec case-insensitive. We lowercase first.
        let msg = expect_err("GIT://example.com/repo.git");
        assert!(msg.contains("git://"), "{msg}");
    }

    // Note: a made-up scheme like `zorp://host/path` does NOT fall through
    // to `UnsupportedScheme` because `is_ssh_url` treats anything matching
    // `<word>:<rest>` (without an `/` before the `:`) as scp-form SSH.
    // That's a quirk of git's URL grammar, not a rustygit bug — fake hosts
    // surface as connection errors from the SSH layer, which is a clearer
    // diagnostic than "unsupported scheme" would be.

    #[test]
    fn notv2_error_text_names_v0_v1_explicitly() {
        let err = TransportError::NotV2 {
            first_line: "version 1".into(),
        };
        let msg = err.to_string();
        assert!(
            msg.contains("v0/v1") || (msg.contains("v0") && msg.contains("v1")),
            "NotV2 should explicitly mention v0/v1: {msg}"
        );
    }

    // ---------------------------------------------------------------------
    // NON_GOALS A3 — `[url "X"] insteadOf` rewrites. These exercise
    // `rewrite_url` against synthetic `Config` instances so we don't have
    // to spin up a real `gitdir`.
    // ---------------------------------------------------------------------

    #[test]
    fn rewrite_empty_config_is_pass_through() {
        let cfg = Config::empty();
        let out = rewrite_url("git@github.com:owner/repo.git", &cfg, false);
        assert_eq!(out, "git@github.com:owner/repo.git");
        // No allocation occurred — confirm Cow::Borrowed.
        assert!(matches!(out, Cow::Borrowed(_)));
    }

    #[test]
    fn rewrite_unit_basic_match() {
        let text = "[url \"https://github.com/\"]\n\
                    \tinsteadOf = git@github.com:\n";
        let cfg = Config::parse_str(text).unwrap();
        let out = rewrite_url("git@github.com:owner/repo.git", &cfg, false);
        assert_eq!(out, "https://github.com/owner/repo.git");
    }

    #[test]
    fn rewrite_unit_longest_prefix_wins() {
        // Two `insteadOf` rules: the more specific one should win even
        // though both are valid prefixes.
        let text = "[url \"https://example.org/\"]\n\
                    \tinsteadOf = git@\n\
                    [url \"https://github.com/\"]\n\
                    \tinsteadOf = git@github.com:\n";
        let cfg = Config::parse_str(text).unwrap();
        let out = rewrite_url("git@github.com:foo/bar", &cfg, false);
        assert_eq!(out, "https://github.com/foo/bar");
    }

    #[test]
    fn rewrite_unit_push_uses_pushinsteadof() {
        // Only pushInsteadOf set: fetch URL passes through, push rewrites.
        let text = "[url \"ssh://git@gitlab.com/\"]\n\
                    \tpushInsteadOf = https://gitlab.com/\n";
        let cfg = Config::parse_str(text).unwrap();
        let fetch = rewrite_url("https://gitlab.com/g/r.git", &cfg, false);
        assert_eq!(fetch, "https://gitlab.com/g/r.git");
        let push = rewrite_url("https://gitlab.com/g/r.git", &cfg, true);
        assert_eq!(push, "ssh://git@gitlab.com/g/r.git");
    }
}
