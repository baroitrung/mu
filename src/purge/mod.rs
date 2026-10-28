//! Project artifact purge command — safely cleans heavy build artifacts.

pub mod model;
pub mod scanner;

use std::io::IsTerminal;
use std::path::{Path, PathBuf};

use crate::error::Result;
use crate::oplog;
use crate::size;
use crate::trash;
use crate::tui::select::{self, SelectOutcome, Selection};
use crate::xdg;

pub use model::Options;

/// Interactive selection hook (review F4). A non-TTY (or a stdin without a
/// keyboard) reports [`SelectOutcome::NonInteractive`] so the caller falls
/// back to the headless `--yes` gate, while a user's own cancel stays
/// [`SelectOutcome::Cancelled`] — the two are never conflated (review p4r1#1).
pub type SelectFn = Box<dyn Fn(&[model::PurgeArtifact]) -> SelectOutcome>;
/// Deletion hook; production always routes through `trash::safe_delete`.
pub type DeleteFn = Box<dyn Fn(&Path) -> Result<()>>;

/// Injectable dependencies for `mu purge` — mirrors the `Deps` pattern in
/// `optimize.rs`/`uninstall/mod.rs` so the operations log, the interactive
/// selector, and the deletion path are testable without mutating the
/// environment or the user's real trash.
pub struct Deps {
    /// XDG config home holding `mu/config.toml`.
    pub config_home: PathBuf,
    /// XDG data home holding `mu/operations.log`.
    pub data_home: PathBuf,
    /// `MU_NO_OPLOG=1` disables the operations log.
    pub no_oplog: bool,
    /// TTY detection hook for stdin.
    pub is_terminal: Box<dyn Fn() -> bool>,
    /// Interactive selection (review F4).
    pub select: SelectFn,
    /// Deletion seam; production always routes through `trash::safe_delete`.
    pub delete: DeleteFn,
}

impl Deps {
    /// The production hooks.
    pub fn real() -> Self {
        Self {
            config_home: xdg::config_home(),
            data_home: xdg::data_home(),
            no_oplog: std::env::var_os("MU_NO_OPLOG").as_deref() == Some(std::ffi::OsStr::new("1")),
            is_terminal: Box::new(|| std::io::stdin().is_terminal()),
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
    if let Err(e) = crate::config::load_whitelist_from(&deps.config_home) {
        eprintln!("invalid mu configuration: {e}");
        return 1;
    }

    // Fail-closed non-interactive gate: without --yes or --dry-run, require a
    // TTY before running any expensive scan or attempting any deletion.
    if !opts.auto_yes && !opts.dry_run && !(deps.is_terminal)() {
        eprintln!("Non-interactive: pass --yes to confirm, or --dry-run to preview.\nAborted.");
        return 1;
    }

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
            SelectOutcome::NonInteractive => {
                eprintln!(
                    "Non-interactive: pass --yes to confirm, or --dry-run to preview.\nAborted."
                );
                return 1;
            }
            outcome => match select::resolve(outcome) {
                Selection::Stop {
                    code,
                    message,
                    is_error,
                } => {
                    if is_error {
                        eprintln!("{message}");
                    } else {
                        println!("\n{message}");
                    }
                    return code;
                }
                Selection::Proceed(indices) => indices
                    .iter()
                    .filter_map(|&i| all_artifacts.get(i))
                    .collect(),
            },
        }
    };

