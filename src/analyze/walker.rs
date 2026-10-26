//! High-performance directory tree scanner with symlink and cross-device protection.

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::time::Instant;

use crate::analyze::model::{FileNode, LargestFile, ScanMessage, ScanStats, SortMode};

const MAX_LARGEST_FILES: usize = 20;
const PROGRESS_BATCH_ITEMS: u64 = 250;

/// Options for filesystem scanning.
#[derive(Clone, Debug)]
pub struct WalkerOptions {
    pub follow_symlinks: bool,
    pub cross_filesystems: bool,
    pub max_largest_files: usize,
    pub bypass_cache: bool,
}

impl Default for WalkerOptions {
    fn default() -> Self {
        Self {
            follow_symlinks: false,
            cross_filesystems: false,
            max_largest_files: MAX_LARGEST_FILES,
            bypass_cache: false,
        }
    }
}

/// Internal accumulator for scan statistics and top largest files.
struct WalkerContext {
    options: WalkerOptions,
    root_dev: u64,
    items_scanned: u64,
    bytes_scanned: u64,
    total_files: u64,
    total_dirs: u64,
    errors: u64,
    largest_heap: BinaryHeap<Reverse<(u64, PathBuf)>>,
    tx: Option<Sender<ScanMessage>>,
    cancel: Arc<AtomicBool>,
}

impl WalkerContext {
    fn new(
        options: WalkerOptions,
        root_dev: u64,
        tx: Option<Sender<ScanMessage>>,
        cancel: Arc<AtomicBool>,
    ) -> Self {
        Self {
            options,
            root_dev,
            items_scanned: 0,
            bytes_scanned: 0,
            total_files: 0,
            total_dirs: 0,
            errors: 0,
            largest_heap: BinaryHeap::new(),
            tx,
            cancel,
        }
    }

    fn record_file(&mut self, path: PathBuf, size: u64) {
        self.total_files += 1;
        self.items_scanned += 1;
        self.bytes_scanned += size;

        self.maybe_send_progress(&path);

        let max = self.options.max_largest_files;
        if max > 0 {
            if self.largest_heap.len() < max {
                self.largest_heap.push(Reverse((size, path)));
            } else if let Some(Reverse((smallest_size, _))) = self.largest_heap.peek()
                && size > *smallest_size
            {
                self.largest_heap.pop();
                self.largest_heap.push(Reverse((size, path)));
            }
        }
    }

    fn record_dir(&mut self, path: &Path) {
        self.total_dirs += 1;
        self.items_scanned += 1;
        self.maybe_send_progress(path);
    }

    fn maybe_send_progress(&mut self, current: &Path) {
        if self.items_scanned.is_multiple_of(PROGRESS_BATCH_ITEMS)
            && let Some(tx) = &self.tx
        {
            let _ = tx.send(ScanMessage::Progress {
                items_scanned: self.items_scanned,
                dirs_scanned: self.total_dirs,
                bytes_scanned: self.bytes_scanned,
                current_dir: current.to_path_buf(),
            });
        }
    }

    fn is_cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }
}

