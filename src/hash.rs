//! Hash abstraction (ADR A1).
//!
//! `ObjectId` is kind-tagged rather than generic over the hash algorithm. This
//! lets `Repository`, the object DB, and ref backends stay non-generic, which
//! keeps `dyn ObjectStore` / `dyn RefStore` viable as trait objects. The 12-byte
//! waste in SHA-1 mode (storing 32 bytes for a 20-byte hash) is intentional and
//! has no measurable cost relative to the API simplicity won.

use std::fmt;
use std::str::FromStr;

use sha1::{Digest as _, Sha1};
use sha2::Sha256;
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum HashKind {
    Sha1 = 1,
    Sha256 = 2,
}

impl HashKind {
    /// Length in bytes of the raw digest.
    pub const fn raw_len(self) -> usize {
        match self {
            HashKind::Sha1 => 20,
            HashKind::Sha256 => 32,
        }
    }

    /// Length in characters of the hex-encoded digest.
    pub const fn hex_len(self) -> usize {
        self.raw_len() * 2
    }

    /// Name as it appears in `extensions.objectFormat` and the CLI flag.
    pub const fn name(self) -> &'static str {
        match self {
            HashKind::Sha1 => "sha1",
            HashKind::Sha256 => "sha256",
        }
    }

    pub fn parse(s: &str) -> Result<Self, HashError> {
        match s {
            "sha1" => Ok(HashKind::Sha1),
            "sha256" => Ok(HashKind::Sha256),
            other => Err(HashError::UnknownAlgorithm(other.to_string())),
        }
    }
}

impl fmt::Display for HashKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

const MAX_RAW_LEN: usize = 32;

/// A git object identifier. Stores the algorithm tag plus a fixed-size buffer
/// large enough for the largest supported digest (SHA-256 = 32 bytes).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ObjectId {
    kind: HashKind,
    bytes: [u8; MAX_RAW_LEN],
}

impl ObjectId {
    pub fn null(kind: HashKind) -> Self {
        Self {
            kind,
            bytes: [0u8; MAX_RAW_LEN],
        }
    }

    /// Build from a raw digest. The slice must be exactly `kind.raw_len()` bytes.
    pub fn from_bytes(kind: HashKind, raw: &[u8]) -> Result<Self, HashError> {
        if raw.len() != kind.raw_len() {
            return Err(HashError::WrongLength {
                expected: kind.raw_len(),
                got: raw.len(),
            });
        }
        let mut bytes = [0u8; MAX_RAW_LEN];
        bytes[..raw.len()].copy_from_slice(raw);
        Ok(Self { kind, bytes })
    }

    /// Parse a hex string. The string length must match the kind's `hex_len`.
    pub fn parse_hex(kind: HashKind, hex: &str) -> Result<Self, HashError> {
        if hex.len() != kind.hex_len() {
            return Err(HashError::WrongHexLength {
                expected: kind.hex_len(),
                got: hex.len(),
            });
        }
        let raw = hex::decode(hex).map_err(|_| HashError::InvalidHex(hex.to_string()))?;
        Self::from_bytes(kind, &raw)
    }

    /// Parse a hex string of unknown length, inferring the algorithm from length.
    pub fn parse_hex_any(hex: &str) -> Result<Self, HashError> {
        let kind = match hex.len() {
            40 => HashKind::Sha1,
            64 => HashKind::Sha256,
            n => return Err(HashError::AmbiguousHexLength(n)),
        };
        Self::parse_hex(kind, hex)
    }

    pub fn kind(&self) -> HashKind {
        self.kind
    }

    /// Returns the raw digest bytes (length depends on the algorithm).
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes[..self.kind.raw_len()]
    }

    /// True if every byte is zero (used as a sentinel "no value" by ref txns).
    pub fn is_null(&self) -> bool {
        self.as_bytes().iter().all(|&b| b == 0)
    }

    /// Returns a hex prefix of `n` characters. `n` is clamped to `hex_len`.
    pub fn short_hex(&self, n: usize) -> String {
        let full = hex::encode(self.as_bytes());
        let n = n.min(full.len());
        full[..n].to_string()
    }
}

impl fmt::Display for ObjectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for &b in self.as_bytes() {
            write!(f, "{b:02x}")?;
        }
        Ok(())
    }
}

/// Order by kind first, then by the raw digest bytes. The kind tag is
/// included so the (rare) case of mixing SHA-1 and SHA-256 oids in one
/// collection doesn't silently compare across algorithms.
impl Ord for ObjectId {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (self.kind as u8)
            .cmp(&(other.kind as u8))
            .then_with(|| self.as_bytes().cmp(other.as_bytes()))
    }
}

impl PartialOrd for ObjectId {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl FromStr for ObjectId {
    type Err = HashError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse_hex_any(s)
    }
}

/// Streaming hasher. The `Repository` owns a factory that returns one of these
/// based on its configured `HashKind`; callers always get a `Box<dyn Hasher>`
/// rather than a generic `H: Hasher`.
pub trait Hasher: Send {
    fn update(&mut self, data: &[u8]);
    /// Consumes the hasher and returns the digest as an `ObjectId`.
    fn finalize(self: Box<Self>) -> ObjectId;
}

