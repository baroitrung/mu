//! Disk analysis scan cache under `$XDG_CACHE_HOME/mu/analyze/`.
//!
//! Keys cache entries by schema version and canonical root path.
//! Writes atomically via temporary files on the same filesystem.
//! Read errors, corrupt files, schema mismatches, and payloads over
//! [`MAX_CACHE_BYTES`] are treated as cache misses.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::analyze::model::{FileNode, ScanStats};

/// Schema version for disk analysis cache payloads.
pub const SCHEMA_VERSION: u32 = 1;

/// Upper bound on the on-disk size of a single cache entry (64 MiB).
///
/// A payload serializes to roughly 420 bytes per scanned entry, so a project
/// with tens of thousands of files fits comfortably, while a `$HOME` or `/`
/// scan (millions of entries) would be hundreds of MB. Such a file is never
/// worth writing: the next run would have to read it back whole. Oversized
/// payloads are silently not cached, and the next run scans again.
pub const MAX_CACHE_BYTES: u64 = 64 * 1024 * 1024;

/// Cache record stored on disk as JSON.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct CacheEntry {
    pub schema_version: u32,
    pub root_path: PathBuf,
    pub root: FileNode,
    pub stats: ScanStats,
}

/// Compute canonical root path for cache keying, falling back to lexical cleanup.
pub fn canonical_root(root: &Path) -> PathBuf {
    fs::canonicalize(root).unwrap_or_else(|_| crate::xdg::clean(root))
}

/// Compute the cache filename for a given root path.
pub fn cache_key(root: &Path) -> String {
    let canonical = canonical_root(root);
    let mut hasher = Sha256::new();
    hasher.update(canonical.as_os_str().as_encoded_bytes());
    let hash = format!("{:x}", hasher.finalize());
    format!("v{}_{}.json", SCHEMA_VERSION, hash)
}

/// Resolve the cache directory under `$XDG_CACHE_HOME/mu/analyze`.
pub fn cache_dir() -> PathBuf {
    cache_dir_in(&crate::xdg::cache_home())
}

/// Resolve the cache directory under a specified cache home.
pub fn cache_dir_in(cache_home: &Path) -> PathBuf {
    cache_home.join("mu").join("analyze")
}

/// Resolve the cache file path for a given root path.
pub fn cache_path(root: &Path) -> PathBuf {
    cache_path_in(&crate::xdg::cache_home(), root)
}

/// Resolve the cache file path for a given root under a specified cache home.
pub fn cache_path_in(cache_home: &Path, root: &Path) -> PathBuf {
    cache_dir_in(cache_home).join(cache_key(root))
}

/// Load cached scan results if available, valid, and matching current schema version.
/// Returns `None` on any I/O error, corrupt data, or version mismatch.
pub fn load_cache(root: &Path) -> Option<(FileNode, ScanStats)> {
    load_cache_in(&crate::xdg::cache_home(), root)
}

/// Load cached scan results from a specified cache home.
pub fn load_cache_in(cache_home: &Path, root: &Path) -> Option<(FileNode, ScanStats)> {
    load_cache_limited(cache_home, root, MAX_CACHE_BYTES)
}

fn load_cache_limited(
    cache_home: &Path,
    root: &Path,
    max_bytes: u64,
) -> Option<(FileNode, ScanStats)> {
    let path = cache_path_in(cache_home, root);
    let len = fs::metadata(&path).ok()?.len();
    if len > max_bytes {
        // Written before the size guard existed, or the limit shrank: drop it
        // so it cannot linger. It is a miss either way.
        let _ = fs::remove_file(&path);
        return None;
    }
    let content = fs::read_to_string(&path).ok()?;
    let entry: CacheEntry = serde_json::from_str(&content).ok()?;
    if entry.schema_version != SCHEMA_VERSION {
        return None;
    }
    Some((entry.root, entry.stats))
}

