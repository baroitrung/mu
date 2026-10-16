//! Clean targets, resolution, and the scan→execute pipeline — port of
//! `internal/clean/{targets,clean}.go`. The Bubble Tea presentation of
//! `flow.go` (views, animated spinner, `tea.Exec`) is tui-port work; what
//! lives here is `Run`'s dispatch — the non-interactive `runPlain` path —
//! plus the complete target/scan layer. `flow` carries the ported
//! state machine; `crate::runtext` is the `ui.Run` line renderer.
//!
//! Go wires dependencies through package vars (`cleanRunner`,
//! `utils.trashRunner`, the `trash*` hooks, env-based XDG lookups). Rust
//! makes them explicit: production goes through [`Deps::real`], and every
//! `*_in` constructor takes the pieces it needs so tests inject tempdirs and
//! [`crate::runner::FakeRunner`] without mutating the process environment
//! (the `t.Setenv` calls in the Go suite become constructor arguments here).

pub(crate) mod flow;
pub(crate) mod scan_browser;
pub(crate) mod scan_docker;
pub(crate) mod scan_kernels;
pub(crate) mod scan_snap;
pub(crate) mod targets;

use std::io::Write;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use crate::error::{Error, Result, msg};
use crate::runner::{ProcessRunner, Runner};
use crate::runtext;
use crate::trash::TrashDeps;
use crate::{config, oplog, paths, size, xdg};

// Go's exported surface, flattened to `clean::X` the way the oracle package
// exposes it. `run_autoremove` is consumed by `crate::optimize`; the rest is
// wave-2/optimize-facing API kept public for the contract.
#[allow(unused_imports)]
pub use scan_kernels::{parse_autoremove_simulation, run_autoremove};
#[allow(unused_imports)]
pub use targets::journal_size;

/// CleanTarget describes one cleaning category — the port of Go's struct of
/// closures (Scan/Preview/Execute are `Box<dyn Fn>` fields).
pub struct CleanTarget {
    pub id: &'static str,
    pub label: &'static str,
    pub requires_sudo: bool,
    /// Go `OptIn` — only included when the ID appears in `--include`.
    pub opt_in: bool,
    pub scan: ScanFn,
    /// Go `Preview func() ([]string, error)` is nil-able — `None` here. The
    /// payload is a pair because Go targets can return a partial preview
    /// together with the error (kernels does; `flow`'s `scanCmd` keeps both
    /// while `runPlain` drops errored previews).
    pub preview: Option<PreviewFn>,
    pub execute: Box<dyn Fn(bool) -> Result<()>>,
    /// Optional dynamic label (e.g. `System Trash (14 items)`) computed at
    /// render time; `None` falls back to the static [`label`].
    pub count_fn: Option<Box<dyn Fn() -> String>>,
}

impl CleanTarget {
    /// The label shown in plain/TUI previews — [`count_fn`] when present
    /// (fresh item count), otherwise the static [`label`].
    pub fn display_label(&self) -> String {
        match &self.count_fn {
            Some(f) => f(),
            None => self.label.to_string(),
        }
    }
}

/// Preview closure payload — Go's `([]string, error)` pair.
pub type PreviewFn = Box<dyn Fn() -> (Vec<String>, Option<Error>)>;

/// Scan closure payload — Go's `(int64, error)` pair. The size is the
/// partial total accumulated before any error, mirroring Go's contract that
/// `sz` is recorded unconditionally by callers.
pub type ScanFn = Box<dyn Fn() -> (i64, Option<Error>)>;

/// One scanned target — Go's `scanResult`.
pub struct ScanResult {
    pub target: CleanTarget,
    pub size: i64,
    pub items: Vec<String>,
}

/// Options controls clean behavior — Go's `clean.Options`.
#[derive(Debug, Default, Clone)]
pub struct Options {
    pub dry_run: bool,
    pub debug: bool,
    /// Opt-in category IDs (e.g. `["browser-cache"]`).
    pub include: Vec<String>,
    /// Skip the confirmation prompt.
    pub auto_yes: bool,
    /// `--external <PATH>` — clean metadata on an external volume mount.
    pub external: Option<PathBuf>,
}

