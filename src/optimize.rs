//! Optimize steps — port of `internal/optimize/optimize.go`.
//!
//! Go runs the steps through a Bubbletea program (`optimizeModel`); the
//! non-interactive path (`isatty(stdin)` false → `tea.WithInput(nil)`) is a
//! purely sequential executor: each step runs in order, failures do not stop
//! later steps, captured output is printed via `tea.Println`, and the joined
//! `stepErrors` becomes the command's nonzero exit. That is what this module
//! ports — the same sequential executor drives TTY and non-TTY runs (no
//! raw-mode input reader exists here, so Go's `ui.ExecTerminal` terminal
//! release has nothing to race against). The spinner checklist, ANSI styles,
//! and the ctrl+c "finish the active step then skip the rest" path are
//! TUI-phase concerns and are intentionally dropped; on a non-TTY Go cannot
//! receive `KeyMsg` at all, so `stopRequested` is unreachable there anyway.
//!
//! Go's swappable package vars become explicit injection: [`Deps`] mirrors
//! `optimizeRunner` (`command.Runner`) and `runAutoremove`
//! (`clean.RunAutoremove`), plus the `ui.Confirm` prompt and the XDG dirs that
//! `utils.LoadWhitelist`/`utils.InitLogger` read from the environment (tests
//! must not mutate env).

use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use crate::config;
use crate::error::{Error, Result, msg};
use crate::oplog;
use crate::runner::{CommandSpec, Output, ProcessRunner, RunError, Runner};
use crate::trash;
use crate::xdg;
/// `Options` — controls optimize behavior.
#[derive(Debug, Default, Clone)]
pub struct Options {
    /// `DryRun` — preview actions without making changes.
    pub dry_run: bool,
    /// `Debug` — verbose logging (warn when the ops log cannot open).
    pub debug: bool,
    /// `Skip` — step IDs from `--skip`, merged over `optimize_skip.steps`.
    pub skip: Vec<String>,
    /// `AutoYes` — skip the confirmation prompt (`--yes`).
    pub auto_yes: bool,
}

/// Dependencies that Go reads from package vars, env, and the terminal.
/// Production wiring lives in [`Deps::real`]; tests substitute fakes.
pub(crate) struct Deps {
    /// `optimizeRunner` — `command.ExecRunner` in production.
    pub runner: Arc<dyn Runner>,
    /// `runAutoremove` — `clean.RunAutoremove(ctx, dryRun)` in production.
    pub autoremove: Arc<dyn Fn(bool) -> Result<()> + Send + Sync>,
    /// `ui.Confirm` — returns true when the user picks YES.
    pub confirm: Arc<dyn Fn(&str) -> bool + Send + Sync>,
    /// `$XDG_CONFIG_HOME` — where `mu/config.toml` is loaded from.
    pub config_home: PathBuf,
    /// `$XDG_DATA_HOME` — where `mu/operations.log` is opened.
    pub data_home: PathBuf,
    /// `$HOME` — user home directory.
    pub home: PathBuf,
    /// `$XDG_STATE_HOME` or `~/.local/state`.
    pub state_home: PathBuf,
    /// `TrashDeps` for safe deletion.
    pub trash_deps: Rc<trash::TrashDeps>,
    /// `MU_NO_OPLOG=1` — disables `InitLogger`.
    pub no_oplog: bool,
}

impl Deps {
    /// Production dependencies, mirroring the Go package vars and env reads.
    pub(crate) fn real() -> Self {
        Self {
            runner: Arc::new(ProcessRunner::new()),
            // `clean.RunAutoremove` on clean's own `cleanRunner` — a separate
            // `ProcessRunner` from `optimizeRunner`, matching the separate Go
            // package vars.
            autoremove: Arc::new(|dry_run| {
                crate::clean::run_autoremove(&ProcessRunner::new(), dry_run)
            }),
            confirm: Arc::new(confirm_prompt),
            config_home: xdg::config_home(),
            data_home: xdg::data_home(),
            home: xdg::home_dir(),
            state_home: xdg::state_home(),
            trash_deps: Rc::new(trash::TrashDeps::real()),
            no_oplog: std::env::var_os("MU_NO_OPLOG").is_some_and(|v| v == "1"),
        }
    }
}

/// Go `func(io.Writer) error` — the step action writing captured
/// stdout/stderr to the sink.
type StepRun = Box<dyn Fn(&mut dyn Write) -> Result<()>>;
type StepDryRunLines = Arc<dyn Fn(&Deps) -> Vec<String> + Send + Sync>;

pub(crate) struct Step {
    pub id: &'static str,
    pub desc: &'static str,
    pub dry_run_cmd: &'static str,
    pub dry_run_lines: Option<StepDryRunLines>,
    pub run: StepRun,
}

impl Step {
    pub(crate) fn new(
        id: &'static str,
        desc: &'static str,
        dry_run_cmd: &'static str,
        run: StepRun,
    ) -> Self {
        Self {
            id,
            desc,
            dry_run_cmd,
            dry_run_lines: None,
            run,
        }
    }
}

