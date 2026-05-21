//! Smart-HTTP-v2 transport.
//!
//! [`HttpConnection`] implements [`Connection`] against a remote git HTTP(S)
//! endpoint. Only `https://` is supported in M10 — `http://` (plain) is
//! rejected upfront so we don't accidentally send `Authorization` headers
//! over the wire in cleartext later. (`http://` may relax in a later
//! milestone; ssh:// and git:// belong elsewhere entirely.)
//!
//! The two HTTP exchanges this implements:
//!
//! 1. `discover_capabilities` → `GET <base>/info/refs?service=git-upload-pack`
//!    with `Git-Protocol: version=2`. A v2 server replies with a pkt-line
//!    stream whose first record is `version 2\n`. If we see anything else
//!    (typically the v1 service-line `# service=git-upload-pack`), we
//!    return [`TransportError::NotV2`] — no v1 fallback in M10.
//! 2. `send_request` → `POST <base>/git-upload-pack` with the appropriate
//!    `Content-Type`/`Accept`/`Git-Protocol` headers and the caller's
//!    pkt-line body. Track B owns the body framing; we just shuttle bytes
//!    and stream the response back as a `Read`.
//!
//! Spec references: `gitprotocol-v2(5)`, `gitprotocol-http(5)`.

use std::io::Read;

use super::pkt_line::{PktLine, PktLineReader};
use super::{Connection, TransportError};

const SERVICE: &str = "git-upload-pack";
const USER_AGENT: &str = "rustygit/0.1";
const GIT_PROTOCOL: &str = "version=2";
const CT_REQUEST: &str = "application/x-git-upload-pack-request";
const CT_RESULT: &str = "application/x-git-upload-pack-result";

/// HTTPS-only git client speaking protocol v2.
#[derive(Debug)]
pub struct HttpConnection {
    base_url: String,
    agent: ureq::Agent,
}

impl HttpConnection {
    /// Construct, validating the URL. `base_url` is the repo's URL WITHOUT
    /// the trailing `/info/refs` (e.g. `https://github.com/git/git.git`).
    pub fn new(base_url: &str) -> Result<Self, TransportError> {
        let trimmed = base_url.trim();
        if trimmed.is_empty() {
            return Err(TransportError::BadUrl(base_url.to_string()));
        }

        // Cheap scheme + structural check. We don't use a full URL parser
        // because the only thing we actually have to enforce here is "no
        // leaking plaintext"; ureq does its own URL parsing on the request.
        let scheme = match trimmed.find("://") {
            Some(i) => &trimmed[..i],
            None => return Err(TransportError::BadUrl(base_url.to_string())),
        };
        if scheme.is_empty() {
            return Err(TransportError::BadUrl(base_url.to_string()));
        }
        if !scheme.eq_ignore_ascii_case("https") {
            return Err(TransportError::UnsupportedScheme(scheme.to_string()));
        }
        // Reject trailing slashes — they confuse URL composition below.
        let base = trimmed.trim_end_matches('/').to_string();
        if base.len() <= scheme.len() + 3 {
            // Only "https://" with no host. ureq will reject this anyway
            // but we want a clean error type.
            return Err(TransportError::BadUrl(base_url.to_string()));
        }

        let agent = ureq::AgentBuilder::new().user_agent(USER_AGENT).build();

        Ok(Self {
            base_url: base,
            agent,
        })
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    fn info_refs_url(&self) -> String {
        format!("{}/info/refs?service={}", self.base_url, SERVICE)
    }

    fn upload_pack_url(&self) -> String {
        format!("{}/{}", self.base_url, SERVICE)
    }
}

impl Connection for HttpConnection {
    fn discover_capabilities(&mut self) -> Result<Vec<PktLine>, TransportError> {
        let url = self.info_refs_url();
        let resp = self
            .agent
            .get(&url)
            .set("Git-Protocol", GIT_PROTOCOL)
            .set("User-Agent", USER_AGENT)
            .call()
            .map_err(|e| ureq_to_transport(&url, e))?;

        let status = resp.status();
        if !(200..300).contains(&status) {
            // ureq treats >=400 as Status error and we caught those above,
            // but for completeness in case redirects land us on 3xx that
            // somehow bypass ureq's follow logic.
            return Err(TransportError::Http {
                url: url.clone(),
                status,
                body: resp.into_string().unwrap_or_default(),
            });
        }

        let mut reader = PktLineReader::new(resp.into_reader());
        let mut first = match reader.next_pkt()? {
            Some(p) => p,
            None => {
                return Err(TransportError::NotV2 {
                    first_line: String::new(),
                });
            }
        };

        // Some servers (github.com, gitlab) preface the v2 advertisement
        // with a v1-style `# service=git-upload-pack\n` + flush-pkt. That's
        // explicitly allowed by the http transport — see git's own
        // `remote-curl.c::process_response_advertisement`. Skip it and
        // continue to the real first pkt-line (which must be `version 2`).
        if let PktLine::Data(d) = &first {
            let line = strip_trailing_lf(d);
            if line.starts_with(b"# service=") {
                // Consume up to and including the next flush-pkt.
                loop {
                    match reader.next_pkt()? {
                        None => {
                            return Err(TransportError::NotV2 {
                                first_line: String::from_utf8_lossy(line).into_owned(),
                            });
                        }
                        Some(PktLine::Flush) => break,
                        Some(_) => continue,
                    }
                }
                // Now grab the actual first record of the v2 capability ad.
                first = match reader.next_pkt()? {
                    Some(p) => p,
                    None => {
                        return Err(TransportError::NotV2 {
                            first_line: "<eof after service header>".into(),
                        });
                    }
                };
            }
        }

        // The first record of a v2 capability advertisement MUST be
        // `version 2\n`. If we see anything else, the server isn't speaking
        // v2 and we have no v1 fallback in M10.
        match &first {
            PktLine::Data(d) if is_version_two(d) => {}
            other => {
                let preview = match other {
                    PktLine::Data(d) => String::from_utf8_lossy(d).into_owned(),
                    PktLine::Flush => "<flush>".into(),
                    PktLine::Delim => "<delim>".into(),
                    PktLine::ResponseEnd => "<response-end>".into(),
                };
                return Err(TransportError::NotV2 {
                    first_line: preview,
                });
            }
        }

        let mut out = Vec::new();
        out.push(first);
        // The rest of the advertisement, up to and including the trailing flush.
        while let Some(pkt) = reader.next_pkt()? {
            let done = matches!(pkt, PktLine::Flush);
            out.push(pkt);
            if done {
                break;
            }
        }
        Ok(out)
    }

