//! Uninstall subsystem — port of `internal/uninstall/`: package discovery
//! (`discover`), remnant scanning (`remnants`), removal ordering
//! (`remove`), the selection model's headless half (`model`), and `Run`'s
//! dispatch (`mod`).
//!
//! `cmd/mu/cli/uninstall.go` has no headless path — Go's `Run` always
//! launches `tea.NewProgram` — so a non-TTY run reproduces tea's startup
//! error text and exit 1, while a TTY run drives the ratatui model in
//! [`crate::tui::run_uninstall_tui`] and feeds the selection to [`finish`]
//! exactly like Go's `phaseDone` tail.
//!
//! Go wires dependencies through package vars (`uninstallRunner`,
//! `utils.trashRunner`, the `trash*` hooks, env-based XDG lookups). Rust
//! makes them explicit: production goes through [`Deps::real`], and the
//! `*_in` functions take the pieces they need so tests inject tempdirs and
//! [`crate::runner::FakeRunner`] without mutating the process environment.

pub(crate) mod discover;
pub(crate) mod model;
pub(crate) mod remnants;
pub(crate) mod remove;

use std::io::{BufRead, IsTerminal, Write};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use crate::runner::{ProcessRunner, Runner};
use crate::trash::TrashDeps;
use crate::{config, oplog, xdg};

// Go's exported surface, flattened to `uninstall::X` the way the oracle
// package exposes it.
#[allow(unused_imports)]
pub use discover::{Package, discover, discover_apt, discover_snap};
#[allow(unused_imports)]
pub use remnants::{find_remnants, remnant_size};
#[allow(unused_imports)]
pub use remove::{RemovalResult, removal_errors, remove_selected};

pub(crate) use model::{Phase, UninstallModel};
pub(crate) use remove::remove_selected_in;

/// `uninstall.Options` — controls uninstall behavior.
#[derive(Clone, Debug, Default)]
pub struct Options {
    /// `DryRun` — preview actions without making changes.
    pub dry_run: bool,
    /// `Debug` — verbose logging (warn when the ops log cannot open).
    pub debug: bool,
    /// `List` — list installed packages and exit.
    pub list: bool,
    /// `Permanent` — bypass trash (rejected for safety).
    pub permanent: bool,
    /// `Packages` — direct CLI package names to remove.
    pub packages: Vec<String>,
    /// `Yes` — skip the confirmation gate for direct CLI removal.
    pub auto_yes: bool,
}

/// Dependency bundle replacing the Go package vars. `runner` is
/// `uninstallRunner`; `trash_runner`/`trash_deps` are `utils.trashRunner` and
/// the `trash*` hooks used inside `utils.SafeDelete`; the `*_home` fields are
/// the XDG/`os.UserHomeDir` lookups `FindRemnants`, `remnantRoot`,
/// `LoadWhitelist`, and `InitLogger` read from the environment.
pub(crate) struct Deps {
    /// `uninstallRunner` — dpkg-query, snap, du, and sudo removal commands.
    pub runner: Arc<dyn Runner>,
    /// `utils.trashRunner` — only for the `gio trash` lookup/run inside
    /// `SafeDelete`; deliberately NOT `runner` (Go keeps them separate).
    pub trash_runner: Arc<dyn Runner>,
    /// `utils.trash*` filesystem hooks.
    pub trash_deps: Rc<TrashDeps>,
    /// `utils.XDGConfigHome()` — `mu/config.toml` and a remnant root.
    pub config_home: PathBuf,
    /// `utils.XDGDataHome()` — `mu/operations.log` and a remnant root.
    pub data_home: PathBuf,
    /// `utils.XDGCacheHome()` — a remnant root.
    pub cache_home: PathBuf,
    /// `os.UserHomeDir()` — `FindRemnants` bails when it cannot be resolved.
    pub home: PathBuf,
    /// `MU_NO_OPLOG=1` — disables `InitLogger`.
    pub no_oplog: bool,
}

