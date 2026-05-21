//! Merge support (M13).
//!
//! Currently this provides the foundation for three-way merging: computing
//! the merge base(s) of two commits — the lowest common ancestor(s) of the
//! pair in the commit-parent DAG. Three-way merge itself (Track B) builds on
//! `merge_base` to decide what to diff each side against.
//!
//! Future submodules will add the three-way merge driver itself, conflict
//! markers, and recursive-merge-base handling (when there is more than one
//! merge base, git's `merge-recursive` strategy recursively merges them to
//! form a virtual base; M13 ships the single-base case and falls back to
//! "earliest base by committer time" when multiple are returned).

pub mod base;
pub mod file;
pub mod tree;

pub use base::{is_ancestor, merge_base, merge_bases, MergeBaseError};
pub use file::{merge_file, FileMergeLabels, FileMergeResult};
pub use tree::{merge_tree, MergeOutcome, MergedPath, PathMergeState, TreeMergeError};