/// `allSteps` — apt, journal, caches, in that order.
fn all_steps(deps: &Deps) -> Vec<Step> {
    vec![
        Step {
            id: "apt",
            desc: "apt-get update && apt-get autoremove --purge",
            dry_run_cmd: "sudo apt-get update && sudo apt-get autoremove --purge -y",
            dry_run_lines: None,
            run: {
                let runner = deps.runner.clone();
                let autoremove = deps.autoremove.clone();
                Box::new(move |out: &mut dyn Write| {
                    apt_autoremove(runner.as_ref(), autoremove.as_ref(), out)
                })
            },
        },
        Step {
            id: "journal",
            desc: "journalctl --vacuum-size=500M",
            dry_run_cmd: "sudo journalctl --vacuum-size=500M",
            dry_run_lines: None,
            run: {
                let runner = deps.runner.clone();
                Box::new(move |out: &mut dyn Write| journal_vacuum(runner.as_ref(), out))
            },
        },
        Step {
            id: "caches",
            desc: "update icon/mime/font caches",
            dry_run_cmd: "sudo update-mime-database /usr/share/mime && fc-cache -f",
            dry_run_lines: None,
            run: {
                let runner = deps.runner.clone();
                Box::new(move |out: &mut dyn Write| update_caches(runner.as_ref(), out))
            },
        },
        Step {
            id: "dns",
            desc: "flush DNS cache",
            dry_run_cmd: "sudo resolvectl flush-caches",
            dry_run_lines: None,
            run: {
                let runner = deps.runner.clone();
                Box::new(move |out: &mut dyn Write| dns_flush(runner.as_ref(), out))
            },
        },
        Step {
            id: "packages",
            desc: "repair interrupted package installs",
            dry_run_cmd: "sudo dpkg --configure -a && sudo apt-get install -f -y",
            dry_run_lines: None,
            run: {
                let runner = deps.runner.clone();
                Box::new(move |out: &mut dyn Write| packages_repair(runner.as_ref(), out))
            },
        },
        Step {
            id: "systemd",
            desc: "reset failed systemd units",
            dry_run_cmd: "sudo systemctl reset-failed && systemctl --user reset-failed",
            dry_run_lines: None,
            run: {
                let runner = deps.runner.clone();
                Box::new(move |out: &mut dyn Write| systemd_reset_failed(runner.as_ref(), out))
            },
        },
        Step {
            id: "flatpak",
            desc: "clean unused Flatpak runtimes",
            dry_run_cmd: "flatpak uninstall --unused -y",
            dry_run_lines: None,
            run: {
                let runner = deps.runner.clone();
                Box::new(move |out: &mut dyn Write| flatpak_clean_unused(runner.as_ref(), out))
            },
        },
        Step {
            id: "sqlite",
            desc: "vacuum application sqlite databases",
            dry_run_cmd: "sqlite3 <db_path> \"VACUUM;\"",
            dry_run_lines: Some(Arc::new(|d: &Deps| {
                let candidates = find_sqlite_candidates(&d.home);
                candidates
                    .into_iter()
                    .map(|p| format!("sqlite3 {} \"VACUUM;\"", p.display()))
                    .collect()
            })),
            run: {
                let runner = deps.runner.clone();
                let home = deps.home.clone();
                Box::new(move |out: &mut dyn Write| sqlite_vacuum(runner.as_ref(), &home, out))
            },
        },
        Step {
            id: "state",
            desc: "clean stale files older than 30 days in ~/.local/state",
            dry_run_cmd: "find ~/.local/state -type f -mtime +30 -delete",
            dry_run_lines: None,
            run: {
                let runner = deps.runner.clone();
                let config_home = deps.config_home.clone();
                let state_home = deps.state_home.clone();
                let trash_deps = deps.trash_deps.clone();
                Box::new(move |out: &mut dyn Write| {
                    state_clean(
                        runner.as_ref(),
                        &config_home,
                        &state_home,
                        trash_deps.as_ref(),
                        out,
                    )
                })
            },
        },
        Step {
            id: "autostart",
            desc: "audit autostart desktop entries for missing binaries",
            dry_run_cmd: "audit ~/.config/autostart/*.desktop for missing Exec binaries",
            dry_run_lines: None,
            run: {
                let runner = deps.runner.clone();
                let config_home = deps.config_home.clone();
                Box::new(move |out: &mut dyn Write| {
                    autostart_audit(runner.as_ref(), &config_home, out)
                })
            },
        },
        Step {
            id: "services",
            desc: "audit broken systemd service symlinks",
            dry_run_cmd: "audit broken symlinks in /etc/systemd/system and ~/.config/systemd/user",
            dry_run_lines: None,
            run: {
                let config_home = deps.config_home.clone();
                Box::new(move |out: &mut dyn Write| services_audit(&config_home, out))
            },
        },
        Step {
            id: "permissions",
            desc: "audit root-owned files in home directory",
            dry_run_cmd: "audit files in $HOME owned by root",
            dry_run_lines: None,
            run: {
                let home = deps.home.clone();
                Box::new(move |out: &mut dyn Write| permissions_audit(&home, out))
            },
        },
        Step {
            id: "disk-health",
            desc: "audit filesystem integrity and SMART disk health",
            dry_run_cmd: "smartctl --scan || journalctl -k --grep=EXT4-fs",
            dry_run_lines: None,
            run: {
                let runner = deps.runner.clone();
                Box::new(move |out: &mut dyn Write| disk_health_audit(runner.as_ref(), out))
            },
        },
    ]
}

/// `StepIDs` — the IDs of all built-in optimize steps in order.
pub fn step_ids() -> Vec<&'static str> {
    // Go derives IDs from allSteps(); the deps do not affect the IDs.
    all_steps(&Deps::real()).iter().map(|s| s.id).collect()
}

/// `StepStatus` — per-step outcome, logged via `LogOutcome`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepStatus {
    /// `StepSuccess`
    Success,
    /// `StepFailed`
    Failed,
    /// `StepSkipped`
    Skipped,
}

impl StepStatus {
    /// `string(msg.status)` — the outcome string logged to the ops log.
    fn as_str(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Failed => "failed",
            Self::Skipped => "skipped",
        }
    }
}

/// `StepResult` — the recorded outcome of one step.
#[derive(Debug)]
pub struct StepResult {
    /// Step ID.
    pub id: String,
    /// Final status.
    pub status: StepStatus,
    /// Step error, when failed.
    pub err: Option<Error>,
}

/// `RunStep` — runs a single optimize step by id (apt, journal, caches).
///
/// Go returns `(skipped bool, err error)`; `skipped == true` only ever pairs
/// with a nil error, so `Result<bool>` is lossless: `Ok(true)` skipped by
/// policy, `Ok(false)` ran successfully, `Err` unknown or failed step.
/// `out` receives the step's captured output (`io.Discard` → `io::sink()`).
pub fn run_step(id: &str, opts: &Options, out: &mut dyn Write) -> Result<bool> {
    run_step_with(id, opts, &Deps::real(), out)
}

pub(crate) fn run_step_with(
    id: &str,
    opts: &Options,
    deps: &Deps,
    out: &mut dyn Write,
) -> Result<bool> {
    let steps = all_steps(deps);
    let target = steps
        .iter()
        .find(|s| s.id == id)
        .ok_or_else(|| Error::Msg(format!("unknown optimize step: {id}")))?;
    let skip = resolve_skip(opts, &deps.config_home)?;
    if skip.iter().any(|s| s == id) {
        return Ok(true);
    }
    (target.run)(out)?;
    Ok(false)
}

/// `Run` — executes (or previews) the optimization steps, returning the
/// one-line summary (`"Aborted."` when the user declines the prompt).
/// Step lines and the summary are also printed, like Go's `ui.Run`.
pub fn run(opts: &Options) -> Result<String> {
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    run_with(opts, &Deps::real(), &mut out)
}

/// TUI entry for optimize — port of Go's interactive path. Prints the plan,
/// shows a YES/NO confirm (default NO), then runs with `auto_yes=true` if
/// confirmed, or returns "Aborted." if declined. Wired by `tui::dispatch_subcommand`.
pub fn run_tui(opts: &Options) -> Result<String> {
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    // Reuse run_with but with a confirm closure that calls the TUI.
    let deps = Deps {
        confirm: Arc::new(|_prompt: &str| crate::tui::confirm_inline("Proceed to optimize?")),
        ..Deps::real()
    };
    run_with(opts, &deps, &mut out)
}

pub(crate) fn run_with(opts: &Options, deps: &Deps, out: &mut dyn Write) -> Result<String> {
    // Go loads the whitelist once up front (fail closed on malformed config)
    // and again inside resolveSkip.
    config::load_whitelist_from(&deps.config_home)
        .map_err(|e| e.with_context("invalid mu configuration".to_string()))?;
    let skip = resolve_skip(opts, &deps.config_home)?;
    let steps = all_steps(deps);

    // `ui.NewRun(os.Stdout)` — on a non-TTY lipgloss styles render as plain
    // text; the port always emits the plain form (animation dropped).
    let _ = writeln!(out, "\n  Optimize plan");
    for s in &steps {
        if skip.iter().any(|k| k == s.id) {
            let _ = writeln!(out, "  [skip]  {}", s.id);
        } else if opts.dry_run {
            if let Some(lines_fn) = &s.dry_run_lines {
                let lines = lines_fn(deps);
                if lines.is_empty() {
                    let _ = writeln!(out, "  [dry-run] {}", s.dry_run_cmd);
                } else {
                    for line in lines {
                        let _ = writeln!(out, "  [dry-run] {}", line);
                    }
                }
            } else {
                let _ = writeln!(out, "  [dry-run] {}", s.dry_run_cmd);
            }
        } else {
            let _ = writeln!(out, "  [run]   {}", s.desc);
        }
    }

    if opts.dry_run {
        let _ = writeln!(out, "  Dry run — nothing executed.");
        return Ok("Dry run — nothing executed.".to_string());
    }

    if !opts.auto_yes && !(deps.confirm)("Proceed to optimize?") {
        let _ = writeln!(out, "  Aborted.");
        return Ok("Aborted.".to_string());
    }

    // `utils.InitLogger` + `defer utils.CloseLogger()` — the RAII guard lives
    // to scope end like Go's defer.
    if !deps.no_oplog
        && let Err(e) = oplog::init_logger_at(&deps.data_home)
        && opts.debug
    {
        eprintln!("warn: could not open log: {e}");
    }
    let _log = CloseLoggerGuard;
    let results = execute_steps(&steps, &skip, out);

    // Go: a failed model run returns the joined step errors and never prints
    // the summary.
    step_errors(&results)?;

    let (mut succeeded, mut failed, mut skipped) = (0, 0, 0);
    for res in &results {
        match res.status {
            StepStatus::Success => succeeded += 1,
            StepStatus::Failed => failed += 1,
            StepStatus::Skipped => skipped += 1,
        }
    }
    let summary = format!(
        "Optimization complete — {succeeded} succeeded, {failed} failed, {skipped} skipped"
    );
    let _ = writeln!(out, "\n  {summary}");
    Ok(summary)
}