/// Dependency bundle replacing the Go package vars. `runner` is
/// `cleanRunner`; `trash_runner`/`trash_deps` are `utils.trashRunner` and the
/// `trashHomeDir`/`trashDeviceID`/`trashMountFor`/`renamePath` hooks; the
/// `*_home` fields are the XDG/`os.UserHomeDir` lookups — resolved once at
/// target construction, exactly where Go calls them.
pub(crate) struct Deps {
    /// `cleanRunner` — clean-package commands (apt, journalctl, snap, docker).
    pub runner: Arc<dyn Runner>,
    /// `utils.XDGCacheHome()`.
    pub cache_home: PathBuf,
    /// `utils.XDGConfigHome()` — where `LoadWhitelist` reads `mu/config.toml`.
    pub config_home: PathBuf,
    /// `os.UserHomeDir()`.
    pub home: PathBuf,
    /// `xdgDataHome()` — where `InitLogger` creates `mu/operations.log`.
    pub data_home: PathBuf,
    /// `utils.trashRunner` — used only for the `gio trash` lookup/run inside
    /// `SafeDelete`; deliberately NOT `runner` (Go keeps them separate).
    pub trash_runner: Arc<dyn Runner>,
    /// `utils.trash*` filesystem hooks.
    pub trash_deps: Rc<TrashDeps>,
}

impl Deps {
    /// Production wiring — every Go package var in its default state.
    /// `pub(crate)` so `crate::audit::Deps` can embed the same bundle (Go's
    /// audit package calls into `internal/clean` directly).
    pub(crate) fn real(runner: Arc<dyn Runner>) -> Self {
        Self {
            runner,
            cache_home: xdg::cache_home(),
            config_home: xdg::config_home(),
            home: xdg::home_dir(),
            data_home: xdg::data_home(),
            trash_runner: Arc::new(ProcessRunner),
            trash_deps: Rc::new(TrashDeps::real()),
        }
    }
}

/// `AllTargets` — all built-in clean targets in display order. The Docker
/// target is included only when the Docker socket is present.
pub fn all_targets(runner: Arc<dyn Runner>) -> Vec<CleanTarget> {
    all_targets_in(&Deps::real(runner))
}

pub(crate) fn all_targets_in(deps: &Deps) -> Vec<CleanTarget> {
    let mut targets = vec![
        targets::user_cache_target_in(deps),
        targets::thumbnails_target_in(deps),
        targets::font_cache_target_in(deps),
        targets::sandbox_font_cache_target_in(deps),
        targets::apt_cache_target_in(deps),
        targets::journal_logs_target_in(deps),
        scan_snap::snap_target_in(deps),
        scan_kernels::kernels_target_in(deps),
        scan_browser::browser_cache_target_in(deps),
        targets::system_trash_target_in(deps),
    ];
    if let Some(docker) = scan_docker::docker_target_in(deps) {
        targets.push(docker);
    }
    targets
}

/// `ResolveTargets` — validates include IDs and returns the enabled target
/// set. Opt-in targets are filtered out unless their ID is included.
pub fn resolve_targets(runner: Arc<dyn Runner>, include: &[String]) -> Result<Vec<CleanTarget>> {
    resolve_targets_in(&Deps::real(runner), include)
}

pub(crate) fn resolve_targets_in(deps: &Deps, include: &[String]) -> Result<Vec<CleanTarget>> {
    let all = all_targets_in(deps);
    let mut include_set = std::collections::HashSet::with_capacity(include.len());
    for id in include {
        match all.iter().find(|t| t.id == id) {
            None => return msg(format!("unknown clean include ID: {id}")),
            Some(t) if !t.opt_in => {
                return msg(format!("clean include ID is not opt-in: {id}"));
            }
            _ => {
                include_set.insert(id.as_str());
            }
        }
    }
    Ok(all
        .into_iter()
        .filter(|t| !t.opt_in || include_set.contains(t.id))
        .collect())
}

/// `TargetByID` — the clean target with the given ID, if present.
pub fn target_by_id(runner: Arc<dyn Runner>, id: &str) -> Option<CleanTarget> {
    all_targets(runner).into_iter().find(|t| t.id == id)
}

/// `errors.Join` equivalent — messages joined by newlines.
fn join_errors(errors: &[Error]) -> Error {
    Error::Msg(
        errors
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n"),
    )
}

