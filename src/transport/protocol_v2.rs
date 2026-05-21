//! Protocol v2 grammar — capability advertisement parsing, `ls-refs` and
//! `fetch` command framing/response decoding.
//!
//! This module sits on top of Track A's pkt-line transport. It owns:
//!
//!   - [`CapabilityAdvertisement`] — parses the server's `version 2` response
//!     into a structured view: agent string, `object-format`, plus a map of
//!     command → list of supported arguments.
//!   - [`ls_refs`] — builds the `command=ls-refs` request body, sends it,
//!     parses every `<oid> <ref>[ symref-target:<r>][ peeled:<oid>]` line into
//!     an [`AdvertisedRef`].
//!   - [`fetch`] — builds the `command=fetch` request body for a "no-haves"
//!     clone (with `done\n`), decodes the sideband-64k packfile stream, and
//!     surfaces channel-2 (progress) / channel-3 (fatal) messages.
//!
//! Reference: `gitprotocol-v2(5)` in upstream git's documentation.
//!
//! The split with Track A is strict: this module never touches HTTP. It only
//! consumes/produces `PktLine`s through the [`Connection`] trait. That makes
//! the request-byte tests below pure (no network) and lets the integrator
//! drive both ends from in-memory buffers in `tests/m10_compat.rs`.

use std::collections::BTreeMap;
use std::io::Read;

use crate::hash::{HashKind, ObjectId};
use crate::transport::pkt_line::{PktLine, PktLineReader};
use crate::transport::{delim_pkt, encode_data_pkt, flush_pkt, Connection, TransportError};

// ---------------------------------------------------------------------------
// Capability advertisement
// ---------------------------------------------------------------------------

/// Parsed form of the server's initial `version 2\n …flush` response.
#[derive(Debug, Clone)]
pub struct CapabilityAdvertisement {
    /// Optional `agent=<id>` line.
    pub agent: Option<String>,
    /// `object-format=<algorithm>` — defaults to SHA-1 when absent.
    pub object_format: HashKind,
    /// `command name → list of supported arguments`.
    ///
    /// A line like `fetch=shallow ofs-delta wait-for-done\n` becomes the key
    /// `"fetch"` with values `["shallow", "ofs-delta", "wait-for-done"]`.
    /// A line with no `=` (e.g. `ls-refs\n`) yields an empty `Vec`.
    pub commands: BTreeMap<String, Vec<String>>,
}

impl Default for CapabilityAdvertisement {
    fn default() -> Self {
        // Per `gitprotocol-v2(5)`, the SHA-1 algorithm is the default when
        // `object-format` is absent from the advertisement.
        Self {
            agent: None,
            object_format: HashKind::Sha1,
            commands: BTreeMap::new(),
        }
    }
}

