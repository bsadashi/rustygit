//! `send-pack` (push) wire protocol over smart-HTTP, **protocol v1**.
//!
//! Push is the opposite of fetch. Server-side handler is `git-receive-pack`
//! (rather than `git-upload-pack`). The wire dialect is protocol-v1 even
//! when fetch is protocol-v2 — the v2 `command=push` extension exists but
//! isn't broadly deployed yet, so M11 sticks with v1 for interoperability.
//!
//! Two HTTP exchanges:
//!
//! 1. `GET <base>/info/refs?service=git-receive-pack` — the v1-style ref
//!    advertisement. Unlike v2 there is no `version 2\n` preface; the
//!    response is a plain pkt-line stream of `<oid> SP <refname> [NUL caps]`
//!    records, optionally prefaced by a smart-HTTP `# service=...` line and
//!    flush. The first record carries a NUL-separated capability stanza;
//!    subsequent records do not.
//! 2. `POST <base>/git-receive-pack` — request body is pkt-line update
//!    commands (with the same NUL-capability convention on the first one)
//!    followed by a flush and then the raw, *un-framed* packfile bytes.
//!    Response is the `report-status` pkt-line stream, optionally wrapped
//!    in sideband-64k framing.
//!
//! Spec references:
//!   - `gitprotocol-pack(5)` ("Pushing Data To a Server" section)
//!   - `gitprotocol-http(5)` for the smart-HTTP transport envelope
//!
//! Design notes for this module:
//!   - We carry our own [`ReceivePackConnection`] rather than extending
//!     [`super::HttpConnection`]. `HttpConnection` is hard-wired to the v2
//!     dialect (it validates `version 2\n` as the first record, sends the
//!     `Git-Protocol: version=2` header, and points at the
//!     `git-upload-pack` URL paths). Threading a "v1 push mode" through it
//!     would muddy the v2 surface for no real win — v1 push has its own
//!     content types, accepts no v2 preface, and reads a substantially
//!     different ad shape. A peer struct keeps each protocol clean.
//!   - Encoding helpers are kept pure (`encode_request`,
//!     `negotiate_capabilities`, `parse_report_status`) so tests don't need
//!     a network round-trip.

use std::io::Read;

use crate::hash::{HashKind, ObjectId};
use crate::transport::pkt_line::{encode_data_pkt, flush_pkt, PktLine, PktLineReader};
use crate::transport::TransportError;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const SERVICE: &str = "git-receive-pack";
const USER_AGENT: &str = "rustygit/0.1";
const CT_REQUEST: &str = "application/x-git-receive-pack-request";
const CT_RESULT: &str = "application/x-git-receive-pack-result";
/// Value advertised in the `agent=` capability when sending push commands.
const AGENT_CAP: &str = "agent=rustygit/0.1";

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Parsed v1 receive-pack reference advertisement.
#[derive(Debug, Clone)]
pub struct ReceivePackAdvertisement {
    /// Refs the server currently has. Empty for an empty repository.
    pub refs: Vec<AdvertisedRef>,
    /// Capabilities the server supports (from the first ref's `NUL caps`
    /// stanza, or the `capabilities^{}` pseudo-ref when the repo is empty).
    pub capabilities: Vec<String>,
    /// Server-reported hash algorithm. SHA-1 by default unless the server
    /// advertised `object-format=sha256`.
    pub object_format: HashKind,
}

/// One entry from a receive-pack ref advertisement.
#[derive(Debug, Clone)]
pub struct AdvertisedRef {
    pub oid: ObjectId,
    pub name: String,
}

/// One reference update the client wants the server to apply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PushCommand {
    /// Create a brand-new ref. The wire encoding uses zero-id for `old`.
    Create { name: String, new: ObjectId },
    /// Update an existing ref. Sends both `old` and `new`.
    Update {
        name: String,
        old: ObjectId,
        new: ObjectId,
    },
    /// Delete a ref. The wire encoding uses zero-id for `new` (and requires
    /// the server to have advertised `delete-refs`, which the caller is
    /// responsible for checking).
    Delete { name: String, old: ObjectId },
}

impl PushCommand {
    /// Refname this command targets.
    pub fn name(&self) -> &str {
        match self {
            PushCommand::Create { name, .. }
            | PushCommand::Update { name, .. }
            | PushCommand::Delete { name, .. } => name,
        }
    }

    /// Old oid as it'll appear on the wire (zero-id for `Create`).
    pub fn old_oid(&self, hash_kind: HashKind) -> ObjectId {
        match self {
            PushCommand::Create { .. } => ObjectId::null(hash_kind),
            PushCommand::Update { old, .. } | PushCommand::Delete { old, .. } => *old,
        }
    }

    /// New oid as it'll appear on the wire (zero-id for `Delete`).
    pub fn new_oid(&self, hash_kind: HashKind) -> ObjectId {
        match self {
            PushCommand::Delete { .. } => ObjectId::null(hash_kind),
            PushCommand::Create { new, .. } | PushCommand::Update { new, .. } => *new,
        }
    }

    /// True if this command requires the server to advertise `delete-refs`.
    pub fn requires_delete_refs(&self) -> bool {
        matches!(self, PushCommand::Delete { .. })
    }

    /// True if this command needs a packfile body. Per the v1 spec, the
    /// pack must be sent whenever a `create` or `update` is present, even
    /// if the server already has every object (in which case it's an empty
    /// pack). Delete-only pushes must NOT send a pack.
    pub fn needs_pack(&self) -> bool {
        !matches!(self, PushCommand::Delete { .. })
    }
}

/// Parsed `report-status` response from the server.
#[derive(Debug, Clone)]
pub struct ReportStatus {
    /// `true` iff the server replied `unpack ok\n`.
    pub unpack_ok: bool,
    /// The message after `unpack ` if it wasn't `ok`.
    pub unpack_message: Option<String>,
    /// Per-ref status, one entry per `ok <ref>` / `ng <ref> <reason>` line.
    pub command_results: Vec<RefStatus>,
}