/// `resolveSkip` — merges `opts.Skip` with the config's `optimize_skip.steps`
/// (CLI entries first, config additions deduplicated) and validates every ID
/// against [`step_ids`]. A malformed config fails closed.
fn resolve_skip(opts: &Options, config_home: &Path) -> Result<Vec<String>> {
    let wl = config::load_whitelist_from(config_home)
        .map_err(|e| e.with_context("invalid mu configuration".to_string()))?;

    let mut skip: Vec<String> = opts.skip.clone();
    for s in &wl.optimize_skip.steps {
        if !skip.contains(s) {
            skip.push(s.clone());
        }
    }
    let valid = step_ids();
    for id in &skip {
        if !valid.contains(&id.as_str()) {
            return msg(format!("unknown optimize skip ID: {id}"));
        }
    }
    Ok(skip)
}

/// The sequential half of `optimizeModel`: run each step in order, record
/// `{id, status, err}`, `LogOutcome` every completion, then emit the same
/// lines Go's `Update` emits via `tea.Println` — the step's captured output
/// (trailing newlines trimmed, truncated past 4096 bytes) and
/// `failed: {id}: {err}` on failure. Independent steps continue after a
/// failure; there is no `stopRequested` path (see module docs).
fn execute_steps(steps: &[Step], skip: &[String], out: &mut dyn Write) -> Vec<StepResult> {
    let mut results = Vec::with_capacity(steps.len());
    for s in steps {
        let (status, err, output) = if skip.iter().any(|k| k == s.id) {
            (StepStatus::Skipped, None, Vec::new())
        } else {
            let mut buf: Vec<u8> = Vec::new();
            let res = (s.run)(&mut buf);
            // strings.TrimRight(buf.String(), "\n") then a 4096-byte cut +
            // "\n... (truncated)" — byte-level, like Go string slicing.
            while buf.last() == Some(&b'\n') {
                buf.pop();
            }
            if buf.len() > 4096 {
                buf.truncate(4096);
                buf.extend_from_slice(b"\n... (truncated)");
            }
            match res {
                Ok(()) => (StepStatus::Success, None, buf),
                Err(e) => (StepStatus::Failed, Some(e), buf),
            }
        };
        // `tea.Println(fmt.Sprintf("failed: %s: %v", ...))` — formatted before
        // `err` moves into the result.
        let failed_line = err.as_ref().map(|e| format!("failed: {}: {}", s.id, e));
        results.push(StepResult {
            id: s.id.to_string(),
            status,
            err,
        });
        oplog::log_outcome("optimize", s.id, status.as_str());
        if !output.is_empty() {
            let _ = out.write_all(&output);
            let _ = out.write_all(b"\n");
        }
        if let Some(line) = failed_line {
            let _ = writeln!(out, "{line}");
        }
    }
    results
}

/// `stepErrors` — joins each failed step as `optimize step {id}: {err}`,
/// newline-separated like `errors.Join`.
fn step_errors(results: &[StepResult]) -> Result<()> {
    let mut errs = Vec::new();
    for result in results {
        if result.status == StepStatus::Failed
            && let Some(e) = &result.err
        {
            errs.push(format!("optimize step {}: {}", result.id, e));
        }
    }
    if errs.is_empty() {
        Ok(())
    } else {
        Err(Error::Msg(errs.join("\n")))
    }
}

/// `defer utils.CloseLogger()` — closes on drop at every return path.
struct CloseLoggerGuard;

impl Drop for CloseLoggerGuard {
    fn drop(&mut self) {
        oplog::close_logger();
    }
}

/// Interim `ui.Confirm`: the YES/NO button model is TUI-phase. On a pipe Go's
/// model can never get a key — it quits on EOF with the NO default — so a
/// non-terminal stdin declines without printing an unanswerable prompt. On a
/// TTY a plain line prompt keeps the NO default: EOF, empty input, or
/// anything but `y`/`yes` declines, matching `Confirm`'s `result == false`
/// exits (q, ctrl+c, error).
fn confirm_prompt(prompt: &str) -> bool {
    let stdin = std::io::stdin();
    if !stdin.is_terminal() {
        return false;
    }
    {
        let stdout = std::io::stdout();
        let mut o = stdout.lock();
        let _ = write!(o, "  {prompt} [y/N] ");
        let _ = o.flush();
    }
    let mut line = String::new();
    match stdin.read_line(&mut line) {
        Ok(0) | Err(_) => false,
        Ok(_) => matches!(line.trim().to_lowercase().as_str(), "y" | "yes"),
    }
}

/// Go callers write `result.Stdout`/`result.Stderr` unconditionally — the
/// buffers stay populated on `ExitError` and are empty on a spawn failure, so
/// a [`RunError`] carrying output writes it and one without writes nothing.
fn write_captured(out: &mut dyn Write, res: &std::result::Result<Output, RunError>) {
    let output = match res {
        Ok(o) => Some(o),
        Err(e) => e.output(),
    };
    if let Some(o) = output {
        let _ = out.write_all(&o.stdout);
        let _ = out.write_all(&o.stderr);
    }
}

/// `aptAutoremove` — `sudo apt-get update`, then APT-policy autoremove via
/// `clean.RunAutoremove` (always non-dry; `Run` short-circuits `--dry-run`
/// before any step executes).
fn apt_autoremove(
    runner: &dyn Runner,
    autoremove: &dyn Fn(bool) -> Result<()>,
    out: &mut dyn Write,
) -> Result<()> {
    let res = runner.run(&CommandSpec::new("sudo", ["apt-get", "update"]));
    write_captured(out, &res);
    if let Err(e) = res {
        return Err(Error::Msg(e.to_string()).with_context("apt-get update".to_string()));
    }
    autoremove(false)
}

/// `journalVacuum` — `sudo journalctl --vacuum-size=500M`, error unwrapped.
fn journal_vacuum(runner: &dyn Runner, out: &mut dyn Write) -> Result<()> {
    let res = runner.run(&CommandSpec::new(
        "sudo",
        ["journalctl", "--vacuum-size=500M"],
    ));
    write_captured(out, &res);
    res.map(|_| ()).map_err(|e| Error::Msg(e.to_string()))
}

/// `updateCaches` — runs each cache refresh independently and joins failures
/// (`errors.Join`): a failed command does not stop the rest.
fn update_caches(runner: &dyn Runner, out: &mut dyn Write) -> Result<()> {
    let mut errs = Vec::new();
    for args in [
        &["sudo", "update-mime-database", "/usr/share/mime"][..],
        &["fc-cache", "-f"][..],
    ] {
        let res = runner.run(&CommandSpec::new(args[0], &args[1..]));
        write_captured(out, &res);
        if let Err(e) = res {
            errs.push(format!("{}: {}", args.join(" "), e));
        }
    }
    if errs.is_empty() {
        Ok(())
    } else {
        Err(Error::Msg(errs.join("\n")))
    }
}

