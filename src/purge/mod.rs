//! Project artifact purge command — safely cleans heavy build artifacts.

pub mod model;
pub mod scanner;

use std::path::{Path, PathBuf};

use crate::error::Result;
use crate::oplog;
use crate::size;
use crate::trash;
use crate::xdg;

pub use model::Options;

/// Interactive selection hook (review F4). `None` means "not interactive"
/// (stdout is not a TTY) — the caller then falls back to the headless
/// `--yes` gate. `Some(indices)` is the chosen subset (possibly empty).
pub type SelectFn = Box<dyn Fn(&[model::PurgeArtifact]) -> Option<Vec<usize>>>;
/// Deletion hook; production always routes through `trash::safe_delete`.
pub type DeleteFn = Box<dyn Fn(&Path) -> Result<()>>;

/// Injectable dependencies for `mu purge` — mirrors the `Deps` pattern in
/// `optimize.rs`/`uninstall/mod.rs` so the operations log, the interactive
/// selector, and the deletion path are testable without mutating the
/// environment or the user's real trash.
pub struct Deps {
    /// XDG data home holding `mu/operations.log`.
    pub data_home: PathBuf,
    /// `MU_NO_OPLOG=1` disables the operations log.
    pub no_oplog: bool,
    /// Interactive selection (review F4).
    pub select: SelectFn,
    /// Deletion seam; production always routes through `trash::safe_delete`.
    pub delete: DeleteFn,
}

impl Deps {
    /// The production hooks.
    pub fn real() -> Self {
        Self {
            data_home: xdg::data_home(),
            no_oplog: std::env::var_os("MU_NO_OPLOG").as_deref() == Some(std::ffi::OsStr::new("1")),
            select: Box::new(|artifacts| {
                let rows: Vec<(String, String)> = artifacts
                    .iter()
                    .map(|a| {
                        (
                            a.path.display().to_string(),
                            format!("{}  {}", a.kind, size::human_size(a.size as i64)),
                        )
                    })
                    .collect();
                crate::tui::select::select_indices("Select project artifacts to purge", rows)
            }),
            delete: Box::new(|p| trash::safe_delete(p, false)),
        }
    }
}

/// Entry point for `mu purge`.
pub fn run(opts: &Options) -> i32 {
    run_in(opts, &Deps::real())
}

