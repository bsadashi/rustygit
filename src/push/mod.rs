//! `git push` (M11). The inverse of clone/fetch — moves objects and ref
//! updates from a local repository up to a remote.
//!
//! Two backends:
//!   - [`local`]: copy objects and update refs in a bare-style destination
//!     (anything we can open as a `Repository` from a path or `file://` URL).
//!   - [`network`]: drive `git-receive-pack` over HTTPS using Track A's
//!     `send_pack` transport primitives.
//!
//! Common pieces (refspec parsing, options, errors) live here in `mod.rs`.
//! The two backends each accept a `&[Refspec]` and a `&PushOpts`.

pub mod local;
pub mod network;

use std::fmt;

use thiserror::Error;

use crate::hash::HashError;
use crate::refs::{RefError, RefNameError};

pub use local::{push_local, LocalPushError, LocalPushReport};
pub use network::{push_network, NetworkPushError, NetworkPushReport};

// ---------------------------------------------------------------------------
// Options
// ---------------------------------------------------------------------------

/// Per-call options.
#[derive(Debug, Clone, Default)]
pub struct PushOpts {
    /// Override the non-fast-forward check.
    pub force: bool,
    /// Forward the `atomic` capability to the server. Local pushes ignore
    /// this — they always update one ref at a time but error out on the
    /// first failure, which is close enough for M11.
    pub atomic: bool,
    /// Suppress progress and per-ref status lines.
    pub quiet: bool,
}

// ---------------------------------------------------------------------------
// Refspecs
// ---------------------------------------------------------------------------

/// A parsed push refspec: `[+]<src>:<dst>`, `<src>`, or `:<dst>`.
///
/// `src.is_empty()` means a delete — push nothing to the remote's `dst`.
/// `dst.is_empty()` is illegal once parsing finishes; we fill in dst from
/// src when the colon form omits it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refspec {
    /// Force update (overrides non-fast-forward check).
    pub force: bool,
    /// Local source ref. Empty means "delete the remote ref".
    pub src: String,
    /// Remote destination ref.
    pub dst: String,
}

impl Refspec {
    /// True when `src` is empty — the `:<dst>` (delete) form.
    pub fn is_delete(&self) -> bool {
        self.src.is_empty()
    }
}

impl fmt::Display for Refspec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.force {
            f.write_str("+")?;
        }
        write!(f, "{}:{}", self.src, self.dst)
    }
}

/// Parse one refspec string into a `Refspec`.
///
/// Forms:
///   - `<src>:<dst>` — explicit src and dst
///   - `<src>` — push src to a same-name dst (with branch expansion)
///   - `:<dst>` — delete dst on the remote
///   - `+<...>` — set the force bit
///
/// Bare names are expanded to `refs/heads/<name>`. For M11 we always pick the
/// `refs/heads/` namespace for short names — push-of-a-tag-by-shorthand isn't
/// supported (use the fully-qualified `refs/tags/v1.0` instead).
pub fn parse_refspec(s: &str) -> Result<Refspec, PushError> {
    if s.is_empty() {
        return Err(PushError::InvalidRefspec("empty refspec".to_string()));
    }
    let (force, rest) = match s.strip_prefix('+') {
        Some(r) => (true, r),
        None => (false, s),
    };

    // Locate the `:` that separates src from dst, if any.
    let (src_raw, dst_raw) = match rest.split_once(':') {
        Some((a, b)) => (a, b),
        None => (rest, ""),
    };

    let src_provided = rest.contains(':');

    // If src is empty and we have no colon, the spec is malformed.
    if rest.is_empty() {
        return Err(PushError::InvalidRefspec(s.to_string()));
    }
    // `+` alone is invalid.
    if force && rest.is_empty() {
        return Err(PushError::InvalidRefspec(s.to_string()));
    }

    let src = if src_raw.is_empty() {
        // `:<dst>` form — delete on the remote. src stays empty.
        String::new()
    } else {
        expand_branch_shorthand(src_raw)
    };

    let dst = if src_provided {
        if dst_raw.is_empty() {
            // `<src>:` with empty dst is malformed; git treats this as a
            // delete of <src> on the remote, but that's surprising and we
            // refuse it for M11 — users should use `:<dst>` for delete.
            return Err(PushError::InvalidRefspec(s.to_string()));
        }
        expand_branch_shorthand(dst_raw)
    } else {
        // No colon — mirror src on the remote.
        if src.is_empty() {
            return Err(PushError::InvalidRefspec(s.to_string()));
        }
        src.clone()
    };

    Ok(Refspec { force, src, dst })
}