fn dns_flush(runner: &dyn Runner, out: &mut dyn Write) -> Result<()> {
    // `flush-caches` is polkit-gated (`org.freedesktop.resolve1.flush-caches`),
    // so the system half must run elevated or every default run fails without
    // a polkit agent (plan Conflict 2: root-level steps run with sudo).
    if runner.look_path("resolvectl").is_ok() {
        let res = runner.run(&CommandSpec::new("sudo", ["resolvectl", "flush-caches"]));
        write_captured(out, &res);
        if let Err(e) = res {
            if runner.look_path("systemd-resolve").is_ok() {
                let res2 = runner.run(&CommandSpec::new(
                    "sudo",
                    ["systemd-resolve", "--flush-caches"],
                ));
                write_captured(out, &res2);
                res2.map(|_| ()).map_err(|e2| {
                    Error::Msg(format!("sudo systemd-resolve --flush-caches: {e2}"))
                })?;
            } else {
                return Err(Error::Msg(format!("sudo resolvectl flush-caches: {e}")));
            }
        }
    } else if runner.look_path("systemd-resolve").is_ok() {
        let res = runner.run(&CommandSpec::new(
            "sudo",
            ["systemd-resolve", "--flush-caches"],
        ));
        write_captured(out, &res);
        res.map(|_| ())
            .map_err(|e| Error::Msg(format!("sudo systemd-resolve --flush-caches: {e}")))?;
    } else {
        let _ = writeln!(out, "neither resolvectl nor systemd-resolve found; skipped");
    }
    Ok(())
}

fn packages_repair(runner: &dyn Runner, out: &mut dyn Write) -> Result<()> {
    let res1 = runner.run(&CommandSpec::new("sudo", ["dpkg", "--configure", "-a"]));
    write_captured(out, &res1);
    let err1 = res1.err();

    let res2 = runner.run(&CommandSpec::new(
        "sudo",
        ["apt-get", "install", "-f", "-y"],
    ));
    write_captured(out, &res2);
    let err2 = res2.err();

    match (err1, err2) {
        (Some(e1), Some(e2)) => Err(Error::Msg(format!("{e1}\n{e2}"))),
        (Some(e1), None) => Err(Error::Msg(e1.to_string())),
        (None, Some(e2)) => Err(Error::Msg(e2.to_string())),
        (None, None) => Ok(()),
    }
}

fn systemd_reset_failed(runner: &dyn Runner, out: &mut dyn Write) -> Result<()> {
    let mut errs = Vec::new();
    // The system scope needs root (polkit `manage-units` otherwise); the user
    // scope runs unprivileged (plan Conflict 2).
    let res1 = runner.run(&CommandSpec::new("sudo", ["systemctl", "reset-failed"]));
    write_captured(out, &res1);
    if let Err(e) = res1 {
        errs.push(e.to_string());
    }
    let res2 = runner.run(&CommandSpec::new("systemctl", ["--user", "reset-failed"]));
    write_captured(out, &res2);
    if let Err(e) = res2 {
        errs.push(e.to_string());
    }
    if errs.is_empty() {
        Ok(())
    } else {
        Err(Error::Msg(errs.join("\n")))
    }
}

fn flatpak_clean_unused(runner: &dyn Runner, out: &mut dyn Write) -> Result<()> {
    if runner.look_path("flatpak").is_err() {
        let _ = writeln!(out, "flatpak not found; skipped");
        return Ok(());
    }
    let res = runner.run(&CommandSpec::new(
        "flatpak",
        ["uninstall", "--unused", "-y"],
    ));
    write_captured(out, &res);
    res.map(|_| ())
        .map_err(|e| Error::Msg(format!("flatpak uninstall --unused: {e}")))?;
    Ok(())
}

fn find_sqlite_candidates(home: &Path) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    // Firefox
    let ff = home.join(".mozilla/firefox");
    if let Ok(entries) = std::fs::read_dir(&ff) {
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir()
                && let Ok(sub) = std::fs::read_dir(&p)
            {
                for f in sub.flatten() {
                    let fp = f.path();
                    if fp.extension().and_then(|s| s.to_str()) == Some("sqlite") {
                        candidates.push(fp);
                    }
                }
            }
        }
    }
    // Google Chrome
    let chrome_default = home.join(".config/google-chrome/Default");
    if chrome_default.is_dir() {
        for name in &["History", "Favicons", "Cookies", "Web Data"] {
            let p = chrome_default.join(name);
            if p.is_file() {
                candidates.push(p);
            }
        }
        if let Ok(sub) = std::fs::read_dir(&chrome_default) {
            for f in sub.flatten() {
                let fp = f.path();
                if fp.extension().and_then(|s| s.to_str()) == Some("sqlite") {
                    candidates.push(fp);
                }
            }
        }
    }
    // Chromium
    let chromium_default = home.join(".config/chromium/Default");
    if chromium_default.is_dir() {
        for name in &["History", "Favicons", "Cookies", "Web Data"] {
            let p = chromium_default.join(name);
            if p.is_file() {
                candidates.push(p);
            }
        }
        if let Ok(sub) = std::fs::read_dir(&chromium_default) {
            for f in sub.flatten() {
                let fp = f.path();
                if fp.extension().and_then(|s| s.to_str()) == Some("sqlite") {
                    candidates.push(fp);
                }
            }
        }
    }
    // Thunderbird
    let tb = home.join(".thunderbird");
    if let Ok(entries) = std::fs::read_dir(&tb) {
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir()
                && let Ok(sub) = std::fs::read_dir(&p)
            {
                for f in sub.flatten() {
                    let fp = f.path();
                    if fp.extension().and_then(|s| s.to_str()) == Some("sqlite") {
                        candidates.push(fp);
                    }
                }
            }
        }
    }
    candidates.sort();
    candidates.dedup();
    candidates
}

fn is_process_running(name: &str) -> bool {
    if let Ok(entries) = std::fs::read_dir("/proc") {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            if let Ok(comm) = std::fs::read_to_string(path.join("comm"))
                && comm.trim() == name
            {
                return true;
            }
        }
    }
    false
}

fn sqlite_vacuum(runner: &dyn Runner, home: &Path, out: &mut dyn Write) -> Result<()> {
    if runner.look_path("sqlite3").is_err() {
        let _ = writeln!(out, "sqlite3 not found in PATH; skipped");
        return Ok(());
    }
    let candidates = find_sqlite_candidates(home);
    if candidates.is_empty() {
        let _ = writeln!(out, "no application sqlite databases found");
        return Ok(());
    }
    let firefox_running = is_process_running("firefox");
    let chrome_running = is_process_running("chrome")
        || is_process_running("google-chrome")
        || is_process_running("chromium");
    let thunderbird_running = is_process_running("thunderbird");

    let mut vacuumed = 0;
    for db in candidates {
        let db_str = db.to_string_lossy();
        if firefox_running && db_str.contains(".mozilla") {
            let _ = writeln!(
                out,
                "sqlite: skipping {} (firefox is running)",
                db.display()
            );
            continue;
        }
        if chrome_running && (db_str.contains("google-chrome") || db_str.contains("chromium")) {
            let _ = writeln!(out, "sqlite: skipping {} (chrome is running)", db.display());
            continue;
        }
        if thunderbird_running && db_str.contains(".thunderbird") {
            let _ = writeln!(
                out,
                "sqlite: skipping {} (thunderbird is running)",
                db.display()
            );
            continue;
        }
        let wal = db.with_extension("sqlite-wal");
        let journal = db.with_extension("sqlite-journal");
        if wal.exists() || journal.exists() {
            let _ = writeln!(
                out,
                "sqlite: skipping {} (active lock/journal)",
                db.display()
            );
            continue;
        }
        let res = runner.run(&CommandSpec::new("sqlite3", [db_str.as_ref(), "VACUUM;"]));
        if let Err(e) = res {
            let _ = writeln!(out, "sqlite: vacuum warning for {}: {}", db.display(), e);
        } else {
            vacuumed += 1;
        }
    }
    let _ = writeln!(out, "sqlite: vacuumed {vacuumed} databases");
    Ok(())
}

