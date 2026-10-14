//! Operational history viewer — inspects `$XDG_DATA_HOME/mu/operations.log`.

use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::xdg;

/// Default and maximum limit for history queries.
pub const DEFAULT_LIMIT: usize = 20;
pub const MAX_LIMIT: usize = 200;

/// A single entry in the operations log.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LogEntry {
    pub timestamp: String,
    pub action: String,
    pub outcome: String,
    pub target: String,
}

/// Options for `mu history`.
#[derive(Debug, Clone, Default)]
pub struct Options {
    pub limit: usize,
    pub json: bool,
}

/// Parse a single line from `operations.log`.
/// Format: `{rfc3339}  {action:<12}  {outcome:<8}  {target}`
pub fn parse_log_line(line: &str) -> Option<LogEntry> {
    let line = line.trim_end_matches(['\r', '\n']);
    if line.trim().is_empty() {
        return None;
    }

    // Split on whitespace boundaries for the first 3 tokens: timestamp, action, outcome.
    // The target is the remainder of the line, which may contain spaces.
    let mut parts = line.split_whitespace();
    let timestamp = parts.next()?;
    let action = parts.next()?;
    let outcome = parts.next()?;

    // To accurately extract target, find where outcome ends in the original line.
    let outcome_offset = line.find(outcome)? + outcome.len();
    let target = line[outcome_offset..].trim_start().to_string();

    Some(LogEntry {
        timestamp: timestamp.to_string(),
        action: action.to_string(),
        outcome: outcome.to_string(),
        target,
    })
}

/// Returns the path to `operations.log` under the XDG data home.
pub fn log_path() -> PathBuf {
    xdg::data_home().join("mu").join("operations.log")
}

/// Read up to `limit` entries from the given log file path.
/// Clamps `limit` between 1 and `MAX_LIMIT` (defaulting to `DEFAULT_LIMIT` if 0).
pub fn read_history_from(path: &Path, limit: usize) -> Vec<LogEntry> {
    let limit = if limit == 0 {
        DEFAULT_LIMIT
    } else {
        limit.min(MAX_LIMIT)
    };

    let file = match File::open(path) {
        Ok(f) => f,
        Err(_) => return Vec::new(),
    };

    let reader = BufReader::new(file);
    let mut entries = Vec::new();

    for line in reader.lines().map_while(Result::ok) {
        if let Some(entry) = parse_log_line(&line) {
            entries.push(entry);
        }
    }

    if entries.len() > limit {
        let start = entries.len() - limit;
        entries.drain(..start);
    }

    entries
}

/// Format log entries in a clean tabular view.
pub fn format_table(entries: &[LogEntry]) -> String {
    if entries.is_empty() {
        return "No recent operations recorded.\n".to_string();
    }

    let mut out = String::new();
    // Headers aligned with columns
    out.push_str(&format!(
        "{:<25}  {:<12}  {:<9}  {}\n",
        "TIMESTAMP", "ACTION", "OUTCOME", "TARGET"
    ));

    for e in entries {
        out.push_str(&format!(
            "{:<25}  {:<12}  {:<9}  {}\n",
            e.timestamp, e.action, e.outcome, e.target
        ));
    }

    out
}

/// Entry point for `mu history`.
pub fn run(opts: &Options) -> i32 {
    let path = log_path();
    let entries = read_history_from(&path, opts.limit);

    if opts.json {
        match serde_json::to_string_pretty(&entries) {
            Ok(json_str) => {
                println!("{json_str}");
                0
            }
            Err(e) => {
                eprintln!("error: failed to serialize history to JSON: {e}");
                1
            }
        }
    } else {
        print!("{}", format_table(&entries));
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::path::PathBuf;

    fn tempdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "mu-test-history-{}-{}-{}",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn parse_valid_log_line() {
        let line = "2026-10-14T10:00:00Z  trash         success   /home/user/test file.txt";
        let entry = parse_log_line(line).expect("should parse");
        assert_eq!(entry.timestamp, "2026-10-14T10:00:00Z");
        assert_eq!(entry.action, "trash");
        assert_eq!(entry.outcome, "success");
        assert_eq!(entry.target, "/home/user/test file.txt");
    }

    #[test]
    fn parse_empty_or_malformed_lines() {
        assert!(parse_log_line("").is_none());
        assert!(parse_log_line("   ").is_none());
        assert!(parse_log_line("2026-10-14T10:00:00Z only_action").is_none());
    }

    #[test]
    fn read_history_with_limit_and_order() {
        let dir = tempdir("order");
        let path = dir.join("operations.log");
        let mut file = File::create(&path).expect("create");

        for i in 1..=10 {
            writeln!(
                file,
                "2026-10-14T10:00:{:02}Z  clean         success   /tmp/target_{}",
                i, i
            )
            .expect("write");
        }
        drop(file);

        let entries = read_history_from(&path, 3);
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].target, "/tmp/target_8");
        assert_eq!(entries[1].target, "/tmp/target_9");
        assert_eq!(entries[2].target, "/tmp/target_10");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_history_clamped_max_limit() {
        let dir = tempdir("clamped");
        let path = dir.join("operations.log");
        let mut file = File::create(&path).expect("create");

        for i in 1..=250 {
            writeln!(
                file,
                "2026-10-14T10:00:00Z  trash         success   /tmp/item_{}",
                i
            )
            .expect("write");
        }
        drop(file);

        // Requested 300, clamped to MAX_LIMIT (200)
        let entries = read_history_from(&path, 300);
        assert_eq!(entries.len(), MAX_LIMIT);
        assert_eq!(entries[0].target, "/tmp/item_51");
        assert_eq!(entries[199].target, "/tmp/item_250");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_history_nonexistent_file() {
        let path = Path::new("/tmp/nonexistent_mu_test_operations.log");
        let entries = read_history_from(path, 10);
        assert!(entries.is_empty());
    }

    #[test]
    fn format_table_output() {
        let entries = vec![LogEntry {
            timestamp: "2026-10-14T10:00:00Z".to_string(),
            action: "trash".to_string(),
            outcome: "success".to_string(),
            target: "/tmp/sample".to_string(),
        }];
        let table = format_table(&entries);
        assert!(table.contains("TIMESTAMP"));
        assert!(table.contains("ACTION"));
        assert!(table.contains("OUTCOME"));
        assert!(table.contains("TARGET"));
        assert!(table.contains("/tmp/sample"));
    }
}