/// Synchronously scan a path and return the root `FileNode` and `ScanStats`.
pub fn scan_path(
    root_path: &Path,
    options: WalkerOptions,
    tx: Option<Sender<ScanMessage>>,
    cancel: Arc<AtomicBool>,
) -> Result<(FileNode, ScanStats), String> {
    let start_time = Instant::now();
    // A bypassing scan (`--json`) never reads the cache, so it must not write one.
    let bypass_cache = options.bypass_cache;

    let root_meta = match fs::metadata(root_path).or_else(|_| fs::symlink_metadata(root_path)) {
        Ok(m) => m,
        Err(e) => {
            crate::analyze::cache::invalidate_cache(root_path);
            let msg = format!("cannot inspect {}: {e}", root_path.display());
            if let Some(tx) = &tx {
                let _ = tx.send(ScanMessage::Error(msg.clone()));
            }
            return Err(msg);
        }
    };

    if !bypass_cache
        && !cancel.load(Ordering::Relaxed)
        && let Some((cached_root, cached_stats)) = crate::analyze::cache::load_cache(root_path)
    {
        if let Some(tx) = &tx {
            let _ = tx.send(ScanMessage::Complete {
                root: cached_root.clone(),
                stats: cached_stats.clone(),
            });
        }
        return Ok((cached_root, cached_stats));
    }

    let root_dev = root_meta.dev();
    let is_dir = root_meta.is_dir();
    let mut ctx = WalkerContext::new(options, root_dev, tx.clone(), cancel.clone());

    let mut root_node = if is_dir {
        ctx.record_dir(root_path);
        scan_dir_recursive(root_path, &root_meta, &mut ctx)
    } else {
        let size = root_meta.len();
        ctx.record_file(root_path.to_path_buf(), size);
        let name = root_path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| root_path.display().to_string());
        FileNode::new(name, root_path.to_path_buf(), size, false, root_dev)
    };

    root_node.sort_children(SortMode::SizeDesc);

    // Drain and order largest files (descending).
    let mut largest_files = Vec::new();
    while let Some(Reverse((size, path))) = ctx.largest_heap.pop() {
        largest_files.push(LargestFile::new(path, size));
    }
    largest_files.reverse();

    let stats = ScanStats {
        total_bytes: root_node.size,
        total_bytes_human: crate::size::human_size(root_node.size as i64),
        total_files: ctx.total_files,
        total_dirs: ctx.total_dirs,
        errors: ctx.errors,
        duration: start_time.elapsed(),
        largest_files,
    };

    if !bypass_cache && !cancel.load(Ordering::Relaxed) {
        let _ = crate::analyze::cache::save_cache(root_path, &root_node, &stats);
    }

    if let Some(tx) = &tx {
        let _ = tx.send(ScanMessage::Complete {
            root: root_node.clone(),
            stats: stats.clone(),
        });
    }

    Ok((root_node, stats))
}