impl CapabilityAdvertisement {
    /// Parse a sequence of `PktLine`s. The first line must be exactly
    /// `version 2`; otherwise we report a clean error so callers can surface
    /// it to the user. Anything after the terminating `Flush` is ignored.
    pub fn parse(pkts: &[PktLine]) -> Result<Self, ProtocolError> {
        let mut iter = pkts.iter();
        let first = iter
            .next()
            .ok_or_else(|| ProtocolError::Advertisement("empty pkt stream".to_string()))?;
        match first {
            PktLine::Data(bytes) => {
                let line = std::str::from_utf8(bytes).map_err(|_| {
                    ProtocolError::Advertisement("first line is not valid UTF-8".to_string())
                })?;
                let line = line.trim_end_matches('\n');
                if line != "version 2" {
                    return Err(ProtocolError::Advertisement(format!(
                        "expected 'version 2' as first line, got {line:?}"
                    )));
                }
            }
            other => {
                return Err(ProtocolError::Advertisement(format!(
                    "expected version data line as first pkt, got {other:?}"
                )));
            }
        }

        // Default has `object_format = Sha1`, per the v2 spec.
        let mut out = CapabilityAdvertisement::default();

        for pkt in iter {
            match pkt {
                PktLine::Flush => break,
                PktLine::Delim | PktLine::ResponseEnd => {
                    // Shouldn't appear in the v2 capability advertisement, but
                    // we tolerate them defensively — some proxies normalize
                    // pkt-line streams.
                    continue;
                }
                PktLine::Data(bytes) => {
                    let line = std::str::from_utf8(bytes).map_err(|_| {
                        ProtocolError::Advertisement(
                            "capability line is not valid UTF-8".to_string(),
                        )
                    })?;
                    let line = line.trim_end_matches('\n');
                    if line.is_empty() {
                        continue;
                    }
                    // `key[=values]`. We split on the first `=`; everything
                    // after is space-separated arguments.
                    let (key, values) = match line.split_once('=') {
                        Some((k, v)) => (k, Some(v)),
                        None => (line, None),
                    };
                    match (key, values) {
                        ("agent", Some(v)) => out.agent = Some(v.to_string()),
                        ("object-format", Some(v)) => {
                            out.object_format = HashKind::parse(v)?;
                        }
                        // Anything that names a server command lives in the
                        // commands map. We also tuck "metadata" capabilities
                        // (`server-option`, `session-id`, …) in there with
                        // empty value lists so callers can probe them via
                        // `supports`.
                        _ => {
                            let args = values
                                .map(|v| {
                                    v.split_ascii_whitespace().map(|s| s.to_string()).collect()
                                })
                                .unwrap_or_default();
                            out.commands.insert(key.to_string(), args);
                        }
                    }
                }
            }
        }

        Ok(out)
    }

    /// True if the server advertised the named command/capability.
    pub fn supports(&self, cmd: &str) -> bool {
        self.commands.contains_key(cmd)
    }
}

// ---------------------------------------------------------------------------
// ls-refs
// ---------------------------------------------------------------------------

/// One entry from an `ls-refs` response.
#[derive(Debug, Clone)]
pub struct AdvertisedRef {
    /// Object id the ref points at.
    pub oid: ObjectId,
    /// Full ref name (e.g. `HEAD`, `refs/heads/main`).
    pub name: String,
    /// For `HEAD` and other symrefs, the underlying target (e.g.
    /// `refs/heads/main`). `None` for non-symbolic refs.
    pub symref_target: Option<String>,
    /// For annotated tags, the oid the tag dereferences to. `None` otherwise.
    pub peeled: Option<ObjectId>,
}

/// Issue an `ls-refs` request with the given `ref-prefix` filters and parse
/// the response into a Vec.
pub fn ls_refs(
    conn: &mut dyn Connection,
    ref_prefixes: &[&str],
    hash_kind: HashKind,
) -> Result<Vec<AdvertisedRef>, ProtocolError> {
    crate::trace!("net", "ls-refs (prefixes: {})", ref_prefixes.len());
    let body = build_ls_refs_request(ref_prefixes);
    let response = conn.send_request(body)?;
    let pkts = read_all_pkts(response)?;
    let parsed = parse_ls_refs_response(&pkts, hash_kind)?;
    crate::trace!("net", "ls-refs got {} advertised refs", parsed.len());
    Ok(parsed)
}

/// Build the raw pkt-line body for an `ls-refs` request. Kept separate from
/// the send so we can unit-test the byte sequence without a connection.
fn build_ls_refs_request(ref_prefixes: &[&str]) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&encode_data_pkt(b"command=ls-refs\n"));
    body.extend_from_slice(&delim_pkt());
    body.extend_from_slice(&encode_data_pkt(b"peel\n"));
    body.extend_from_slice(&encode_data_pkt(b"symrefs\n"));
    for prefix in ref_prefixes {
        let line = format!("ref-prefix {prefix}\n");
        body.extend_from_slice(&encode_data_pkt(line.as_bytes()));
    }
    body.extend_from_slice(&flush_pkt());
    body
}

