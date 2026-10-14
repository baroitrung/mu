//! `scan_browser.go` — the opt-in browser/VSCode cache target.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::error::Error;
use crate::{paths, trash};

use super::{CleanTarget, Deps, join_errors};

/// `browserCachePaths` — all browser cache paths that exist on the system.
/// `home` is `os.UserHomeDir()`.
/// Protected productivity domains whose ServiceWorker and IndexedDB storage
/// must not be removed to prevent losing offline PWA state.
const PROTECTED_PWA_DOMAINS: &[&str] = &["google", "docs.google.com", "github", "figma", "linear"];

/// Returns true if the path points to ServiceWorker or IndexedDB data belonging
/// to a protected productivity domain.
pub(crate) fn is_protected_pwa_path(path: &Path) -> bool {
    let s = path.to_string_lossy().to_lowercase();
    let is_pwa_storage =
        s.contains("service worker") || s.contains("serviceworker") || s.contains("indexeddb");
    if !is_pwa_storage {
        return false;
    }
    PROTECTED_PWA_DOMAINS.iter().any(|d| s.contains(d))
}

/// Helper to find Firefox-like `*/cache2` profile directories under a root.
fn find_firefox_cache2_paths(parent: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    if let Ok(entries) = std::fs::read_dir(parent) {
        for e in entries.flatten() {
            let profile = e.path();
            let Ok(meta) = std::fs::metadata(&profile) else {
                continue;
            };
            if !meta.is_dir() {
                continue;
            }
            if let Ok(inner) = std::fs::read_dir(&profile)
                && inner
                    .flatten()
                    .any(|ie| ie.file_name() == std::ffi::OsStr::new("cache2"))
            {
                found.push(profile.join("cache2"));
            }
        }
    }
    found
}

/// `browserCachePaths` — all browser cache paths that exist on the system.
/// `home` is `os.UserHomeDir()`.
pub(crate) fn browser_cache_paths_in(home: &Path) -> Vec<PathBuf> {
    let mut candidates = vec![
        // Google Chrome
        home.join(".config")
            .join("google-chrome")
            .join("Default")
            .join("Cache"),
        // Chromium
        home.join(".config")
            .join("chromium")
            .join("Default")
            .join("Cache"),
        // VSCode
        home.join(".config").join("Code").join("CachedData"),
        home.join(".config").join("Code").join("CachedExtensions"),
        // Brave
        home.join(".config")
            .join("BraveSoftware")
            .join("Brave-Browser")
            .join("Default")
            .join("Cache"),
        // Microsoft Edge
        home.join(".config")
            .join("microsoft-edge")
            .join("Default")
            .join("Cache"),
        // Opera
        home.join(".config").join("opera").join("Cache"),
        // Vivaldi
        home.join(".config")
            .join("vivaldi")
            .join("Default")
            .join("Cache"),
        // Snap Chromium
        home.join("snap")
            .join("chromium")
            .join("common")
            .join(".cache")
            .join("chromium")
            .join("Default")
            .join("Cache"),
    ];

    // Firefox native: ~/.mozilla/firefox/*/cache2
    candidates.extend(find_firefox_cache2_paths(
        &home.join(".mozilla").join("firefox"),
    ));

    // Zen Browser: ~/.zen/*/cache2
    candidates.extend(find_firefox_cache2_paths(&home.join(".zen")));

    // Snap Firefox: ~/snap/firefox/common/.cache/mozilla/firefox/*/cache2
    candidates.extend(find_firefox_cache2_paths(
        &home
            .join("snap")
            .join("firefox")
            .join("common")
            .join(".cache")
            .join("mozilla")
            .join("firefox"),
    ));

    // Flatpak browsers under ~/.var/app/*/cache
    let var_app = home.join(".var").join("app");
    if let Ok(entries) = std::fs::read_dir(&var_app) {
        for e in entries.flatten() {
            let app_dir = e.path();
            let name = e.file_name();
            let name_str = name.to_string_lossy().to_lowercase();
            if name_str.contains("firefox")
                || name_str.contains("chrome")
                || name_str.contains("brave")
                || name_str.contains("chromium")
                || name_str.contains("zen")
            {
                let cache_dir = app_dir.join("cache");
                if paths::path_exists(&cache_dir) {
                    // Check for inner cache2 if firefox-based
                    let ff_inner = find_firefox_cache2_paths(&cache_dir);
                    if !ff_inner.is_empty() {
                        candidates.extend(ff_inner);
                    } else {
                        candidates.push(cache_dir);
                    }
                }
            }
        }
    }

    candidates
        .into_iter()
        .filter(|p| paths::path_exists(p) && !is_protected_pwa_path(p))
        .collect()
}