fn scan_dir_recursive(dir_path: &Path, meta: &fs::Metadata, ctx: &mut WalkerContext) -> FileNode {
    let name = dir_path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| dir_path.display().to_string());

    let mut node = FileNode::new(name, dir_path.to_path_buf(), 0, true, meta.dev());

    if ctx.is_cancelled() {
        return node;
    }

    let entries = match fs::read_dir(dir_path) {
        Ok(e) => e,
        Err(err) => {
            ctx.errors += 1;
            node.err = Some(err.to_string());
            return node;
        }
    };

    let mut total_size = 0u64;
    let mut total_items = 0usize;

    for entry_res in entries {
        if ctx.is_cancelled() {
            break;
        }

        let entry = match entry_res {
            Ok(e) => e,
            Err(err) => {
                ctx.errors += 1;
                node.err = Some(err.to_string());
                continue;
            }
        };

        let child_path = entry.path();
        let child_meta = match entry.metadata() {
            Ok(m) => m,
            Err(_) => match fs::symlink_metadata(&child_path) {
                Ok(m) => m,
                Err(err) => {
                    ctx.errors += 1;
                    let file_name = entry.file_name().to_string_lossy().into_owned();
                    let mut child = FileNode::new(file_name, child_path, 0, false, ctx.root_dev);
                    child.err = Some(err.to_string());
                    node.children.push(child);
                    continue;
                }
            },
        };

        let file_type = child_meta.file_type();

        // 1. Never follow symlinks by default
        if file_type.is_symlink() && !ctx.options.follow_symlinks {
            let size = child_meta.len();
            ctx.record_file(child_path.clone(), size);
            total_size += size;
            total_items += 1;
            let file_name = entry.file_name().to_string_lossy().into_owned();
            node.children.push(FileNode::new(
                file_name,
                child_path,
                size,
                false,
                child_meta.dev(),
            ));
            continue;
        }

        // 2. Never cross filesystems by default
        if file_type.is_dir() {
            if child_meta.dev() != ctx.root_dev && !ctx.options.cross_filesystems {
                // Different filesystem boundary — record as a mount boundary and do not descend
                let file_name = entry.file_name().to_string_lossy().into_owned();
                let mut mount_node =
                    FileNode::new(file_name, child_path, 0, true, child_meta.dev());
                mount_node.err = Some("mount point (different filesystem)".into());
                node.children.push(mount_node);
                total_items += 1;
                continue;
            }

            ctx.record_dir(&child_path);
            let mut child_node = scan_dir_recursive(&child_path, &child_meta, ctx);
            child_node.sort_children(SortMode::SizeDesc);

            total_size += child_node.size;
            total_items += 1 + child_node.item_count;
            node.children.push(child_node);
        } else {
            let size = child_meta.len();
            ctx.record_file(child_path.clone(), size);
            total_size += size;
            total_items += 1;
            let file_name = entry.file_name().to_string_lossy().into_owned();
            node.children.push(FileNode::new(
                file_name,
                child_path,
                size,
                false,
                child_meta.dev(),
            ));
        }
    }

    node.size = total_size;
    node.size_human = crate::size::human_size(total_size as i64);
    node.item_count = total_items;
    node
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn tempdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "mu-test-analyze-{}-{}-{}",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let _ = fs::create_dir_all(&d);
        d
    }

    #[test]
    fn test_scan_aggregates_sizes_and_files() {
        let dir = tempdir("scan");
        let sub = dir.join("subdir");
        fs::create_dir(&sub).unwrap();

        let f1 = dir.join("file1.txt");
        let mut file1 = fs::File::create(&f1).unwrap();
        file1.write_all(&[b'a'; 100]).unwrap();

        let f2 = sub.join("file2.txt");
        let mut file2 = fs::File::create(&f2).unwrap();
        file2.write_all(&[b'b'; 250]).unwrap();

        let cancel = Arc::new(AtomicBool::new(false));
        let (root, stats) = scan_path(&dir, WalkerOptions::default(), None, cancel).unwrap();

        assert_eq!(root.size, 350);
        assert_eq!(stats.total_bytes, 350);
        assert_eq!(stats.total_files, 2);
        assert_eq!(stats.total_dirs, 2);
        assert_eq!(stats.largest_files.len(), 2);
        assert_eq!(stats.largest_files[0].size, 250);
        assert_eq!(stats.largest_files[1].size, 100);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_symlinks_are_not_followed() {
        let dir = tempdir("symlink");
        let target = dir.join("real_dir");
        fs::create_dir(&target).unwrap();

        let deep_file = target.join("deep.txt");
        let mut f = fs::File::create(&deep_file).unwrap();
        f.write_all(&[b'x'; 500]).unwrap();

        let symlink_path = dir.join("symlink_to_real");
        std::os::unix::fs::symlink(&target, &symlink_path).unwrap();

        let cancel = Arc::new(AtomicBool::new(false));
        let (root, stats) = scan_path(&dir, WalkerOptions::default(), None, cancel).unwrap();

        assert_eq!(stats.total_dirs, 2);
        let symlink_node = root
            .children
            .iter()
            .find(|c| c.name == "symlink_to_real")
            .unwrap();
        assert!(!symlink_node.is_dir);
        assert!(symlink_node.children.is_empty());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_cancellation() {
        let dir = tempdir("cancel");
        let f = dir.join("file.txt");
        fs::write(&f, "content").unwrap();

        let cancel = Arc::new(AtomicBool::new(true)); // Pre-cancelled
        let (root, _) = scan_path(&dir, WalkerOptions::default(), None, cancel).unwrap();
        assert!(root.children.is_empty());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_bypass_cache_scan_does_not_write_cache() {
        let dir = tempdir("bypass-nowrite");
        fs::write(dir.join("file.txt"), [b'x'; 64].as_slice()).unwrap();
        let cache_file = crate::analyze::cache::cache_path(&dir);
        let _ = fs::remove_file(&cache_file);

        let cancel = Arc::new(AtomicBool::new(false));
        let opts = WalkerOptions {
            bypass_cache: true,
            ..WalkerOptions::default()
        };
        let (_, stats) = scan_path(&dir, opts, None, cancel).unwrap();
        assert_eq!(stats.total_files, 1);
        assert!(
            !cache_file.exists(),
            "a bypassing scan must not write a cache file"
        );

        // The paired caching scan proves the assertion above can see a write.
        let cancel = Arc::new(AtomicBool::new(false));
        scan_path(&dir, WalkerOptions::default(), None, cancel).unwrap();
        assert!(
            cache_file.exists(),
            "a non-bypassing scan must seed the cache"
        );

        crate::analyze::cache::invalidate_cache(&dir);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_warm_scan_reads_cache_without_rescanning() {
        let dir = tempdir("warm");
        fs::write(dir.join("a.txt"), [b'a'; 100].as_slice()).unwrap();

        let cancel = Arc::new(AtomicBool::new(false));
        let (_, first) = scan_path(&dir, WalkerOptions::default(), None, cancel).unwrap();
        assert_eq!(first.total_bytes, 100);

        // Disk changed after the cached scan: a warm run reports the cached
        // tree, which is what makes the second TUI run cheap.
        fs::write(dir.join("b.txt"), [b'b'; 4000].as_slice()).unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        let (_, second) = scan_path(&dir, WalkerOptions::default(), None, cancel).unwrap();
        assert_eq!(second.total_bytes, 100);
        assert_eq!(second.total_files, 1);

        // Bypassing always sees the real disk.
        let cancel = Arc::new(AtomicBool::new(false));
        let opts = WalkerOptions {
            bypass_cache: true,
            ..WalkerOptions::default()
        };
        let (_, fresh) = scan_path(&dir, opts, None, cancel).unwrap();
        assert_eq!(fresh.total_bytes, 4100);
        assert_eq!(fresh.total_files, 2);

        crate::analyze::cache::invalidate_cache(&dir);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_scan_path_sends_error_on_root_failure() {
        let (tx, rx) = std::sync::mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let non_existent = Path::new("/tmp/mu-test-nonexistent-path-12345");
        let res = scan_path(non_existent, WalkerOptions::default(), Some(tx), cancel);
        assert!(res.is_err());
        let msg = rx
            .try_recv()
            .expect("ScanMessage::Error should be sent over tx on root inspect error");
        match msg {
            ScanMessage::Error(err) => {
                assert!(
                    err.contains("cannot inspect"),
                    "expected error message, got: {err}"
                );
            }
            _ => panic!("expected ScanMessage::Error, got: {:?}", msg),
        }
    }

    #[test]
    fn test_scan_root_symlink_to_directory_is_traversed() {
        let dir = tempdir("root_symlink_test");
        let real_dir = dir.join("real");
        fs::create_dir(&real_dir).unwrap();
        let inner_file = real_dir.join("inner.txt");
        fs::write(&inner_file, b"hello world").unwrap();

        let child_sub = real_dir.join("sub");
        fs::create_dir(&child_sub).unwrap();
        let child_sub_file = child_sub.join("sub_file.txt");
        fs::write(&child_sub_file, b"deep").unwrap();

        let child_link = real_dir.join("child_symlink");
        std::os::unix::fs::symlink(&child_sub, &child_link).unwrap();

        let link_dir = dir.join("link_to_real");
        std::os::unix::fs::symlink(&real_dir, &link_dir).unwrap();

        let cancel = Arc::new(AtomicBool::new(false));
        let (root, stats) = scan_path(&link_dir, WalkerOptions::default(), None, cancel).unwrap();

        assert!(
            root.is_dir,
            "root symlink pointing to directory should be scanned as directory"
        );
        assert_eq!(stats.total_files, 3); // inner.txt (11), sub_file.txt (4), child_symlink (len)
        assert_eq!(stats.total_dirs, 2); // root + sub (child_symlink is NOT counted as dir)
        let child_link_node = root
            .children
            .iter()
            .find(|c| c.name == "child_symlink")
            .expect("child_symlink should be in children");
        assert!(
            !child_link_node.is_dir,
            "child symlinks must not be followed as directories"
        );

        let _ = fs::remove_dir_all(&dir);
    }
}