pub struct Sha1Hasher(Sha1);
pub struct Sha256Hasher(Sha256);

impl Sha1Hasher {
    pub fn new() -> Self {
        Self(Sha1::new())
    }
}

impl Default for Sha1Hasher {
    fn default() -> Self {
        Self::new()
    }
}

impl Sha256Hasher {
    pub fn new() -> Self {
        Self(Sha256::new())
    }
}

impl Default for Sha256Hasher {
    fn default() -> Self {
        Self::new()
    }
}

impl Hasher for Sha1Hasher {
    fn update(&mut self, data: &[u8]) {
        self.0.update(data);
    }
    fn finalize(self: Box<Self>) -> ObjectId {
        let digest = self.0.finalize();
        ObjectId::from_bytes(HashKind::Sha1, &digest).expect("sha1 digest is always 20 bytes")
    }
}

impl Hasher for Sha256Hasher {
    fn update(&mut self, data: &[u8]) {
        self.0.update(data);
    }
    fn finalize(self: Box<Self>) -> ObjectId {
        let digest = self.0.finalize();
        ObjectId::from_bytes(HashKind::Sha256, &digest).expect("sha256 digest is always 32 bytes")
    }
}

/// Construct a fresh hasher for the given algorithm.
pub fn new_hasher(kind: HashKind) -> Box<dyn Hasher> {
    match kind {
        HashKind::Sha1 => Box::new(Sha1Hasher::new()),
        HashKind::Sha256 => Box::new(Sha256Hasher::new()),
    }
}

/// One-shot hash of a contiguous buffer.
pub fn hash_all(kind: HashKind, data: &[u8]) -> ObjectId {
    let mut h = new_hasher(kind);
    h.update(data);
    h.finalize()
}

#[derive(Error, Debug)]
pub enum HashError {
    #[error("unknown hash algorithm: {0}")]
    UnknownAlgorithm(String),
    #[error("invalid raw digest length: expected {expected}, got {got}")]
    WrongLength { expected: usize, got: usize },
    #[error("invalid hex digest length: expected {expected}, got {got}")]
    WrongHexLength { expected: usize, got: usize },
    #[error("hex string length {0} does not match any known hash algorithm")]
    AmbiguousHexLength(usize),
    #[error("invalid hex digits in: {0}")]
    InvalidHex(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_blob_sha1_matches_git() {
        // git's empty blob: sha1 of "blob 0\0"
        let oid = hash_all(HashKind::Sha1, b"blob 0\0");
        assert_eq!(oid.to_string(), "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391");
    }

    #[test]
    fn empty_blob_sha256_matches_git() {
        // SHA-256 empty-blob hash from git's documentation
        let oid = hash_all(HashKind::Sha256, b"blob 0\0");
        assert_eq!(
            oid.to_string(),
            "473a0f4c3be8a93681a267e3b1e9a7dcda1185436fe141f7749120a303721813"
        );
    }

    #[test]
    fn parse_hex_round_trip_sha1() {
        let s = "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391";
        let oid = ObjectId::parse_hex(HashKind::Sha1, s).unwrap();
        assert_eq!(oid.to_string(), s);
        assert_eq!(oid.kind(), HashKind::Sha1);
        assert_eq!(oid.as_bytes().len(), 20);
    }

    #[test]
    fn parse_hex_round_trip_sha256() {
        let s = "473a0f4c3be8a93681a267e3b1e9a7dcda1185436fe141f7749120a303721813";
        let oid = ObjectId::parse_hex(HashKind::Sha256, s).unwrap();
        assert_eq!(oid.to_string(), s);
        assert_eq!(oid.kind(), HashKind::Sha256);
        assert_eq!(oid.as_bytes().len(), 32);
    }

    #[test]
    fn parse_hex_any_picks_algorithm_by_length() {
        let s1 = "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391";
        let s256 = "473a0f4c3be8a93681a267e3b1e9a7dcda1185436fe141f7749120a303721813";
        assert_eq!(ObjectId::parse_hex_any(s1).unwrap().kind(), HashKind::Sha1);
        assert_eq!(
            ObjectId::parse_hex_any(s256).unwrap().kind(),
            HashKind::Sha256
        );
        assert!(matches!(
            ObjectId::parse_hex_any("deadbeef"),
            Err(HashError::AmbiguousHexLength(8))
        ));
    }

    #[test]
    fn null_oid_is_zero() {
        let oid = ObjectId::null(HashKind::Sha1);
        assert!(oid.is_null());
        assert_eq!(oid.to_string(), "0".repeat(40));
    }

    #[test]
    fn rejects_wrong_length_hex() {
        assert!(ObjectId::parse_hex(HashKind::Sha1, "deadbeef").is_err());
        assert!(matches!(
            ObjectId::parse_hex(HashKind::Sha1, "z".repeat(40).as_str()),
            Err(HashError::InvalidHex(_))
        ));
    }
}
