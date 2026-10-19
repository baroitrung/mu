//! Scanner for finding installer packages in download directories.

use std::fs;
use std::path::{Path, PathBuf};

use crate::installer::inspect::{inspect_targz, inspect_zip};
use crate::installer::model::{InstallerPackage, PackageKind};
use crate::paths;
use crate::xdg;

/// Resolve roots to scan for installer packages.
pub fn resolve_installer_roots(configured: &[PathBuf]) -> Vec<PathBuf> {
    if !configured.is_empty() {
        return configured
            .iter()
            .map(|p| {
                if p.is_absolute() {
                    p.clone()
                } else if let Ok(canon) = fs::canonicalize(p) {
                    canon
                } else {
                    std::env::current_dir()
                        .unwrap_or_else(|_| PathBuf::from("/"))
                        .join(p)
                }
            })
            .collect();
    }

    let home = xdg::home_dir();
    let candidates = ["Downloads", "Desktop", "Documents"];
    candidates
        .iter()
        .map(|d| home.join(d))
        .filter(|p| p.is_dir())
        .collect()
}

/// Scan a root directory for installer packages.
pub fn scan_root(root: &Path) -> Vec<InstallerPackage> {
    let mut packages = Vec::new();

    if paths::validate_cleanup_root(root).is_err() {
        return packages;
    }

    walk_installer_dir(root, root, 0, &mut packages);
    packages
}

fn walk_installer_dir(
    root: &Path,
    current: &Path,
    depth: usize,
    packages: &mut Vec<InstallerPackage>,
) {
    if depth > 3 {
        return;
    }

    let entries = match fs::read_dir(current) {
        Ok(e) => e,
        Err(_) => return,
    };

    for entry in entries.flatten() {
        let path = entry.path();
        let file_type = match entry.file_type() {
            Ok(ft) => ft,
            Err(_) => continue,
        };

        if file_type.is_symlink() {
            continue;
        }

        let name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n,
            None => continue,
        };

        if name.starts_with('.') {
            continue;
        }

        if file_type.is_dir() {
            walk_installer_dir(root, &path, depth + 1, packages);
            continue;
        }

        // File candidate
        if paths::validate_cleanup_candidate(root, &path).is_err() {
            continue;
        }

        let lower = name.to_ascii_lowercase();
        let kind = if lower.ends_with(".deb") {
            Some(PackageKind::Deb)
        } else if lower.ends_with(".appimage") {
            Some(PackageKind::AppImage)
        } else if lower.ends_with(".iso") {
            Some(PackageKind::Iso)
        } else if (lower.ends_with(".tar.gz") || lower.ends_with(".tgz")) && inspect_targz(&path) {
            Some(PackageKind::TarGz)
        } else if lower.ends_with(".zip") && inspect_zip(&path) {
            Some(PackageKind::Zip)
        } else {
            None
        };

        if let Some(kind) = kind {
            let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
            packages.push(InstallerPackage {
                path: path.clone(),
                name: name.to_string(),
                kind,
                size,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;

    fn tempdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "mu-test-installer-scan-{}-{}-{}",
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
    fn scan_finds_deb_and_appimage() {
        let root = tempdir("scan");
        let deb = root.join("app_1.0.0_amd64.deb");
        let appimage = root.join("editor.AppImage");
        let iso = root.join("ubuntu.iso");
        let text = root.join("notes.txt");

        File::create(&deb).unwrap();
        File::create(&appimage).unwrap();
        File::create(&iso).unwrap();
        File::create(&text).unwrap();

        let pkgs = scan_root(&root);
        assert_eq!(pkgs.len(), 3);
        let names: Vec<&str> = pkgs.iter().map(|p| p.name.as_str()).collect();
        assert!(names.contains(&"app_1.0.0_amd64.deb"));
        assert!(names.contains(&"editor.AppImage"));
        assert!(names.contains(&"ubuntu.iso"));
        assert!(!names.contains(&"notes.txt"));

        let _ = fs::remove_dir_all(&root);
    }
}
