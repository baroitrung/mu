//! JSON serialization for disk analysis.

use std::path::PathBuf;

use serde::Serialize;

use crate::analyze::model::{FileNode, LargestFile, ScanStats};

/// Top-level report emitted by `mu analyze --json`.
#[derive(Clone, Debug, Serialize)]
pub struct AnalyzeReport {
    pub path: PathBuf,
    pub size: u64,
    pub size_human: String,
    pub item_count: usize,
    pub total_files: u64,
    pub total_dirs: u64,
    pub root: FileNode,
    pub largest_files: Vec<LargestFile>,
}

/// Serialize scan results to a pretty-printed JSON string.
pub fn to_json_pretty(root: &FileNode, stats: &ScanStats) -> Result<String, serde_json::Error> {
    let report = AnalyzeReport {
        path: root.path.clone(),
        size: root.size,
        size_human: root.size_human.clone(),
        item_count: root.item_count,
        total_files: stats.total_files,
        total_dirs: stats.total_dirs,
        root: root.clone(),
        largest_files: stats.largest_files.clone(),
    };
    serde_json::to_string_pretty(&report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_to_json_pretty_emits_valid_json() {
        let root = FileNode::new("test".into(), PathBuf::from("/tmp/test"), 1024, true, 1);
        let stats = ScanStats {
            total_bytes: 1024,
            total_bytes_human: "1.0 KB".into(),
            total_files: 1,
            total_dirs: 1,
            errors: 0,
            duration: std::time::Duration::from_millis(10),
            largest_files: vec![LargestFile::new(PathBuf::from("/tmp/test/f"), 1024)],
        };

        let json = to_json_pretty(&root, &stats).unwrap();
        assert!(json.contains("\"path\": \"/tmp/test\""));
        assert!(json.contains("\"size\": 1024"));
        assert!(json.contains("\"largest_files\""));
    }
}
