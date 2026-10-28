//! Package installer cleanup command — discovers and removes .deb, .AppImage, .iso, and archive installers.

pub mod inspect;
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

/// Interactive selection hook (review F4). [`SelectOutcome::NonInteractive`]
/// means "no terminal, fall back to the headless `--yes` gate"; a user's
/// cancel is [`SelectOutcome::Cancelled`] (review p4r1#1).
pub type SelectFn = Box<dyn Fn(&[model::InstallerPackage]) -> SelectOutcome>;
/// Deletion hook; production always routes through `trash::safe_delete`.
pub type DeleteFn = Box<dyn Fn(&Path) -> Result<()>>;

/// Injectable dependencies for `mu installer` — see `purge::Deps`.
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
            select: Box::new(|packages| {
                let rows: Vec<(String, String)> = packages
                    .iter()
                    .map(|p| {
                        (
                            p.path.display().to_string(),
                            format!("{}  {}", p.kind.as_str(), size::human_size(p.size as i64)),
                        )
                    })
                    .collect();
                crate::tui::select::select_indices("Select installer packages to remove", rows)
            }),
            delete: Box::new(|p| trash::safe_delete(p, false)),
        }
    }
}

/// Entry point for `mu installer`.
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
    // TTY before running any expensive scan or attempting any removal.
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

    // Review F4: without `--yes` and on a TTY, let the user pick which
    // packages to remove; a non-TTY keeps the headless gate.
    let selected: Vec<&model::InstallerPackage> = if opts.auto_yes {
        all_packages.iter().collect()
    } else {
        match (deps.select)(&all_packages) {
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
                    .filter_map(|&i| all_packages.get(i))
                    .collect(),
            },
        }
    };

    // Execution: route strictly through the SafeDelete seam.
    let mut removed_count = 0;
    let mut freed_bytes = 0;
    let attempted = selected.len();
    for p in selected {
        match (deps.delete)(&p.path) {
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
    // Review rvp4#6: a run where every removal failed must not look like
    // success to a script.
    if attempted > 0 && removed_count == 0 {
        eprintln!("error: every installer package failed to trash — nothing was removed");
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

    fn fixture(root: &Path, name: &str) -> PathBuf {
        let f = root.join(name);
        File::create(&f).unwrap();
        f
    }

    #[test]
    fn installer_dry_run_leaves_files_intact() {
        let root = tempdir("dry");
        let dummy = fixture(&root, "dummy.deb");

        let (deps, removed) = test_deps(&root, Box::new(|_| panic!("dry run must not select")));
        let opts = Options {
            paths: vec![root.clone()],
            dry_run: true,
            auto_yes: false,
            debug: false,
        };

        let code = run_in(&opts, &deps);
        assert_eq!(code, 0);
        assert!(dummy.exists(), "dry run must leave installer file intact");
        assert!(removed.lock().unwrap().is_empty());

        oplog::close_logger();
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn installer_aborts_without_auto_yes() {
        let root = tempdir("abort");
        let dummy = fixture(&root, "app.AppImage");

        // Non-TTY: the selector reports "not interactive".
        let (deps, removed) =
            test_deps_terminal(&root, false, Box::new(|_| SelectOutcome::NonInteractive));
        let opts = Options {
            paths: vec![root.clone()],
            dry_run: false,
            auto_yes: false,
            debug: false,
        };

        let code = run_in(&opts, &deps);
        assert_eq!(code, 1);
        assert!(dummy.exists(), "aborted installer must not delete anything");
        assert!(removed.lock().unwrap().is_empty());

        oplog::close_logger();
        let _ = fs::remove_dir_all(&root);
    }

    // Review p4r1#1: a user cancelling the TTY multi-select deletes nothing
    // and is not reported as a non-interactive caller.
    #[test]
    fn installer_cancel_is_not_reported_as_non_interactive() {
        let root = tempdir("cancel");
        let dummy = fixture(&root, "app.AppImage");

        let (deps, removed) = test_deps(&root, Box::new(|_| SelectOutcome::Cancelled));
        let opts = Options {
            paths: vec![root.clone()],
            dry_run: false,
            auto_yes: false,
            debug: false,
        };

        assert_eq!(run_in(&opts, &deps), 0, "a cancel is not an error");
        assert!(
            dummy.exists(),
            "cancelled installer must not delete anything"
        );
        assert!(removed.lock().unwrap().is_empty());

        oplog::close_logger();
        let _ = fs::remove_dir_all(&root);
    }

    // Review rvp4#6: when every removal fails the run must not exit 0.
    #[test]
    fn installer_exits_nonzero_when_every_removal_fails() {
        let root = tempdir("allfail");
        let dummy = fixture(&root, "app.AppImage");

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
            debug: false,
        };

        assert_eq!(run_in(&opts, &deps), 1, "all-fail must be nonzero");
        assert!(dummy.exists(), "a failed removal must leave the file");

        oplog::close_logger();
        let _ = fs::remove_dir_all(&root);
    }

    // Review F1: `mu installer` must open the operations log itself.
    #[test]
    fn installer_dry_run_writes_operations_log() {
        let root = tempdir("log");
        fixture(&root, "dummy.deb");

        let (deps, _removed) = test_deps(&root, Box::new(|_| SelectOutcome::NonInteractive));
        let opts = Options {
            paths: vec![root.clone()],
            dry_run: true,
            auto_yes: false,
            debug: false,
        };

        assert_eq!(run_in(&opts, &deps), 0);
        oplog::close_logger();

        let log = fs::read_to_string(deps.data_home.join("mu").join("operations.log"))
            .expect("operations.log must be created by installer");
        assert!(
            log.contains("installer"),
            "missing installer log line: {log:?}"
        );
        assert!(log.contains("dry-run"), "missing dry-run outcome: {log:?}");

        let _ = fs::remove_dir_all(&root);
    }

    // Review F4: an interactive selection removes exactly the chosen subset.
    #[test]
    fn installer_interactive_selection_removes_only_selected() {
        let root = tempdir("select");
        let a = fixture(&root, "a.deb");
        let b = fixture(&root, "b.deb");

        let (deps, removed) = test_deps(
            &root,
            Box::new(|items| {
                assert_eq!(items.len(), 2, "both candidates offered");
                SelectOutcome::Confirmed(vec![1])
            }),
        );
        let opts = Options {
            paths: vec![root.clone()],
            dry_run: false,
            auto_yes: false,
            debug: false,
        };

        assert_eq!(run_in(&opts, &deps), 0);
        let removed = removed.lock().unwrap().clone();
        assert_eq!(
            removed.len(),
            1,
            "exactly one package selected: {removed:?}"
        );
        let kept = if removed[0] == a { &b } else { &a };
        assert!(kept.exists(), "unselected package must be left alone");

        oplog::close_logger();
        let _ = fs::remove_dir_all(&root);
    }

    // Review F4: selecting nothing removes nothing and exits 0.
    #[test]
    fn installer_empty_selection_removes_nothing() {
        let root = tempdir("none");
        let dummy = fixture(&root, "dummy.deb");

        let (deps, removed) = test_deps(&root, Box::new(|_| SelectOutcome::Confirmed(Vec::new())));
        let opts = Options {
            paths: vec![root.clone()],
            dry_run: false,
            auto_yes: false,
            debug: false,
        };

        assert_eq!(run_in(&opts, &deps), 0);
        assert!(removed.lock().unwrap().is_empty());
        assert!(dummy.exists());

        oplog::close_logger();
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn installer_fails_closed_on_malformed_or_unreadable_config() {
        let root = tempdir("bad-cfg");
        let cfg_dir = root.join("config-home").join("mu");
        fs::create_dir_all(&cfg_dir).unwrap();
        fs::write(cfg_dir.join("config.toml"), "invalid = [toml").unwrap();

        let dummy = fixture(&root, "dummy.deb");

        let (deps, removed) = test_deps(
            &root,
            Box::new(|_| panic!("bad config must not reach select")),
        );
        let opts = Options {
            paths: vec![root.clone()],
            dry_run: true,
            auto_yes: false,
            debug: false,
        };

        let code = run_in(&opts, &deps);
        assert_eq!(code, 1, "malformed config must cause nonzero exit");
        assert!(dummy.exists(), "malformed config must not delete anything");
        assert!(removed.lock().unwrap().is_empty());

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn installer_real_run_non_interactive_without_yes_exits_one() {
        let root = tempdir("real-nonint");
        let dummy = fixture(&root, "app.deb");
        let opts = Options {
            paths: vec![root.clone()],
            dry_run: false,
            auto_yes: false,
            debug: false,
        };

        let code = run(&opts);
        assert_eq!(code, 1, "mu installer without --yes on non-TTY must exit 1");
        assert!(dummy.exists(), "must not delete anything");

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn installer_real_run_dry_run_exits_zero() {
        let root = tempdir("real-dryrun");
        let dummy = fixture(&root, "app.deb");
        let opts = Options {
            paths: vec![root.clone()],
            dry_run: true,
            auto_yes: false,
            debug: false,
        };

        let code = run(&opts);
        assert_eq!(code, 0, "mu installer --dry-run must exit 0");
        assert!(dummy.exists(), "must not delete anything");

        let _ = fs::remove_dir_all(&root);
    }
}
