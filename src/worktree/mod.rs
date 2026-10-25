//! Multi-repo worktree management building blocks.
//!
//! Provides types, store operations, git operations, helpers, and hooks
//! for worktree management. Command handlers live in `meta_git_cli::commands::worktree`.

pub mod git_ops;
pub mod helpers;
pub mod hooks;
pub mod safety;
pub mod store;
pub mod types;

// Re-export commonly-used types
pub use types::RepoSpec;