/// `Run` — the clean workflow: scan → display → confirm → execute. Returns
/// the one-line summary (freed space, reclaimable on dry runs, or
/// "Aborted." when the user declines); the cobra caller drops it.
///
/// On a real terminal Go forks to `runFlow` — a single interactive program
/// (scan progress → size table → YES/NO confirm → run rows → done),
/// driven here by [`crate::tui::clean::run_clean_flow`]. On pipes/CI it
/// falls back to `runPlain`.
pub fn run(opts: &Options) -> Result<String> {
    let deps = Deps::real(Arc::new(ProcessRunner));
    let targets = prepare(opts, &deps)?;
    if flow::interactive() {
        // Go: runFlow(opts, targets).
        return run_tui_in(opts, targets, &deps);
    }
    let mut out = std::io::stdout().lock();
    run_plain(opts, targets, &deps, &mut out)
}

/// TUI entry for clean — port of Go's `runFlow`: the interactive program
/// (scan → summary → confirm → run → done). Wired by
/// `tui::dispatch_subcommand`.
pub fn run_tui(opts: &Options) -> Result<String> {
    let deps = Deps::real(Arc::new(ProcessRunner));
    let targets = prepare(opts, &deps)?;
    run_tui_in(opts, targets, &deps)
}

fn run_tui_in(opts: &Options, targets: Vec<CleanTarget>, deps: &Deps) -> Result<String> {
    // Go's runFlow: a single Bubble Tea program that scans with progress,
    // shows the size table, THEN asks "Proceed to clean?" (default NO) —
    // the user must see what will be deleted before confirming.
    crate::tui::clean::run_clean_flow(opts, targets, deps)
}

/// The pre-fork half of `Run`: `LoadWhitelist` (fail closed on malformed
/// config), `ValidateCleanupRoot(XDGCacheHome())`, `ResolveTargets`, then the
/// `--external <PATH>` target when a mount path was given.
fn prepare(opts: &Options, deps: &Deps) -> Result<Vec<CleanTarget>> {
    if let Err(e) = config::load_whitelist_from(&deps.config_home) {
        return Err(Error::Msg(format!("invalid mu configuration: {e}")));
    }
    paths::validate_cleanup_root(&deps.cache_home)?;
    let mut targets = resolve_targets_in(deps, &opts.include)?;
    if let Some(mount) = &opts.external {
        // Fail closed before the target is even built: the mount must be a
        // safe cleanup root (absolute, not home/an ancestor, not protected).
        paths::validate_cleanup_root(mount)?;
        targets.push(targets::external_volume_target_in(deps, mount));
    }
    Ok(targets)
}

/// Injectable `runPlain` for tests: `deps` supplies the XDG roots and
/// runners the Go version reads from env/package vars; `out` is `os.Stdout`.
/// The `interactive()` fork lives in [`run`] — it tests process fds, not
/// the injected writer.
pub(crate) fn run_in(opts: &Options, deps: &Deps, out: &mut dyn Write) -> Result<String> {
    let targets = prepare(opts, deps)?;
    run_plain(opts, targets, deps, out)
}

