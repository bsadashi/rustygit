//! `rustygit verify-tag` — verify the PGP signature on an annotated tag.
//!
//! Unlike commits (which carry the signature in a `gpgsig` header), signed
//! tags append the armored PGP block to the *message body* itself,
//! immediately after the user's tag message and before the end of the
//! object. The signature is computed over the tag bytes with the PGP
//! block stripped.
//!
//! Exit codes mirror `git verify-tag`:
//!   * `0` — good, trusted signature
//!   * `1` — bad signature or unknown key
//!   * `128` — tag doesn't exist, isn't a tag object, or has no signature

use std::io;

use clap::Args;

use crate::config::Config;
use crate::object::ObjectKind;
use crate::repo::Repository;
use crate::revparse::resolve;
use crate::signing::{GpgSigner, Signer, VerifyOutcome};

#[derive(Debug, Args)]
pub struct VerifyTagArgs {
    /// One or more tag refs / oids to verify.
    #[arg(value_name = "TAG", required = true)]
    pub tags: Vec<String>,

    /// Accepted for upstream-flag parity.
    #[arg(short = 'v', long = "verbose")]
    pub verbose: bool,
}

const PGP_BEGIN: &[u8] = b"-----BEGIN PGP SIGNATURE-----";
const PGP_END: &[u8] = b"-----END PGP SIGNATURE-----";

pub fn run(args: VerifyTagArgs) -> io::Result<i32> {
    let repo = Repository::discover_from_cwd().map_err(io_err)?;
    let config = Config::from_repo_dir(repo.gitdir()).map_err(io_err)?;
    let signer = GpgSigner::from_config(&config);

    let mut worst: i32 = 0;
    for rev in &args.tags {
        let oid = resolve(repo.refs(), repo.odb(), rev).map_err(io_err)?;
        let obj = repo.odb().read(&oid).map_err(io_err)?;
        if obj.kind != ObjectKind::Tag {
            eprintln!("rustygit: verify-tag: {rev} is not a tag");
            worst = worst.max(128);
            continue;
        }
        let (payload, signature) = match split_signed_tag(&obj.data) {
            Some(s) => s,
            None => {
                eprintln!("rustygit: verify-tag: {rev} has no signature");
                worst = worst.max(128);
                continue;
            }
        };

        match signer.verify(&payload, &signature).map_err(io_err)? {
            VerifyOutcome::Good {
                fingerprint,
                signer: who,
            } => {
                eprintln!(
                    "rustygit: verify-tag: {rev}: GOODSIG{}{}",
                    who.as_deref().map(|s| format!(" {s}")).unwrap_or_default(),
                    fingerprint
                        .as_deref()
                        .map(|s| format!(" (fingerprint {s})"))
                        .unwrap_or_default(),
                );
            }
            VerifyOutcome::UnknownKey => {
                eprintln!(
                    "rustygit: verify-tag: {rev}: signature OK but signing key is not in our keyring"
                );
                worst = worst.max(1);
            }
            VerifyOutcome::Bad { reason } => {
                eprintln!("rustygit: verify-tag: {rev}: BADSIG: {reason}");
                worst = worst.max(1);
            }
        }
    }
    Ok(worst)
}

/// Split a signed tag's body into (unsigned_payload, signature_block).
///
/// The signature block is the contiguous range from the line
/// `-----BEGIN PGP SIGNATURE-----` through and including
/// `-----END PGP SIGNATURE-----\n`. The unsigned payload is everything
/// before that range — i.e. the tag bytes the signer ran the digest over.
///
/// Returns `None` if no PGP block is present.
pub(crate) fn split_signed_tag(body: &[u8]) -> Option<(Vec<u8>, Vec<u8>)> {
    let begin = find_line(body, PGP_BEGIN)?;
    // Find the END line at or after the BEGIN.
    let after_begin = &body[begin..];
    let end_rel = find_line(after_begin, PGP_END)?;
    // End includes the entire END line plus its trailing newline (if any).
    let end_line_start = begin + end_rel;
    let end_line_end = match body[end_line_start..].iter().position(|&b| b == b'\n') {
        Some(off) => end_line_start + off + 1,
        None => body.len(),
    };

    let payload = body[..begin].to_vec();
    let signature = body[begin..end_line_end].to_vec();
    Some((payload, signature))
}

/// Find the byte offset of a line that EXACTLY equals `needle`. Returns
/// the offset of the first byte of that line. Honors line boundaries:
/// the candidate position must be at the start of `haystack` or
/// immediately after a `\n`, and the next byte after the match must be
/// `\n` or EOF.
fn find_line(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    let mut i = 0;
    while let Some(pos) = haystack[i..]
        .windows(needle.len())
        .position(|w| w == needle)
    {
        let abs = i + pos;
        let at_line_start = abs == 0 || haystack[abs - 1] == b'\n';
        let next = abs + needle.len();
        let at_line_end = next == haystack.len() || haystack[next] == b'\n';
        if at_line_start && at_line_end {
            return Some(abs);
        }
        i = abs + needle.len();
        if i > haystack.len() {
            break;
        }
    }
    None
}

fn io_err<E: std::fmt::Display>(e: E) -> io::Error {
    io::Error::other(format!("{e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_extracts_payload_and_signature_blocks() {
        let body = b"object dead\ntype commit\ntag v1\ntagger t <t@t> 1700000000 +0000\n\
                    \nMessage line\n\
                    -----BEGIN PGP SIGNATURE-----\nsigbytes\n-----END PGP SIGNATURE-----\n";
        let (payload, sig) = split_signed_tag(body).unwrap();
        assert!(payload.ends_with(b"Message line\n"));
        assert!(!payload
            .windows(b"BEGIN PGP SIGNATURE".len())
            .any(|w| w == b"BEGIN PGP SIGNATURE"));
        assert!(sig.starts_with(b"-----BEGIN PGP SIGNATURE-----"));
        assert!(sig.ends_with(b"-----END PGP SIGNATURE-----\n"));
    }

    #[test]
    fn split_returns_none_when_unsigned() {
        let body = b"object dead\ntype commit\ntag v1\ntagger t <t@t> 1700000000 +0000\n\nMsg\n";
        assert!(split_signed_tag(body).is_none());
    }

    #[test]
    fn find_line_respects_line_boundaries() {
        // The PGP_BEGIN marker appears mid-line — must not be picked up.
        let body = b"hello -----BEGIN PGP SIGNATURE----- inline\n";
        assert!(find_line(body, PGP_BEGIN).is_none());
    }

    #[test]
    fn split_handles_no_trailing_newline_after_end_block() {
        let body = b"object dead\ntype commit\ntag v\ntagger t <t@t> 1700000000 +0000\n\
                    \nm\n\
                    -----BEGIN PGP SIGNATURE-----\nsig\n-----END PGP SIGNATURE-----";
        let (_, sig) = split_signed_tag(body).unwrap();
        assert!(sig.ends_with(b"-----END PGP SIGNATURE-----"));
    }
}
