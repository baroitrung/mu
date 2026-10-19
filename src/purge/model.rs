//! Data structures for project artifact purge.

use std::path::PathBuf;

/// A discovered project build artifact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PurgeArtifact {
    /// Absolute path to the artifact directory.
    pub path: PathBuf,
    /// Artifact kind / name (e.g. "node_modules", "target", ".venv").
    pub kind: String,
    /// Size in bytes.
    pub size: u64,
}

/// Options for `mu purge`.
#[derive(Debug, Clone, Default)]
pub struct Options {
    /// Optional specific search roots. If empty, default roots are used.
    pub paths: Vec<PathBuf>,
    /// Preview actions without deleting anything.
    pub dry_run: bool,
    /// Automatically confirm deletion without prompting.
    pub auto_yes: bool,
    /// Include zero-byte / empty build artifact directories.
    pub include_empty: bool,
    /// Enable verbose debug logging.
    pub debug: bool,
}