impl Deps {
    /// Production wiring — every Go package var in its default state.
    fn real() -> Self {
        Self {
            runner: Arc::new(ProcessRunner),
            trash_runner: Arc::new(ProcessRunner),
            trash_deps: Rc::new(TrashDeps::real()),
            config_home: xdg::config_home(),
            data_home: xdg::data_home(),
            cache_home: xdg::cache_home(),
            home: xdg::home_dir(),
            no_oplog: std::env::var_os("MU_NO_OPLOG").is_some_and(|v| v == "1"),
        }
    }
}

/// `Run` — launches the interactive uninstall TUI. Returns the process exit
/// code: 1 on a fail-closed config error, headless run, TUI failure, or
/// removal errors (cobra prints the returned error on stderr and exits 1);
/// 0 on success or a quit/cancel.
pub fn run(opts: &Options) -> i32 {
    let deps = Deps::real();
    let stderr = std::io::stderr();
    let mut err_out = stderr.lock();
    run_in(opts, &deps, &mut err_out)
}

/// `LoadWhitelist` fail-closed preflight — Go validates config BEFORE
/// opening the TUI, so a malformed `config.toml` aborts instantly. The
/// menu dispatch calls this before launching the selector for the same
/// ordering (run_in/run_with_items repeat it inside their own preamble).
pub fn preflight() -> Result<(), String> {
    let deps = Deps::real();
    config::load_whitelist_from(&deps.config_home)
        .map(|_| ())
        .map_err(|e| format!("invalid mu configuration: {e}"))
}

/// Run the removal after the interactive TUI — wired by
/// `tui::dispatch_subcommand`. `items` is the TUI's full discovered
/// `all_items` with selected flags (Go: `finish()` consumes the whole
/// `phaseDone` model, passing all discovered packages as `installed`).
pub fn run_with_items(opts: &Options, items: &[crate::uninstall::model::PkgItem]) -> i32 {
    let deps = Deps::real();
    let stderr = std::io::stderr();
    let mut err_out = stderr.lock();
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    // Validate config (fail-closed) like run_in.
    if let Err(e) = config::load_whitelist_from(&deps.config_home) {
        let _ = writeln!(err_out, "invalid mu configuration: {e}");
        return 1;
    }
    if !deps.no_oplog
        && let Err(e) = oplog::init_logger_at(&deps.data_home)
        && opts.debug
    {
        let _ = writeln!(err_out, "warn: could not open log: {e}");
    }
    let _log = CloseLoggerGuard;
    finish_with_items(opts, items, &deps, &mut out, &mut err_out)
}

/// Shared post-selection tail — builds the `phaseDone` model the way the
/// interactive TUI leaves it and hands it to [`finish`]. `items` is the TUI's
/// FULL discovered `all_items` (selected flags on chosen entries) — Go passes
/// every discovered package as `installed` to `RemoveSelected` so shared
/// remnants owned by non-selected packages are retained, never deleted.
/// `selected_packages` reads `m.selected` (the `pkg.Key()` map), NOT
/// `PkgItem.selected`, so both representations are populated — the TUI
/// updates both on Space.
fn finish_with_items(
    opts: &Options,
    items: &[crate::uninstall::model::PkgItem],
    deps: &Deps,
    out: &mut dyn Write,
    err_out: &mut dyn Write,
) -> i32 {
    let mut model = crate::uninstall::model::new_model(opts.clone());
    model.phase = Phase::Done;
    for it in items.iter().filter(|it| it.selected) {
        model.selected.insert(it.pkg.key(), true);
    }
    model.all_items = items.to_vec();
    finish(opts, &model, deps, out, err_out)
}