/// `browserCacheTarget` — CleanTarget for browser caches (opt-in).
pub(crate) fn browser_cache_target_in(deps: &Deps) -> CleanTarget {
    let scan_home = deps.home.clone();
    let exec_home = deps.home.clone();
    let exec_cfg = deps.config_home.clone();
    let exec_runner = Arc::clone(&deps.trash_runner);
    let exec_deps = deps.trash_deps.clone();

    CleanTarget {
        id: "browser-cache",
        label: "Browser Caches (Chrome/Firefox/VSCode/Snap/Flatpak)",
        requires_sudo: false,
        opt_in: true,
        scan: Box::new(move || {
            let mut total: i64 = 0;
            let mut scan_errors: Vec<Error> = Vec::new();
            for p in browser_cache_paths_in(&scan_home) {
                let (sz, err) = crate::size::dir_size(&p);
                match err {
                    None => total += sz as i64,
                    Some(e) => scan_errors.push(e),
                }
            }
            if scan_errors.is_empty() {
                (total, None)
            } else {
                (total, Some(join_errors(&scan_errors)))
            }
        }),
        preview: None,
        execute: Box::new(move |dry_run| {
            let mut delete_errors: Vec<Error> = Vec::new();
            for p in browser_cache_paths_in(&exec_home) {
                if is_protected_pwa_path(&p) {
                    continue;
                }
                let root = browser_cleanup_root(&exec_home, &p);
                if let Err(e) = paths::validate_cleanup_candidate(&root, &p) {
                    delete_errors.push(e);
                    continue;
                }
                if let Err(e) =
                    trash::safe_delete_with(&*exec_runner, &exec_cfg, &exec_deps, &p, dry_run)
                {
                    delete_errors.push(e);
                }
            }
            if delete_errors.is_empty() {
                Ok(())
            } else {
                Err(join_errors(&delete_errors))
            }
        }),
    }
}

