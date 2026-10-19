//! Audit subsystem — port of `internal/audit/`.
//!
//! `audit.go` (`Run`/`ValidateOptions`/`runReport`/`printHumanReport`/
//! `ExitError`) lives here; `scan.go` (`Collect`/`BuildFindings`/
//! `BuildReport`) is [`scan`], `findings.go` (the `Finding`/`Report` model,
//! sorting, exit codes, recommended commands) is [`findings`], `apply.go`
//! (the wizard's apply path) is [`apply`], and the phase machine + plain
//! views of `model.go` are [`model`]. The ratatui wizard driver is
//! [`crate::tui::audit::run_audit_wizard`].
//!
//! Go wires dependencies through package functions (`clean.AllTargets`,
//! `status.ReadCPU`, `optimize.RunStep`, `utils.LogOutcome`); Rust makes
//! them explicit: production goes through [`Deps::real`], and every `*_in`
//! entry point takes the bundle so tests inject tempdirs and
//! [`crate::runner::FakeRunner`] without mutating the environment.

pub(crate) mod apply;
mod findings;
pub(crate) mod model;
mod scan;

use std::io::{IsTerminal, Write};
use std::sync::Arc;

use crate::clean;
use crate::error::{Result, msg};
use crate::runner::ProcessRunner;
use crate::{optimize, size, status};

// Go's exported surface, flattened to `audit::X` the way the oracle package
// exposes it. The wizard-facing items (`apply`, `collect`, `max_severity`, …)
// are consumed by the deferred TUI phase — kept public for the contract.
#[allow(unused_imports)]
pub use apply::{ApplyResult, apply, count_apply_errors};
#[allow(unused_imports)]
pub use findings::{
    Finding, Report, Severity, exit_code_for_report, max_severity, parse_action,
    recommended_commands, sort_findings,
};
#[allow(unused_imports)]
pub use scan::{
    BYTES_1_GIB, BYTES_2_GIB, BYTES_500_MIB, Snapshot, TargetSize, build_findings, build_report,
    collect,
};

/// `Options` controls audit behavior — Go's `audit.Options`.
#[derive(Debug, Default, Clone)]
pub struct Options {
    pub report: bool,
    pub json: bool,
    pub dry_run: bool,
    pub debug: bool,
    /// Pre-select opt-in clean IDs (`--include`).
    pub include: Vec<String>,
}

/// `ExitError` preserves report exit codes without terminating library
/// callers — Go's struct kept for API parity; `run` returns the code
/// directly instead of boxing it as an error.
#[derive(Debug)]
pub struct ExitError {
    pub code: i32,
}

impl ExitError {
    /// Go `ExitCode()`.
    pub fn exit_code(&self) -> i32 {
        self.code
    }
}

impl std::fmt::Display for ExitError {
    /// Go `Error()`: `"audit findings require exit code %d"`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "audit findings require exit code {}", self.code)
    }
}

impl std::error::Error for ExitError {}

/// Dependency bundle replacing what Go reaches through sibling packages.
/// `clean` is `cleanRunner` + the XDG roots the clean targets close over;
/// `readers` are `status.{ReadCPU,ReadMemory,ReadDisk}`; `optimize` is
/// `optimizeRunner` + the `optimize_skip` policy config `RunStep` loads.
pub(crate) struct Deps {
    /// `internal/clean` package state (`cleanRunner`, XDG dirs, trash hooks).
    pub clean: clean::Deps,
    /// `internal/status` metric readers (cpu/memory/disk — network is part
    /// of the shared bundle but unused by audit, as in Go).
    pub readers: status::Readers,
    /// `internal/optimize` deps for `optimize.RunStep` inside apply.
    pub optimize: optimize::Deps,
}

impl Deps {
    /// Production wiring — every Go package var in its default state.
    pub(crate) fn real() -> Self {
        Self {
            clean: clean::Deps::real(Arc::new(ProcessRunner)),
            readers: status::Readers::real(),
            optimize: optimize::Deps::real(),
        }
    }
}