/// Expand a possibly-short ref name to a full `refs/heads/<name>` form. Names
/// that are already fully qualified (`refs/heads/x`, `refs/tags/x`, `HEAD`)
/// are returned unchanged.
fn expand_branch_shorthand(name: &str) -> String {
    if name.starts_with("refs/") || name == "HEAD" {
        return name.to_string();
    }
    format!("refs/heads/{name}")
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Errors shared between local and network push, plus a few backend-agnostic
/// ones that callers (the CLI) need to handle uniformly.
#[derive(Error, Debug)]
pub enum PushError {
    #[error("invalid refspec: {0}")]
    InvalidRefspec(String),

    #[error("source ref does not exist: {0}")]
    SourceMissing(String),

    #[error("non-fast-forward update of {dst} from {old} to {new}")]
    NonFastForward {
        dst: String,
        old: String,
        new: String,
    },

    #[error("delete refused: {dst} does not exist on the remote")]
    DeleteMissing { dst: String },

    #[error("invalid ref name: {0}")]
    Name(#[from] RefNameError),

    #[error(transparent)]
    Refs(#[from] RefError),

    #[error(transparent)]
    Hash(#[from] HashError),

    #[error("io on {path}: {source}")]
    Io {
        path: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },
}

// ---------------------------------------------------------------------------
// Tests for refspec parsing
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shorthand_expands_to_refs_heads() {
        let r = parse_refspec("main").unwrap();
        assert!(!r.force);
        assert_eq!(r.src, "refs/heads/main");
        assert_eq!(r.dst, "refs/heads/main");
        assert!(!r.is_delete());
    }

    #[test]
    fn explicit_src_and_dst() {
        let r = parse_refspec("refs/heads/topic:refs/heads/main").unwrap();
        assert_eq!(r.src, "refs/heads/topic");
        assert_eq!(r.dst, "refs/heads/main");
        assert!(!r.force);
    }

    #[test]
    fn shorthand_both_sides() {
        let r = parse_refspec("topic:main").unwrap();
        assert_eq!(r.src, "refs/heads/topic");
        assert_eq!(r.dst, "refs/heads/main");
    }

    #[test]
    fn force_prefix_sets_flag() {
        let r = parse_refspec("+topic:main").unwrap();
        assert!(r.force);
        assert_eq!(r.src, "refs/heads/topic");
        assert_eq!(r.dst, "refs/heads/main");
    }

    #[test]
    fn delete_form_no_force() {
        let r = parse_refspec(":refs/heads/topic").unwrap();
        assert!(r.is_delete());
        assert_eq!(r.src, "");
        assert_eq!(r.dst, "refs/heads/topic");
        assert!(!r.force);
    }

    #[test]
    fn delete_with_shorthand() {
        let r = parse_refspec(":topic").unwrap();
        assert!(r.is_delete());
        assert_eq!(r.dst, "refs/heads/topic");
    }

    #[test]
    fn fully_qualified_refs_unchanged() {
        let r = parse_refspec("refs/tags/v1:refs/tags/v1").unwrap();
        assert_eq!(r.src, "refs/tags/v1");
        assert_eq!(r.dst, "refs/tags/v1");
    }

    #[test]
    fn rejects_empty() {
        assert!(parse_refspec("").is_err());
    }

    #[test]
    fn rejects_trailing_colon_empty_dst() {
        // `<src>:` with empty dst is rejected to keep the API explicit.
        assert!(parse_refspec("main:").is_err());
    }
}
