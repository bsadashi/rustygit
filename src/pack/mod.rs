//! Pack format readers (M7).
//!
//! A git pack is a `.pack` file (the actual object data, possibly delta-encoded
//! against earlier entries in the same pack or against an oid in some other
//! object store) accompanied by a `.idx` file (a sorted oid → pack-offset
//! lookup table). This module is concerned only with the byte-level reading of
//! those two files. Delta application and `ObjectStore` integration live in
//! Track B (`crate::odb::pack`), which builds on top of `PackFile`/`IdxFile`.
//!
//! References:
//!   - `gitformat-pack(5)`: pack and idx file layouts.
//!   - The original "size encoding" is little-endian 7-bit chunks; the
//!     OFS_DELTA "offset encoding" is a separate, more compact encoding that
//!     adds 1 to every non-final chunk to recover the bits the gap leaves on
//!     the table — see `idx::read_offset_varint` (and the spec).

pub mod build;
pub mod delta;
pub mod file;
pub mod idx;
pub mod store;

pub use build::{write_pack, write_pack_from_objects, PackBuildError, PackBuildResult};
pub use delta::{apply_delta, DeltaError};
pub use file::{EntryIter, PackEntryKind, PackFile, RawPackEntry};
pub use idx::{IdxFile, IdxIter};
pub use store::PackStore;

#[derive(thiserror::Error, Debug)]
pub enum PackError {
    #[error("io on {path}: {source}")]
    Io {
        path: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("malformed pack: {0}")]
    Malformed(&'static str),
    #[error("malformed idx: {0}")]
    MalformedIdx(&'static str),
    #[error("bad signature in pack: expected 'PACK', got {0:?}")]
    BadPackSignature([u8; 4]),
    #[error("bad signature in idx: expected 0xff744f63, got {0:?}")]
    BadIdxSignature([u8; 4]),
    #[error("unsupported pack version: {0}")]
    UnsupportedPackVersion(u32),
    #[error("unsupported idx version: {0}")]
    UnsupportedIdxVersion(u32),
    #[error("pack/idx checksum mismatch")]
    ChecksumMismatch,
    #[error("zlib inflate failed at offset {offset}: {source}")]
    Inflate {
        offset: u64,
        #[source]
        source: std::io::Error,
    },
    #[error(transparent)]
    Hash(#[from] crate::hash::HashError),
}