/// Atomically write scan results to cache under `$XDG_CACHE_HOME/mu/analyze`.
///
/// Returns `Ok(None)` when the serialized payload exceeds [`MAX_CACHE_BYTES`];
/// nothing is written in that case.
pub fn save_cache(
    root: &Path,
    root_node: &FileNode,
    stats: &ScanStats,
) -> Result<Option<PathBuf>, std::io::Error> {
    save_cache_in(&crate::xdg::cache_home(), root, root_node, stats)
}

/// Atomically write scan results to cache under a specified cache home.
///
/// Returns `Ok(None)` when the serialized payload exceeds [`MAX_CACHE_BYTES`];
/// nothing is written in that case.
pub fn save_cache_in(
    cache_home: &Path,
    root: &Path,
    root_node: &FileNode,
    stats: &ScanStats,
) -> Result<Option<PathBuf>, std::io::Error> {
    save_cache_limited(cache_home, root, root_node, stats, MAX_CACHE_BYTES)
}

fn save_cache_limited(
    cache_home: &Path,
    root: &Path,
    root_node: &FileNode,
    stats: &ScanStats,
    max_bytes: u64,
) -> Result<Option<PathBuf>, std::io::Error> {
    let target_path = cache_path_in(cache_home, root);
    let entry = CacheEntry {
        schema_version: SCHEMA_VERSION,
        root_path: canonical_root(root),
        root: root_node.clone(),
        stats: stats.clone(),
    };

    let json_bytes = serde_json::to_vec_pretty(&entry)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;

    if json_bytes.len() as u64 > max_bytes {
        return Ok(None);
    }

    let dir = cache_dir_in(cache_home);
    fs::create_dir_all(&dir)?;

    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let count = COUNTER.fetch_add(1, Ordering::Relaxed);
    let tmp_name = format!(".tmp.{}.{}.{}", std::process::id(), count, cache_key(root));
    let tmp_path = dir.join(tmp_name);

    let write_res = (|| {
        let mut file = fs::File::create(&tmp_path)?;
        file.write_all(&json_bytes)?;
        file.sync_all()?;
        fs::rename(&tmp_path, &target_path)?;
        Ok(())
    })();

    if let Err(e) = write_res {
        let _ = fs::remove_file(&tmp_path);
        return Err(e);
    }

    Ok(Some(target_path))
}

/// Invalidate (remove) the cached scan for the given root path.
pub fn invalidate_cache(root: &Path) {
    invalidate_cache_in(&crate::xdg::cache_home(), root);
}