    fn send_request(&mut self, body: Vec<u8>) -> Result<Box<dyn Read + Send>, TransportError> {
        let url = self.upload_pack_url();
        crate::trace!("net", "POST {} ({} bytes)", url, body.len());
        let resp = self
            .agent
            .post(&url)
            .set("Content-Type", CT_REQUEST)
            .set("Accept", CT_RESULT)
            .set("Git-Protocol", GIT_PROTOCOL)
            .set("User-Agent", USER_AGENT)
            .send_bytes(&body)
            .map_err(|e| ureq_to_transport(&url, e))?;

        let status = resp.status();
        crate::trace!("net", "← {} {}", url, status);
        if !(200..300).contains(&status) {
            return Err(TransportError::Http {
                url: url.clone(),
                status,
                body: resp.into_string().unwrap_or_default(),
            });
        }

        // `into_reader` returns `Box<dyn Read + Send + Sync + 'static>` —
        // we narrow the bound to `Read + Send` to match the trait.
        Ok(resp.into_reader())
    }
}

/// `payload` typically ends in `\n`; the spec mandates `"version 2" LF` but
/// we accept the bare prefix too for paranoia.
fn is_version_two(payload: &[u8]) -> bool {
    let p = strip_trailing_lf(payload);
    p == b"version 2"
}

fn strip_trailing_lf(b: &[u8]) -> &[u8] {
    if let Some((&b'\n', rest)) = b.split_last() {
        rest
    } else {
        b
    }
}

/// Funnel a `ureq::Error` into our typed [`TransportError`]. `Status` errors
/// carry both the code and the response body (which servers love to put
/// useful detail into — e.g. github's "Repository not found.").
fn ureq_to_transport(url: &str, err: ureq::Error) -> TransportError {
    match err {
        ureq::Error::Status(code, resp) => {
            let body = resp.into_string().unwrap_or_default();
            TransportError::Http {
                url: url.to_string(),
                status: code,
                body,
            }
        }
        ureq::Error::Transport(t) => {
            // Try to surface the underlying io::Error so callers can match
            // on `ErrorKind::ConnectionRefused` etc. ureq doesn't always
            // wrap an io::Error, so fall through to a string error.
            if let Some(src) = std::error::Error::source(&t) {
                if let Some(io_err) = src.downcast_ref::<std::io::Error>() {
                    return TransportError::Io(std::io::Error::new(
                        io_err.kind(),
                        io_err.to_string(),
                    ));
                }
            }
            TransportError::Ureq(t.to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_validation_rejects_garbage() {
        let err = HttpConnection::new("not a url").unwrap_err();
        assert!(matches!(err, TransportError::BadUrl(_)));
    }

    #[test]
    fn url_validation_rejects_empty() {
        let err = HttpConnection::new("").unwrap_err();
        assert!(matches!(err, TransportError::BadUrl(_)));
        let err = HttpConnection::new("   ").unwrap_err();
        assert!(matches!(err, TransportError::BadUrl(_)));
    }

    #[test]
    fn url_validation_rejects_scheme_only() {
        let err = HttpConnection::new("https://").unwrap_err();
        assert!(matches!(err, TransportError::BadUrl(_)));
    }

    #[test]
    fn scheme_check_rejects_ftp() {
        let err = HttpConnection::new("ftp://example.com/repo.git").unwrap_err();
        assert!(matches!(err, TransportError::UnsupportedScheme(_)));
    }

    #[test]
    fn scheme_check_rejects_plain_http() {
        let err = HttpConnection::new("http://example.com/repo.git").unwrap_err();
        match err {
            TransportError::UnsupportedScheme(s) => assert_eq!(s, "http"),
            other => panic!("expected UnsupportedScheme, got {other:?}"),
        }
    }

    #[test]
    fn scheme_check_rejects_ssh() {
        let err = HttpConnection::new("ssh://git@example.com/repo.git").unwrap_err();
        assert!(matches!(err, TransportError::UnsupportedScheme(_)));
    }

    #[test]
    fn url_normalization_strips_trailing_slash() {
        let c = HttpConnection::new("https://example.com/foo.git/").unwrap();
        assert_eq!(c.base_url(), "https://example.com/foo.git");
        assert_eq!(
            c.info_refs_url(),
            "https://example.com/foo.git/info/refs?service=git-upload-pack"
        );
        assert_eq!(
            c.upload_pack_url(),
            "https://example.com/foo.git/git-upload-pack"
        );
    }

    #[test]
    fn https_accepted_case_insensitive() {
        assert!(HttpConnection::new("HTTPS://example.com/repo.git").is_ok());
        assert!(HttpConnection::new("Https://example.com/repo.git").is_ok());
    }

    /// Live network test against github.com. Skips silently when there's no
    /// network — we treat any transport/io error as "no network" and bail
    /// rather than failing the suite. Only acceptance-level signal: when a
    /// network IS available, the response's first pkt-line says
    /// `version 2\n`.
    #[test]
    fn live_https_discover_capabilities_round_trip() {
        let mut conn = match HttpConnection::new("https://github.com/git/git.git") {
            Ok(c) => c,
            Err(e) => panic!("construction failed: {e}"),
        };
        let pkts = match conn.discover_capabilities() {
            Ok(p) => p,
            Err(TransportError::Io(_)) | Err(TransportError::Ureq(_)) => {
                eprintln!("skipping live HTTPS test: no network");
                return;
            }
            Err(other) => panic!("discover_capabilities failed: {other:?}"),
        };
        assert!(!pkts.is_empty(), "expected at least one pkt-line");
        match &pkts[0] {
            PktLine::Data(d) => {
                assert!(
                    d.starts_with(b"version 2\n") || d == b"version 2",
                    "first pkt-line was not 'version 2': {:?}",
                    String::from_utf8_lossy(d)
                );
            }
            other => panic!("first pkt-line was not Data: {other:?}"),
        }
        // The advertisement must end in a flush-pkt.
        assert!(
            matches!(pkts.last(), Some(PktLine::Flush)),
            "advertisement did not end in flush-pkt: {:?}",
            pkts.last()
        );
    }
}