/// Injectable `Run` for tests — `deps` supplies the XDG roots and runners the
/// Go version reads from env/package vars; `err_out` is `os.Stderr`.
pub(crate) fn run_in(opts: &Options, deps: &Deps, err_out: &mut dyn Write) -> i32 {
    // `LoadWhitelist` fails closed — `invalid mu configuration: %w` is the
    // error text cobra's Execute prints via `fmt.Fprintln(os.Stderr, err)`.
    if let Err(e) = config::load_whitelist_from(&deps.config_home) {
        let _ = writeln!(err_out, "invalid mu configuration: {e}");
        return 1;
    }
    // `InitLogger` failure only warns in debug; `defer utils.CloseLogger()`
    // is the RAII guard below, held to scope end.
    if !deps.no_oplog
        && let Err(e) = oplog::init_logger_at(&deps.data_home)
        && opts.debug
    {
        let _ = writeln!(err_out, "warn: could not open log: {e}");
    }
    let _log = CloseLoggerGuard;

    // Reject --permanent with exit code 1 and explanatory safety error
    if opts.permanent {
        let _ = writeln!(
            err_out,
            "error: --permanent is rejected: mu enforces SafeDelete via trash only to guarantee recoverability. To free space after uninstalling, review and empty trash via 'gio trash --empty' or 'mu clean --include=system-trash'."
        );
        return 1;
    }

    // Handle --list
    if opts.list {
        let stdout = std::io::stdout();
        let mut out = stdout.lock();
        let (pkgs, err) = discover(&*deps.runner);
        if let Some(e) = err
            && opts.debug
        {
            let _ = writeln!(err_out, "warn: discovery errors: {e}");
        }
        for pkg in pkgs {
            let sz = crate::size::human_kb(pkg.installed_kb as u64);
            let _ = writeln!(out, "{}:{} ({})", pkg.source, pkg.name, sz);
        }
        return 0;
    }

    // Handle direct CLI package uninstall
    if !opts.packages.is_empty() {
        let stdout = std::io::stdout();
        let mut out = stdout.lock();
        return run_cli_uninstall(opts, deps, &mut out, err_out);
    }

    // Go: `tea.NewProgram(m, tea.WithAltScreen()).Run()` is the only path.
    // A headless run fails inside tea's program init with
    // `could not open a new TTY: open /dev/tty: no such device or address`,
    // which cobra prints bare on stderr before exiting 1.
    if !std::io::stdout().is_terminal() {
        let _ = writeln!(
            err_out,
            "could not open a new TTY: open /dev/tty: no such device or address"
        );
        return 1;
    }
    // TTY: the interactive TUI returns its full item list with selected
    // flags (None when they quit/abort — Go's `final.phase != phaseDone` →
    // nil → exit 0).
    let items = match crate::tui::run_uninstall_tui() {
        Ok(items) => items,
        Err(e) => {
            let _ = writeln!(err_out, "{e}");
            return 1;
        }
    };
    let Some(items) = items else {
        return 0;
    };
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    finish_with_items(opts, &items, deps, &mut out, err_out)
}

