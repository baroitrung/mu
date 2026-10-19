//! Project artifact purge command — safely cleans heavy build artifacts.

pub mod model;
pub mod scanner;

use crate::oplog;
use crate::size;
use crate::trash;

pub use model::Options;

/// Entry point for `mu purge`.
pub fn run(opts: &Options) -> i32 {
    let roots = scanner::resolve_scan_roots(&opts.paths);
    if roots.is_empty() {
        println!("No project directories found to scan.");
        return 0;
    }

    let mut all_artifacts = Vec::new();
    for root in &roots {
        let artifacts = scanner::scan_root(root, opts.include_empty);
        all_artifacts.extend(artifacts);
    }

    if all_artifacts.is_empty() {
        println!("No project build artifacts found.");
        return 0;
    }

    let total_bytes: u64 = all_artifacts.iter().map(|a| a.size).sum();

    println!(
        "Found {} project build artifact(s) (total {}):",
        all_artifacts.len(),
        size::human_size(total_bytes as i64)
    );
    for a in &all_artifacts {
        println!(
            "  {:<16}  {:<10}  {}",
            a.kind,
            size::human_size(a.size as i64),
            a.path.display()
        );
    }

    if opts.dry_run {
        println!("\nThis is a DRY RUN. No files will be deleted.");
        println!(
            "Dry run — {} reclaimable across {} artifact(s)",
            size::human_size(total_bytes as i64),
            all_artifacts.len()
        );
        for a in &all_artifacts {
            oplog::log_outcome("purge", &a.path.to_string_lossy(), "dry-run");
        }
        return 0;
    }

    if !opts.auto_yes {
        println!("\nNon-interactive: pass --yes to confirm, or --dry-run to preview.");
        println!("Aborted.");
        return 0;
    }

    // Non-dry-run execution: route strictly through trash::safe_delete
    let mut deleted_count = 0;
    let mut freed_bytes = 0;
    for a in &all_artifacts {
        match trash::safe_delete(&a.path, false) {
            Ok(()) => {
                oplog::log_outcome("purge", &a.path.to_string_lossy(), "success");
                deleted_count += 1;
                freed_bytes += a.size;
            }
            Err(e) => {
                oplog::log_outcome("purge", &a.path.to_string_lossy(), "failure");
                eprintln!("  warn: failed to trash {}: {}", a.path.display(), e);
            }
        }
    }

    println!(
        "\nAll done — purged {} artifact(s) (freed {})",
        deleted_count,
        size::human_size(freed_bytes as i64)
    );
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{self, File};
    use std::path::PathBuf;

    fn tempdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "mu-test-purge-exec-{}-{}-{}",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn purge_dry_run_leaves_files_intact() {
        let root = tempdir("dry");
        let nm = root.join("node_modules");
        fs::create_dir_all(&nm).unwrap();
        let sentinel = nm.join("sentinel.js");
        File::create(&sentinel).unwrap();

        let opts = Options {
            paths: vec![root.clone()],
            dry_run: true,
            auto_yes: false,
            include_empty: false,
            debug: false,
        };

        let code = run(&opts);
        assert_eq!(code, 0);
        assert!(sentinel.exists(), "dry run must leave sentinel intact");

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn purge_aborts_without_auto_yes() {
        let root = tempdir("abort");
        let nm = root.join("target");
        fs::create_dir_all(&nm).unwrap();
        let sentinel = nm.join("output.bin");
        File::create(&sentinel).unwrap();

        let opts = Options {
            paths: vec![root.clone()],
            dry_run: false,
            auto_yes: false,
            include_empty: false,
            debug: false,
        };

        let code = run(&opts);
        assert_eq!(code, 0);
        assert!(sentinel.exists(), "aborted purge must not delete anything");

        let _ = fs::remove_dir_all(&root);
    }
}