/// `Run` executes audit: report/json modes or the interactive wizard.
/// Returns the process exit code — Go's `ExitError` code for report modes
/// (0/1/2), 1 for validation/runtime/apply errors (Cobra prints and
/// exits 1).
pub fn run(opts: &Options) -> i32 {
    let mut opts = opts.clone();
    // Non-TTY without explicit wizard intent → report.
    if !opts.report && !opts.json && !std::io::stdout().is_terminal() {
        opts.report = true;
    }
    let mut deps = Deps::real();
    if let Err(e) = validate_options_in(&opts, &deps) {
        // Go: cobra Execute prints the bare error on stderr, exit 1.
        eprintln!("{e}");
        return 1;
    }

    if opts.json || opts.report {
        return run_report(&opts, &mut deps);
    }
    // `runWizard` — the interactive scan→select→confirm→apply→rescore
    // program. The apply transcript is printed after the alt-screen exits
    // (Go bled it into the frame); `CountApplyErrors` maps to the error
    // cobra prints bare + exit 1.
    match crate::tui::audit::run_audit_wizard(&opts) {
        Ok((results, transcript)) => {
            let stdout = std::io::stdout();
            let mut out = stdout.lock();
            let _ = out.write_all(&transcript);
            let _ = out.flush();
            let failed = count_apply_errors(&results);
            if failed > 0 {
                eprintln!("{failed} audit action(s) failed");
                return 1;
            }
            0
        }
        Err(e) => {
            eprintln!("{e}");
            1
        }
    }
}

/// `ValidateOptions` — flag conflict and include-ID validation.
pub fn validate_options(opts: &Options) -> Result<()> {
    validate_options_in(opts, &Deps::real())
}

/// Injectable `ValidateOptions` — `deps` supplies `clean.ResolveTargets`'s
/// inputs (Go reads the `cleanRunner` package var and XDG env).
pub(crate) fn validate_options_in(opts: &Options, deps: &Deps) -> Result<()> {
    if opts.report && opts.json {
        return msg("--report and --json are mutually exclusive");
    }
    if opts.dry_run && (opts.report || opts.json) {
        return msg("--dry-run is only valid for the interactive audit workflow");
    }
    if !opts.include.is_empty() {
        clean::resolve_targets_in(&deps.clean, &opts.include)?;
    }
    Ok(())
}

/// `runReport` — collect, build, print (JSON or human), return the
/// severity-driven exit code Go delivers via `ExitError`.
fn run_report(opts: &Options, deps: &mut Deps) -> i32 {
    let progress: Option<&dyn Fn(&str)> = if opts.report && !opts.json {
        eprintln!("\n🔍 Auditing system…");
        Some(&|msg: &str| eprintln!("  {msg}"))
    } else {
        None
    };

    let snap = scan::collect_in(deps, progress);
    let rep = build_report(&snap, &opts.include);

    if opts.json {
        // Go: json.NewEncoder + SetIndent("", "  ") — indented, HTML-escaped,
        // newline-terminated. serde_json::to_string_pretty is byte-identical
        // in layout; escape_html_json reapplies the Go escaping.
        let stdout = std::io::stdout();
        let mut out = stdout.lock();
        let written = serde_json::to_string_pretty(&rep)
            .map(|text| status::model::escape_html_json(&text))
            .map_err(|e| e.to_string())
            .and_then(|text| {
                out.write_all(text.as_bytes())
                    .and_then(|()| out.write_all(b"\n"))
                    .and_then(|()| out.flush())
                    .map_err(|e| e.to_string())
            });
        if let Err(e) = written {
            eprintln!("{e}");
            return 1;
        }
    } else {
        let stdout = std::io::stdout();
        let mut out = stdout.lock();
        print_human_report(&rep, &mut out);
    }

    // Go: ExitError{Code} — root.go reads ExitCode() without printing.
    ExitError {
        code: exit_code_for_report(&rep.findings),
    }
    .exit_code()
}