/// Invalidate (remove) the cached scan for the given root under a specified cache home.
pub fn invalidate_cache_in(cache_home: &Path, root: &Path) {
    let path = cache_path_in(cache_home, root);
    let _ = fs::remove_file(path);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analyze::model::LargestFile;

    fn test_temp_dir(suffix: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("mu-cache-test-{}-{}", suffix, std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn sample_tree(root_dir: &Path) -> (FileNode, ScanStats) {
        let child_file = root_dir.join("a.txt");
        let mut root = FileNode::new("test_root".into(), root_dir.to_path_buf(), 1024, true, 1);
        let child = FileNode::new("a.txt".into(), child_file.clone(), 1024, false, 1);
        root.children = vec![child];
        root.item_count = 1;

        let stats = ScanStats {
            total_bytes: 1024,
            total_bytes_human: "1.0 KB".into(),
            total_files: 1,
            total_dirs: 1,
            errors: 0,
            duration: std::time::Duration::from_millis(50),
            largest_files: vec![LargestFile::new(child_file, 1024)],
        };

        (root, stats)
    }

    #[test]
    fn test_cache_round_trip() {
        let temp = test_temp_dir("roundtrip");
        let cache_home = temp.join("cache");
        let scan_root = temp.join("scandir");
        fs::create_dir_all(&scan_root).unwrap();

        let (orig_root, orig_stats) = sample_tree(&scan_root);

        let saved_path = save_cache_in(&cache_home, &scan_root, &orig_root, &orig_stats)
            .expect("save_cache must succeed")
            .expect("a small payload must be written");
        assert!(saved_path.exists(), "cache file must exist");

        let loaded = load_cache_in(&cache_home, &scan_root);
        assert!(
            loaded.is_some(),
            "load_cache must return Some on valid cache"
        );
        let (loaded_root, loaded_stats) = loaded.unwrap();

        assert_eq!(loaded_root.name, orig_root.name);
        assert_eq!(loaded_root.path, orig_root.path);
        assert_eq!(loaded_root.size, orig_root.size);
        assert_eq!(loaded_root.size_human, orig_root.size_human);
        assert_eq!(loaded_root.is_dir, orig_root.is_dir);
        assert_eq!(loaded_root.item_count, orig_root.item_count);
        assert_eq!(loaded_root.children.len(), orig_root.children.len());
        assert_eq!(loaded_root.children[0].name, orig_root.children[0].name);
        assert_eq!(loaded_root.children[0].path, orig_root.children[0].path);
        assert_eq!(loaded_root.children[0].size, orig_root.children[0].size);

        assert_eq!(loaded_stats.total_bytes, orig_stats.total_bytes);
        assert_eq!(loaded_stats.total_bytes_human, orig_stats.total_bytes_human);
        assert_eq!(loaded_stats.total_files, orig_stats.total_files);
        assert_eq!(loaded_stats.total_dirs, orig_stats.total_dirs);
        assert_eq!(loaded_stats.errors, orig_stats.errors);
        assert_eq!(loaded_stats.largest_files, orig_stats.largest_files);

        let _ = fs::remove_dir_all(&temp);
    }

    #[test]
    fn test_oversized_payload_is_not_saved() {
        let temp = test_temp_dir("oversized-save");
        let cache_home = temp.join("cache");
        let scan_root = temp.join("scandir");
        fs::create_dir_all(&scan_root).unwrap();

        let (root, stats) = sample_tree(&scan_root);
        let saved = save_cache_limited(&cache_home, &scan_root, &root, &stats, 16)
            .expect("an oversized payload is not an error");
        assert_eq!(saved, None, "nothing may be written over the size limit");
        assert!(
            !cache_path_in(&cache_home, &scan_root).exists(),
            "oversized payload must leave no cache file"
        );
        assert!(
            !cache_dir_in(&cache_home).exists(),
            "oversized payload must not even create the cache directory"
        );
        assert!(
            load_cache_in(&cache_home, &scan_root).is_none(),
            "a scan with no cache file must be a miss"
        );

        // Same payload under a limit it fits: the guard must not block writes.
        let saved = save_cache_in(&cache_home, &scan_root, &root, &stats).unwrap();
        assert!(saved.is_some(), "a small payload must be written");

        let _ = fs::remove_dir_all(&temp);
    }

    #[test]
    fn test_oversized_cache_file_on_disk_is_miss_and_pruned() {
        let temp = test_temp_dir("oversized-load");
        let cache_home = temp.join("cache");
        let scan_root = temp.join("scandir");
        fs::create_dir_all(&scan_root).unwrap();

        let (root, stats) = sample_tree(&scan_root);
        let path = save_cache_in(&cache_home, &scan_root, &root, &stats)
            .unwrap()
            .expect("small payload is written");
        assert!(path.exists());

        // A file left by an older build (or a lower limit) is over the current
        // budget: it must be a miss, and must not stay on disk.
        assert!(load_cache_limited(&cache_home, &scan_root, 16).is_none());
        assert!(
            !path.exists(),
            "an oversized cache file must be dropped on load"
        );

        let _ = fs::remove_dir_all(&temp);
    }

    #[test]
    fn test_corrupt_file_is_miss() {
        let temp = test_temp_dir("corrupt");
        let cache_home = temp.join("cache");
        let scan_root = temp.join("scandir");
        fs::create_dir_all(&scan_root).unwrap();

        let target_path = cache_path_in(&cache_home, &scan_root);
        fs::create_dir_all(target_path.parent().unwrap()).unwrap();
        fs::write(&target_path, b"{{ corrupted invalid json payload !!!").unwrap();

        let loaded = load_cache_in(&cache_home, &scan_root);
        assert!(
            loaded.is_none(),
            "corrupt cache file must result in cache miss"
        );

        let _ = fs::remove_dir_all(&temp);
    }

    #[test]
    fn test_version_mismatch_is_miss() {
        let temp = test_temp_dir("version-mismatch");
        let cache_home = temp.join("cache");
        let scan_root = temp.join("scandir");
        fs::create_dir_all(&scan_root).unwrap();

        let (orig_root, orig_stats) = sample_tree(&scan_root);
        let entry = CacheEntry {
            schema_version: SCHEMA_VERSION + 1, // future / mismatched version
            root_path: canonical_root(&scan_root),
            root: orig_root,
            stats: orig_stats,
        };

        let target_path = cache_path_in(&cache_home, &scan_root);
        fs::create_dir_all(target_path.parent().unwrap()).unwrap();
        let json = serde_json::to_string_pretty(&entry).unwrap();
        fs::write(&target_path, json.as_bytes()).unwrap();

        let loaded = load_cache_in(&cache_home, &scan_root);
        assert!(
            loaded.is_none(),
            "schema version mismatch must result in cache miss"
        );

        let _ = fs::remove_dir_all(&temp);
    }

    #[test]
    fn test_invalidate_after_delete() {
        let temp = test_temp_dir("invalidate");
        let cache_home = temp.join("cache");
        let scan_root = temp.join("scandir");
        fs::create_dir_all(&scan_root).unwrap();

        let (orig_root, orig_stats) = sample_tree(&scan_root);
        let path = save_cache_in(&cache_home, &scan_root, &orig_root, &orig_stats)
            .unwrap()
            .expect("a small payload must be written");
        assert!(path.exists(), "cache file must exist prior to invalidation");

        invalidate_cache_in(&cache_home, &scan_root);
        assert!(
            !path.exists(),
            "cache file must be removed after invalidation"
        );
        assert!(
            load_cache_in(&cache_home, &scan_root).is_none(),
            "loading after invalidation must return None"
        );

        let _ = fs::remove_dir_all(&temp);
    }

    #[test]
    fn test_relative_xdg_cache_home_ignored() {
        // Test that xdg::cache_home() ignores relative paths and returns absolute path.
        // We set env var in a safe scope and verify resolution.
        unsafe {
            std::env::set_var("XDG_CACHE_HOME", "relative/path/cache");
        }
        let ch = crate::xdg::cache_home();
        assert!(
            ch.is_absolute(),
            "cache_home() must return an absolute fallback path, not relative: {:?}",
            ch
        );
        assert!(
            !ch.starts_with("relative"),
            "relative XDG_CACHE_HOME must be ignored"
        );

        // Also verify cache_dir() builds upon it
        let cd = cache_dir();
        assert!(cd.is_absolute(), "cache_dir() must be absolute");
        assert!(
            cd.ends_with("mu/analyze"),
            "cache_dir must end with mu/analyze"
        );

        unsafe {
            std::env::remove_var("XDG_CACHE_HOME");
        }
    }

    #[test]
    fn test_atomic_write_no_leftover_temp() {
        let temp = test_temp_dir("atomic");
        let cache_home = temp.join("cache");
        let scan_root = temp.join("scandir");
        fs::create_dir_all(&scan_root).unwrap();

        let (orig_root, orig_stats) = sample_tree(&scan_root);
        let target = save_cache_in(&cache_home, &scan_root, &orig_root, &orig_stats)
            .unwrap()
            .expect("a small payload must be written");
        assert!(target.exists());

        let dir = cache_dir_in(&cache_home);
        let entries: Vec<_> = fs::read_dir(&dir).unwrap().flatten().collect();
        // Should only contain the target cache file, no .tmp files
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].path(), target);

        let _ = fs::remove_dir_all(&temp);
    }

    #[test]
    fn test_json_fresh_scan() {
        let temp = test_temp_dir("json-fresh");
        let scan_root = temp.join("scandir");
        fs::create_dir_all(&scan_root).unwrap();
        let file = scan_root.join("file.txt");
        fs::write(&file, b"fresh content on disk").unwrap();

        // Save a stale/poisoned entry to default cache
        let mut stale_root = FileNode::new("scandir".into(), scan_root.clone(), 999999, true, 1);
        stale_root.children = vec![FileNode::new(
            "file.txt".into(),
            file.clone(),
            999999,
            false,
            1,
        )];
        let stale_stats = ScanStats {
            total_bytes: 999999,
            total_bytes_human: "999.9 KB".into(),
            total_files: 1,
            total_dirs: 1,
            errors: 0,
            duration: std::time::Duration::from_millis(1),
            largest_files: vec![LargestFile::new(file.clone(), 999999)],
        };
        save_cache(&scan_root, &stale_root, &stale_stats).unwrap();
        assert!(load_cache(&scan_root).is_some());

        // When walker is invoked with bypass_cache: true (as in --json), it must ignore cache
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let walker_opts = crate::analyze::walker::WalkerOptions {
            bypass_cache: true,
            ..crate::analyze::walker::WalkerOptions::default()
        };
        let (root, stats) =
            crate::analyze::walker::scan_path(&scan_root, walker_opts, None, cancel).unwrap();

        // Must reflect real disk size (21 bytes), not stale cache (999999 bytes)
        assert_eq!(root.size, 21);
        assert_eq!(stats.total_bytes, 21);
        assert_ne!(root.size, 999999);

        // Also verify mu analyze --json runner behavior
        let opts = crate::analyze::Options {
            path: Some(scan_root.clone()),
            dry_run: false,
            json: true,
            debug: false,
        };
        let code = crate::analyze::run(&opts);
        assert_eq!(code, 0);

        // Clean up
        invalidate_cache(&scan_root);
        let _ = fs::remove_dir_all(&temp);
    }

    #[test]
    fn test_non_utf8_path_cache_round_trip() {
        use std::os::unix::ffi::OsStrExt;
        let temp = test_temp_dir("non-utf8");
        let cache_home = temp.join("cache");
        let bad_path = PathBuf::from(std::ffi::OsStr::from_bytes(
            b"/tmp/mu-test/bad-\xff-file.dat",
        ));
        let mut root = FileNode::new("test".into(), PathBuf::from("/tmp/mu-test"), 1024, true, 1);
        let child = FileNode::new(
            "bad-\u{fffd}-file.dat".into(),
            bad_path.clone(),
            1024,
            false,
            1,
        );
        root.children = vec![child];
        let stats = ScanStats {
            total_bytes: 1024,
            total_bytes_human: "1.0 KB".into(),
            total_files: 1,
            total_dirs: 1,
            errors: 0,
            duration: std::time::Duration::from_millis(10),
            largest_files: vec![LargestFile::new(bad_path, 1024)],
        };

        let saved = save_cache_in(&cache_home, Path::new("/tmp/mu-test"), &root, &stats);
        assert!(saved.is_ok(), "saving non-utf8 path to cache must succeed");

        let loaded = load_cache_in(&cache_home, Path::new("/tmp/mu-test"));
        assert!(
            loaded.is_some(),
            "loading non-utf8 path from cache must succeed"
        );
        let (loaded_root, loaded_stats) = loaded.unwrap();
        assert_eq!(loaded_root.size, 1024);
        assert_eq!(loaded_stats.largest_files.len(), 1);

        let _ = fs::remove_dir_all(&temp);
    }
}
