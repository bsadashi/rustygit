//! `git clone` (M8). Currently local-only — copying a source repository's
//! object store and refs into a freshly initialized destination, then checking
//! out the working tree.
//!
//! Network protocols (smart-HTTP, ssh) arrive in M11+. The local backend lives
//! in [`local`] and is exposed through `clone_local`.

pub mod local;
pub mod network;

pub use local::{clone_local, CloneError, CloneOpts};
pub use network::{clone_network, NetworkCloneError, NetworkCloneOpts};