/// `runPlain` — the non-interactive fallback: line-by-line output, no
/// animation, no prompts (pass `--yes` to run without a terminal).
fn run_plain(
    opts: &Options,
    mut targets: Vec<CleanTarget>,
    deps: &Deps,
    out: &mut dyn Write,
) -> Result<String> {
    let mut r = runtext::Run::new(out);

    let mut results: Vec<ScanResult> = Vec::new();
    let mut total: i64 = 0;
    let mut scan_errors: Vec<Error> = Vec::new();
    // `r.Spinner("Scanning system", ...)` — on a pipe this prints a plain
    // label line and runs the work inline. Go drops the returned error
    // (`_ =`) because the closure never fails.
    let _ = r.spinner("Scanning system", || -> Result<()> {
        // `drain` keeps the closure's captures as borrows — the results are
        // rendered below, after the label line.
        for t in targets.drain(..) {
            // Go records `size: sz` unconditionally — the partial total on
            // error — but only adds it to `total` and runs Preview when the
            // scan succeeded.
            let (sz, scan_err) = (t.scan)();
            if let Some(e) = scan_err {
                scan_errors.push(Error::Msg(format!("scan {}: {e}", t.id)));
                results.push(ScanResult {
                    target: t,
                    size: sz,
                    items: Vec::new(),
                });
            } else {
                total += sz;
                let mut res = ScanResult {
                    target: t,
                    size: sz,
                    items: Vec::new(),
                };
                if let Some(preview) = &res.target.preview {
                    // Go: `res.items` is set only when previewErr is nil
                    // — an errored preview contributes a scan error, not
                    // rows (flow.go's scanCmd keeps the items instead).
                    let (items, preview_err) = preview();
                    match preview_err {
                        Some(e) => {
                            scan_errors.push(Error::Msg(format!("preview {}: {e}", res.target.id)))
                        }
                        None => res.items = items,
                    }
                }
                results.push(res);
            }
        }
        Ok(())
    });

    for res in &results {
        let label = res.target.display_label();
        r.line(format_args!("{:<40} {}", label, size::human_size(res.size)))?;
        for item in &res.items {
            r.line(format_args!("  - {item}"))?;
        }
    }
    r.line(format_args!("{}", "-".repeat(50)))?;
    r.line(format_args!(
        "Potential space to free: {}",
        size::human_size(total)
    ))?;
    export_preview_ledger(&results, total, &deps.config_home);
    if !scan_errors.is_empty() {
        return Err(join_errors(&scan_errors));
    }

    if opts.dry_run {
        r.faint(format_args!("This is a DRY RUN. No files will be deleted."))?;
        r.faint(format_args!(
            "Validating the exact cleanup actions without changing data."
        ))?;
        execute(&mut r, &results, opts, deps)?;
        let _ = print_downloads_large_files_hint(&deps.home, &mut r);
        let summary = format!("Dry run — {} reclaimable", size::human_size(total));
        r.summary(&summary)?;
        return Ok(summary);
    }

    if !opts.auto_yes {
        r.faint(format_args!(
            "Non-interactive: pass --yes to confirm, or --dry-run to preview."
        ))?;
        r.faint(format_args!("Aborted."))?;
        return Ok("Aborted.".to_string());
    }

    let freed = execute(&mut r, &results, opts, deps)?;
    let _ = print_downloads_large_files_hint(&deps.home, &mut r);
    r.summary(&format!("All done — freed {}", size::human_size(freed)))?;
    Ok(format!("Freed {}", size::human_size(freed)))
}

fn export_preview_ledger(results: &[ScanResult], total: i64, config_home: &std::path::Path) {
    let mu_dir = config_home.join("mu");
    let _ = std::fs::create_dir_all(&mu_dir);
    let ledger_path = mu_dir.join("clean-list.txt");
    let mut f = match std::fs::File::create(&ledger_path) {
        Ok(f) => f,
        Err(_) => return,
    };
    let _ = writeln!(f, "# mu clean preview ledger");
    let _ = writeln!(
        f,
        "# Total potential space to free: {}",
        size::human_size(total)
    );
    let _ = writeln!(f, "--------------------------------------------------");
    for res in results {
        let label = res.target.display_label();
        let _ = writeln!(f, "{:<40} {}", label, size::human_size(res.size));
        for item in &res.items {
            let _ = writeln!(f, "  - {item}");
        }
    }
}

