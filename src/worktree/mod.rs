//! Working-tree comparisons against the index and HEAD tree.
//!
//! M4 ships [`status`] — the three-way diff between HEAD's tree, the index,
//! and the on-disk working tree. Later milestones will add `checkout`,
//! `restore`, `reset --hard`, and ignore-aware traversal here.
//!
//! All entry points take a [`crate::repo::Repository`] and return owned data;
//! they never mutate the repo or hold open file handles longer than necessary.

pub mod status;

pub use status::{
    status, Human, PorcelainV1, StageState, StatusEntry, StatusError, StatusReport, WorktreeState,
};