/// The post-TUI half of `Run`, reachable only after the interactive model
/// exits with `phaseDone`. Wired in when the TUI lands; ported now because
/// `uninstall.go`'s removal dispatch — `selectedPackages` → `RemoveSelected`
/// → `RemovalErrors` → the `error: %v` stderr line and the
/// `Nothing to remove.`/`Uninstall complete.` prints — is non-view logic.
///
/// `out`/`err_out` stand in for `os.Stdout`/`os.Stderr`. The returned code
/// mirrors cobra: `Run`'s error becomes `Fprintln(os.Stderr, err)` + exit 1,
/// which is why the aggregate prints twice (`error: %v` inside `Run`, then
/// the bare error again from `Execute`).
pub(crate) fn finish(
    opts: &Options,
    m: &UninstallModel,
    deps: &Deps,
    out: &mut dyn Write,
    err_out: &mut dyn Write,
) -> i32 {
    if m.phase != Phase::Done {
        return 0; // user quit or aborted — Go returns nil
    }
    let pkgs = m.selected_packages();
    if pkgs.is_empty() {
        let _ = writeln!(out, "Nothing to remove.");
        return 0;
    }
    let installed: Vec<Package> = m.all_items.iter().map(|it| it.pkg.clone()).collect();
    let results = remove_selected_in(deps, &pkgs, &installed, opts.dry_run, out, err_out);
    if let Some(e) = removal_errors(&results) {
        let _ = writeln!(err_out, "error: {e}");
        let _ = writeln!(err_out, "{e}");
        return 1;
    }
    let _ = writeln!(out, "\nUninstall complete.");
    0
}
fn run_cli_uninstall(
    opts: &Options,
    deps: &Deps,
    out: &mut dyn Write,
    err_out: &mut dyn Write,
) -> i32 {
    let (all_installed, err) = discover(&*deps.runner);
    if let Some(e) = err
        && opts.debug
    {
        let _ = writeln!(err_out, "warn: discovery error: {e}");
    }
    let mut selected = Vec::new();
    for target in &opts.packages {
        // A source-qualified key (`apt:foo`, `snap:foo`) is unambiguous.
        if let Some(pkg) = all_installed.iter().find(|p| p.key() == *target) {
            selected.push(with_remnants(deps, pkg));
            continue;
        }
        let by_name: Vec<&Package> = all_installed.iter().filter(|p| p.name == *target).collect();
        match by_name.as_slice() {
            [] => {
                let _ = writeln!(err_out, "error: package '{target}' not found");
                return 1;
            }
            [pkg] => selected.push(with_remnants(deps, pkg)),
            many => {
                // Fail closed instead of silently purging whichever source
                // sorts first.
                let keys: Vec<String> = many.iter().map(|p| p.key()).collect();
                let _ = writeln!(
                    err_out,
                    "error: '{target}' is ambiguous; matches {} — specify a source prefix (e.g. {})",
                    keys.join(", "),
                    keys[0]
                );
                return 1;
            }
        }
    }

    // Always print the removal plan so the action is previewable before any
    // package is touched.
    for pkg in &selected {
        let _ = writeln!(
            out,
            "Package to remove: {}:{} ({})",
            pkg.source,
            pkg.name,
            crate::size::human_kb(pkg.installed_kb as u64)
        );
        if !pkg.remnants_found.is_empty() {
            let _ = writeln!(out, "Remnants to remove:");
            for r in &pkg.remnants_found {
                let _ = writeln!(out, "  - {r}");
            }
        }
    }

    if opts.dry_run {
        let _ = writeln!(out, "Dry run: previewing package removal plan.");
        let results = remove_selected_in(deps, &selected, &all_installed, true, out, err_out);
        if let Some(e) = removal_errors(&results) {
            let _ = writeln!(err_out, "error: {e}");
            return 1;
        }
        return 0;
    }

    // Fail-closed confirmation gate (finding: unprompted `apt-get purge -y`).
    if !opts.auto_yes {
        if std::io::stdin().is_terminal() {
            let _ = write!(
                err_out,
                "Proceed with removal of {} package(s)? [y/N] ",
                selected.len()
            );
            let _ = err_out.flush();
            let mut line = String::new();
            let confirmed = std::io::stdin().lock().read_line(&mut line).is_ok_and(|_| {
                let t = line.trim().to_ascii_lowercase();
                t == "y" || t == "yes"
            });
            if !confirmed {
                let _ = writeln!(out, "Aborted.");
                return 0;
            }
        } else {
            let _ = writeln!(
                out,
                "Non-interactive: pass --yes to confirm, or --dry-run to preview."
            );
            let _ = writeln!(out, "Aborted.");
            return 0;
        }
    }

    let results = remove_selected_in(deps, &selected, &all_installed, false, out, err_out);
    if let Some(e) = removal_errors(&results) {
        let _ = writeln!(err_out, "error: {e}");
        return 1;
    }
    let _ = writeln!(out, "\nUninstall complete.");
    0
}

/// Clone a discovered package and attach its remnant scan (shared by every
/// direct-CLI resolution branch).
fn with_remnants(deps: &Deps, pkg: &Package) -> Package {
    let mut pkg = pkg.clone();
    pkg.remnants_found = remnants::find_remnants_in(deps, &pkg.name);
    pkg.remnants_kb = remnants::remnant_size(&pkg.remnants_found) / 1024;
    pkg
}

/// `defer utils.CloseLogger()` — closes on drop at every return path.
struct CloseLoggerGuard;