fn print_downloads_large_files_hint(
    home: &std::path::Path,
    r: &mut runtext::Run<'_>,
) -> Result<()> {
    let downloads = home.join("Downloads");
    let Ok(entries) = std::fs::read_dir(&downloads) else {
        return Ok(());
    };
    let mut count = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = path.symlink_metadata() else {
            continue;
        };
        if meta.file_type().is_symlink() || !meta.is_file() {
            continue;
        }
        if meta.len() > 500 * 1024 * 1024 {
            count += 1;
        }
    }
    if count > 0 {
        r.line(format_args!(
            "\nHint: found {count} large file{} (>500MB) in ~/Downloads (run 'mu analyze' to inspect)",
            if count == 1 { "" } else { "s" }
        ))?;
    }
    Ok(())
}
/// `execute` — run each target's `Execute` in order, logging per-target
/// outcomes and warning on stderr for failures. Returns the summed size of
/// the successful targets; one or more failures yield
/// `"N clean target(s) failed"` after all targets ran.
fn execute(
    r: &mut runtext::Run<'_>,
    results: &[ScanResult],
    opts: &Options,
    deps: &Deps,
) -> Result<i64> {
    // `utils.InitLogger` honors MU_NO_OPLOG=1; a failure only warns in debug.
    let logger = if std::env::var_os("MU_NO_OPLOG").as_deref() == Some(std::ffi::OsStr::new("1")) {
        Ok(())
    } else {
        oplog::init_logger_at(&deps.data_home)
    };
    if let Err(e) = logger
        && opts.debug
    {
        eprintln!("warn: could not open log: {e}");
    }

    let mut freed: i64 = 0;
    let mut failed = 0;
    for res in results {
        let t = &res.target;
        if let Err(e) = r.spinner(&format!("Cleaning {}", t.label), || {
            (t.execute)(opts.dry_run)
        }) {
            oplog::log_outcome("clean", t.id, "failure");
            eprintln!("  warn: {e}");
            failed += 1;
            continue;
        }
        oplog::log_outcome(
            "clean",
            t.id,
            if opts.dry_run { "dry-run" } else { "success" },
        );
        freed += res.size;
    }
    // Go's `defer utils.CloseLogger()`.
    oplog::close_logger();
    if failed > 0 {
        return msg(format!("{failed} clean target(s) failed"));
    }
    Ok(freed)
}

/// `fmt.Sscanf` "%f%s" core: scan a leading float then one whitespace-free
/// token. Returns `(value, token, items_matched)` — `items_matched` counts
/// successfully scanned operands (0, 1, or 2) like Go's `Sscanf` count.
/// Whitespace before each operand is skipped, as in Go.
pub(crate) fn scan_f_s(input: &str) -> (f64, String, usize) {
    let rest = input.trim_start();
    let Some(flen) = float_prefix(rest) else {
        return (0.0, String::new(), 0);
    };
    let val: f64 = rest[..flen].parse().unwrap_or(0.0);
    let after = rest[flen..].trim_start();
    let tok_end = after.find(char::is_whitespace).unwrap_or(after.len());
    let unit = &after[..tok_end];
    if unit.is_empty() {
        (val, String::new(), 1)
    } else {
        (val, unit.to_string(), 2)
    }
}

/// Length of the longest float token at the start of `s` — Go's `%f`
/// operand: `[+-]?(digits[.digits*] | .digits)([eE][+-]?digits)?` plus the
/// `inf`/`infinity`/`nan` words `strconv.ParseFloat` accepts.
fn float_prefix(s: &str) -> Option<usize> {
    let b = s.as_bytes();
    let mut i = 0;
    if matches!(b.first(), Some(b'+') | Some(b'-')) {
        i = 1;
    }
    let lower = s[i..].to_ascii_lowercase();
    for word in ["infinity", "inf", "nan"] {
        if lower.starts_with(word) {
            return Some(i + word.len());
        }
    }
    let mut j = i;
    while matches!(b.get(j), Some(c) if c.is_ascii_digit()) {
        j += 1;
    }
    let int_digits = j - i;
    let mut frac_digits = 0;
    if b.get(j) == Some(&b'.') {
        let mut k = j + 1;
        while matches!(b.get(k), Some(c) if c.is_ascii_digit()) {
            k += 1;
        }
        frac_digits = k - j - 1;
        if int_digits > 0 || frac_digits > 0 {
            j = k;
        }
    }
    if int_digits == 0 && frac_digits == 0 {
        return None;
    }
    // Optional exponent — consumed only when digits follow `e[+-]`.
    if matches!(b.get(j), Some(b'e') | Some(b'E')) {
        let mut k = j + 1;
        if matches!(b.get(k), Some(b'+') | Some(b'-')) {
            k += 1;
        }
        let e0 = k;
        while matches!(b.get(k), Some(c) if c.is_ascii_digit()) {
            k += 1;
        }
        if k > e0 {
            j = k;
        }
    }
    Some(j)
}

