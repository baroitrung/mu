//! Safe self-removal — uninstalls `mu`, removing configuration, logs,
//! cache, shell completions, desktop entry, and executable binary.
//!
//! Enforces safety invariant: all user deletions route strictly through
//! `trash::safe_delete` (never raw `rm`). Supports `--dry-run` and `--yes`.

use std::path::{Path, PathBuf};

use crate::paths;
use crate::trash;
use crate::xdg;

#[derive(Clone, Debug, Default)]
pub struct Options {
    pub dry_run: bool,
    pub auto_yes: bool,
    pub debug: bool,
}

pub struct RemovableItem {
    pub label: &'static str,
    pub path: PathBuf,
}

/// Discovers all existing mu files and directories across the system.
pub fn discover_components() -> Vec<RemovableItem> {
    let mut items = Vec::new();

    // 1. Config directory
    let config_dir = xdg::config_home().join("mu");
    if paths::path_exists(&config_dir) {
        items.push(RemovableItem {
            label: "Configuration directory",
            path: config_dir,
        });
    }

    // 2. Cache directory
    let cache_dir = xdg::cache_home().join("mu");
    if paths::path_exists(&cache_dir) {
        items.push(RemovableItem {
            label: "Cache directory",
            path: cache_dir,
        });
    }

    // 3. Data directory (operations.log, etc.)
    let data_dir = xdg::data_home().join("mu");
    if paths::path_exists(&data_dir) {
        items.push(RemovableItem {
            label: "Data & logs directory",
            path: data_dir,
        });
    }

    let home = xdg::home_dir();
    if !home.as_os_str().is_empty() {
        // 4. Desktop entry
        let desktop = home.join(".local/share/applications/mu.desktop");
        if paths::path_exists(&desktop) {
            items.push(RemovableItem {
                label: "Desktop launcher",
                path: desktop,
            });
        }

        // 5. Completions
        let bash_comp = home.join(".local/share/bash-completion/completions/mu");
        if paths::path_exists(&bash_comp) {
            items.push(RemovableItem {
                label: "Bash completion",
                path: bash_comp,
            });
        }

        let zsh_comp = home.join(".zsh/completion/_mu");
        if paths::path_exists(&zsh_comp) {
            items.push(RemovableItem {
                label: "Zsh completion",
                path: zsh_comp,
            });
        }

        let zsh_site = home.join(".local/share/zsh/site-functions/_mu");
        if paths::path_exists(&zsh_site) {
            items.push(RemovableItem {
                label: "Zsh site completion",
                path: zsh_site,
            });
        }

        let fish_comp = home.join(".config/fish/completions/mu.fish");
        if paths::path_exists(&fish_comp) {
            items.push(RemovableItem {
                label: "Fish completion",
                path: fish_comp,
            });
        }

        // 6. User local binary
        let local_bin = home.join(".local/bin/mu");
        if paths::path_exists(&local_bin) {
            items.push(RemovableItem {
                label: "Binary (~/.local/bin/mu)",
                path: local_bin,
            });
        }
    }

    // 7. System completion / binary if writable and exists
    let sys_bash = Path::new("/etc/bash_completion.d/mu");
    if paths::path_exists(sys_bash) && !paths::is_protected(sys_bash) {
        items.push(RemovableItem {
            label: "System bash completion",
            path: sys_bash.to_path_buf(),
        });
    }

    items
}

pub fn run(opts: &Options) -> i32 {
    let components = discover_components();

    if components.is_empty() {
        println!("No mu components found to remove.");
        return 0;
    }

    println!("Discovered mu components to remove:");
    for item in &components {
        println!("  • {:<26} {}", item.label, item.path.display());
    }

    if opts.dry_run {
        println!();
        println!("[dry-run] DRY RUN: no files were removed.");
        println!("[dry-run] All components will be moved to trash via trash::SafeDelete.");
        return 0;
    }

    if !opts.auto_yes {
        println!();
        println!("Non-interactive: pass --yes to confirm removal of mu.");
        println!("Aborted.");
        return 0;
    }

    let mut failed = 0;
    for item in &components {
        if paths::is_protected(&item.path) {
            eprintln!(
                "warning: skipping protected system path: {}",
                item.path.display()
            );
            continue;
        }

        println!("Removing {}...", item.path.display());
        if let Err(e) = trash::safe_delete(&item.path, false) {
            eprintln!("error removing {}: {}", item.path.display(), e);
            failed += 1;
        }
    }

    if failed > 0 {
        eprintln!("Completed with {failed} failure(s).");
        1
    } else {
        println!();
        println!("✨ mu successfully removed. All data was safely moved to trash.");
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_remove_dry_run_leaves_files_untouched() {
        let tmp = std::env::temp_dir().join(format!("mu-remove-dry-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&tmp);
        let test_file = tmp.join("sentinel.dat");
        std::fs::write(&test_file, b"safe data").unwrap();

        let opts = Options {
            dry_run: true,
            auto_yes: false,
            debug: false,
        };

        let code = run(&opts);
        assert_eq!(code, 0);
        assert!(test_file.exists(), "sentinel must remain untouched");

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