/// Subtrees under `~/.local/state` that hold live user state rather than
/// disposable transient data (undo/shada history, audio defaults, playback
/// bookmarks). Deleting stale files inside them loses recoverable user work,
/// so the stale-state step never touches them.
const LIVE_STATE_DIRS: &[&str] = &["nvim", "wireplumber", "mpv"];

fn state_clean(
    runner: &dyn Runner,
    config_home: &Path,
    state_home: &Path,
    trash_deps: &trash::TrashDeps,
    out: &mut dyn Write,
) -> Result<()> {
    if !state_home.is_dir() {
        let _ = writeln!(out, "state: state directory not found");
        return Ok(());
    }
    // Fail closed: the state root must be an absolute, non-symlinked,
    // non-protected directory before anything under it is deleted.
    crate::paths::validate_cleanup_root(state_home)?;
    let now = std::time::SystemTime::now();
    let thirty_days = std::time::Duration::from_secs(30 * 86400);
    let histfile = std::env::var_os("HISTFILE").map(PathBuf::from);
    let mut cleaned = 0;
    let mut errors: Vec<Error> = Vec::new();
    clean_stale_recursive(
        runner,
        config_home,
        state_home,
        state_home,
        now,
        thirty_days,
        histfile.as_deref(),
        trash_deps,
        &mut cleaned,
        &mut errors,
    )?;
    let _ = writeln!(
        out,
        "state: cleaned {cleaned} stale files older than 30 days"
    );
    if errors.is_empty() {
        Ok(())
    } else {
        Err(Error::Msg(
            errors
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("\n"),
        ))
    }
}

#[allow(clippy::too_many_arguments)]
fn clean_stale_recursive(
    runner: &dyn Runner,
    config_home: &Path,
    state_home: &Path,
    dir: &Path,
    now: std::time::SystemTime,
    max_age: std::time::Duration,
    histfile: Option<&Path>,
    trash_deps: &trash::TrashDeps,
    cleaned: &mut usize,
    errors: &mut Vec<Error>,
) -> Result<()> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Ok(());
    };
    for entry in entries.flatten() {
        let p = entry.path();
        if is_live_state(state_home, &p, histfile) {
            continue;
        }
        if let Ok(meta) = p.symlink_metadata() {
            if meta.is_dir() && !meta.file_type().is_symlink() {
                clean_stale_recursive(
                    runner,
                    config_home,
                    state_home,
                    &p,
                    now,
                    max_age,
                    histfile,
                    trash_deps,
                    cleaned,
                    errors,
                )?;
            } else if meta.is_file()
                && let Ok(mtime) = meta.modified()
                && let Ok(age) = now.duration_since(mtime)
                && age >= max_age
            {
                if let Err(e) = crate::paths::validate_cleanup_candidate(state_home, &p) {
                    errors.push(e);
                    continue;
                }
                if let Err(e) = trash::safe_delete_with(runner, config_home, trash_deps, &p, false)
                {
                    errors.push(e);
                } else {
                    *cleaned += 1;
                }
            }
        }
    }
    Ok(())
}

/// True when `p` is live user state the stale-state step must never delete:
/// anything inside a [`LIVE_STATE_DIRS`] subtree, a known shell/live-state
/// file name, or the file `$HISTFILE` points at.
///
/// `HISTFILE` is usually a shell variable (bash/zsh do not export it), so the
/// env var alone is unreliable — the name-based guard is the real protection.
fn is_live_state(state_home: &Path, p: &Path, histfile: Option<&Path>) -> bool {
    let name = p.file_name().and_then(|n| n.to_str()).unwrap_or_default();
    if name == "history" || name == "lesshst" || name.ends_with("_history") {
        return true;
    }
    if let Some(hf) = histfile {
        // Compare the raw path and the base name so an unexpanded or
        // differently-rooted HISTFILE still protects the same file.
        if hf == p || hf.file_name() == p.file_name() {
            return true;
        }
    }
    if let Ok(rel) = p.strip_prefix(state_home)
        && let Some(std::path::Component::Normal(first)) = rel.components().next()
    {
        return LIVE_STATE_DIRS
            .iter()
            .any(|d| first == std::ffi::OsStr::new(d));
    }
    false
}

fn autostart_audit(runner: &dyn Runner, config_home: &Path, out: &mut dyn Write) -> Result<()> {
    let autostart_dir = config_home.join("autostart");
    if !autostart_dir.is_dir() {
        let _ = writeln!(out, "autostart: no autostart directory found");
        return Ok(());
    }
    let mut broken = Vec::new();
    let entries = std::fs::read_dir(&autostart_dir)
        .map(|rd| rd.flatten().map(|e| e.path()).collect::<Vec<_>>())
        .unwrap_or_default();
    for path in entries {
        if path.extension().and_then(|s| s.to_str()) != Some("desktop") {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        for line in content.lines() {
            let trimmed = line.trim();
            if let Some(exec) = trimmed.strip_prefix("Exec=") {
                let bin = exec.split_whitespace().next().unwrap_or_default();
                let bin = bin.trim_matches('"').trim_matches('\'');
                if bin.is_empty() {
                    continue;
                }
                let exists = if bin.starts_with('/') {
                    Path::new(bin).exists()
                } else {
                    runner.look_path(bin).is_ok()
                };
                if !exists {
                    broken.push(format!("{}: binary '{}' not found", path.display(), bin));
                }
                break;
            }
        }
    }
    if broken.is_empty() {
        let _ = writeln!(out, "autostart: all autostart desktop entries valid");
    } else {
        let _ = writeln!(out, "autostart: found {} broken entries:", broken.len());
        for b in broken {
            let _ = writeln!(out, "  - {}", b);
        }
    }
    Ok(())
}

fn services_audit(config_home: &Path, out: &mut dyn Write) -> Result<()> {
    let mut broken = Vec::new();
    let dirs = [
        PathBuf::from("/etc/systemd/system"),
        config_home.join("systemd/user"),
    ];
    for dir in &dirs {
        if !dir.is_dir() {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if let Ok(meta) = path.symlink_metadata()
                && meta.file_type().is_symlink()
                && std::fs::metadata(&path).is_err()
            {
                broken.push(path.display().to_string());
            }
        }
    }
    if broken.is_empty() {
        let _ = writeln!(out, "services: no broken unit symlinks found");
    } else {
        let _ = writeln!(
            out,
            "services: found {} broken unit symlinks:",
            broken.len()
        );
        for b in broken {
            let _ = writeln!(out, "  - {}", b);
        }
    }
    Ok(())
}

fn permissions_audit(home: &Path, out: &mut dyn Write) -> Result<()> {
    if !home.is_dir() {
        return Ok(());
    }
    let mut root_owned = Vec::new();
    audit_perms_dir(home, 1, 3, &mut root_owned);
    if root_owned.is_empty() {
        let _ = writeln!(out, "permissions: no root-owned files found in $HOME");
    } else {
        let _ = writeln!(
            out,
            "permissions: found {} root-owned files in $HOME:",
            root_owned.len()
        );
        for r in root_owned.iter().take(10) {
            let _ = writeln!(out, "  - {}", r);
        }
        if root_owned.len() > 10 {
            let _ = writeln!(out, "  ... and {} more", root_owned.len() - 10);
        }
    }
    Ok(())
}

fn audit_perms_dir(dir: &Path, depth: usize, max_depth: usize, root_owned: &mut Vec<String>) {
    if depth > max_depth {
        return;
    }
    use std::os::unix::fs::MetadataExt;
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if let Ok(meta) = path.symlink_metadata() {
            if meta.uid() == 0 {
                root_owned.push(path.display().to_string());
            }
            if meta.is_dir() && !meta.file_type().is_symlink() {
                let name = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or_default();
                if name == ".git"
                    || name == "node_modules"
                    || name == "target"
                    || name == ".cargo"
                    || name == ".rustup"
                {
                    continue;
                }
                audit_perms_dir(&path, depth + 1, max_depth, root_owned);
            }
        }
    }
}