#[cfg(test)]
pub(crate) mod test_support {
    //! Shared scaffolding for the ported clean tests — the Go suite's
    //! `t.TempDir()` + `t.Setenv(HOME/XDG_*)` setup, expressed as injected
    //! [`Deps`] rooted at a tempdir.
    use super::*;
    use crate::runner::FakeRunner;
    use std::fs;
    use std::path::Path;

    pub fn tempdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "mu-test-clean-{}-{}-{}",
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

    /// `Deps` rooted at `root`: `.cache`, `.config`, `home`, and a data home
    /// whose creation mirrors `trashHomeDir` (mkdir 0700 on demand so the
    /// FreeDesktop trash fallback can use it). `trash_runner` is a bare
    /// [`FakeRunner`] — `gio` is never found, forcing the FreeDesktop
    /// fallback the Go tests exercise through `trashRunner`.
    pub fn deps_for_test(root: &Path, runner: Arc<dyn Runner>) -> Deps {
        let data_home = root.join("data-home");
        Deps {
            runner,
            cache_home: root.join(".cache"),
            config_home: root.join(".config"),
            home: root.to_path_buf(),
            data_home: data_home.clone(),
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
        }
    }

    /// The snap list fixture from `scan_snap_test.go`.
    pub const SNAP_LIST_FIXTURE: &str = "Name    Version   Rev   Tracking       Publisher   Notes\n\
core20  20230801  1974  latest/stable  canonical*  base\n\
lxd     5.21.1    27183 latest/stable  canonical*  disabled\n\
firefox 123.0     3440  latest/stable  mozilla*    -\n\
vlc     3.0.20    3078  latest/stable  videolan*   disabled\n";
}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;
    use crate::runner::{FakeRunner, RunError};
    use std::fs;
    use std::sync::Arc;

    fn stub_target(
        id: &'static str,
        execute: impl Fn(bool) -> Result<()> + 'static,
    ) -> CleanTarget {
        CleanTarget {
            id,
            label: id,
            requires_sudo: false,
            opt_in: false,
            scan: Box::new(|| (0, None)),
            preview: None,
            execute: Box::new(execute),
            count_fn: None,
        }
    }

    fn opts(dry_run: bool) -> Options {
        Options {
            dry_run,
            ..Options::default()
        }
    }

    // safety_test.go: TestExecuteAggregatesPartialFailures
    #[test]
    fn execute_aggregates_partial_failures() {
        let tmp = tempdir("execagg");
        let deps = deps_for_test(&tmp, Arc::new(FakeRunner::new()));
        let results = vec![
            ScanResult {
                target: stub_target("ok", |_| Ok(())),
                size: 100,
                items: Vec::new(),
            },
            ScanResult {
                target: stub_target("bad", |_| Err(Error::Msg("blocked".to_string()))),
                size: 200,
                items: Vec::new(),
            },
        ];
        let mut out = Vec::new();
        let err = {
            let mut r = runtext::Run::new(&mut out);
            execute(&mut r, &results, &opts(true), &deps)
        }
        .expect_err("expected aggregate failure");
        assert!(
            err.to_string().contains("1 clean target"),
            "expected aggregate failure, got {err}"
        );
        fs::remove_dir_all(&tmp).ok();
    }

    // safety_test.go: TestResolveTargetsRejectsUnknownAndNonOptInIDs
    #[test]
    fn resolve_targets_rejects_unknown_and_non_opt_in_ids() {
        let tmp = tempdir("resolve");
        let deps = deps_for_test(&tmp, Arc::new(FakeRunner::new()));
        let err = resolve_targets_in(&deps, &["unknown".to_string()])
            .err()
            .expect("expected unknown include rejection");
        assert_eq!(err.to_string(), "unknown clean include ID: unknown");
        let err = resolve_targets_in(&deps, &["user-cache".to_string()])
            .err()
            .expect("expected non-opt-in include rejection");
        assert_eq!(
            err.to_string(),
            "clean include ID is not opt-in: user-cache"
        );
        fs::remove_dir_all(&tmp).ok();
    }

    // safety_test.go: TestRunFailsClosedOnMalformedConfigBeforeScanning
    #[test]
    fn run_fails_closed_on_malformed_config_before_scanning() {
        let tmp = tempdir("badcfg");
        let config_root = tmp.join(".config");
        fs::create_dir_all(config_root.join("mu")).unwrap();
        fs::create_dir_all(tmp.join(".cache")).unwrap();
        fs::write(config_root.join("mu").join("config.toml"), "broken = [").unwrap();
        let deps = deps_for_test(&tmp, Arc::new(FakeRunner::new()));
        let mut out = Vec::new();
        let err = run_in(&opts(true), &deps, &mut out).expect_err("expected config error");
        assert!(
            err.to_string().contains("invalid mu configuration"),
            "expected config error, got {err}"
        );
        fs::remove_dir_all(&tmp).ok();
    }

    // safety_test.go: TestRunDryRunCompletesWithReadOnlyScanners
    #[test]
    fn run_dry_run_completes_with_read_only_scanners() {
        let tmp = tempdir("dryrun");
        fs::create_dir_all(tmp.join(".cache")).unwrap();
        let runner = Arc::new(FakeRunner::new());
        // Go's default LookPath reports every binary present except `snap`
        // (stubbed missing); FakeRunner misses by default, so grant the rest.
        runner.set_look_path("journalctl", "/usr/bin/journalctl");
        runner.set_handler(|spec| {
            if spec.program == "env"
                && spec.args
                    == ["LC_ALL=C", "journalctl", "--disk-usage"].map(std::ffi::OsString::from)
            {
                return Ok(crate::runner::Output {
                    stdout: b"Archived and active journals take up 12.0M in the file system.\n"
                        .to_vec(),
                    ..Default::default()
                });
            }
            if spec.program == "apt-get" {
                return Ok(crate::runner::Output {
                    stdout: b"0 upgraded, 0 newly installed, 0 to remove.\n".to_vec(),
                    ..Default::default()
                });
            }
            Ok(crate::runner::Output::default())
        });
        let deps = deps_for_test(&tmp, runner);
        let mut out = Vec::new();
        let summary = run_in(&opts(true), &deps, &mut out).expect("dry run should complete");
        assert!(
            summary.starts_with("Dry run — "),
            "unexpected summary {summary:?}"
        );
        let text = String::from_utf8_lossy(&out);
        for id in [
            "User Cache (~/.cache)",
            "Thumbnail Cache",
            "Font Cache (all users)",
            "APT Package Cache",
            "Journal Logs",
            "Snap Disabled Revisions",
            "APT Autoremove Candidates",
        ] {
            assert!(text.contains(id), "missing row {id:?} in {text:?}");
        }
        fs::remove_dir_all(&tmp).ok();
    }

    // safety_test.go: TestSystemTargetsUseRunnerAndPropagateErrors
    #[test]
    fn system_targets_use_runner_and_propagate_errors() {
        let tmp = tempdir("systargets");
        let runner = Arc::new(FakeRunner::new());
        runner.set_look_path("journalctl", "/usr/bin/journalctl");
        runner.set_handler(|spec| {
            if spec.program == "env"
                && spec.args
                    == ["LC_ALL=C", "journalctl", "--disk-usage"].map(std::ffi::OsString::from)
            {
                return Ok(crate::runner::Output {
                    stdout: b"Archived and active journals take up 1.5G in the file system.\n"
                        .to_vec(),
                    ..Default::default()
                });
            }
            Err(RunError::Spawn(std::io::Error::other("command failed")))
        });
        let deps = deps_for_test(&tmp, runner);

        let size = journal_size(&*deps.runner).expect("journal size");
        assert_eq!(size, (1.5 * 1024.0 * 1024.0 * 1024.0) as i64);

        let deps2 = deps_for_test(&tmp, Arc::clone(&deps.runner));
        let apt = targets::apt_cache_target_in(&deps2);
        (apt.execute)(true).expect("apt dry-run");
        assert!((apt.execute)(false).is_err(), "expected apt clean failure");

        let journal = targets::journal_logs_target_in(&deps2);
        assert!(
            (journal.execute)(false).is_err(),
            "expected journal vacuum failure"
        );

        let deps3 = deps_for_test(&tmp, Arc::clone(&deps.runner));
        assert!(
            all_targets_in(&deps3)
                .into_iter()
                .any(|t| t.id == "thumbnails"),
            "target lookup failed"
        );
        assert!(
            !all_targets_in(&deps3)
                .into_iter()
                .any(|t| t.id == "missing"),
            "missing target unexpectedly found"
        );
        fs::remove_dir_all(&tmp).ok();
    }

    // safety_test.go: TestThumbnailAndSnapDryRunPaths (snap half lives in
    // scan_snap.rs; here: thumbnails)
    #[test]
    fn thumbnail_dry_run_path() {
        let tmp = tempdir("thumbs");
        let thumbs = tmp.join(".cache").join("thumbnails");
        fs::create_dir_all(&thumbs).unwrap();
        fs::write(thumbs.join("x"), "123").unwrap();
        let deps = deps_for_test(&tmp, Arc::new(FakeRunner::new()));
        let target = targets::thumbnails_target_in(&deps);
        let size = (target.scan)().0;
        assert_eq!(size, 3, "thumbnail size");
        (target.execute)(true).expect("dry-run execute");
        assert!(paths::path_exists(&thumbs), "dry-run removed thumbnails");
        fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn system_trash_target_and_ledger_test() {
        let tmp = tempdir("systrash");
        let fake_runner = Arc::new(FakeRunner::new());
        fake_runner.set_look_path("gio", "/usr/bin/gio");
        let deps = deps_for_test(&tmp, fake_runner);
        let trash_files = deps.data_home.join("Trash").join("files");
        fs::create_dir_all(&trash_files).unwrap();
        fs::write(trash_files.join("item1.txt"), "hello").unwrap();
        fs::write(trash_files.join("item2.txt"), "world!").unwrap();

        let trash_target = targets::system_trash_target_in(&deps);
        assert!(trash_target.opt_in, "system-trash should be opt-in");
        assert_eq!(trash_target.id, "system-trash");

        let (sz, err) = (trash_target.scan)();
        assert!(err.is_none());
        assert!(sz > 0);
        assert_eq!(targets::count_trash_items(&deps.data_home), 2);

        // Execute in dry-run mode
        (trash_target.execute)(true).expect("dry-run should succeed");
        assert!(trash_files.join("item1.txt").exists());

        // Test export_preview_ledger
        let res = ScanResult {
            target: trash_target,
            size: sz,
            items: vec!["item1.txt".into(), "item2.txt".into()],
        };
        export_preview_ledger(&[res], sz, &deps.config_home);
        let ledger = tmp.join(".config").join("mu").join("clean-list.txt");
        assert!(ledger.exists(), "clean-list.txt should be created");
        let content = fs::read_to_string(&ledger).unwrap();
        assert!(content.contains("System Trash (2 items)"));

        fs::remove_dir_all(&tmp).ok();
    }

    // Finding 5 (r1): `--external` roots are validated fail-closed.
    #[test]
    fn external_volume_rejects_unsafe_roots() {
        let tmp = tempdir("ext-bad");
        let deps = deps_for_test(&tmp, Arc::new(FakeRunner::new()));
        for bad in ["/", "/etc", "relative/path"] {
            let mut out = Vec::new();
            let err = run_in(
                &Options {
                    dry_run: true,
                    external: Some(PathBuf::from(bad)),
                    ..Options::default()
                },
                &deps,
                &mut out,
            )
            .expect_err("expected external root rejection");
            let text = err.to_string();
            assert!(
                text.contains("unsafe cleanup root") || text.contains("must be absolute"),
                "bad={bad} err={text}"
            );
        }
        fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn downloads_large_files_hint_test() {
        let tmp = tempdir("dlhint");
        let downloads = tmp.join("Downloads");
        fs::create_dir_all(&downloads).unwrap();
        let big_file = downloads.join("large_iso.img");
        // Allocate sparse or small file? We can check with mock or small if size is tested
        // But wait: metadata.len() needs to be > 500MB.
        // With set_len:
        let f = fs::File::create(&big_file).unwrap();
        f.set_len(501 * 1024 * 1024).unwrap(); // sparse file, takes 0 actual disk space

        let mut out = Vec::new();
        let mut r = runtext::Run::new(&mut out);
        print_downloads_large_files_hint(&tmp, &mut r).unwrap();
        let s = String::from_utf8_lossy(&out);
        assert!(s.contains("Hint: found 1 large file (>500MB) in ~/Downloads"));

        fs::remove_dir_all(&tmp).ok();
    }
}