#[derive(Debug, Clone)]
pub struct RefStatus {
    pub name: String,
    pub ok: bool,
    /// `Some(reason)` for `ng` lines, `None` for `ok`.
    pub message: Option<String>,
}

#[derive(thiserror::Error, Debug)]
pub enum SendPackError {
    #[error(transparent)]
    Transport(#[from] TransportError),
    #[error(transparent)]
    Hash(#[from] crate::hash::HashError),
    #[error("malformed v1 receive-pack advertisement: {0}")]
    Advertisement(String),
    #[error("malformed report-status response: {0}")]
    ReportStatus(String),
    #[error("server rejected push: {0}")]
    Rejected(String),
    #[error("non-fast-forward update for ref '{0}' (use --force to override)")]
    NonFastForward(String),
}

// ---------------------------------------------------------------------------
// Connection: HTTPS-only client speaking v1 receive-pack
// ---------------------------------------------------------------------------

/// HTTPS-only git push transport. Mirrors `HttpConnection` in shape but hits
/// the `git-receive-pack` endpoints and speaks the v1 wire dialect.
#[derive(Debug)]
pub struct ReceivePackConnection {
    base_url: String,
    agent: ureq::Agent,
}

impl ReceivePackConnection {
    /// Construct, validating the URL. `base_url` should be the repo URL
    /// *without* a trailing `/info/refs` (e.g. `https://github.com/x/y.git`).
    ///
    /// Does NOT apply `[url "<base>"] insteadOf / pushInsteadOf` rewrites —
    /// see [`ReceivePackConnection::new_with_config`] for the rewrite-aware
    /// constructor. This shape is kept so existing callsites and tests that
    /// build a connection directly from a literal URL keep working.
    pub fn new(base_url: &str) -> Result<Self, TransportError> {
        Self::new_inner(base_url)
    }

    /// Like [`ReceivePackConnection::new`] but applies the user's URL
    /// rewrites first. `for_push` is `true` here because this connection
    /// only ever speaks `git-receive-pack`, so `pushInsteadOf` wins over
    /// `insteadOf`.
    pub fn new_with_config(
        base_url: &str,
        cfg: &crate::config::Config,
    ) -> Result<Self, TransportError> {
        let rewritten = crate::transport::rewrite_url(base_url, cfg, /* for_push = */ true);
        Self::new_inner(&rewritten)
    }

    fn new_inner(base_url: &str) -> Result<Self, TransportError> {
        let trimmed = base_url.trim();
        if trimmed.is_empty() {
            return Err(TransportError::BadUrl(base_url.to_string()));
        }
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
        let base = trimmed.trim_end_matches('/').to_string();
        if base.len() <= scheme.len() + 3 {
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

    fn receive_pack_url(&self) -> String {
        format!("{}/{}", self.base_url, SERVICE)
    }

    /// `GET /info/refs?service=git-receive-pack` and parse the v1 ad.
    ///
    /// The smart-HTTP transport prefaces the ad with a `# service=...\n`
    /// pkt-line plus a flush before the real ad begins; we transparently
    /// skip it.
    pub fn discover(&self) -> Result<ReceivePackAdvertisement, SendPackError> {
        let url = self.info_refs_url();
        let resp = self
            .agent
            .get(&url)
            .set("User-Agent", USER_AGENT)
            .call()
            .map_err(|e| ureq_to_transport(&url, e))?;

        let status = resp.status();
        if !(200..300).contains(&status) {
            return Err(SendPackError::Transport(TransportError::Http {
                url: url.clone(),
                status,
                body: resp.into_string().unwrap_or_default(),
            }));
        }

        let pkts = read_all_pkts(resp.into_reader())?;
        parse_advertisement(&pkts)
    }

    /// `POST /git-receive-pack` with the encoded body. Returns the parsed
    /// report-status. The caller must request `report-status` in
    /// [`encode_request`] for the server to send one back.
    pub fn send(&self, body: Vec<u8>) -> Result<ReportStatus, SendPackError> {
        let url = self.receive_pack_url();
        let resp = self
            .agent
            .post(&url)
            .set("Content-Type", CT_REQUEST)
            .set("Accept", CT_RESULT)
            .set("User-Agent", USER_AGENT)
            .send_bytes(&body)
            .map_err(|e| ureq_to_transport(&url, e))?;

        let status = resp.status();
        if !(200..300).contains(&status) {
            return Err(SendPackError::Transport(TransportError::Http {
                url: url.clone(),
                status,
                body: resp.into_string().unwrap_or_default(),
            }));
        }

        let pkts = read_all_pkts(resp.into_reader())?;
        parse_report_status(&pkts)
    }
}

// ---------------------------------------------------------------------------
// Advertisement parsing
// ---------------------------------------------------------------------------

/// Parse the v1 receive-pack ref advertisement from a list of pkt-lines.
///
/// Shape of the input we accept:
///
/// ```text
/// (optional)   "# service=git-receive-pack\n"   Flush
///              <oid> SP <refname> NUL <capability-list>\n
///              <oid> SP <refname>\n
///              ...
///              Flush
/// ```
///
/// For an empty repo the single ref line is:
///
/// ```text
///              <zero-oid> SP "capabilities^{}" NUL <capability-list>\n
/// ```
///
/// The `capabilities^{}` pseudo-ref carries caps but produces no
/// [`AdvertisedRef`].
pub fn parse_advertisement(pkts: &[PktLine]) -> Result<ReceivePackAdvertisement, SendPackError> {
    let mut iter = pkts.iter();

    // Optional smart-HTTP service header.
    let first = match iter.next() {
        Some(p) => p,
        None => {
            return Err(SendPackError::Advertisement(
                "empty pkt-line stream".to_string(),
            ));
        }
    };

    let mut first_ref_pkt: Option<&PktLine> = None;

    match first {
        PktLine::Data(d) if starts_with_service_header(d) => {
            // Consume up to and including the flush after the service header.
            let mut saw_flush = false;
            for pkt in iter.by_ref() {
                if matches!(pkt, PktLine::Flush) {
                    saw_flush = true;
                    break;
                }
            }
            if !saw_flush {
                return Err(SendPackError::Advertisement(
                    "service header not terminated by flush".to_string(),
                ));
            }
        }
        PktLine::Data(_) => {
            // No service header — this is the first real ref line.
            first_ref_pkt = Some(first);
        }
        PktLine::Flush => {
            // Just a flush with no data — server has nothing at all to say.
            return Err(SendPackError::Advertisement(
                "advertisement contains no records".to_string(),
            ));
        }
        PktLine::Delim | PktLine::ResponseEnd => {
            return Err(SendPackError::Advertisement(format!(
                "unexpected leading control packet: {first:?}"
            )));
        }
    }

    // First real ref record. If we already snagged it (no service header
    // case), reuse; otherwise pull the next pkt.
    let first_ref = match first_ref_pkt {
        Some(p) => p,
        None => match iter.next() {
            Some(p) => p,
            None => {
                // A receive-pack ad with only the service header and a flush
                // is degenerate but we treat it as "no refs, no caps".
                return Ok(ReceivePackAdvertisement {
                    refs: Vec::new(),
                    capabilities: Vec::new(),
                    object_format: HashKind::Sha1,
                });
            }
        },
    };

    let first_data = match first_ref {
        PktLine::Data(d) => d,
        PktLine::Flush => {
            // No refs and no caps. Unusual but legal — empty repo without
            // even the `capabilities^{}` line. Treat as fully empty.
            return Ok(ReceivePackAdvertisement {
                refs: Vec::new(),
                capabilities: Vec::new(),
                object_format: HashKind::Sha1,
            });
        }
        other => {
            return Err(SendPackError::Advertisement(format!(
                "expected first ref data line, got {other:?}"
            )));
        }
    };

    let (first_oid_hex, first_name, capabilities) = split_first_ref_line(first_data)?;
    let object_format = detect_hash_kind(&capabilities, first_oid_hex.len())?;

    let mut refs = Vec::new();
    let is_empty_repo_marker = first_name == "capabilities^{}";
    if !is_empty_repo_marker {
        let oid = ObjectId::parse_hex(object_format, first_oid_hex)?;
        refs.push(AdvertisedRef {
            oid,
            name: first_name.to_string(),
        });
    }

    // Remaining refs until the trailing flush.
    for pkt in iter {
        match pkt {
            PktLine::Flush => break,
            PktLine::Delim | PktLine::ResponseEnd => continue,
            PktLine::Data(bytes) => {
                let line = strip_trailing_lf(bytes);
                if line.is_empty() {
                    continue;
                }
                let line_str = std::str::from_utf8(line)
                    .map_err(|_| SendPackError::Advertisement("non-UTF-8 ref line".to_string()))?;
                // Subsequent refs MUST NOT carry a NUL cap stanza; if one
                // sneaks in, splitting on space-first is still safe — we
                // just want the oid and the name.
                let (oid_hex, name) = line_str.split_once(' ').ok_or_else(|| {
                    SendPackError::Advertisement(format!("malformed ref line: {line_str:?}"))
                })?;
                // Peeled refs ("<oid> <ref>^{}") are still refs from the
                // ad's POV; the caller can filter by suffix if it cares.
                let oid = ObjectId::parse_hex(object_format, oid_hex)?;
                refs.push(AdvertisedRef {
                    oid,
                    name: name.to_string(),
                });
            }
        }
    }

    Ok(ReceivePackAdvertisement {
        refs,
        capabilities,
        object_format,
    })
}

/// Split a first-ref data line into `(oid_hex, refname, capability list)`.
/// Format: `"<oid> <refname>\0<caps>\n"`.
fn split_first_ref_line(data: &[u8]) -> Result<(&str, &str, Vec<String>), SendPackError> {
    let trimmed = strip_trailing_lf(data);
    // Find the NUL boundary that separates ref+oid from the capability list.
    let nul_idx = trimmed.iter().position(|&b| b == 0).ok_or_else(|| {
        SendPackError::Advertisement(
            "first ref line missing NUL-separated capability stanza".to_string(),
        )
    })?;
    let (head, caps) = trimmed.split_at(nul_idx);
    let caps = &caps[1..]; // skip the NUL itself

    let head_str = std::str::from_utf8(head)
        .map_err(|_| SendPackError::Advertisement("non-UTF-8 first ref line".to_string()))?;
    let caps_str = std::str::from_utf8(caps)
        .map_err(|_| SendPackError::Advertisement("non-UTF-8 capability stanza".to_string()))?;

    let (oid_hex, name) = head_str.split_once(' ').ok_or_else(|| {
        SendPackError::Advertisement(format!("malformed first ref line: {head_str:?}"))
    })?;

    let capabilities = caps_str
        .split_ascii_whitespace()
        .map(|s| s.to_string())
        .collect::<Vec<_>>();

    Ok((oid_hex, name, capabilities))
}

/// Pick a hash algorithm based on the capability stanza, falling back to
/// the OID hex length if `object-format=` is absent.
fn detect_hash_kind(
    capabilities: &[String],
    oid_hex_len: usize,
) -> Result<HashKind, SendPackError> {
    for cap in capabilities {
        if let Some(value) = cap.strip_prefix("object-format=") {
            return Ok(HashKind::parse(value)?);
        }
    }
    match oid_hex_len {
        40 => Ok(HashKind::Sha1),
        64 => Ok(HashKind::Sha256),
        // Default to SHA-1 — most servers won't advertise object-format
        // and oid length is the only other tell.
        _ => Ok(HashKind::Sha1),
    }
}

fn starts_with_service_header(data: &[u8]) -> bool {
    let line = strip_trailing_lf(data);
    line.starts_with(b"# service=")
}

fn strip_trailing_lf(b: &[u8]) -> &[u8] {
    if let Some((&b'\n', rest)) = b.split_last() {
        rest
    } else {
        b
    }
}

// ---------------------------------------------------------------------------
// Capability negotiation
// ---------------------------------------------------------------------------

/// Decide which capabilities to request given what the server advertised.
/// Always includes `report-status` so we can parse the response (without it
/// the server stays silent and we can't surface per-ref failures). Adds
/// `side-band-64k` and `ofs-delta` only if the server supports them, and
/// always appends our `agent=` line.
pub fn negotiate_capabilities(server_caps: &[String]) -> String {
    let mut out: Vec<&str> = Vec::new();
    let has = |c: &str| server_caps.iter().any(|s| s == c);

    // report-status: we always want a status report. Servers that don't
    // advertise it will still parse the cap list and most likely ignore
    // unknown tokens — but the spec lets them reject, so callers should
    // verify with `negotiate_capabilities_strict` when paranoid.
    out.push("report-status");

    if has("side-band-64k") {
        out.push("side-band-64k");
    }
    if has("ofs-delta") {
        out.push("ofs-delta");
    }

    // Build the final string. agent= must be one of the tokens.
    let mut s = out.join(" ");
    if !s.is_empty() {
        s.push(' ');
    }
    s.push_str(AGENT_CAP);
    s
}

// ---------------------------------------------------------------------------
// Request encoding
// ---------------------------------------------------------------------------

/// Build the request body for `POST /git-receive-pack`:
///
/// ```text
/// pkt-line(<old> SP <new> SP <ref>\0<cap-request>\n)      // first command
/// pkt-line(<old> SP <new> SP <ref>\n)                     // remaining
/// ...
/// flush-pkt
/// <raw pack bytes>                                          // NOT pkt-line framed
/// ```
///
/// Per the spec, the pack is NOT pkt-line wrapped — it's appended raw, and
/// the server reads it as the rest of the HTTP request body. A delete-only
/// command list MUST NOT include any pack bytes; callers should pass an
/// empty slice in that case (the function does NOT enforce this — that's a
/// porcelain decision).
pub fn encode_request(
    commands: &[PushCommand],
    pack_bytes: &[u8],
    cap_request: &str,
    hash_kind: HashKind,
) -> Vec<u8> {
    let mut out = Vec::new();
    for (i, cmd) in commands.iter().enumerate() {
        let line = if i == 0 {
            // First command attaches the capability stanza after a NUL.
            format_command_with_caps(cmd, cap_request, hash_kind)
        } else {
            format_command(cmd, hash_kind)
        };
        out.extend_from_slice(&encode_data_pkt(line.as_bytes()));
    }
    out.extend_from_slice(&flush_pkt());
    out.extend_from_slice(pack_bytes);
    out
}

fn format_command(cmd: &PushCommand, hash_kind: HashKind) -> String {
    let old = cmd.old_oid(hash_kind);
    let new = cmd.new_oid(hash_kind);
    format!("{} {} {}\n", old, new, cmd.name())
}

fn format_command_with_caps(cmd: &PushCommand, caps: &str, hash_kind: HashKind) -> String {
    let old = cmd.old_oid(hash_kind);
    let new = cmd.new_oid(hash_kind);
    format!("{} {} {}\0{}\n", old, new, cmd.name(), caps)
}

// ---------------------------------------------------------------------------
// report-status parsing
// ---------------------------------------------------------------------------

/// Parse a `report-status` pkt-line stream into a [`ReportStatus`].
///
/// Accepts both sideband-wrapped and bare forms — if every data pkt's
/// first byte is one of 1/2/3, we treat it as sideband-64k and demux on
/// channel 1 before parsing.
///
/// Grammar (after sideband demux):
///
/// ```text
/// report-status   = unpack-status 1*(command-status) flush-pkt
/// unpack-status   = "unpack" SP ("ok" / <err-msg>) "\n"
/// command-status  = ("ok" SP <ref> / "ng" SP <ref> SP <reason>) "\n"
/// ```
pub fn parse_report_status(pkts: &[PktLine]) -> Result<ReportStatus, SendPackError> {
    let demuxed = demux_sideband_if_needed(pkts)?;
    let lines = report_lines(&demuxed);

    let mut iter = lines.into_iter();
    let unpack_line = iter
        .next()
        .ok_or_else(|| SendPackError::ReportStatus("missing unpack-status line".to_string()))?;
    let unpack_payload = unpack_line.strip_prefix("unpack ").ok_or_else(|| {
        SendPackError::ReportStatus(format!(
            "expected 'unpack <result>' as first line, got {unpack_line:?}"
        ))
    })?;
    let (unpack_ok, unpack_message) = if unpack_payload == "ok" {
        (true, None)
    } else {
        (false, Some(unpack_payload.to_string()))
    };

    let mut command_results = Vec::new();
    for line in iter {
        if let Some(name) = line.strip_prefix("ok ") {
            command_results.push(RefStatus {
                name: name.to_string(),
                ok: true,
                message: None,
            });
        } else if let Some(rest) = line.strip_prefix("ng ") {
            // `ng <refname> SP <reason>`. Refnames don't contain spaces
            // per refname rules, so splitting once on space is correct.
            let (name, reason) = rest.split_once(' ').ok_or_else(|| {
                SendPackError::ReportStatus(format!("malformed ng line (missing reason): {line:?}"))
            })?;
            command_results.push(RefStatus {
                name: name.to_string(),
                ok: false,
                message: Some(reason.to_string()),
            });
        } else {
            // Tolerate `option ...` lines (report-status-v2 extension) by
            // silently skipping — they're attached to the preceding `ok`.
            if line.starts_with("option ") {
                continue;
            }
            return Err(SendPackError::ReportStatus(format!(
                "unrecognized report line: {line:?}"
            )));
        }
    }

    Ok(ReportStatus {
        unpack_ok,
        unpack_message,
        command_results,
    })
}

/// Decide whether the given pkt-line vector is sideband-wrapped (every
/// data pkt starts with channel byte 1/2/3) and, if so, return a new
/// vector containing only the channel-1 payload re-framed as pkt-lines.
/// Otherwise return a shallow copy of the input.
fn demux_sideband_if_needed(pkts: &[PktLine]) -> Result<Vec<PktLine>, SendPackError> {
    if !looks_like_sideband(pkts) {
        return Ok(pkts.to_vec());
    }

    // Collect channel-1 bytes; surface channel-3 as a hard error; drop ch2.
    let mut ch1: Vec<u8> = Vec::new();
    for pkt in pkts {
        match pkt {
            PktLine::Data(data) => {
                if data.is_empty() {
                    return Err(SendPackError::ReportStatus(
                        "empty data pkt in sideband stream".to_string(),
                    ));
                }
                match data[0] {
                    1 => ch1.extend_from_slice(&data[1..]),
                    2 => {
                        // Progress; route to stderr like fetch does.
                        let msg = String::from_utf8_lossy(&data[1..]);
                        eprint!("remote: {msg}");
                    }
                    3 => {
                        let msg = String::from_utf8_lossy(&data[1..]).to_string();
                        return Err(SendPackError::Rejected(msg));
                    }
                    other => {
                        return Err(SendPackError::ReportStatus(format!(
                            "unknown sideband channel {other}"
                        )));
                    }
                }
            }
            PktLine::Flush | PktLine::Delim | PktLine::ResponseEnd => {
                // The outer flush ends the sideband stream; the inner
                // (channel-1) pkts already carry their own flush. Stop
                // emitting on the outer flush.
            }
        }
    }

    // Now reparse the channel-1 byte stream as pkt-lines.
    let inner = read_all_pkts(std::io::Cursor::new(ch1))?;
    Ok(inner)
}

/// Heuristic: every data pkt-line starts with a byte in {1,2,3}. The
/// non-sideband report-status format starts with `unpack`/`ok`/`ng`, none
/// of which begin with a low control byte.
fn looks_like_sideband(pkts: &[PktLine]) -> bool {
    let mut saw_data = false;
    for pkt in pkts {
        if let PktLine::Data(d) = pkt {
            saw_data = true;
            match d.first() {
                Some(&1) | Some(&2) | Some(&3) => continue,
                _ => return false,
            }
        }
    }
    saw_data
}

/// Pull out the `\n`-trimmed data lines from a report-status pkt-line
/// stream, stopping at the trailing flush.
fn report_lines(pkts: &[PktLine]) -> Vec<String> {
    let mut out = Vec::new();
    for pkt in pkts {
        match pkt {
            PktLine::Flush => break,
            PktLine::Delim | PktLine::ResponseEnd => continue,
            PktLine::Data(d) => {
                let s = String::from_utf8_lossy(strip_trailing_lf(d)).into_owned();
                if !s.is_empty() {
                    out.push(s);
                }
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn read_all_pkts<R: Read>(reader: R) -> Result<Vec<PktLine>, SendPackError> {
    let mut out = Vec::new();
    let mut pr = PktLineReader::new(reader);
    while let Some(pkt) = pr.next_pkt()? {
        out.push(pkt);
    }
    Ok(out)
}

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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::pkt_line::PktLineWriter;

    // --------- helpers -----------------------------------------------------

    fn oid_1s() -> String {
        "1".repeat(40)
    }
    fn oid_2s() -> String {
        "2".repeat(40)
    }
    fn oid_3s() -> String {
        "3".repeat(40)
    }
    fn zero_oid() -> String {
        "0".repeat(40)
    }

    /// Build a v1 advertisement byte buffer with a smart-HTTP service
    /// header preface, then a list of refs and a trailing flush. The
    /// first ref's capability stanza is `caps` (joined by spaces).
    fn build_ad_bytes(first: &str, rest: &[&str], caps: &str) -> Vec<u8> {
        let mut buf = Vec::new();
        {
            let mut w = PktLineWriter::new(&mut buf);
            w.write_data(b"# service=git-receive-pack\n").unwrap();
            w.write_flush().unwrap();
            // first ref + NUL + caps + LF
            let mut first_line = first.as_bytes().to_vec();
            first_line.push(0u8);
            first_line.extend_from_slice(caps.as_bytes());
            first_line.push(b'\n');
            w.write_data(&first_line).unwrap();
            for r in rest {
                let mut line = r.as_bytes().to_vec();
                line.push(b'\n');
                w.write_data(&line).unwrap();
            }
            w.write_flush().unwrap();
        }
        buf
    }

    // --------- 1. parse v1 ad with refs ------------------------------------

    #[test]
    fn parse_v1_ad_with_refs() {
        let oid1 = oid_1s();
        let oid2 = oid_2s();
        let oid3 = oid_3s();
        let first = format!("{oid1} refs/heads/main");
        let other_a = format!("{oid2} refs/heads/dev");
        let other_b = format!("{oid3} refs/tags/v1");
        let bytes = build_ad_bytes(
            &first,
            &[&other_a, &other_b],
            "report-status delete-refs side-band-64k ofs-delta agent=git/2.45",
        );

        let pkts = read_all_pkts(std::io::Cursor::new(bytes)).unwrap();
        let ad = parse_advertisement(&pkts).expect("parses");

        assert_eq!(ad.refs.len(), 3);
        assert_eq!(ad.refs[0].name, "refs/heads/main");
        assert_eq!(ad.refs[0].oid.to_string(), oid1);
        assert_eq!(ad.refs[1].name, "refs/heads/dev");
        assert_eq!(ad.refs[1].oid.to_string(), oid2);
        assert_eq!(ad.refs[2].name, "refs/tags/v1");
        assert_eq!(ad.refs[2].oid.to_string(), oid3);

        assert!(ad.capabilities.iter().any(|c| c == "report-status"));
        assert!(ad.capabilities.iter().any(|c| c == "delete-refs"));
        assert!(ad.capabilities.iter().any(|c| c == "side-band-64k"));
        assert!(ad.capabilities.iter().any(|c| c == "ofs-delta"));
        assert!(ad.capabilities.iter().any(|c| c.starts_with("agent=")));

        assert_eq!(ad.object_format, HashKind::Sha1);
    }

    // --------- 2. parse v1 ad with empty repo ------------------------------

    #[test]
    fn parse_v1_ad_empty_repo() {
        // `<zero-oid> capabilities^{}\0<caps>\n` then flush.
        let first = format!("{} capabilities^{{}}", zero_oid());
        let bytes = build_ad_bytes(
            &first,
            &[],
            "report-status delete-refs ofs-delta agent=git/2.45",
        );
        let pkts = read_all_pkts(std::io::Cursor::new(bytes)).unwrap();
        let ad = parse_advertisement(&pkts).expect("parses");
        assert!(ad.refs.is_empty(), "empty repo should produce no refs");
        assert!(ad.capabilities.iter().any(|c| c == "report-status"));
        assert!(ad.capabilities.iter().any(|c| c == "delete-refs"));
    }

    #[test]
    fn parse_v1_ad_detects_sha256() {
        let oid_sha256 = "ab".repeat(32); // 64 hex chars
        let first = format!("{oid_sha256} refs/heads/main");
        let bytes = build_ad_bytes(
            &first,
            &[],
            "report-status object-format=sha256 agent=git/2.45",
        );
        let pkts = read_all_pkts(std::io::Cursor::new(bytes)).unwrap();
        let ad = parse_advertisement(&pkts).unwrap();
        assert_eq!(ad.object_format, HashKind::Sha256);
        assert_eq!(ad.refs[0].oid.kind(), HashKind::Sha256);
    }

    #[test]
    fn parse_v1_ad_without_service_header() {
        // Some servers / git-daemon don't emit `# service=...`. We must
        // still parse the ad starting at the first ref line.
        let oid1 = oid_1s();
        let first = format!("{oid1} refs/heads/main");
        let mut buf = Vec::new();
        {
            let mut w = PktLineWriter::new(&mut buf);
            let mut line = first.as_bytes().to_vec();
            line.push(0u8);
            line.extend_from_slice(b"report-status agent=git/2.45");
            line.push(b'\n');
            w.write_data(&line).unwrap();
            w.write_flush().unwrap();
        }
        let pkts = read_all_pkts(std::io::Cursor::new(buf)).unwrap();
        let ad = parse_advertisement(&pkts).expect("parses without service line");
        assert_eq!(ad.refs.len(), 1);
        assert_eq!(ad.refs[0].name, "refs/heads/main");
    }

    #[test]
    fn parse_v1_ad_missing_caps_nul_is_error() {
        // First ref line without a NUL — malformed.
        let oid1 = oid_1s();
        let mut buf = Vec::new();
        {
            let mut w = PktLineWriter::new(&mut buf);
            w.write_data(b"# service=git-receive-pack\n").unwrap();
            w.write_flush().unwrap();
            let line = format!("{oid1} refs/heads/main\n");
            w.write_data(line.as_bytes()).unwrap();
            w.write_flush().unwrap();
        }
        let pkts = read_all_pkts(std::io::Cursor::new(buf)).unwrap();
        let err = parse_advertisement(&pkts).unwrap_err();
        assert!(
            matches!(err, SendPackError::Advertisement(ref s) if s.contains("NUL")),
            "expected NUL-related error, got {err:?}"
        );
    }

    // --------- 3. encode a Create -----------------------------------------

    #[test]
    fn encode_request_create_command() {
        let new = ObjectId::parse_hex(HashKind::Sha1, &oid_2s()).unwrap();
        let cmd = PushCommand::Create {
            name: "refs/heads/main".to_string(),
            new,
        };
        let pack = b"PACK\0\0\0\x02\0\0\0\0RANDOM";
        let body = encode_request(
            &[cmd],
            pack,
            "report-status agent=rustygit/0.1",
            HashKind::Sha1,
        );

        // Read pkt-lines off the FRONT only — stop at the first flush so
        // the raw pack bytes (which start with "PACK", decoded as bad hex)
        // never reach the pkt-line decoder.
        let pre_flush = decode_pkts_until_flush(&body);
        assert_eq!(pre_flush.len(), 1, "expected 1 data pkt before flush");
        let data = match &pre_flush[0] {
            PktLine::Data(d) => d.clone(),
            _ => panic!("first pkt not data"),
        };
        // shape: "<zero> <new> <ref>\0<caps>\n"
        let nul_pos = data.iter().position(|&b| b == 0).expect("has NUL");
        let head = std::str::from_utf8(&data[..nul_pos]).unwrap();
        let caps = std::str::from_utf8(&data[nul_pos + 1..]).unwrap();
        assert_eq!(head, format!("{} {} refs/heads/main", zero_oid(), oid_2s()));
        assert!(caps.starts_with("report-status agent=rustygit/0.1"));
        assert!(caps.ends_with('\n'));

        // And the pack bytes appear AFTER the flush, raw. We can't just
        // string-search for "0000" because the command line itself
        // contains the zero-oid (40 '0's). Instead, compute the byte
        // offset by replaying the pkt-line framing: 4-byte header +
        // payload for each data pkt, +4 for the flush.
        let mut offset = 0usize;
        for pkt in &pre_flush {
            if let PktLine::Data(d) = pkt {
                offset += 4 + d.len();
            }
        }
        offset += 4; // the flush itself
        let after_flush = &body[offset..];
        assert_eq!(after_flush, pack, "raw pack bytes appear after flush");
    }

    // --------- 4. encode multiple commands --------------------------------

    #[test]
    fn encode_request_multiple_commands_only_first_has_caps() {
        let new1 = ObjectId::parse_hex(HashKind::Sha1, &oid_1s()).unwrap();
        let new2 = ObjectId::parse_hex(HashKind::Sha1, &oid_2s()).unwrap();
        let old3 = ObjectId::parse_hex(HashKind::Sha1, &oid_3s()).unwrap();
        let cmds = vec![
            PushCommand::Create {
                name: "refs/heads/a".to_string(),
                new: new1,
            },
            PushCommand::Update {
                name: "refs/heads/b".to_string(),
                old: old3,
                new: new2,
            },
            PushCommand::Delete {
                name: "refs/heads/c".to_string(),
                old: old3,
            },
        ];
        let body = encode_request(
            &cmds,
            b"",
            "report-status agent=rustygit/0.1",
            HashKind::Sha1,
        );

        let pre_flush = decode_pkts_until_flush(&body);
        assert_eq!(pre_flush.len(), 3, "one data pkt per command");

        // First has NUL caps, others must not.
        for (i, pkt) in pre_flush.iter().enumerate() {
            let data = match pkt {
                PktLine::Data(d) => d.clone(),
                _ => panic!("not data"),
            };
            let has_nul = data.contains(&0u8);
            if i == 0 {
                assert!(has_nul, "first pkt must carry NUL caps");
            } else {
                assert!(!has_nul, "pkt #{i} must NOT have NUL caps stanza");
            }
        }

        // Spot-check the third command — it's a Delete and its new-oid
        // should be all zeros.
        let third = match &pre_flush[2] {
            PktLine::Data(d) => std::str::from_utf8(d).unwrap().to_string(),
            _ => panic!(),
        };
        let third = third.trim_end_matches('\n');
        let parts: Vec<&str> = third.split(' ').collect();
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[0], oid_3s());
        assert_eq!(parts[1], zero_oid());
        assert_eq!(parts[2], "refs/heads/c");
    }

    // --------- 5. parse report-status (no sideband) -----------------------

    #[test]
    fn parse_report_status_plain() {
        let pkts = vec![
            PktLine::Data(b"unpack ok\n".to_vec()),
            PktLine::Data(b"ok refs/heads/main\n".to_vec()),
            PktLine::Data(b"ng refs/heads/x non-fast-forward\n".to_vec()),
            PktLine::Flush,
        ];
        let rs = parse_report_status(&pkts).expect("parses");
        assert!(rs.unpack_ok);
        assert!(rs.unpack_message.is_none());
        assert_eq!(rs.command_results.len(), 2);
        assert_eq!(rs.command_results[0].name, "refs/heads/main");
        assert!(rs.command_results[0].ok);
        assert!(rs.command_results[0].message.is_none());
        assert_eq!(rs.command_results[1].name, "refs/heads/x");
        assert!(!rs.command_results[1].ok);
        assert_eq!(
            rs.command_results[1].message.as_deref(),
            Some("non-fast-forward")
        );
    }

    #[test]
    fn parse_report_status_unpack_failure() {
        let pkts = vec![
            PktLine::Data(b"unpack index-pack abort\n".to_vec()),
            PktLine::Flush,
        ];
        let rs = parse_report_status(&pkts).unwrap();
        assert!(!rs.unpack_ok);
        assert_eq!(rs.unpack_message.as_deref(), Some("index-pack abort"));
        assert!(rs.command_results.is_empty());
    }

    #[test]
    fn parse_report_status_missing_unpack_line_is_error() {
        let pkts = vec![PktLine::Flush];
        let err = parse_report_status(&pkts).unwrap_err();
        assert!(matches!(err, SendPackError::ReportStatus(_)));
    }

    // --------- 6. parse report-status (sideband-64k) ----------------------

    #[test]
    fn parse_report_status_sideband() {
        // Build an inner pkt-line stream...
        let mut inner = Vec::new();
        {
            let mut w = PktLineWriter::new(&mut inner);
            w.write_data(b"unpack ok\n").unwrap();
            w.write_data(b"ok refs/heads/main\n").unwrap();
            w.write_flush().unwrap();
        }
        // ...and wrap each chunk as a channel-1 sideband data pkt.
        // For simplicity emit the whole inner stream as a single ch1 pkt.
        let mut ch1 = vec![1u8];
        ch1.extend_from_slice(&inner);
        let mut ch2 = vec![2u8];
        ch2.extend_from_slice(b"progress: 100%\n");

        let outer = vec![PktLine::Data(ch1), PktLine::Data(ch2), PktLine::Flush];

        let rs = parse_report_status(&outer).expect("parses sideband");
        assert!(rs.unpack_ok);
        assert_eq!(rs.command_results.len(), 1);
        assert_eq!(rs.command_results[0].name, "refs/heads/main");
        assert!(rs.command_results[0].ok);
    }

    #[test]
    fn parse_report_status_sideband_channel3_is_rejected() {
        let mut ch3 = vec![3u8];
        ch3.extend_from_slice(b"fatal: server hates us");
        let outer = vec![PktLine::Data(ch3), PktLine::Flush];
        let err = parse_report_status(&outer).unwrap_err();
        match err {
            SendPackError::Rejected(s) => assert!(s.contains("server hates us")),
            other => panic!("expected Rejected, got {other:?}"),
        }
    }

    // --------- 7. capability negotiation ----------------------------------

    #[test]
    fn negotiate_caps_always_requests_report_status() {
        let server: Vec<String> = vec![];
        let caps = negotiate_capabilities(&server);
        assert!(caps.contains("report-status"));
        assert!(caps.contains("agent="));
    }

    #[test]
    fn negotiate_caps_includes_sideband_if_supported() {
        let server = vec![
            "report-status".to_string(),
            "side-band-64k".to_string(),
            "ofs-delta".to_string(),
            "delete-refs".to_string(),
        ];
        let caps = negotiate_capabilities(&server);
        assert!(caps.contains("side-band-64k"));
        assert!(caps.contains("ofs-delta"));
    }

    #[test]
    fn negotiate_caps_omits_unsupported() {
        let server = vec!["report-status".to_string()];
        let caps = negotiate_capabilities(&server);
        assert!(!caps.contains("side-band-64k"));
        assert!(!caps.contains("ofs-delta"));
    }

    // --------- 8. PushCommand helpers -------------------------------------

    #[test]
    fn push_command_wire_oids() {
        let oid_a = ObjectId::parse_hex(HashKind::Sha1, &oid_1s()).unwrap();
        let oid_b = ObjectId::parse_hex(HashKind::Sha1, &oid_2s()).unwrap();
        let null = ObjectId::null(HashKind::Sha1);

        let create = PushCommand::Create {
            name: "x".into(),
            new: oid_a,
        };
        assert_eq!(create.old_oid(HashKind::Sha1), null);
        assert_eq!(create.new_oid(HashKind::Sha1), oid_a);
        assert!(create.needs_pack());

        let update = PushCommand::Update {
            name: "x".into(),
            old: oid_a,
            new: oid_b,
        };
        assert_eq!(update.old_oid(HashKind::Sha1), oid_a);
        assert_eq!(update.new_oid(HashKind::Sha1), oid_b);
        assert!(update.needs_pack());

        let delete = PushCommand::Delete {
            name: "x".into(),
            old: oid_a,
        };
        assert_eq!(delete.old_oid(HashKind::Sha1), oid_a);
        assert_eq!(delete.new_oid(HashKind::Sha1), null);
        assert!(!delete.needs_pack());
        assert!(delete.requires_delete_refs());
    }

    // --------- 9. URL validation mirrors HttpConnection -------------------

    #[test]
    fn connection_rejects_non_https() {
        let err = ReceivePackConnection::new("http://example.com/r.git").unwrap_err();
        assert!(matches!(err, TransportError::UnsupportedScheme(_)));
    }

    #[test]
    fn connection_rejects_garbage_url() {
        let err = ReceivePackConnection::new("not a url").unwrap_err();
        assert!(matches!(err, TransportError::BadUrl(_)));
    }

    #[test]
    fn connection_normalizes_trailing_slash() {
        let c = ReceivePackConnection::new("https://example.com/r.git/").unwrap();
        assert_eq!(c.base_url(), "https://example.com/r.git");
        assert_eq!(
            c.info_refs_url(),
            "https://example.com/r.git/info/refs?service=git-receive-pack"
        );
        assert_eq!(
            c.receive_pack_url(),
            "https://example.com/r.git/git-receive-pack"
        );
    }

    // --------- 10. Network test: discover only (read-only) ----------------

    /// Live test. We can't push to a random repo, but `info/refs?
    /// service=git-receive-pack` is a public GET on GitHub for read-only
    /// access and returns the same advertisement shape we'd see if we had
    /// write rights. Skip silently when offline.
    #[test]
    fn live_discover_against_github() {
        let conn = match ReceivePackConnection::new("https://github.com/octocat/Hello-World.git") {
            Ok(c) => c,
            Err(e) => panic!("construction failed: {e}"),
        };
        let ad = match conn.discover() {
            Ok(a) => a,
            Err(SendPackError::Transport(TransportError::Io(_)))
            | Err(SendPackError::Transport(TransportError::Ureq(_))) => {
                eprintln!("skipping live receive-pack discover: no network");
                return;
            }
            Err(SendPackError::Transport(TransportError::Http { status, .. }))
                if status == 401 || status == 403 =>
            {
                // GitHub may require auth for receive-pack on some repos.
                // The Hello-World repo is normally open, but be tolerant.
                eprintln!("skipping live receive-pack discover: server demanded auth");
                return;
            }
            Err(other) => panic!("discover failed: {other:?}"),
        };
        // Hello-World has at least a master branch (or main).
        assert!(
            !ad.refs.is_empty(),
            "expected at least one ref in receive-pack ad"
        );
        // The cap stanza should mention at least one well-known capability.
        assert!(
            ad.capabilities
                .iter()
                .any(|c| c == "report-status" || c.starts_with("report-status-v2")),
            "capabilities did not include report-status: {:?}",
            ad.capabilities
        );
    }

    // --------- internal helpers -------------------------------------------

    /// Decode pkt-lines off the front of `buf` and return everything up to
    /// (but not including) the first flush-pkt. Stops before consuming any
    /// bytes after the flush, so callers can safely have raw post-flush
    /// payload (like a pack file) in the same buffer.
    fn decode_pkts_until_flush(buf: &[u8]) -> Vec<PktLine> {
        let mut pr = PktLineReader::new(std::io::Cursor::new(buf));
        let mut out = Vec::new();
        loop {
            match pr.next_pkt().unwrap() {
                Some(PktLine::Flush) => break,
                Some(p) => out.push(p),
                None => break,
            }
        }
        out
    }
}
