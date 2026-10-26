//! Data models for interactive disk analysis.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Serialize a `PathBuf` as a lossy UTF-8 string, preventing serialization failure
/// when encountering non-UTF-8 filenames on Unix filesystems.
pub fn serialize_path_lossy<S>(path: &Path, serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    serializer.serialize_str(&path.to_string_lossy())
}
/// Sorting criteria for directory entries.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum SortMode {
    #[default]
    SizeDesc,
    SizeAsc,
    NameAsc,
}

impl SortMode {
    pub fn next(self) -> Self {
        match self {
            Self::SizeDesc => Self::SizeAsc,
            Self::SizeAsc => Self::NameAsc,
            Self::NameAsc => Self::SizeDesc,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::SizeDesc => "Size (desc)",
            Self::SizeAsc => "Size (asc)",
            Self::NameAsc => "Name (asc)",
        }
    }
}

/// View mode in the TUI explorer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum ViewMode {
    #[default]
    Tree,
    LargestFiles,
    Overview,
}

/// A node in the scanned directory tree.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileNode {
    pub name: String,
    #[serde(serialize_with = "serialize_path_lossy")]
    pub path: PathBuf,
    pub size: u64,
    pub size_human: String,
    pub is_dir: bool,
    pub item_count: usize,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<FileNode>,
    #[serde(default, skip)]
    pub dev: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub err: Option<String>,
}

impl FileNode {
    pub fn new(name: String, path: PathBuf, size: u64, is_dir: bool, dev: u64) -> Self {
        Self {
            name,
            path,
            size,
            size_human: crate::size::human_size(size as i64),
            is_dir,
            item_count: 0,
            children: Vec::new(),
            dev,
            err: None,
        }
    }

    /// Sort direct children according to the given sort mode.
    pub fn sort_children(&mut self, mode: SortMode) {
        match mode {
            SortMode::SizeDesc => {
                self.children
                    .sort_by(|a, b| b.size.cmp(&a.size).then_with(|| a.name.cmp(&b.name)));
            }
            SortMode::SizeAsc => {
                self.children
                    .sort_by(|a, b| a.size.cmp(&b.size).then_with(|| a.name.cmp(&b.name)));
            }
            SortMode::NameAsc => {
                self.children
                    .sort_by(|a, b| a.name.cmp(&b.name).then_with(|| b.size.cmp(&a.size)));
            }
        }
    }

    /// Recursively recalculate sizes and human-readable strings after deletion.
    pub fn recalculate_sizes(&mut self) -> u64 {
        if !self.is_dir {
            self.size_human = crate::size::human_size(self.size as i64);
            return self.size;
        }
        let mut total = 0u64;
        let mut total_items = 0usize;
        for child in &mut self.children {
            total += child.recalculate_sizes();
            total_items += 1 + child.item_count;
        }
        self.size = total;
        self.item_count = total_items;
        self.size_human = crate::size::human_size(self.size as i64);
        total
    }
}

/// A discovered large file.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct LargestFile {
    #[serde(serialize_with = "serialize_path_lossy")]
    pub path: PathBuf,
    pub size: u64,
    pub size_human: String,
}

impl LargestFile {
    pub fn new(path: PathBuf, size: u64) -> Self {
        Self {
            path,
            size,
            size_human: crate::size::human_size(size as i64),
        }
    }
}

/// Scan statistics.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ScanStats {
    pub total_bytes: u64,
    pub total_bytes_human: String,
    pub total_files: u64,
    pub total_dirs: u64,
    pub errors: u64,
    #[serde(default, skip)]
    pub duration: Duration,
    pub largest_files: Vec<LargestFile>,
}

/// Progress message emitted by the walker during scanning.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug)]
pub enum ScanMessage {
    Progress {
        items_scanned: u64,
        dirs_scanned: u64,
        bytes_scanned: u64,
        current_dir: PathBuf,
    },
    Complete {
        root: FileNode,
        stats: ScanStats,
    },
    Error(String),
}
