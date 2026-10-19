//! Filesystem scanner for project build directories.

use std::fs;
use std::path::{Path, PathBuf};

use crate::paths;
use crate::purge::model::PurgeArtifact;
use crate::size;
use crate::xdg;

/// Known project build artifact directory names.
pub const BUILD_DIR_NAMES: &[&str] = &[
    "node_modules",
    "target",
    "dist",
    "build",
    ".venv",
    "__pycache__",
    ".pytest_cache",
    ".tox",
    ".gradle",
    ".next",
];

/// Names that are unambiguous build artifacts by name alone.
pub const STRONG_BUILD_DIR_NAMES: &[&str] = &[
    "node_modules",
    ".venv",
    "__pycache__",
    ".pytest_cache",
    ".tox",
    ".gradle",
    ".next",
];

/// Generic build-output names that only count as artifacts next to a project
/// manifest — otherwise they are ordinary source directories (review F2).
pub const GENERIC_BUILD_DIR_NAMES: &[&str] = &["target", "dist", "build"];

/// Manifests that mark the parent directory as a project root.
pub const PROJECT_MARKERS: &[&str] = &[
    "package.json",
    "Cargo.toml",
    "pyproject.toml",
    "setup.py",
    "setup.cfg",
    "requirements.txt",
    "go.mod",
    "build.gradle",
    "build.gradle.kts",
    "settings.gradle",
    "settings.gradle.kts",
    "pom.xml",
    "Makefile",
];

/// True when `artifact`'s parent looks like a project root (review F2).
fn has_project_marker(artifact: &Path) -> bool {
    match artifact.parent() {
        Some(parent) => PROJECT_MARKERS.iter().any(|m| parent.join(m).is_file()),
        None => false,
    }
}

/// Resolve scan roots. If `configured` is non-empty, use those.
/// Otherwise, check `$XDG_CONFIG_HOME/mu/purge_paths`.
/// If absent, fall back to standard project directories under `$HOME`.
pub fn resolve_scan_roots(configured: &[PathBuf]) -> Vec<PathBuf> {
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

    let config_file = xdg::config_home().join("mu").join("purge_paths");
    if let Ok(content) = fs::read_to_string(&config_file) {
        let mut roots = Vec::new();
        for line in content.lines() {
            let trimmed = line.trim();
            if !trimmed.is_empty() && !trimmed.starts_with('#') {
                let p = PathBuf::from(trimmed);
                if p.exists() {
                    roots.push(p);
                }
            }
        }
        if !roots.is_empty() {
            return roots;
        }
    }

    let home = xdg::home_dir();
    let defaults = ["Projects", "Code", "src", "Development"];
    defaults
        .iter()
        .map(|d| home.join(d))
        .filter(|p| p.is_dir())
        .collect()
}

/// Recursively scan a root directory for build artifacts.
pub fn scan_root(root: &Path, include_empty: bool) -> Vec<PurgeArtifact> {
    let mut artifacts = Vec::new();

    // Validate root boundary
    if paths::validate_cleanup_root(root).is_err() {
        return artifacts;
    }

    walk_dir(root, root, include_empty, 0, &mut artifacts);
    artifacts
}

fn is_dir_empty(path: &Path) -> bool {
    match fs::read_dir(path) {
        Ok(mut it) => it.next().is_none(),
        Err(_) => true,
    }
}

