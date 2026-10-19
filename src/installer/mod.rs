//! Package installer cleanup command — discovers and removes .deb, .AppImage, .iso, and archive installers.

pub mod inspect;
pub mod model;
pub mod scanner;

use crate::oplog;
use crate::size;
use crate::trash;

pub use model::Options;

/// Entry point for `mu installer`.
pub fn run(opts: &Options) -> i32 {
    let roots = scanner::resolve_installer_roots(&opts.paths);
    if roots.is_empty() {
        println!("No installer directories found to scan.");
        return 0;
    }

    let mut all_packages = Vec::new();
    for root in &roots {
        let pkgs = scanner::scan_root(root);
        all_packages.extend(pkgs);
    }

    if all_packages.is_empty() {
        println!("No installer packages found.");
        return 0;
    }

    let total_bytes: u64 = all_packages.iter().map(|p| p.size).sum();

    println!(
        "Found {} installer package(s) (total {}):",
        all_packages.len(),
        size::human_size(total_bytes as i64)
    );
    for p in &all_packages {
        println!(
            "  {:<12}  {:<10}  {}",
            p.kind.as_str(),
            size::human_size(p.size as i64),
            p.path.display()
        );
    }

    if opts.dry_run {
        println!("\nThis is a DRY RUN. No files will be deleted.");
        println!(
            "Dry run — {} reclaimable across {} package(s)",
            size::human_size(total_bytes as i64),
            all_packages.len()
        );
        for p in &all_packages {
            oplog::log_outcome("installer", &p.path.to_string_lossy(), "dry-run");
        }
        return 0;
    }

    if !opts.auto_yes {
        println!("\nNon-interactive: pass --yes to confirm, or --dry-run to preview.");
        println!("Aborted.");
        return 0;
    }

    // Execution: route strictly through trash::safe_delete
    let mut removed_count = 0;
    let mut freed_bytes = 0;
    for p in &all_packages {
        match trash::safe_delete(&p.path, false) {
            Ok(()) => {
                oplog::log_outcome("installer", &p.path.to_string_lossy(), "success");
                removed_count += 1;
                freed_bytes += p.size;
            }
            Err(e) => {
                oplog::log_outcome("installer", &p.path.to_string_lossy(), "failure");
                eprintln!("  warn: failed to trash {}: {}", p.path.display(), e);
            }
        }
    }

    println!(
        "\nAll done — removed {} installer package(s) (freed {})",
        removed_count,
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
            "mu-test-installer-exec-{}-{}-{}",
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
    fn installer_dry_run_leaves_files_intact() {
        let root = tempdir("dry");
        let dummy = root.join("dummy.deb");
        File::create(&dummy).unwrap();

        let opts = Options {
            paths: vec![root.clone()],
            dry_run: true,
            auto_yes: false,
            debug: false,
        };

        let code = run(&opts);
        assert_eq!(code, 0);
        assert!(dummy.exists(), "dry run must leave installer file intact");

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn installer_aborts_without_auto_yes() {
        let root = tempdir("abort");
        let dummy = root.join("app.AppImage");
        File::create(&dummy).unwrap();

        let opts = Options {
            paths: vec![root.clone()],
            dry_run: false,
            auto_yes: false,
            debug: false,
        };

        let code = run(&opts);
        assert_eq!(code, 0);
        assert!(dummy.exists(), "aborted installer must not delete anything");

        let _ = fs::remove_dir_all(&root);
    }
}