/// Parse the `<oid> <ref-name>[ symref-target:<ref>][ peeled:<oid>]\n` stream
/// terminated by a flush.
fn parse_ls_refs_response(
    pkts: &[PktLine],
    hash_kind: HashKind,
) -> Result<Vec<AdvertisedRef>, ProtocolError> {
    let mut out = Vec::new();
    for pkt in pkts {
        match pkt {
            PktLine::Flush => break,
            PktLine::Delim | PktLine::ResponseEnd => continue,
            PktLine::Data(bytes) => {
                let line = std::str::from_utf8(bytes)
                    .map_err(|_| ProtocolError::LsRefs("non-UTF-8 ref line".to_string()))?;
                let line = line.trim_end_matches('\n');
                if line.is_empty() {
                    continue;
                }
                out.push(parse_ls_refs_line(line, hash_kind)?);
            }
        }
    }
    Ok(out)
}

fn parse_ls_refs_line(line: &str, hash_kind: HashKind) -> Result<AdvertisedRef, ProtocolError> {
    // `<oid> SP <ref-name> [SP "symref-target:" <ref>] [SP "peeled:" <oid>]`
    let mut parts = line.split(' ');
    let oid_hex = parts
        .next()
        .ok_or_else(|| ProtocolError::LsRefs(format!("missing oid in line: {line:?}")))?;
    let name = parts
        .next()
        .ok_or_else(|| ProtocolError::LsRefs(format!("missing ref-name in line: {line:?}")))?;
    let oid = ObjectId::parse_hex(hash_kind, oid_hex)?;

    let mut symref_target = None;
    let mut peeled = None;
    for attr in parts {
        if let Some(target) = attr.strip_prefix("symref-target:") {
            symref_target = Some(target.to_string());
        } else if let Some(hex) = attr.strip_prefix("peeled:") {
            peeled = Some(ObjectId::parse_hex(hash_kind, hex)?);
        } else if attr.is_empty() {
            continue;
        } else {
            // Unknown attribute — silently ignore. Future spec extensions land
            // here and we shouldn't fail a clone over them.
        }
    }

    Ok(AdvertisedRef {
        oid,
        name: name.to_string(),
        symref_target,
        peeled,
    })
}

// ---------------------------------------------------------------------------
// fetch
// ---------------------------------------------------------------------------

/// What a `fetch` call returns.
pub struct FetchResult {
    /// Raw pack bytes the server sent us, with all sideband framing already
    /// stripped. Suitable for writing straight to a `.pack` file.
    pub pack_bytes: Vec<u8>,
}

/// Issue a `command=fetch` request asking for `wants` (no haves — this is the
/// clone case). Returns the concatenated channel-1 (pack) bytes from the
/// sideband-64k response.
pub fn fetch(
    conn: &mut dyn Connection,
    wants: &[ObjectId],
    hash_kind: HashKind,
) -> Result<FetchResult, ProtocolError> {
    crate::trace!("net", "fetch wants={}", wants.len());
    let body = build_fetch_request(wants, hash_kind);
    let response = conn.send_request(body)?;
    let pkts = read_all_pkts(response)?;
    let pack_bytes = parse_fetch_response(&pkts)?;
    crate::trace!("net", "fetch got pack of {} bytes", pack_bytes.len());
    Ok(FetchResult { pack_bytes })
}

/// Build the raw pkt-line body for a `fetch` request. Kept separate for
/// byte-level tests.
///
/// **NON_GOALS.md Batch D — declined capabilities**: this builder
/// deliberately does NOT opt in to:
///
/// - `packfile-uris` — protocol-v2 capability that lets a server respond
///   with a list of pre-built `.pack` URLs the client should download
///   out-of-band instead of streaming objects in the response. We always
///   want a single in-protocol stream so the same code path covers
///   github.com, gitlab, gitea, and self-hosted servers.
/// - `filter` — partial-clone hint (`--filter=blob:none` etc.). The plan
///   explicitly defers partial clone; opting in would mean we silently
///   skip blob fetches that the rest of rustygit assumes are present.
///
/// **`bundle-uri`** is a separate protocol-v2 command (`command=bundle-uri`),
/// not a fetch capability. We never send `command=bundle-uri`, so even
/// if a server advertises it we get a normal fetch. See
/// [`CapabilityAdvertisement::parse`] for the receive side — declared
/// capabilities are stored without error, never acted on.
fn build_fetch_request(wants: &[ObjectId], _hash_kind: HashKind) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&encode_data_pkt(b"command=fetch\n"));
    body.extend_from_slice(&delim_pkt());
    // Common capabilities for the clone case. `no-progress` keeps the server
    // from sending channel-2 progress packets we'd otherwise have to drain.
    body.extend_from_slice(&encode_data_pkt(b"thin-pack\n"));
    body.extend_from_slice(&encode_data_pkt(b"ofs-delta\n"));
    body.extend_from_slice(&encode_data_pkt(b"no-progress\n"));
    for oid in wants {
        let line = format!("want {oid}\n");
        body.extend_from_slice(&encode_data_pkt(line.as_bytes()));
    }
    body.extend_from_slice(&encode_data_pkt(b"done\n"));
    body.extend_from_slice(&flush_pkt());
    body
}

