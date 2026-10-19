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
                path: local_bin.clone(),
            });
        }

        // Running executable (if user-owned and outside system prefixes)
        if let Ok(current_exe) = std::env::current_exe() {
            if current_exe != local_bin
                && paths::path_exists(&current_exe)
                && !paths::is_protected(&current_exe)
                && !items.iter().any(|i| i.path == current_exe)
            {
                items.push(RemovableItem {
                    label: "Binary (running)",
                    path: current_exe,
                });
            }
        }
    }

    // 7. System binary (/usr/local/bin/mu)
    let sys_bin = Path::new("/usr/local/bin/mu");
    if paths::path_exists(sys_bin) && !items.iter().any(|i| i.path == sys_bin) {
        items.push(RemovableItem {
            label: "System binary (/usr/local/bin/mu)",
            path: sys_bin.to_path_buf(),
        });
    }

    // 8. System completion (/etc/bash_completion.d/mu)
    let sys_bash = Path::new("/etc/bash_completion.d/mu");
    if paths::path_exists(sys_bash) && !items.iter().any(|i| i.path == sys_bash) {
        items.push(RemovableItem {
            label: "System bash completion",
            path: sys_bash.to_path_buf(),
        });
    }

    items
}

/// Generate standard FreeDesktop desktop launcher entry.
pub fn generate_desktop_entry(exec_path: Option<&Path>) -> String {
    let exec = exec_path.and_then(|p| p.to_str()).unwrap_or("mu");
    format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=mu\n\
         Comment=System cleaner and optimizer for Ubuntu\n\
         Exec={exec}\n\
         Icon=utilities-system-monitor\n\
         Terminal=true\n\
         Categories=System;Utility;\n\
         Keywords=cleaner;optimizer;system;cache;\n"
    )
}

/// Installs desktop launcher to `~/.local/share/applications/mu.desktop` (or custom directory).
pub fn install_desktop_entry(custom_path: Option<&Path>) -> std::io::Result<PathBuf> {
    let desktop_dir = custom_path
        .map(PathBuf::from)
        .unwrap_or_else(|| xdg::data_home().join("applications"));
    std::fs::create_dir_all(&desktop_dir)?;
    let target = desktop_dir.join("mu.desktop");
    let content = generate_desktop_entry(None);
    std::fs::write(&target, content)?;
    Ok(target)
}

pub fn run(opts: &Options) -> i32 {
    let components = discover_components();

    if components.is_empty() {
        println!("No mu components found to remove.");
        return 0;
    }

    let (user_components, sys_components): (Vec<_>, Vec<_>) = components
        .into_iter()
        .partition(|item| !paths::is_protected(&item.path));

    println!("Discovered mu components to remove:");
    for item in user_components.iter().chain(sys_components.iter()) {
        println!("  • {:<26} {}", item.label, item.path.display());
    }

    if opts.dry_run {
        println!();
        println!("[dry-run] DRY RUN: no files were removed.");
        println!("[dry-run] All user components will be moved to trash via trash::SafeDelete.");
        if !sys_components.is_empty() {
            println!();
            println!("[dry-run] System components require manual removal with root privileges:");
            for item in &sys_components {
                println!("[dry-run]   sudo rm {}", item.path.display());
            }
        }
        return 0;
    }

    if !opts.auto_yes {
        println!();
        println!("Non-interactive: pass --yes to confirm removal of mu.");
        println!("Aborted.");
        return 0;
    }

    let mut failed = 0;
    for item in &user_components {
        println!("Removing {}...", item.path.display());
        if let Err(e) = trash::safe_delete(&item.path, false) {
            eprintln!("error removing {}: {}", item.path.display(), e);
            failed += 1;
        }
    }

    if !sys_components.is_empty() {
        println!();
        println!("The following system files require root privileges to remove. Run:");
        for item in &sys_components {
            println!("  sudo rm {}", item.path.display());
        }
    }

    if failed > 0 {
        eprintln!("Completed with {failed} failure(s).");
        1
    } else {
        println!();
        println!("✨ mu successfully removed. All user data was safely moved to trash.");
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

    #[test]
    fn test_desktop_entry_generation() {
        let content = generate_desktop_entry(Some(Path::new("/usr/local/bin/mu")));
        assert!(content.contains("[Desktop Entry]"));
        assert!(content.contains("Exec=/usr/local/bin/mu"));
        assert!(content.contains("Name=mu"));
    }

    #[test]
    fn test_install_desktop_entry() {
        let tmp = std::env::temp_dir().join(format!("mu-desktop-test-{}", std::process::id()));
        let installed = install_desktop_entry(Some(&tmp)).expect("install succeeds");
        assert!(installed.exists());
        let content = std::fs::read_to_string(&installed).unwrap();
        assert!(content.contains("[Desktop Entry]"));
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