/// `browserCleanupRoot` — paths under `~/.mozilla`, `~/snap`, `~/.var`, `~/.zen`
/// validate against their respective roots; everything else against `~/.config`.
pub(crate) fn browser_cleanup_root(home: &Path, path: &Path) -> PathBuf {
    let mozilla = home.join(".mozilla");
    let snap = home.join("snap");
    let var = home.join(".var");
    let zen = home.join(".zen");
    if path == mozilla || path.starts_with(&mozilla) {
        mozilla
    } else if path == snap || path.starts_with(&snap) {
        snap
    } else if path == var || path.starts_with(&var) {
        var
    } else if path == zen || path.starts_with(&zen) {
        zen
    } else {
        home.join(".config")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clean::test_support::*;
    use crate::runner::FakeRunner;
    use std::fs;

    // scan_browser_test.go: TestBrowserCacheTarget_scan
    #[test]
    fn browser_cache_target_scan() {
        let tmp = tempdir("browserscan");
        let cache_dir = tmp
            .join(".config")
            .join("google-chrome")
            .join("Default")
            .join("Cache");
        fs::create_dir_all(&cache_dir).unwrap();
        fs::write(cache_dir.join("testfile"), "hello cache").unwrap();
        let deps = deps_for_test(&tmp, Arc::new(FakeRunner::new()));
        let target = browser_cache_target_in(&deps);
        let (sz, scan_err) = (target.scan)();
        assert!(scan_err.is_none(), "browser scan: {scan_err:?}");
        assert!(sz > 0, "expected non-zero scan size, got {sz}");
        fs::remove_dir_all(&tmp).ok();
    }

    // scan_browser_test.go: TestBrowserCacheTarget_isOptIn
    #[test]
    fn browser_cache_target_is_opt_in() {
        let tmp = tempdir("browseroptin");
        let deps = deps_for_test(&tmp, Arc::new(FakeRunner::new()));
        let target = browser_cache_target_in(&deps);
        assert!(target.opt_in, "browser cache target should be opt-in");
        fs::remove_dir_all(&tmp).ok();
    }

    // scan_browser_test.go: TestBrowserCacheTarget_scanEmpty
    #[test]
    fn browser_cache_target_scan_empty() {
        let tmp = tempdir("browserempty");
        let deps = deps_for_test(&tmp, Arc::new(FakeRunner::new()));
        let target = browser_cache_target_in(&deps);
        let (sz, scan_err) = (target.scan)();
        assert!(scan_err.is_none(), "browser scan: {scan_err:?}");
        assert_eq!(sz, 0, "expected 0 for empty home, got {sz}");
        fs::remove_dir_all(&tmp).ok();
    }

    // scan_browser_test.go: TestBrowserCacheTargetDryRunValidatesAndKeepsCache
    #[test]
    fn browser_cache_dry_run_validates_and_keeps_cache() {
        let tmp = tempdir("browserdry");
        let cache = tmp.join(".config").join("Code").join("CachedData");
        fs::create_dir_all(&cache).unwrap();
        let deps = deps_for_test(&tmp, Arc::new(FakeRunner::new()));
        let target = browser_cache_target_in(&deps);
        (target.execute)(true).expect("dry-run execute");
        assert!(
            std::fs::symlink_metadata(&cache).is_ok(),
            "dry-run removed browser cache"
        );
        assert_eq!(
            browser_cleanup_root(&tmp, &cache),
            tmp.join(".config"),
            "cleanup root"
        );
        let mozilla_cache = tmp
            .join(".mozilla")
            .join("firefox")
            .join("x")
            .join("cache2");
        assert_eq!(
            browser_cleanup_root(&tmp, &mozilla_cache),
            tmp.join(".mozilla"),
            "mozilla root"
        );
        fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn test_is_protected_pwa_path() {
        assert!(is_protected_pwa_path(Path::new(
            "/home/user/.config/google-chrome/Default/Service Worker/CacheStorage/docs.google.com"
        )));
        assert!(is_protected_pwa_path(Path::new(
            "/home/user/.config/google-chrome/Default/IndexedDB/https_github.com_0.indexeddb.leveldb"
        )));
        assert!(is_protected_pwa_path(Path::new(
            "/home/user/.config/BraveSoftware/Brave-Browser/Default/ServiceWorker/figma"
        )));
        assert!(!is_protected_pwa_path(Path::new(
            "/home/user/.config/google-chrome/Default/Cache"
        )));
        assert!(!is_protected_pwa_path(Path::new(
            "/home/user/.mozilla/firefox/abc/cache2"
        )));
    }

    #[test]
    fn test_browser_cleanup_root_expanded() {
        let home = Path::new("/home/user");
        assert_eq!(
            browser_cleanup_root(home, &home.join("snap/firefox/common/.cache")),
            home.join("snap")
        );
        assert_eq!(
            browser_cleanup_root(home, &home.join(".var/app/org.mozilla.firefox/cache")),
            home.join(".var")
        );
        assert_eq!(
            browser_cleanup_root(home, &home.join(".zen/profile.default/cache2")),
            home.join(".zen")
        );
        assert_eq!(
            browser_cleanup_root(home, &home.join(".mozilla/firefox/profile.default/cache2")),
            home.join(".mozilla")
        );
        assert_eq!(
            browser_cleanup_root(home, &home.join(".config/google-chrome/Default/Cache")),
            home.join(".config")
        );
    }
}