impl Drop for CloseLoggerGuard {
    fn drop(&mut self) {
        oplog::close_logger();
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    //! Shared scaffolding for the ported uninstall tests — the Go suite's
    //! `t.TempDir()` + `t.Setenv(HOME/XDG_*)` setup, expressed as injected
    //! [`Deps`] rooted at a tempdir.
    use super::*;
    use crate::runner::FakeRunner;
    use std::fs;
    use std::path::Path;

    pub fn tempdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "mu-test-uninstall-{}-{}-{}",
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

    /// `Deps` rooted at `root`: `.config`/`.data`/`.cache` XDG dirs (the
    /// values the Go tests assign to `XDG_*_HOME`), `home` = `root`, a bare
    /// [`FakeRunner`] `trash_runner` (`gio` is never found, forcing the
    /// FreeDesktop fallback the Go tests exercise through `trashRunner`),
    /// and a trash `data_home` that mkdirs 0700 on demand like
    /// `trashHomeDir`. `no_oplog` keeps test writes out of the real log.
    pub fn deps_for_test(root: &Path, runner: Arc<dyn Runner>) -> Deps {
        let data_home = root.join(".data");
        Deps {
            runner,
            trash_runner: Arc::new(FakeRunner::new()),
            trash_deps: Rc::new(TrashDeps {
                data_home: Box::new(move || {
                    if !data_home.exists() {
                        std::fs::create_dir_all(&data_home)?;
                        #[cfg(unix)]
                        {
                            use std::os::unix::fs::PermissionsExt;
                            std::fs::set_permissions(
                                &data_home,
                                std::fs::Permissions::from_mode(0o700),
                            )?;
                        }
                    }
                    Ok(data_home.clone())
                }),
                ..TrashDeps::real()
            }),
            config_home: root.join(".config"),
            data_home: root.join(".data"),
            cache_home: root.join(".cache"),
            home: root.to_path_buf(),
            no_oplog: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;
    use crate::runner::FakeRunner;
    use std::fs;
    use std::sync::Arc;

    // Regression guard for the selection bridge: `finish_with_items`
    // must populate `UninstallModel.selected` (the map `selected_packages`
    // reads) — setting only `PkgItem.selected` prints "Nothing to remove."
    // and never invokes removal.
    #[test]
    fn finish_with_items_reaches_removal() {
        let tmp = tempdir("bridge");
        let runner = Arc::new(FakeRunner::new());
        let deps = deps_for_test(&tmp, runner.clone());
        let items = vec![model::PkgItem {
            pkg: Package {
                name: "solo".to_string(),
                source: "apt".to_string(),
                version: "1.0".to_string(),
                ..Default::default()
            },
            selected: true,
        }];
        let opts = Options {
            dry_run: false,
            debug: false,
            ..Default::default()
        };
        let mut out = Vec::new();
        let mut err_out = Vec::new();
        let code = finish_with_items(&opts, &items, &deps, &mut out, &mut err_out);
        let text = String::from_utf8_lossy(&out);
        assert_eq!(code, 0, "stderr={}", String::from_utf8_lossy(&err_out));
        assert!(
            text.contains("Uninstall complete."),
            "expected removal to run, got {text:?}"
        );
        let calls: Vec<Vec<String>> = runner
            .invocations()
            .iter()
            .map(|spec| {
                std::iter::once(spec.program.to_string_lossy().into_owned())
                    .chain(spec.args.iter().map(|a| a.to_string_lossy().into_owned()))
                    .collect()
            })
            .collect();
        assert!(
            calls
                .iter()
                .any(|c| c == &["sudo", "apt-get", "purge", "-y", "solo"].map(String::from)),
            "expected purge invocation, got {calls:?}"
        );
        fs::remove_dir_all(&tmp).ok();
    }

    // Regression guard for shared-remnant protection: `installed` passed to
    // `remove_selected_in` must be the FULL discovered item list, not just
    // the selection — a remnant owned by a non-selected package (e.g.
    // ~/.mozilla shared by apt:firefox and snap:firefox) must be retained.
    #[test]
    fn finish_with_items_preserves_shared_remnant() {
        let tmp = tempdir("shared-remnant");
        let runner = Arc::new(FakeRunner::new());
        let deps = deps_for_test(&tmp, runner.clone());
        // Shared remnant under a managed root (.config) so the ownership
        // check is what retains it — not an out-of-root validation error.
        let shared = tmp.join(".config").join("shared");
        fs::create_dir_all(&shared).unwrap();
        let shared_s = shared.to_string_lossy().into_owned();
        let keeper = Package {
            name: "keeper".to_string(),
            source: "snap".to_string(),
            version: "1.0".to_string(),
            remnants_found: vec![shared_s.clone()],
            ..Default::default()
        };
        let victim = Package {
            name: "victim".to_string(),
            source: "apt".to_string(),
            version: "1.0".to_string(),
            remnants_found: vec![shared_s.clone()],
            ..Default::default()
        };
        let items = vec![
            model::PkgItem {
                pkg: keeper,
                selected: false,
            },
            model::PkgItem {
                pkg: victim,
                selected: true,
            },
        ];
        let opts = Options {
            dry_run: false,
            debug: false,
            ..Default::default()
        };
        let mut out = Vec::new();
        let mut err_out = Vec::new();
        let code = finish_with_items(&opts, &items, &deps, &mut out, &mut err_out);
        assert_eq!(code, 0, "stderr={}", String::from_utf8_lossy(&err_out));
        // The keeper still "installed" → shared remnant retained.
        assert!(
            shared.exists(),
            "shared remnant was deleted though keeper retains it"
        );
        fs::remove_dir_all(&tmp).ok();
    }

    // remove_test.go: TestUninstallFailsClosedOnMalformedConfig — Go's
    // `Run` returns the wrapped LoadWhitelist error; the Rust port surfaces
    // it as exit 1 with the same stderr text cobra would print.
    #[test]
    fn run_fails_closed_on_malformed_config() {
        let root = tempdir("badcfg");
        let config_dir = root.join(".config").join("mu");
        fs::create_dir_all(&config_dir).unwrap();
        fs::write(config_dir.join("config.toml"), "broken = [").unwrap();
        let deps = deps_for_test(&root, Arc::new(FakeRunner::new()));
        let mut err_out = Vec::new();
        let code = run_in(
            &Options {
                dry_run: true,
                debug: false,
                ..Default::default()
            },
            &deps,
            &mut err_out,
        );
        let text = String::from_utf8_lossy(&err_out);
        assert_eq!(code, 1, "expected config failure exit, stderr={text:?}");
        assert!(
            text.contains("invalid mu configuration"),
            "expected config error, got {text:?}"
        );
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn run_rejects_permanent_flag_with_code_1() {
        let root = tempdir("perm");
        let deps = deps_for_test(&root, Arc::new(FakeRunner::new()));
        let mut err_out = Vec::new();
        let code = run_in(
            &Options {
                permanent: true,
                ..Default::default()
            },
            &deps,
            &mut err_out,
        );
        assert_eq!(code, 1);
        let text = String::from_utf8_lossy(&err_out);
        assert!(
            text.contains("error: --permanent is rejected: mu enforces SafeDelete via trash only")
        );
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn run_direct_cli_uninstall_dry_run() {
        let root = tempdir("direct-dry");
        let runner = Arc::new(FakeRunner::new());
        runner.set_look_path("dpkg-query", "/usr/bin/dpkg-query");
        runner.set_handler(|spec| {
            if spec.program == "dpkg-query" {
                return Ok(crate::runner::Output {
                    stdout: b"install ok installed\t1024\ttest-pkg\t1.0.0\n".to_vec(),
                    ..Default::default()
                });
            }
            Ok(crate::runner::Output::default())
        });
        let deps = deps_for_test(&root, runner);
        let mut err_out = Vec::new();
        let code = run_in(
            &Options {
                dry_run: true,
                packages: vec!["test-pkg".to_string()],
                ..Default::default()
            },
            &deps,
            &mut err_out,
        );
        assert_eq!(code, 0, "stderr: {}", String::from_utf8_lossy(&err_out));
        fs::remove_dir_all(&root).ok();
    }

    fn runner_with_pkg(pkgs: &'static str) -> Arc<FakeRunner> {
        let runner = Arc::new(FakeRunner::new());
        runner.set_look_path("dpkg-query", "/usr/bin/dpkg-query");
        runner.set_handler(move |spec| {
            if spec.program == "dpkg-query" {
                return Ok(crate::runner::Output {
                    stdout: pkgs.as_bytes().to_vec(),
                    ..Default::default()
                });
            }
            Ok(crate::runner::Output::default())
        });
        runner
    }

    fn purge_invoked(runner: &FakeRunner, name: &str) -> bool {
        runner.invocations().iter().any(|s| {
            s.program == "sudo"
                && s.args == ["apt-get", "purge", "-y", name].map(std::ffi::OsString::from)
        })
    }

    // Finding 1: direct CLI removal must not purge without `--yes`
    // (fail-closed), mirroring clean's non-interactive gate.
    #[test]
    fn run_cli_uninstall_requires_yes_gate() {
        let root = tempdir("gate");
        let runner = runner_with_pkg("install ok installed\t1024\ttest-pkg\t1.0.0\n");
        let deps = deps_for_test(&root, runner.clone());
        let mut out = Vec::new();
        let mut err_out = Vec::new();
        let code = run_cli_uninstall(
            &Options {
                packages: vec!["test-pkg".to_string()],
                auto_yes: false,
                ..Default::default()
            },
            &deps,
            &mut out,
            &mut err_out,
        );
        assert_eq!(code, 0, "stderr: {}", String::from_utf8_lossy(&err_out));
        assert!(
            String::from_utf8_lossy(&out).contains("Aborted."),
            "expected abort note, got {:?}",
            String::from_utf8_lossy(&out)
        );
        assert!(
            !purge_invoked(&runner, "test-pkg"),
            "purge ran without --yes: {:?}",
            runner.invocations()
        );
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn run_cli_uninstall_with_yes_executes() {
        let root = tempdir("yes");
        let runner = runner_with_pkg("install ok installed\t1024\ttest-pkg\t1.0.0\n");
        let deps = deps_for_test(&root, runner.clone());
        let mut out = Vec::new();
        let mut err_out = Vec::new();
        let code = run_cli_uninstall(
            &Options {
                packages: vec!["test-pkg".to_string()],
                auto_yes: true,
                ..Default::default()
            },
            &deps,
            &mut out,
            &mut err_out,
        );
        assert_eq!(code, 0, "stderr: {}", String::from_utf8_lossy(&err_out));
        assert!(purge_invoked(&runner, "test-pkg"), "expected purge call");
        fs::remove_dir_all(&root).ok();
    }

    // Finding 7: a bare name matching several sources must fail closed.
    #[test]
    fn run_cli_uninstall_rejects_ambiguous_name() {
        let root = tempdir("ambiguous");
        let runner = Arc::new(FakeRunner::new());
        runner.set_look_path("dpkg-query", "/usr/bin/dpkg-query");
        runner.set_look_path("snap", "/usr/bin/snap");
        runner.set_handler(|spec| match spec.program.to_string_lossy().as_ref() {
            "dpkg-query" => Ok(crate::runner::Output {
                stdout: b"install ok installed\t1024\tfirefox\t1.0\n".to_vec(),
                ..Default::default()
            }),
            "snap" => Ok(crate::runner::Output {
                stdout:
                    b"Name Version Rev Tracking Publisher Notes\nfirefox 124 1 stable mozilla -\n"
                        .to_vec(),
                ..Default::default()
            }),
            _ => Ok(crate::runner::Output::default()),
        });
        let deps = deps_for_test(&root, runner.clone());
        let mut out = Vec::new();
        let mut err_out = Vec::new();
        let code = run_cli_uninstall(
            &Options {
                dry_run: true,
                packages: vec!["firefox".to_string()],
                ..Default::default()
            },
            &deps,
            &mut out,
            &mut err_out,
        );
        assert_eq!(code, 1);
        let text = String::from_utf8_lossy(&err_out);
        assert!(
            text.contains("ambiguous"),
            "expected ambiguity error: {text}"
        );
        // A source prefix still resolves.
        let mut out = Vec::new();
        let mut err_out = Vec::new();
        let code = run_cli_uninstall(
            &Options {
                dry_run: true,
                packages: vec!["snap:firefox".to_string()],
                ..Default::default()
            },
            &deps,
            &mut out,
            &mut err_out,
        );
        assert_eq!(code, 0, "stderr: {}", String::from_utf8_lossy(&err_out));
        fs::remove_dir_all(&root).ok();
    }
}