    // Non-dry-run execution: route strictly through the SafeDelete seam.
    let mut deleted_count = 0;
    let mut freed_bytes = 0;
    let attempted = selected.len();
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
    // Review rvp4#6: a run where every deletion failed must not look like
    // success to a script.
    if attempted > 0 && deleted_count == 0 {
        eprintln!("error: every artifact failed to trash — nothing was deleted");
        return 1;
    }
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
        test_deps_terminal(root, true, select)
    }

    fn test_deps_terminal(
        root: &Path,
        is_terminal: bool,
        select: SelectFn,
    ) -> (Deps, Arc<Mutex<Vec<PathBuf>>>) {
        let log = Arc::new(Mutex::new(Vec::new()));
        let sink = log.clone();
        let deps = Deps {
            config_home: root.join("config-home"),
            data_home: root.join("data-home"),
            no_oplog: false,
            is_terminal: Box::new(move || is_terminal),
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

        // Non-TTY: the selector reports "not interactive".
        let (deps, deleted) =
            test_deps_terminal(&root, false, Box::new(|_| SelectOutcome::NonInteractive));
        let opts = Options {
            paths: vec![root.clone()],
            dry_run: false,
            auto_yes: false,
            include_empty: false,
            debug: false,
        };

        let code = run_in(&opts, &deps);
        assert_eq!(code, 1);
        assert!(sentinel.exists(), "aborted purge must not delete anything");
        assert!(deleted.lock().unwrap().is_empty());

        oplog::close_logger();
        let _ = fs::remove_dir_all(&root);
    }

    // Review p4r1#1: a user pressing q/Esc on a TTY must see a cancel, not
    // the "Non-interactive" text meant for a caller with no terminal.
    #[test]
    fn purge_cancel_is_not_reported_as_non_interactive() {
        let root = tempdir("cancel");
        let proj = marked_project(&root, "proj", "target");
        let sentinel = proj.join("target").join("output.bin");

        let (deps, deleted) = test_deps(&root, Box::new(|_| SelectOutcome::Cancelled));
        let opts = Options {
            paths: vec![root.clone()],
            dry_run: false,
            auto_yes: false,
            include_empty: false,
            debug: false,
        };

        assert_eq!(run_in(&opts, &deps), 0, "a cancel is not an error");
        assert!(
            sentinel.exists(),
            "cancelled purge must not delete anything"
        );
        assert!(deleted.lock().unwrap().is_empty());

        oplog::close_logger();
        let _ = fs::remove_dir_all(&root);
    }

    // Review p4r1#1: the two "nothing was deleted" paths are distinct — the
    // real selector reports non-interactive only when there is no TTY.
    #[test]
    fn purge_non_tty_selector_reports_non_interactive() {
        // No TTY in the test process: the real selector says exactly that.
        assert_eq!(
            crate::tui::select::select_indices("pick", vec![("/tmp/a".into(), "1 B".into())]),
            SelectOutcome::NonInteractive
        );
    }

    // Review rvp4#6: when every deletion fails the run must not exit 0.
    #[test]
    fn purge_exits_nonzero_when_every_deletion_fails() {
        let root = tempdir("allfail");
        let proj = marked_project(&root, "proj", "target");
        let sentinel = proj.join("target").join("output.bin");

        // `--yes` selects everything; the delete seam fails every call.
        let deps = Deps {
            config_home: root.join("config-home"),
            data_home: root.join("data-home"),
            no_oplog: true,
            is_terminal: Box::new(|| false),
            select: Box::new(|_| panic!("--yes must not prompt")),
            delete: Box::new(|_| Err(crate::error::Error::Msg("trash unavailable".into()))),
        };
        let opts = Options {
            paths: vec![root.clone()],
            dry_run: false,
            auto_yes: true,
            include_empty: false,
            debug: false,
        };

        assert_eq!(run_in(&opts, &deps), 1, "all-fail must be nonzero");
        assert!(sentinel.exists());

        oplog::close_logger();
        let _ = fs::remove_dir_all(&root);
    }

    // A mixed run still exits 0 — the item is only about the all-fail case.
    #[test]
    fn purge_exits_zero_when_at_least_one_deletion_succeeds() {
        let root = tempdir("mixed");
        marked_project(&root, "proj", "target");

        let deps = Deps {
            config_home: root.join("config-home"),
            data_home: root.join("data-home"),
            no_oplog: true,
            is_terminal: Box::new(|| false),
            select: Box::new(|_| panic!("--yes must not prompt")),
            delete: Box::new(|p| {
                if p.ends_with("target") {
                    Ok(())
                } else {
                    Err(crate::error::Error::Msg("trash unavailable".into()))
                }
            }),
        };
        let opts = Options {
            paths: vec![root.clone()],
            dry_run: false,
            auto_yes: true,
            include_empty: false,
            debug: false,
        };

        assert_eq!(run_in(&opts, &deps), 0);
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

        let (deps, _deleted) = test_deps(&root, Box::new(|_| SelectOutcome::NonInteractive));
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
                SelectOutcome::Confirmed(vec![0])
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

        let (deps, deleted) = test_deps(&root, Box::new(|_| SelectOutcome::Confirmed(Vec::new())));
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

    #[test]
    fn purge_fails_closed_on_malformed_or_unreadable_config() {
        let root = tempdir("bad-cfg");
        let cfg_dir = root.join("config-home").join("mu");
        fs::create_dir_all(&cfg_dir).unwrap();
        fs::write(cfg_dir.join("config.toml"), "invalid = [toml").unwrap();

        let proj = marked_project(&root, "proj", "target");
        let sentinel = proj.join("target").join("output.bin");

        let (deps, deleted) = test_deps(
            &root,
            Box::new(|_| panic!("bad config must not reach select")),
        );
        let opts = Options {
            paths: vec![root.clone()],
            dry_run: true,
            auto_yes: false,
            include_empty: false,
            debug: false,
        };

        let code = run_in(&opts, &deps);
        assert_eq!(code, 1, "malformed config must cause nonzero exit");
        assert!(
            sentinel.exists(),
            "malformed config must not delete anything"
        );
        assert!(deleted.lock().unwrap().is_empty());

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn purge_real_run_non_interactive_without_yes_exits_one() {
        let root = tempdir("real-nonint");
        let proj = marked_project(&root, "proj", "target");
        let sentinel = proj.join("target").join("output.bin");
        let opts = Options {
            paths: vec![root.clone()],
            dry_run: false,
            auto_yes: false,
            include_empty: false,
            debug: false,
        };

        let code = run(&opts);
        assert_eq!(code, 1, "mu purge without --yes on non-TTY must exit 1");
        assert!(sentinel.exists(), "must not delete anything");

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn purge_real_run_dry_run_exits_zero() {
        let root = tempdir("real-dryrun");
        let proj = marked_project(&root, "proj", "target");
        let sentinel = proj.join("target").join("output.bin");
        let opts = Options {
            paths: vec![root.clone()],
            dry_run: true,
            auto_yes: false,
            include_empty: false,
            debug: false,
        };

        let code = run(&opts);
        assert_eq!(code, 0, "mu purge --dry-run must exit 0");
        assert!(sentinel.exists(), "must not delete anything");

        let _ = fs::remove_dir_all(&root);
    }
}
