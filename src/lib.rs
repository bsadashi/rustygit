// Doc-comment indentation style across this crate predates the lints that
// clippy started enforcing; the warnings are cosmetic and were explicitly
// triaged in POLISH.md item #1. Allow them crate-wide rather than
// reshuffling hundreds of doc lines.
#![allow(clippy::doc_lazy_continuation, clippy::doc_overindented_list_items)]

//! `rustygit` core library.
//!
//! Module map (more added per milestone):
//! - [`hash`] — `HashKind`, `ObjectId`, `Hasher` trait + SHA-1/SHA-256 impls (M0)
//! - [`object`] — `ObjectKind`, `RawObject` framing (M0)
//! - [`tree`] — `FileMode`, `Tree`, `TreeEntry`, parse/serialize (M0)
//! - [`repo`] — `Repository` discovery + path helpers (M0)
//! - [`cli`] — clap-derive command dispatch (M0)
//!
//! Coming in later milestones: `odb` (M1), `refs`+`lockfile` (M2), `index` (M3),
//! `worktree` (M4), `xdiff` (M5), and so on per the milestone plan.

pub mod add_patch;
pub mod bisect;
pub mod blame;
pub mod cli;
pub mod clone;
pub mod color;
pub mod commit;
pub mod commit_graph;
pub mod config;
pub mod credential;
pub mod diff;
pub mod fsck;
pub mod hash;
pub mod hooks;
pub mod i18n;
pub mod identity;
pub mod ignore;
pub mod index;
pub mod lockfile;
pub mod merge;
pub mod midx;
pub mod notes;
pub mod object;
pub mod odb;
pub mod pack;
pub mod pathspec;
pub mod push;
pub mod reachable;
pub mod refs;
pub mod repo;
pub mod revparse;
pub mod sequencer;
pub mod signing;
pub mod tag;
pub mod trace;
pub mod transport;
pub mod tree;
pub mod unpack_trees;
pub mod wildmatch;
pub mod worktree;
pub mod xdiff;

pub use repo::Repository;