/// `printHumanReport` — the `--report` text, byte-for-byte Go's fmt calls.
/// Write errors are dropped exactly like Go's `fmt.Print*`.
pub(crate) fn print_human_report(rep: &Report, out: &mut dyn Write) {
    let _ = write!(out, "\n📋 Audit report\n\n");
    let _ = writeln!(out, "  Health score:     {}/100", rep.health);
    let _ = writeln!(out, "  Disk / free:      {:.0}%", rep.disk_free_pct_root);
    let _ = writeln!(
        out,
        "  Reclaimable:      {}",
        size::human_size(rep.reclaimable_bytes)
    );
    let _ = writeln!(out);
    if !rep.warnings.is_empty() || !rep.scan_errors.is_empty() {
        let _ = writeln!(out, "  Scan warnings:");
        for warning in rep.warnings.iter().chain(rep.scan_errors.iter()) {
            let _ = writeln!(out, "  • {warning}");
        }
        let _ = writeln!(out);
    }

    if rep.findings.is_empty() {
        let _ = writeln!(out, "  ✅ No issues found. System looks clean.");
        let _ = writeln!(out);
        return;
    }

    let _ = writeln!(out, "  Findings:");
    for f in &rep.findings {
        let badge = f.severity.as_str();
        let size_str = if f.bytes > 0 {
            format!("  {}", size::human_size(f.bytes))
        } else {
            String::new()
        };
        let sel = if f.selectable && f.default_selected {
            " [recommended]"
        } else if f.opt_in {
            " [opt-in]"
        } else if !f.selectable {
            " [guide]"
        } else {
            ""
        };
        let _ = writeln!(out, "  • {badge:<8} {}{}{}", f.title, size_str, sel);
        if !f.detail.is_empty() {
            let _ = writeln!(out, "           {}", f.detail);
        }
    }

    let _ = writeln!(out);
    let _ = writeln!(out, "  Recommended commands:");
    for c in &rep.recommended_commands {
        let _ = writeln!(out, "    {c}");
    }
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "  Run `mu audit` (interactive) to select and apply fixes."
    );
    let _ = writeln!(out);
}

#[cfg(test)]
pub(crate) mod test_support {
    //! Shared scaffolding for the ported audit tests — the Go suite's
    //! `t.Setenv(XDG_CONFIG_HOME, t.TempDir())` + `t.TempDir()` setup,
    //! expressed as injected [`Deps`] rooted at a tempdir (no env mutation).
    use super::*;
    use crate::runner::FakeRunner;
    use std::fs;
    use std::path::{Path, PathBuf};

    pub fn tempdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "mu-test-audit-{}-{}-{}",
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

    /// `Deps` for tests: `cleanRunner`/`optimizeRunner` become
    /// [`FakeRunner`]s, the `optimize_skip` policy config root and ops-log
    /// dir point at `root`, and the status readers are real-but-unused
    /// (apply/validate paths never sample metrics).
    pub fn deps_for_test(root: &Path) -> Deps {
        Deps {
            clean: clean::test_support::deps_for_test(root, Arc::new(FakeRunner::new())),
            readers: status::Readers::real(),
            optimize: optimize::Deps {
                runner: Arc::new(FakeRunner::new()),
                autoremove: Arc::new(|_| Ok(())),
                confirm: Arc::new(|_| panic!("confirm must not be reached")),
                config_home: root.to_path_buf(),
                data_home: root.join("data"),
                home: root.to_path_buf(),
                state_home: root.join("state"),
                trash_deps: std::rc::Rc::new(crate::trash::TrashDeps::real()),
                no_oplog: true,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    //! Port of `internal/audit/options_test.go` (minus the model test, which
    //! lives in `model.rs` next to the type it exercises).
    use super::test_support::*;
    use super::*;
    use std::fs;

    // TestValidateOptionsRejectsConflictingAndMeaninglessFlags
    #[test]
    fn validate_options_rejects_conflicting_and_meaningless_flags() {
        let root = tempdir("validate");
        let deps = deps_for_test(&root);
        let cases = [
            Options {
                report: true,
                json: true,
                ..Options::default()
            },
            Options {
                report: true,
                dry_run: true,
                ..Options::default()
            },
            Options {
                json: true,
                dry_run: true,
                ..Options::default()
            },
            Options {
                include: vec!["unknown".to_string()],
                ..Options::default()
            },
        ];
        for options in &cases {
            assert!(
                validate_options_in(options, &deps).is_err(),
                "expected options rejection: {options:?}"
            );
        }
        fs::remove_dir_all(&root).ok();
    }

    // TestExitErrorCarriesReportCode
    #[test]
    fn exit_error_carries_report_code() {
        let err = ExitError { code: 2 };
        assert_eq!(err.exit_code(), 2, "exit code mapping failed: {err}");
    }

    // TestReportJSONKeepsExistingFieldsAndAddsScanErrors
    #[test]
    fn report_json_keeps_existing_fields_and_adds_scan_errors() {
        let rep = Report {
            health: 80,
            disk_free_pct_root: 40.0,
            reclaimable_bytes: 12,
            scan_errors: vec!["cpu".to_string()],
            ..Report::default()
        };
        let text = serde_json::to_string(&rep).unwrap();
        for field in [
            "health",
            "disk_free_pct_root",
            "reclaimable_bytes",
            "findings",
            "recommended_commands",
            "scan_errors",
        ] {
            assert!(
                text.contains(&format!("\"{field}\"")),
                "missing JSON field {field:?} in {text}"
            );
        }
    }
}