fn disk_health_audit(runner: &dyn Runner, out: &mut dyn Write) -> Result<()> {
    let mut probed = false;
    if runner.look_path("smartctl").is_ok() {
        let res = runner.run(&CommandSpec::new("smartctl", ["--scan"]));
        if let Ok(output) = res
            && !output.stdout.is_empty()
        {
            let _ = writeln!(
                out,
                "SMART devices detected:\n{}",
                output.stdout_lossy().trim()
            );
            probed = true;
        }
    }
    if !probed && runner.look_path("journalctl").is_ok() {
        let res = runner.run(&CommandSpec::new(
            "journalctl",
            ["-k", "--priority=err", "-n", "10", "--no-pager"],
        ));
        if let Ok(output) = res {
            let text = output.stdout_lossy();
            let fs_errors: Vec<&str> = text
                .lines()
                .filter(|l| {
                    l.contains("EXT4-fs")
                        || l.contains("BTRFS")
                        || l.contains("XFS")
                        || l.contains("I/O error")
                })
                .collect();
            if !fs_errors.is_empty() {
                let _ = writeln!(
                    out,
                    "Filesystem alerts in kernel log:\n{}",
                    fs_errors.join("\n")
                );
                probed = true;
            }
        }
    }
    if !probed {
        let _ = writeln!(out, "disk-health: no hardware/filesystem errors detected");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    //! Port of `internal/optimize/step_test.go`.
    //!
    //! Two Go tests guard Bubbletea internals that do not exist in this port
    //! and are intentionally not carried over:
    //! - `TestOptimizeStepRunsWithTerminalRelease` asserts an interactive
    //!   step dispatches via `tea.Exec` (so bubbletea releases the terminal
    //!   before sudo reads its password). There is no raw-mode input reader
    //!   here, so nothing races sudo's prompt — the bug class cannot occur.
    //! - `TestOptimizeModelStopsAfterActiveStepAndSkipsRemaining` drives the
    //!   `ctrl+c` `stopRequested` path through `tea.KeyMsg`. The sequential
    //!   port has no key pump (and Go's non-TTY `WithInput(nil)` path can't
    //!   receive keys either), so it is deferred to the TUI phase.
    use super::*;
    use crate::runner::FakeRunner;
    use std::fs;
    use std::sync::Mutex;

    fn tempdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "mu-test-{}-{}-{}",
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

    fn write_config(root: &Path, body: &str) {
        let dir = root.join("mu");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("config.toml"), body).unwrap();
    }

    /// Deps wired to a temp `config_home`/`data_home` with the oplog disabled
    /// (no env mutation, no shared log-file state between parallel tests).
    fn test_deps(root: &Path, runner: Arc<FakeRunner>) -> Deps {
        Deps {
            runner,
            autoremove: Arc::new(|_| Ok(())),
            confirm: Arc::new(|_| panic!("confirm must not be reached")),
            config_home: root.to_path_buf(),
            data_home: root.join("data"),
            home: root.to_path_buf(),
            state_home: root.join("state"),
            trash_deps: Rc::new(trash::TrashDeps::real()),
            no_oplog: true,
        }
    }

    fn spec(argv: &[&str]) -> CommandSpec {
        CommandSpec::new(argv[0], &argv[1..])
    }

    // Port of TestStepIDs.
    #[test]
    fn step_ids_match_go_order() {
        assert_eq!(
            step_ids(),
            vec![
                "apt",
                "journal",
                "caches",
                "dns",
                "packages",
                "systemd",
                "flatpak",
                "sqlite",
                "state",
                "autostart",
                "services",
                "permissions",
                "disk-health",
            ]
        );
    }

    // Port of TestOptimizeModelRecordsFailedAndSuccessfulSteps — the model's
    // Update/stepErrors behavior is the sequential executor here.
    #[test]
    fn executor_records_failed_and_successful_steps() {
        let steps = vec![
            Step::new("bad", "bad", "bad", Box::new(|_| msg("blocked"))),
            Step::new("good", "good", "good", Box::new(|_| Ok(()))),
        ];
        let mut out = Vec::new();
        let results = execute_steps(&steps, &[], &mut out);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].status, StepStatus::Failed);
        assert_eq!(results[1].status, StepStatus::Success);
        let text = String::from_utf8_lossy(&out);
        assert!(
            text.contains("failed: bad: blocked"),
            "failed line missing: {text:?}"
        );
        let err = step_errors(&results).expect_err("expected aggregate error");
        assert!(err.to_string().contains("blocked"), "got {err}");
        // Go wraps each failure as "optimize step {id}: {err}".
        assert_eq!(err.to_string(), "optimize step bad: blocked");
    }

    // Port of TestOptimizeModelRecordsSkippedStep — a skipped step never runs.
    #[test]
    fn executor_records_skipped_step() {
        let steps = vec![Step::new(
            "apt",
            "apt",
            "apt",
            Box::new(|_| panic!("skipped step must not run")),
        )];
        let mut out = Vec::new();
        let results = execute_steps(&steps, &["apt".to_string()], &mut out);
        assert_eq!(results[0].status, StepStatus::Skipped);
    }

    // Port of TestUpdateCachesAggregatesFailuresAndContinues.
    #[test]
    fn update_caches_aggregates_failures_and_continues() {
        let runner = Arc::new(FakeRunner::new());
        runner.set_handler(|spec| {
            if spec.program == "sudo" {
                Err(RunError::Spawn(std::io::Error::other("mime failed")))
            } else {
                Ok(Output::default())
            }
        });
        let mut out = Vec::new();
        let err = update_caches(runner.as_ref(), &mut out).expect_err("expected cache error");
        assert!(err.to_string().contains("mime failed"), "got {err}");
        let want = [
            spec(&["sudo", "update-mime-database", "/usr/share/mime"]),
            spec(&["fc-cache", "-f"]),
        ];
        let calls = runner.invocations();
        assert_eq!(calls, want, "independent step did not continue");
    }

    // Port of TestOptimizeRejectsUnknownSkip.
    #[test]
    fn run_rejects_unknown_skip() {
        let root = tempdir("opt-unknown-skip");
        let deps = test_deps(&root, Arc::new(FakeRunner::new()));
        let opts = Options {
            dry_run: true,
            skip: vec!["unknown".to_string()],
            ..Options::default()
        };
        let mut out = Vec::new();
        let err = run_with(&opts, &deps, &mut out).expect_err("expected unknown skip error");
        assert!(
            err.to_string().contains("unknown optimize skip ID"),
            "got {err}"
        );
        fs::remove_dir_all(&root).ok();
    }

    // Port of TestRunStepFailsClosedOnMalformedConfig.
    #[test]
    fn run_step_fails_closed_on_malformed_config() {
        let root = tempdir("opt-malformed");
        write_config(&root, "broken = [");
        let deps = test_deps(&root, Arc::new(FakeRunner::new()));
        let mut out = Vec::new();
        let err = run_step_with("caches", &Options::default(), &deps, &mut out)
            .expect_err("expected fail-closed config error");
        assert!(
            err.to_string().contains("invalid mu configuration"),
            "got {err}"
        );
        fs::remove_dir_all(&root).ok();
    }

    // Port of TestRunAllSkippedCompletesAndRecordsSkippedStates.
    #[test]
    fn run_all_skipped_completes() {
        let root = tempdir("opt-all-skip");
        let runner = Arc::new(FakeRunner::new());
        let deps = test_deps(&root, runner.clone());
        let opts = Options {
            auto_yes: true,
            skip: step_ids().into_iter().map(String::from).collect(),
            ..Options::default()
        };
        let mut out = Vec::new();
        let summary = run_with(&opts, &deps, &mut out).expect("all-skipped run");
        assert_eq!(
            summary,
            "Optimization complete — 0 succeeded, 0 failed, 13 skipped"
        );
        assert!(
            runner.invocations().is_empty(),
            "skipped steps must not run: {:?}",
            runner.invocations()
        );
        fs::remove_dir_all(&root).ok();
    }

    // Port of TestRunReturnsNonzeroAfterIndependentStepFailure.
    #[test]
    fn run_returns_error_after_independent_step_failure() {
        let root = tempdir("opt-stepfail");
        let runner = Arc::new(FakeRunner::new());
        runner.set_handler(|_| {
            Err(RunError::Spawn(std::io::Error::other(
                "cache refresh failed",
            )))
        });
        let deps = test_deps(&root, runner.clone());
        let mut skip_ids = step_ids();
        skip_ids.retain(|&id| id != "caches");
        let opts = Options {
            auto_yes: true,
            skip: skip_ids.into_iter().map(String::from).collect(),
            ..Options::default()
        };
        let mut out = Vec::new();
        let err = run_with(&opts, &deps, &mut out).expect_err("expected optimize failure");
        assert!(
            err.to_string().contains("cache refresh failed"),
            "got {err}"
        );
        // Go's error is prefixed "optimize step {id}: " and joins both
        // subcommand failures; the run continues past the first one.
        assert_eq!(
            runner.invocations().len(),
            2,
            "caches step should run both commands"
        );
        let text = String::from_utf8_lossy(&out);
        assert!(
            text.contains("failed: caches:"),
            "missing failed line: {text:?}"
        );
        fs::remove_dir_all(&root).ok();
    }

    // Port of TestAptAndJournalStepsUseInjectedRunners.
    #[test]
    fn apt_and_journal_steps_use_injected_runners() {
        let runner = Arc::new(FakeRunner::new());
        runner.set_handler(|_| {
            Ok(Output {
                stdout: b"ok\n".to_vec(),
                ..Output::default()
            })
        });
        let calls = Mutex::new(Vec::new());
        let autoremove = |_: bool| {
            calls.lock().unwrap().push(vec!["autoremove".to_string()]);
            Ok(())
        };
        let mut out = Vec::new();
        apt_autoremove(runner.as_ref(), &autoremove, &mut out).expect("apt step");
        journal_vacuum(runner.as_ref(), &mut out).expect("journal step");

        let want = [
            spec(&["sudo", "apt-get", "update"]),
            spec(&["sudo", "journalctl", "--vacuum-size=500M"]),
        ];
        assert_eq!(runner.invocations().as_slice(), want.as_slice());
        assert_eq!(
            calls.lock().unwrap().as_slice(),
            &[vec!["autoremove".to_string()]]
        );
        // Go's step writes captured stdout/stderr into out.
        assert_eq!(out, b"ok\nok\n");
    }

    // Port of TestRunStep_unknown.
    #[test]
    fn run_step_unknown() {
        let root = tempdir("opt-step-unknown");
        let deps = test_deps(&root, Arc::new(FakeRunner::new()));
        let mut out = Vec::new();
        let err = run_step_with("nope", &Options::default(), &deps, &mut out)
            .expect_err("expected error for unknown step");
        assert!(
            err.to_string().contains("unknown optimize step"),
            "got {err}"
        );
        fs::remove_dir_all(&root).ok();
    }

    // Port of TestRunStep_skipsConfiguredStep.
    #[test]
    fn run_step_skips_configured_step() {
        let root = tempdir("opt-cfg-skip");
        write_config(&root, "[optimize_skip]\nsteps = [\"apt\"]\n");
        let runner = Arc::new(FakeRunner::new());
        let deps = test_deps(&root, runner.clone());
        let mut out = Vec::new();
        let skipped = run_step_with("apt", &Options::default(), &deps, &mut out).expect("RunStep");
        assert!(skipped, "expected configured apt step to be skipped");
        assert!(
            runner.invocations().is_empty(),
            "skipped step must not spawn commands"
        );
        fs::remove_dir_all(&root).ok();
    }

    // `Run`'s decline path: `!AutoYes && !ui.Confirm(...)` → "Aborted.".
    #[test]
    fn run_aborts_when_confirmation_declined() {
        let root = tempdir("opt-abort");
        let runner = Arc::new(FakeRunner::new());
        let mut deps = test_deps(&root, runner.clone());
        deps.confirm = Arc::new(|_| false);
        let opts = Options::default(); // auto_yes = false
        let mut out = Vec::new();
        let summary = run_with(&opts, &deps, &mut out).expect("aborted run");
        assert_eq!(summary, "Aborted.");
        assert!(
            runner.invocations().is_empty(),
            "declined run must not execute steps"
        );
        let text = String::from_utf8_lossy(&out);
        assert!(text.contains("  Aborted.\n"), "got {text:?}");
        fs::remove_dir_all(&root).ok();
    }

    // `--dry-run` prints the plan and returns before confirm/execute,
    // matching tests/golden/optimize-dry-run.txt.
    #[test]
    fn run_dry_run_prints_plan_and_executes_nothing() {
        let root = tempdir("opt-dry");
        let runner = Arc::new(FakeRunner::new());
        let deps = test_deps(&root, runner.clone());
        let opts = Options {
            dry_run: true,
            skip: vec!["journal".to_string()],
            ..Options::default()
        };
        let mut out = Vec::new();
        let summary = run_with(&opts, &deps, &mut out).expect("dry run");
        assert_eq!(summary, "Dry run — nothing executed.");
        assert_eq!(
            String::from_utf8_lossy(&out),
            "\n  Optimize plan\n  [dry-run] sudo apt-get update && sudo apt-get autoremove --purge -y\n  [skip]  journal\n  [dry-run] sudo update-mime-database /usr/share/mime && fc-cache -f\n  [dry-run] sudo resolvectl flush-caches\n  [dry-run] sudo dpkg --configure -a && sudo apt-get install -f -y\n  [dry-run] sudo systemctl reset-failed && systemctl --user reset-failed\n  [dry-run] flatpak uninstall --unused -y\n  [dry-run] sqlite3 <db_path> \"VACUUM;\"\n  [dry-run] find ~/.local/state -type f -mtime +30 -delete\n  [dry-run] audit ~/.config/autostart/*.desktop for missing Exec binaries\n  [dry-run] audit broken symlinks in /etc/systemd/system and ~/.config/systemd/user\n  [dry-run] audit files in $HOME owned by root\n  [dry-run] smartctl --scan || journalctl -k --grep=EXT4-fs\n  Dry run — nothing executed.\n"
        );
        assert!(runner.invocations().is_empty());
        fs::remove_dir_all(&root).ok();
    }

    fn trash_deps_at(data_home: PathBuf) -> trash::TrashDeps {
        trash::TrashDeps {
            data_home: Box::new(move || {
                if !data_home.exists() {
                    std::fs::create_dir_all(&data_home)?;
                }
                Ok(data_home.clone())
            }),
            ..trash::TrashDeps::real()
        }
    }

    fn set_mtime_secs_ago(p: &Path, secs: u64) {
        let f = fs::File::options().write(true).open(p).unwrap();
        f.set_times(
            fs::FileTimes::new()
                .set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(secs)),
        )
        .unwrap();
    }

    // Finding 4 (review p3 f4): the state step trashes stale files through
    // SafeDelete but never live user state, and surfaces trash failures.
    #[test]
    fn state_clean_trashes_stale_files_only() {
        let root = tempdir("opt-state");
        let config_home = root.join("config");
        let state = root.join("state");
        fs::create_dir_all(&config_home).unwrap();
        fs::create_dir_all(state.join("nvim")).unwrap();

        let stale = state.join("cache.log");
        fs::write(&stale, "stale state").unwrap();
        set_mtime_secs_ago(&stale, 40 * 86400);
        let fresh = state.join("recent.log");
        fs::write(&fresh, "fresh state").unwrap();
        let live = state.join("nvim").join("undo.log");
        fs::write(&live, "nvim undo history").unwrap();
        set_mtime_secs_ago(&live, 40 * 86400);

        let runner = FakeRunner::new();
        let trash_deps = trash_deps_at(root.join("data-home"));
        let mut out = Vec::new();
        state_clean(&runner, &config_home, &state, &trash_deps, &mut out).expect("state clean");

        assert!(!stale.exists(), "stale file must be trashed");
        assert!(fresh.exists(), "fresh file must be kept");
        assert!(live.exists(), "live nvim state must be kept");
        let text = String::from_utf8_lossy(&out);
        assert!(
            text.contains("state: cleaned 1 stale files"),
            "got {text:?}"
        );
        fs::remove_dir_all(&root).ok();
    }

    // Review r1 f1: shell history files must be protected by name because
    // HISTFILE is a shell variable (not exported) in practice.
    #[test]
    fn state_clean_skips_history_files_by_name() {
        let root = tempdir("opt-state-hist");
        let config_home = root.join("config");
        let state = root.join("state");
        fs::create_dir_all(&config_home).unwrap();
        fs::create_dir_all(state.join("zsh")).unwrap();

        for name in ["zsh/history", "zsh_history", "lesshst"] {
            let p = state.join(name);
            fs::write(&p, "history data").unwrap();
            set_mtime_secs_ago(&p, 40 * 86400);
        }
        let stale = state.join("cache.log");
        fs::write(&stale, "stale").unwrap();
        set_mtime_secs_ago(&stale, 40 * 86400);

        let runner = FakeRunner::new();
        let trash_deps = trash_deps_at(root.join("data-home"));
        let mut out = Vec::new();
        state_clean(&runner, &config_home, &state, &trash_deps, &mut out).expect("state clean");

        for name in ["zsh/history", "zsh_history", "lesshst"] {
            assert!(state.join(name).exists(), "{name} must be kept");
        }
        assert!(!stale.exists(), "cache.log must be trashed");
        fs::remove_dir_all(&root).ok();
    }

    // Review r1 f1: the HISTFILE path (when present) is honoured, and a
    // non-history file with no HISTFILE match is eligible.
    #[test]
    fn is_live_state_honors_histfile_path() {
        let state = Path::new("/h/.local/state");
        let hist = Path::new("/h/.local/state/zsh/history");
        assert!(is_live_state(state, hist, Some(hist)), "HISTFILE path");
        // A differently-rooted HISTFILE still protects the same file name.
        assert!(
            is_live_state(
                state,
                Path::new("/h/.local/state/.myhist"),
                Some(Path::new("/x/.myhist"))
            ),
            "same base name as HISTFILE"
        );
        assert!(
            !is_live_state(state, Path::new("/h/.local/state/other"), None),
            "unrelated file is eligible"
        );
    }

    // Finding 4 (review p3 f4): a symlinked state root is rejected (SafeDelete
    // cleanup-root invariant).
    #[test]
    fn state_clean_rejects_symlinked_root() {
        let root = tempdir("opt-state-symlink");
        let config_home = root.join("config");
        let real = root.join("real-state");
        fs::create_dir_all(&config_home).unwrap();
        fs::create_dir_all(&real).unwrap();
        let link = root.join("link-state");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let runner = FakeRunner::new();
        let trash_deps = trash_deps_at(root.join("data-home"));
        let mut out = Vec::new();
        let err = state_clean(&runner, &config_home, &link, &trash_deps, &mut out)
            .expect_err("symlinked state root must be rejected");
        assert!(err.to_string().contains("symlink"), "got {err}");
        fs::remove_dir_all(&root).ok();
    }

    // Finding 4 (review p3 f4): a SafeDelete failure is reported, not swallowed.
    #[test]
    fn state_clean_surfaces_trash_errors() {
        let root = tempdir("opt-state-err");
        let config_home = root.join("config");
        let state = root.join("state");
        fs::create_dir_all(&config_home).unwrap();
        fs::create_dir_all(&state).unwrap();
        let stale = state.join("cache.log");
        fs::write(&stale, "stale state").unwrap();
        set_mtime_secs_ago(&stale, 40 * 86400);

        let runner = FakeRunner::new();
        let bad_trash = trash::TrashDeps {
            data_home: Box::new(|| Err(Error::Msg("no trash available".to_string()))),
            ..trash::TrashDeps::real()
        };
        let mut out = Vec::new();
        let err = state_clean(&runner, &config_home, &state, &bad_trash, &mut out)
            .expect_err("trash failure must surface");
        assert!(err.to_string().contains("trash"), "got {err}");
        fs::remove_dir_all(&root).ok();
    }

    // Finding 6 (review p3 f6): root-level commands run under sudo.
    #[test]
    fn root_scoped_steps_use_sudo() {
        let runner = FakeRunner::new();
        let mut out = Vec::new();
        systemd_reset_failed(&runner, &mut out).expect("systemd step");
        let inv = runner.invocations();
        assert_eq!(inv[0].program.to_string_lossy(), "sudo");
        assert_eq!(
            inv[0].args,
            vec![
                std::ffi::OsString::from("systemctl"),
                std::ffi::OsString::from("reset-failed")
            ]
        );
        assert_eq!(inv[1].program.to_string_lossy(), "systemctl");
        assert_eq!(
            inv[1].args,
            vec![
                std::ffi::OsString::from("--user"),
                std::ffi::OsString::from("reset-failed")
            ]
        );

        let dns_runner = FakeRunner::new();
        dns_runner.set_look_path("resolvectl", "/usr/bin/resolvectl");
        let mut out = Vec::new();
        dns_flush(&dns_runner, &mut out).expect("dns step");
        let inv = dns_runner.invocations();
        assert_eq!(inv[0].program.to_string_lossy(), "sudo");
        assert_eq!(inv[0].args[0].to_string_lossy(), "resolvectl");
        assert_eq!(inv[0].args[1].to_string_lossy(), "flush-caches");
    }
}