/// Testable core of [`run`].
pub(crate) fn run_in(opts: &Options, deps: &Deps) -> i32 {
    // Review F1: every destructive command opens the operations log itself —
    // `log_outcome` silently no-ops while `LOG_FILE` is closed.
    if !deps.no_oplog
        && let Err(e) = oplog::init_logger_at(&deps.data_home)
        && opts.debug
    {
        eprintln!("warn: could not open log: {e}");
    }

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

    // Review F4: without `--yes` and on a TTY, let the user pick which
    // artifacts to purge (the spec's "interactive selection ... or headless
    // removal with --yes"). A non-TTY keeps the headless gate.
    let selected: Vec<&model::PurgeArtifact> = if opts.auto_yes {
        all_artifacts.iter().collect()
    } else {
        match (deps.select)(&all_artifacts) {
            None => {
                println!("\nNon-interactive: pass --yes to confirm, or --dry-run to preview.");
                println!("Aborted.");
                return 0;
            }
            Some(indices) if indices.is_empty() => {
                println!("\nNothing selected. Aborted.");
                return 0;
            }
            Some(indices) => indices
                .iter()
                .filter_map(|&i| all_artifacts.get(i))
                .collect(),
        }
    };

    // Non-dry-run execution: route strictly through the SafeDelete seam.
    let mut deleted_count = 0;
    let mut freed_bytes = 0;
    for a in selected {
        match (deps.delete)(&a.path) {
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
    use std::sync::{Arc, Mutex};

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

    /// Deps rooted at a temp `data_home` with a no-op delete that records the
    /// paths it was asked to remove — no env mutation, no real trash.
    fn test_deps(root: &Path, select: SelectFn) -> (Deps, Arc<Mutex<Vec<PathBuf>>>) {
        let log = Arc::new(Mutex::new(Vec::new()));
        let sink = log.clone();
        let deps = Deps {
            data_home: root.join("data-home"),
            no_oplog: false,
            select,
            delete: Box::new(move |p| {
                sink.lock().unwrap().push(p.to_path_buf());
                Ok(())
            }),
        };
        (deps, log)
    }

    fn marked_project(root: &Path, name: &str, artifact: &str) -> PathBuf {
        let proj = root.join(name);
        fs::create_dir_all(proj.join(artifact)).unwrap();
        // `target`/`build`/`dist` require a project manifest (review F2).
        File::create(proj.join("Cargo.toml")).unwrap();
        File::create(proj.join(artifact).join("output.bin")).unwrap();
        proj
    }

    #[test]
    fn purge_dry_run_leaves_files_intact() {
        let root = tempdir("dry");
        let proj = marked_project(&root, "proj", "target");
        let sentinel = proj.join("target").join("output.bin");

        let (deps, deleted) = test_deps(&root, Box::new(|_| panic!("dry run must not select")));
        let opts = Options {
            paths: vec![root.clone()],
            dry_run: true,
            auto_yes: false,
            include_empty: false,
            debug: false,
        };

        let code = run_in(&opts, &deps);
        assert_eq!(code, 0);
        assert!(sentinel.exists(), "dry run must leave sentinel intact");
        assert!(deleted.lock().unwrap().is_empty());

        oplog::close_logger();
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn purge_aborts_without_auto_yes() {
        let root = tempdir("abort");
        let proj = marked_project(&root, "proj", "target");
        let sentinel = proj.join("target").join("output.bin");

        // Non-TTY: the selector reports "not interactive" (None).
        let (deps, deleted) = test_deps(&root, Box::new(|_| None));
        let opts = Options {
            paths: vec![root.clone()],
            dry_run: false,
            auto_yes: false,
            include_empty: false,
            debug: false,
        };

        let code = run_in(&opts, &deps);
        assert_eq!(code, 0);
        assert!(sentinel.exists(), "aborted purge must not delete anything");
        assert!(deleted.lock().unwrap().is_empty());

        oplog::close_logger();
        let _ = fs::remove_dir_all(&root);
    }

    // Review F1: a destructive command must open the operations log itself;
    // the dry-run path proves it because `log_outcome` writes only while
    // `LOG_FILE` is open.
    #[test]
    fn purge_dry_run_writes_operations_log() {
        let root = tempdir("log");
        marked_project(&root, "proj", "target");

        let (deps, _deleted) = test_deps(&root, Box::new(|_| None));
        let opts = Options {
            paths: vec![root.clone()],
            dry_run: true,
            auto_yes: false,
            include_empty: false,
            debug: false,
        };

        assert_eq!(run_in(&opts, &deps), 0);
        oplog::close_logger();

        let log = fs::read_to_string(deps.data_home.join("mu").join("operations.log"))
            .expect("operations.log must be created by purge");
        assert!(log.contains("purge"), "missing purge log line: {log:?}");
        assert!(log.contains("dry-run"), "missing dry-run outcome: {log:?}");

        let _ = fs::remove_dir_all(&root);
    }

    // Review F4: an interactive selection purges exactly the chosen subset
    // (and only it), still one logged SafeDelete per path.
    #[test]
    fn purge_interactive_selection_deletes_only_selected() {
        let root = tempdir("select");
        marked_project(&root, "proj-a", "target");
        marked_project(&root, "proj-b", "target");

        let (deps, deleted) = test_deps(
            &root,
            Box::new(|items| {
                assert_eq!(items.len(), 2, "both candidates offered");
                Some(vec![0])
            }),
        );
        let opts = Options {
            paths: vec![root.clone()],
            dry_run: false,
            auto_yes: false,
            include_empty: false,
            debug: false,
        };

        assert_eq!(run_in(&opts, &deps), 0);
        let deleted = deleted.lock().unwrap().clone();
        assert_eq!(
            deleted.len(),
            1,
            "exactly one artifact selected: {deleted:?}"
        );
        let mut others = vec![root.join("proj-a/target"), root.join("proj-b/target")];
        others.retain(|p| p != &deleted[0]);
        assert!(
            others[0].exists(),
            "the unselected artifact must be left alone"
        );

        oplog::close_logger();
        let _ = fs::remove_dir_all(&root);
    }

    // Review F4: selecting nothing deletes nothing and exits 0.
    #[test]
    fn purge_empty_selection_deletes_nothing() {
        let root = tempdir("none");
        let proj = marked_project(&root, "proj", "target");

        let (deps, deleted) = test_deps(&root, Box::new(|_| Some(Vec::new())));
        let opts = Options {
            paths: vec![root.clone()],
            dry_run: false,
            auto_yes: false,
            include_empty: false,
            debug: false,
        };

        assert_eq!(run_in(&opts, &deps), 0);
        assert!(deleted.lock().unwrap().is_empty());
        assert!(proj.join("target").exists());

        oplog::close_logger();
        let _ = fs::remove_dir_all(&root);
    }
}