fn walk_dir(
    root: &Path,
    current: &Path,
    include_empty: bool,
    depth: usize,
    artifacts: &mut Vec<PurgeArtifact>,
) {
    if depth > 10 {
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

        // Inviolable rule: never follow symlinks
        if file_type.is_symlink() {
            continue;
        }

        if !file_type.is_dir() {
            continue;
        }

        let name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n,
            None => continue,
        };

        // Skip version control internals
        if name == ".git" || name == ".hg" || name == ".svn" {
            continue;
        }

        if BUILD_DIR_NAMES.contains(&name) {
            // Review F2: generic names (`target`/`dist`/`build`) only count
            // when the parent carries a project manifest; a directory that
            // merely shares the name (e.g. `tool/src/build`) is ordinary
            // source, so recurse instead of reporting it.
            if GENERIC_BUILD_DIR_NAMES.contains(&name) && !has_project_marker(&path) {
                walk_dir(root, &path, include_empty, depth + 1, artifacts);
                continue;
            }
            // Validate candidate safety boundary
            if paths::validate_cleanup_candidate(root, &path).is_ok() {
                let empty = is_dir_empty(&path);
                if !empty || include_empty {
                    let (bytes, _) = size::dir_size(&path);
                    artifacts.push(PurgeArtifact {
                        path: path.clone(),
                        kind: name.to_string(),
                        size: bytes,
                    });
                }
            }
            continue;
        }

        // Recurse into subdirectories
        walk_dir(root, &path, include_empty, depth + 1, artifacts);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;

    fn tempdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "mu-test-purge-scan-{}-{}-{}",
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
    fn scan_finds_project_artifacts_and_skips_internal_recursion() {
        let root = tempdir("scan");
        let proj = root.join("my-project");
        let node_modules = proj.join("node_modules");
        let nested_inside_nm = node_modules.join("target"); // Should not be reported separately
        let venv = proj.join(".venv");

        fs::create_dir_all(&nested_inside_nm).unwrap();
        fs::create_dir_all(&venv).unwrap();

        File::create(node_modules.join("package.json")).unwrap();
        File::create(venv.join("pyvenv.cfg")).unwrap();

        let artifacts = scan_root(&root, false);
        assert_eq!(artifacts.len(), 2);
        let kinds: Vec<&str> = artifacts.iter().map(|a| a.kind.as_str()).collect();
        assert!(kinds.contains(&"node_modules"));
        assert!(kinds.contains(&".venv"));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn scan_respects_include_empty() {
        let root = tempdir("empty");
        let proj = root.join("empty-project");
        let dist = proj.join("dist");
        fs::create_dir_all(&dist).unwrap();
        // `dist` is a generic name — it needs a project manifest (review F2).
        File::create(proj.join("package.json")).unwrap();

        let without_empty = scan_root(&root, false);
        assert!(without_empty.is_empty());

        let with_empty = scan_root(&root, true);
        assert_eq!(with_empty.len(), 1);
        assert_eq!(with_empty[0].kind, "dist");

        let _ = fs::remove_dir_all(&root);
    }

    // Review F2: `build`/`dist`/`target` without a project manifest in the
    // parent are ordinary source directories, not build artifacts.
    #[test]
    fn scan_skips_generic_names_without_project_marker() {
        let root = tempdir("marker-none");
        // A Go/Python source package literally named `build` under `src/`.
        fs::create_dir_all(root.join("tool/src/build")).unwrap();
        File::create(root.join("tool/src/build/pkg.go")).unwrap();
        // Committed release assets named `dist/` with no manifest above it.
        fs::create_dir_all(root.join("site/dist/release")).unwrap();
        File::create(root.join("site/dist/release/asset.js")).unwrap();
        // A plain `target/` holder with no build system.
        fs::create_dir_all(root.join("notes/target")).unwrap();
        File::create(root.join("notes/target/scratch.txt")).unwrap();

        let artifacts = scan_root(&root, false);
        assert!(
            artifacts.is_empty(),
            "unmarked generic dirs must be skipped: {artifacts:?}"
        );

        let _ = fs::remove_dir_all(&root);
    }

    // Review F2: the same generic names ARE artifacts next to their manifest.
    #[test]
    fn scan_finds_generic_names_with_parent_marker() {
        let root = tempdir("marker-yes");
        fs::create_dir_all(root.join("rustproj/target/debug")).unwrap();
        File::create(root.join("rustproj/Cargo.toml")).unwrap();
        fs::create_dir_all(root.join("gradleproj/build/libs")).unwrap();
        File::create(root.join("gradleproj/build.gradle")).unwrap();
        fs::create_dir_all(root.join("jsproj/dist")).unwrap();
        File::create(root.join("jsproj/package.json")).unwrap();
        File::create(root.join("jsproj/dist/bundle.js")).unwrap();

        let artifacts = scan_root(&root, false);
        let kinds: Vec<&str> = artifacts.iter().map(|a| a.kind.as_str()).collect();
        assert_eq!(artifacts.len(), 3, "got {kinds:?}");
        for name in ["target", "build", "dist"] {
            assert!(kinds.contains(&name), "missing {name}: {kinds:?}");
        }

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn scan_skips_symlinks() {
        let root = tempdir("symlink");
        let real_dir = tempdir("target-real");
        fs::create_dir_all(real_dir.join("sub")).unwrap();
        File::create(real_dir.join("artifact.bin")).unwrap();

        #[cfg(unix)]
        {
            let link = root.join("node_modules");
            std::os::unix::fs::symlink(&real_dir, &link).unwrap();

            let artifacts = scan_root(&root, false);
            assert!(artifacts.is_empty());
        }

        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&real_dir);
    }
}
