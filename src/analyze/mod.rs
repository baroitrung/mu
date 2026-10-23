//! Disk analysis module (`mu analyze`).

pub mod json;
pub mod model;
pub mod walker;

use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use model::ScanStats;
use walker::{WalkerOptions, scan_path};

/// CLI options for `mu analyze`.
#[derive(Clone, Debug, Default)]
pub struct Options {
    pub path: Option<PathBuf>,
    pub dry_run: bool,
    pub json: bool,
    pub debug: bool,
}

/// Execute `mu analyze` according to options and terminal environment.
pub fn run(opts: &Options) -> i32 {
    let target = resolve_target_path(opts.path.as_deref());

    if opts.json {
        let cancel = Arc::new(AtomicBool::new(false));
        match scan_path(&target, WalkerOptions::default(), None, cancel) {
            Ok((root, stats)) => match json::to_json_pretty(&root, &stats) {
                Ok(j) => {
                    let stdout = std::io::stdout();
                    let mut out = stdout.lock();
                    let _ = writeln!(out, "{j}");
                    0
                }
                Err(e) => {
                    eprintln!("error serializing analyze report: {e}");
                    1
                }
            },
            Err(e) => {
                eprintln!("error analyzing {}: {e}", target.display());
                1
            }
        }
    } else if !std::io::stdout().is_terminal() {
        if opts.dry_run {
            let cancel = Arc::new(AtomicBool::new(false));
            match scan_path(&target, WalkerOptions::default(), None, cancel) {
                Ok((root, stats)) => {
                    print_headless_summary(&root, &stats, true);
                    0
                }
                Err(e) => {
                    eprintln!("error analyzing {}: {e}", target.display());
                    1
                }
            }
        } else {
            eprintln!("could not open a new TTY: open /dev/tty: no such device or address");
            1
        }
    } else {
        match crate::tui::run_analyze_tui(opts, &target) {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("{e}");
                1
            }
        }
    }
}

pub fn resolve_target_path(path: Option<&Path>) -> PathBuf {
    let p = match path {
        Some(p) => p.to_path_buf(),
        None => std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))),
    };
    let abs = if p.is_relative() {
        std::env::current_dir().map(|cwd| cwd.join(&p)).unwrap_or(p)
    } else {
        p
    };
    crate::xdg::clean(&abs)
}

fn print_headless_summary(root: &model::FileNode, stats: &ScanStats, dry_run: bool) {
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let _ = writeln!(out, "Target: {}", root.path.display());
    let _ = writeln!(out, "Total Size: {}", root.size_human);
    let _ = writeln!(
        out,
        "Total Items: {} ({} files, {} directories)",
        root.item_count, stats.total_files, stats.total_dirs
    );
    if !stats.largest_files.is_empty() {
        let _ = writeln!(out, "\nLargest files:");
        for f in stats.largest_files.iter().take(5) {
            let _ = writeln!(out, "  {:>10}  {}", f.size_human, f.path.display());
        }
    }
    if dry_run {
        let _ = writeln!(
            out,
            "\n[DRY RUN] In-app deletion disabled; no files were removed."
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolve_target_path_makes_relative_paths_absolute() {
        let cwd = std::env::current_dir().unwrap();
        let res = resolve_target_path(Some(Path::new(".")));
        assert!(res.is_absolute(), "path should be absolute, got {:?}", res);
        assert_eq!(res, crate::xdg::clean(&cwd));
    }

    #[test]
    fn test_non_tty_without_dry_run_fails_with_tty_error() {
        let opts = Options {
            path: None,
            dry_run: false,
            json: false,
            debug: false,
        };
        let exit_code = run(&opts);
        assert_eq!(exit_code, 1);
    }

    #[test]
    fn test_json_output_starts_with_brace_and_is_valid_json() {
        let d = std::env::temp_dir().join(format!("mu-analyze-json-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&d);
        let sample = d.join("sample.txt");
        let _ = std::fs::write(&sample, b"test data");

        let cancel = Arc::new(AtomicBool::new(false));
        let (root, stats) = scan_path(&d, WalkerOptions::default(), None, cancel).unwrap();
        let json_str = json::to_json_pretty(&root, &stats).unwrap();

        assert!(json_str.starts_with('{'));
        assert!(json_str.contains("\"path\""));
        assert!(json_str.contains("\"size\""));

        let val: serde_json::Value = serde_json::from_str(&json_str).unwrap();
        assert!(val.is_object());
        assert_eq!(
            val["root"]["name"],
            d.file_name().unwrap().to_str().unwrap()
        );

        let _ = std::fs::remove_dir_all(&d);
    }
}