/// Walk the section-structured fetch response and pull out the pack bytes.
///
/// Response shape (from `gitprotocol-v2(5)`):
///
/// ```text
/// output = *section
/// section = (acknowledgments | shallow-info | wanted-refs |
///            packfile-uris | packfile)
/// section = section-header *section-body section-end
/// section-header = "acknowledgments\n" | "shallow-info\n" | ... | "packfile\n"
/// section-end = delim-pkt | flush-pkt    // delim if more sections follow
/// ```
///
/// Inside the `packfile` section every Data pkt's first byte is the sideband
/// channel: 1 = pack data, 2 = progress (stderr-bound), 3 = fatal error.
fn parse_fetch_response(pkts: &[PktLine]) -> Result<Vec<u8>, ProtocolError> {
    let mut pack_bytes: Vec<u8> = Vec::new();
    let mut in_packfile = false;
    let mut saw_packfile = false;

    for pkt in pkts {
        match pkt {
            PktLine::Flush => {
                // Final flush ends the response. We *could* break here, but
                // robust servers sometimes flush mid-section (followed by
                // more sections in a later "phase" — not in our v2 case, but
                // worth not assuming).
                in_packfile = false;
            }
            PktLine::Delim => {
                // End of the current section.
                in_packfile = false;
            }
            PktLine::ResponseEnd => break,
            PktLine::Data(data) => {
                // Section headers are plain pkt-lines with a trailing newline.
                if let Some(name) = section_header_name(data) {
                    saw_packfile |= name == "packfile";
                    in_packfile = name == "packfile";
                    continue;
                }
                if !in_packfile {
                    // We're in some other section (acknowledgments,
                    // shallow-info, wanted-refs). For a clone with no haves
                    // the server may emit `ready\n` here; we don't care
                    // about its contents — just skip.
                    continue;
                }
                if data.is_empty() {
                    return Err(ProtocolError::Fetch(
                        "empty sideband packet in packfile section".to_string(),
                    ));
                }
                let channel = data[0];
                let payload = &data[1..];
                match channel {
                    1 => pack_bytes.extend_from_slice(payload),
                    2 => {
                        // Progress: route to stderr so a user sees server-side
                        // progress (counting objects, compressing, …). We've
                        // asked for `no-progress` but the server is free to
                        // ignore that.
                        let msg = String::from_utf8_lossy(payload);
                        eprint!("remote: {msg}");
                    }
                    3 => {
                        let msg = String::from_utf8_lossy(payload).to_string();
                        return Err(ProtocolError::ServerFatal(msg));
                    }
                    other => {
                        return Err(ProtocolError::Fetch(format!(
                            "unknown sideband channel {other}"
                        )));
                    }
                }
            }
        }
    }

    if !saw_packfile {
        return Err(ProtocolError::Fetch(
            "server response did not contain a packfile section".to_string(),
        ));
    }

    Ok(pack_bytes)
}

/// If `data` is one of the v2 section-header forms (`acknowledgments\n`,
/// `shallow-info\n`, `wanted-refs\n`, `packfile-uris\n`, `packfile\n`), return
/// the bare name; otherwise None.
fn section_header_name(data: &[u8]) -> Option<&'static str> {
    // Trim a single trailing LF for the comparison.
    let core = if data.last() == Some(&b'\n') {
        &data[..data.len() - 1]
    } else {
        data
    };
    match core {
        b"acknowledgments" => Some("acknowledgments"),
        b"shallow-info" => Some("shallow-info"),
        b"wanted-refs" => Some("wanted-refs"),
        b"packfile-uris" => Some("packfile-uris"),
        b"packfile" => Some("packfile"),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Drain a `Read` into a `Vec<PktLine>` using Track A's `PktLineReader`.
fn read_all_pkts<R: Read>(reader: R) -> Result<Vec<PktLine>, ProtocolError> {
    let mut out = Vec::new();
    let mut pr = PktLineReader::new(reader);
    while let Some(pkt) = pr.next_pkt()? {
        out.push(pkt);
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(thiserror::Error, Debug)]
pub enum ProtocolError {
    #[error(transparent)]
    Transport(#[from] TransportError),
    #[error(transparent)]
    Hash(#[from] crate::hash::HashError),
    #[error("malformed v2 advertisement: {0}")]
    Advertisement(String),
    #[error("malformed ls-refs response: {0}")]
    LsRefs(String),
    #[error("malformed fetch response: {0}")]
    Fetch(String),
    #[error("server returned fatal: {0}")]
    ServerFatal(String),
    #[error("server doesn't advertise '{0}' command")]
    UnsupportedCommand(String),
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: turn a list of `(data-without-trailing-LF)` lines + trailing
    /// flush into the pkt-line vector we'd get from `PktLineReader`.
    fn pkts(lines: &[&str]) -> Vec<PktLine> {
        let mut out: Vec<PktLine> = lines
            .iter()
            .map(|l| PktLine::Data(l.as_bytes().to_vec()))
            .collect();
        out.push(PktLine::Flush);
        out
    }

    // ---- 1. parse v2 advertisement -----------------------------------------

    #[test]
    fn parse_v2_advertisement_typical() {
        let cap = CapabilityAdvertisement::parse(&pkts(&[
            "version 2\n",
            "agent=git/2.45.0\n",
            "ls-refs=unborn\n",
            "fetch=shallow ofs-delta wait-for-done\n",
            "server-option\n",
            "object-format=sha1\n",
        ]))
        .expect("parses");

        assert_eq!(cap.agent.as_deref(), Some("git/2.45.0"));
        assert_eq!(cap.object_format, HashKind::Sha1);
        assert!(cap.supports("ls-refs"));
        assert!(cap.supports("fetch"));
        assert!(cap.supports("server-option"));
        assert_eq!(
            cap.commands.get("fetch").unwrap(),
            &vec![
                "shallow".to_string(),
                "ofs-delta".to_string(),
                "wait-for-done".to_string(),
            ]
        );
        assert_eq!(
            cap.commands.get("ls-refs").unwrap(),
            &vec!["unborn".to_string()]
        );
        // server-option has no values.
        assert!(cap.commands.get("server-option").unwrap().is_empty());
    }

    #[test]
    fn parse_v2_advertisement_sha256() {
        let cap = CapabilityAdvertisement::parse(&pkts(&[
            "version 2\n",
            "object-format=sha256\n",
            "ls-refs\n",
        ]))
        .unwrap();
        assert_eq!(cap.object_format, HashKind::Sha256);
    }

    // ---- 2. refuses non-v2 -------------------------------------------------

    #[test]
    fn refuses_non_v2_first_line() {
        let pkts = vec![
            PktLine::Data(b"# service=git-upload-pack\n".to_vec()),
            PktLine::Flush,
        ];
        let err = CapabilityAdvertisement::parse(&pkts).unwrap_err();
        match err {
            ProtocolError::Advertisement(s) => {
                assert!(s.contains("expected 'version 2'"), "wrong message: {s}");
            }
            other => panic!("expected Advertisement, got {other:?}"),
        }
    }

    #[test]
    fn refuses_empty_advertisement() {
        let err = CapabilityAdvertisement::parse(&[]).unwrap_err();
        match err {
            ProtocolError::Advertisement(_) => {}
            other => panic!("expected Advertisement, got {other:?}"),
        }
    }

    // ---- 3. parse ls-refs response -----------------------------------------

    #[test]
    fn parse_ls_refs_response_basic() {
        let hex_oid = "1111111111111111111111111111111111111111";
        let hex_target = "2222222222222222222222222222222222222222";
        let hex_peeled = "3333333333333333333333333333333333333333";

        let input = vec![
            PktLine::Data(format!("{hex_oid} HEAD symref-target:refs/heads/main\n").into_bytes()),
            PktLine::Data(format!("{hex_target} refs/heads/main\n").into_bytes()),
            PktLine::Data(format!("{hex_oid} refs/tags/v1 peeled:{hex_peeled}\n").into_bytes()),
            PktLine::Flush,
        ];

        let refs = parse_ls_refs_response(&input, HashKind::Sha1).unwrap();
        assert_eq!(refs.len(), 3);

        assert_eq!(refs[0].name, "HEAD");
        assert_eq!(refs[0].oid.to_string(), hex_oid);
        assert_eq!(refs[0].symref_target.as_deref(), Some("refs/heads/main"));
        assert!(refs[0].peeled.is_none());

        assert_eq!(refs[1].name, "refs/heads/main");
        assert_eq!(refs[1].oid.to_string(), hex_target);
        assert!(refs[1].symref_target.is_none());

        assert_eq!(refs[2].name, "refs/tags/v1");
        assert_eq!(refs[2].peeled.as_ref().unwrap().to_string(), hex_peeled);
    }

    #[test]
    fn parse_ls_refs_response_rejects_short_hex() {
        let input = vec![PktLine::Data(b"deadbeef HEAD\n".to_vec()), PktLine::Flush];
        let err = parse_ls_refs_response(&input, HashKind::Sha1).unwrap_err();
        // We surface this as a HashError-flavoured ProtocolError, since
        // `parse_hex` failed.
        match err {
            ProtocolError::Hash(_) => {}
            other => panic!("expected Hash error, got {other:?}"),
        }
    }

    // ---- 4. build fetch request bytes --------------------------------------

    #[test]
    fn build_fetch_request_bytes_match_spec() {
        let want1 = ObjectId::parse_hex(HashKind::Sha1, "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
            .unwrap();
        let want2 = ObjectId::parse_hex(HashKind::Sha1, "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb")
            .unwrap();
        let body = build_fetch_request(&[want1, want2], HashKind::Sha1);

        // Decode it back through a PktLineReader-equivalent walk so we don't
        // hard-code length-prefixes (those are Track A's concern).
        let pkts = read_all_pkts(std::io::Cursor::new(body.clone())).unwrap();

        // Filter out the structural framing — we just want the data lines.
        let datas: Vec<String> = pkts
            .iter()
            .filter_map(|p| match p {
                PktLine::Data(b) => Some(
                    std::str::from_utf8(b)
                        .unwrap()
                        .trim_end_matches('\n')
                        .to_string(),
                ),
                _ => None,
            })
            .collect();

        assert_eq!(
            datas,
            vec![
                "command=fetch".to_string(),
                "thin-pack".to_string(),
                "ofs-delta".to_string(),
                "no-progress".to_string(),
                format!("want {}", want1),
                format!("want {}", want2),
                "done".to_string(),
            ]
        );

        // Structure: we must see exactly one Delim between the command line
        // and the want-block, and exactly one Flush at the end.
        let frame_kinds: Vec<&str> = pkts
            .iter()
            .map(|p| match p {
                PktLine::Data(_) => "D",
                PktLine::Flush => "F",
                PktLine::Delim => "X",
                PktLine::ResponseEnd => "E",
            })
            .collect();
        assert_eq!(
            frame_kinds,
            vec!["D", "X", "D", "D", "D", "D", "D", "D", "F"],
            "frame structure for fetch request body changed"
        );
    }

    #[test]
    fn build_ls_refs_request_bytes_match_spec() {
        let body = build_ls_refs_request(&["HEAD", "refs/heads/", "refs/tags/"]);
        let pkts = read_all_pkts(std::io::Cursor::new(body)).unwrap();
        let datas: Vec<String> = pkts
            .iter()
            .filter_map(|p| match p {
                PktLine::Data(b) => Some(
                    std::str::from_utf8(b)
                        .unwrap()
                        .trim_end_matches('\n')
                        .to_string(),
                ),
                _ => None,
            })
            .collect();
        assert_eq!(
            datas,
            vec![
                "command=ls-refs".to_string(),
                "peel".to_string(),
                "symrefs".to_string(),
                "ref-prefix HEAD".to_string(),
                "ref-prefix refs/heads/".to_string(),
                "ref-prefix refs/tags/".to_string(),
            ]
        );
    }

    // ---- 5. sideband decoding ---------------------------------------------

    #[test]
    fn decode_sideband_packets() {
        // Build a fake packfile-section response: header + a few channel-1
        // pkts whose payloads concatenate to "PACK<rest>", a channel-2
        // progress pkt (to stderr — we don't assert), then flush.
        let pack_chunk_1 = b"PACK\x00\x00\x00\x02\x00\x00\x00";
        let pack_chunk_2 = b"\x03blob payload here";
        let progress = b"Counting objects: 3, done.\n";

        let mut ch1a = vec![1u8];
        ch1a.extend_from_slice(pack_chunk_1);
        let mut ch1b = vec![1u8];
        ch1b.extend_from_slice(pack_chunk_2);
        let mut ch2 = vec![2u8];
        ch2.extend_from_slice(progress);

        let pkts = vec![
            PktLine::Data(b"packfile\n".to_vec()),
            PktLine::Data(ch1a),
            PktLine::Data(ch2),
            PktLine::Data(ch1b),
            PktLine::Flush,
        ];

        let pack = parse_fetch_response(&pkts).expect("decodes");
        let mut expected = Vec::new();
        expected.extend_from_slice(pack_chunk_1);
        expected.extend_from_slice(pack_chunk_2);
        assert_eq!(pack, expected);
    }

    #[test]
    fn decode_sideband_channel3_is_fatal() {
        let pkts = vec![
            PktLine::Data(b"packfile\n".to_vec()),
            PktLine::Data({
                let mut v = vec![3u8];
                v.extend_from_slice(b"upload-pack: not our ref");
                v
            }),
            PktLine::Flush,
        ];
        let err = parse_fetch_response(&pkts).unwrap_err();
        match err {
            ProtocolError::ServerFatal(s) => {
                assert!(s.contains("not our ref"), "wrong message: {s}");
            }
            other => panic!("expected ServerFatal, got {other:?}"),
        }
    }

    #[test]
    fn decode_skips_intermediate_sections() {
        // acknowledgments → delim → packfile → ch1 → flush.
        let mut ch1 = vec![1u8];
        ch1.extend_from_slice(b"PACK....");
        let pkts = vec![
            PktLine::Data(b"acknowledgments\n".to_vec()),
            PktLine::Data(b"ready\n".to_vec()),
            PktLine::Delim,
            PktLine::Data(b"packfile\n".to_vec()),
            PktLine::Data(ch1),
            PktLine::Flush,
        ];
        let pack = parse_fetch_response(&pkts).expect("decodes");
        assert_eq!(pack, b"PACK....");
    }

    #[test]
    fn decode_requires_packfile_section() {
        // No packfile section at all → error.
        let pkts = vec![
            PktLine::Data(b"acknowledgments\n".to_vec()),
            PktLine::Data(b"NAK\n".to_vec()),
            PktLine::Flush,
        ];
        let err = parse_fetch_response(&pkts).unwrap_err();
        match err {
            ProtocolError::Fetch(s) => assert!(s.contains("packfile")),
            other => panic!("expected Fetch, got {other:?}"),
        }
    }

    // ----- NON_GOALS.md Batch D: bundle-uri / packfile-uris decline -----

    /// A capability advertisement that includes `bundle-uri` and
    /// `packfile-uris` MUST parse cleanly. The caps land in `commands`
    /// for inspection but we never act on them.
    #[test]
    fn parse_v2_advertisement_with_bundle_and_packfile_uris() {
        let pkts = vec![
            PktLine::Data(b"version 2\n".to_vec()),
            PktLine::Data(b"agent=git/2.45.0\n".to_vec()),
            PktLine::Data(b"ls-refs=unborn\n".to_vec()),
            PktLine::Data(b"fetch=shallow filter ref-in-want sideband-all\n".to_vec()),
            PktLine::Data(b"bundle-uri\n".to_vec()),
            // packfile-uris in the advertisement names allowed hash algorithms.
            PktLine::Data(b"packfile-uris=https\n".to_vec()),
            PktLine::Flush,
        ];
        let cap = CapabilityAdvertisement::parse(&pkts).expect("parse should succeed");
        assert!(
            cap.supports("bundle-uri"),
            "bundle-uri should land in commands map"
        );
        assert!(
            cap.supports("packfile-uris"),
            "packfile-uris should land in commands map"
        );
        assert!(cap.supports("ls-refs"));
        assert!(cap.supports("fetch"));
    }

    /// The fetch request body we generate MUST NOT include `packfile-uris`
    /// or `filter` capabilities. Lock in the byte pattern: we send
    /// thin-pack + ofs-delta + no-progress, then wants, then `done`,
    /// nothing more.
    #[test]
    fn fetch_request_does_not_opt_into_packfile_uris() {
        let hash = HashKind::Sha1;
        let oid = ObjectId::parse_hex(hash, "0123456789abcdef0123456789abcdef01234567").unwrap();
        let body = build_fetch_request(&[oid], hash);
        let printable = String::from_utf8_lossy(&body);

        // Required capabilities we DO send.
        assert!(
            printable.contains("thin-pack"),
            "should send thin-pack: {printable}"
        );
        assert!(
            printable.contains("ofs-delta"),
            "should send ofs-delta: {printable}"
        );
        assert!(
            printable.contains("no-progress"),
            "should send no-progress: {printable}"
        );

        // Forbidden capabilities — declined per NON_GOALS.md Batch D.
        assert!(
            !printable.contains("packfile-uris"),
            "must NOT opt in to packfile-uris (clone optimization is declined): {printable}"
        );
        assert!(
            !printable.contains("filter "),
            "must NOT send a filter spec (partial clone is out of scope): {printable}"
        );
        // bundle-uri is a separate command, not a fetch capability — confirm
        // we never even mention it.
        assert!(
            !printable.contains("bundle-uri"),
            "must NOT mention bundle-uri in the fetch body: {printable}"
        );
    }

    /// Capability advertisement parsing must NOT reject the `packfile-uris`
    /// section header inside a fetch response if a misconfigured server
    /// sent one — our `parse_fetch_response` already skips unknown
    /// sections; lock that in.
    #[test]
    fn fetch_response_with_packfile_uris_section_is_tolerated() {
        // Build a response with: packfile-uris section (which we skip),
        // then a normal packfile section. The packfile section contains
        // a single sideband-channel-1 packet so `parse_fetch_response`
        // has something to extract.
        let pkts = vec![
            PktLine::Data(b"packfile-uris\n".to_vec()),
            PktLine::Data(b"sha1-deadbeef https://cdn.example.com/foo.pack\n".to_vec()),
            PktLine::Delim,
            PktLine::Data(b"packfile\n".to_vec()),
            PktLine::Data({
                let mut v = vec![1u8]; // sideband channel
                v.extend_from_slice(b"PACK\x00\x00\x00\x02"); // fake pack header
                v
            }),
            PktLine::Flush,
        ];
        let pack = parse_fetch_response(&pkts).expect("should tolerate packfile-uris section");
        assert!(
            pack.starts_with(b"PACK"),
            "must extract the real pack bytes"
        );
    }
}
