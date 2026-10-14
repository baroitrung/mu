//! Integration tests — drive the compiled `mu` binary end-to-end through
//! its non-TTY surface, the paths the independent review found had escaped
//! unit coverage (direct-command stubs, headless error parity).
//!
//! `CARGO_BIN_EXE_mu` resolves to the binary cargo just built; stdin is
//! closed (the Go oracle's `< /dev/null` shape — `script`-driven TTY
//! behavior is covered manually, not in CI).

use std::process::{Command, Stdio};

fn mu(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_mu"))
        .args(args)
        .stdin(Stdio::null())
        .output()
        .expect("failed to run mu")
}

fn stdout(o: &std::process::Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn stderr(o: &std::process::Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

/// Go headless `mu uninstall` → tea's `could not open a new TTY…` on
/// stderr, exit 1. The deferred-stub regression returned exit 2 instead.
#[test]
fn uninstall_headless_matches_go_error() {
    let o = mu(&["uninstall"]);
    assert_eq!(o.status.code(), Some(1), "output: {o:?}");
    assert_eq!(
        stderr(&o),
        "could not open a new TTY: open /dev/tty: no such device or address\n"
    );
}

/// `mu` with no args, headless → same tea startup error, exit 1.
#[test]
fn root_menu_headless_matches_go_error() {
    let o = mu(&[]);
    assert_eq!(o.status.code(), Some(1), "output: {o:?}");
    assert!(stderr(&o).contains("could not open a new TTY"), "{o:?}");
}

/// `mu status` piped → JSON snapshot, exit 0. Asserts the Go schema keys.
#[test]
fn status_piped_emits_json() {
    let o = mu(&["status"]);
    assert_eq!(o.status.code(), Some(0), "stderr: {}", stderr(&o));
    let v: serde_json::Value = serde_json::from_str(&stdout(&o)).expect("status JSON parse");
    for key in ["cpu_percent", "memory", "disks", "network", "health"] {
        assert!(v.get(key).is_some(), "missing {key} in {v}");
    }
}

/// `mu status --json` is identical to the piped form, exit 0.
#[test]
fn status_json_flag() {
    let o = mu(&["status", "--json"]);
    assert_eq!(o.status.code(), Some(0), "stderr: {}", stderr(&o));
    serde_json::from_str::<serde_json::Value>(&stdout(&o)).expect("status --json parse");
}

/// `mu audit` piped → human report on stdout (Go falls back to report
/// mode), exit code is the severity-driven 0/1/2 — never the old stub's 2+
/// or an arg error.
#[test]
fn audit_piped_falls_back_to_report() {
    let o = mu(&["audit"]);
    let code = o.status.code().unwrap_or(-1);
    assert!(
        (0..=2).contains(&code),
        "audit exit {code}, stderr: {}",
        stderr(&o)
    );
    let out = stdout(&o);
    assert!(out.contains("Audit report"), "missing report in {out:?}");
    assert!(
        out.contains("Run `mu audit` (interactive)"),
        "missing interactive hint in {out:?}"
    );
}

/// `mu audit --report` — same report, same exit range.
#[test]
fn audit_report_flag() {
    let o = mu(&["audit", "--report"]);
    let code = o.status.code().unwrap_or(-1);
    assert!((0..=2).contains(&code), "audit --report exit {code}");
    assert!(stdout(&o).contains("Audit report"));
}

/// `mu audit --json` → JSON report, exit 0/1/2.
#[test]
fn audit_json_flag() {
    let o = mu(&["audit", "--json"]);
    let code = o.status.code().unwrap_or(-1);
    assert!((0..=2).contains(&code), "audit --json exit {code}");
    let v: serde_json::Value = serde_json::from_str(&stdout(&o)).expect("audit JSON parse");
    assert!(
        v.get("health").is_some() && v.get("findings").is_some(),
        "{v}"
    );
}

/// `mu clean --dry-run` piped → scan results + DRY RUN notice + per-target
/// sizes, exit 0. On hosts where a scanner fails (missing snap/journalctl)
/// Go's runPlain exits 1 with joined scan errors — accept either, assert
/// the contract content when the scan is clean.
#[test]
fn clean_dry_run_piped() {
    let o = mu(&["clean", "--dry-run"]);
    let code = o.status.code().unwrap_or(-1);
    assert!(
        (0..=1).contains(&code),
        "exit {code}, stderr: {}",
        stderr(&o)
    );
    if code == 0 {
        let out = stdout(&o);
        assert!(out.contains("Potential space to free:"), "{out:?}");
        assert!(out.contains("DRY RUN"), "{out:?}");
    }
}

/// `mu clean` piped without --yes or --dry-run → prints the non-interactive
/// abort note, exit 0 (Go never runs the TUI on a pipe).
#[test]
fn clean_piped_aborts_without_yes() {
    let o = mu(&["clean"]);
    assert_eq!(o.status.code(), Some(0), "stderr: {}", stderr(&o));
    let out = stdout(&o);
    assert!(
        out.contains("Non-interactive: pass --yes") && out.contains("Aborted."),
        "{out:?}"
    );
}

/// `mu optimize --dry-run` piped → step listing, exit 0.
#[test]
fn optimize_dry_run_piped() {
    let o = mu(&["optimize", "--dry-run"]);
    assert_eq!(o.status.code(), Some(0), "stderr: {}", stderr(&o));
}

/// `--version` → `mu version <v>` — the build.rs git-describe injection,
/// same contract as Go's -X ldflag. Value is `v<tag>-dirty`, a bare hash
/// (no reachable tag), or `dev` (no git) — only the prefix is contractual.
#[test]
fn version_prints_injected_tag() {
    let o = mu(&["--version"]);
    assert_eq!(o.status.code(), Some(0));
    assert!(stdout(&o).starts_with("mu version "), "{:?}", stdout(&o));
}

/// `--help` → usage block listing all subcommands, exit 0.
#[test]
fn help_lists_subcommands() {
    let o = mu(&["--help"]);
    assert_eq!(o.status.code(), Some(0));
    let out = stdout(&o);
    for cmd in [
        "analyze",
        "clean",
        "optimize",
        "audit",
        "status",
        "uninstall",
        "completion",
    ] {
        assert!(out.contains(cmd), "missing {cmd} in help");
    }
}

/// `mu analyze --json /tmp` -> valid JSON directory tree, exit 0.
#[test]
fn analyze_json_output() {
    let o = mu(&["analyze", "--json", "/tmp"]);
    assert_eq!(o.status.code(), Some(0));
    let out = stdout(&o);
    assert!(out.contains("\"path\""), "missing path in JSON: {out}");
    assert!(out.contains("\"size\""), "missing size in JSON: {out}");
    assert!(
        out.contains("\"largest_files\""),
        "missing largest_files in JSON: {out}"
    );
}

/// `mu analyze --dry-run` sentinel immutability test -> exit 0, sentinel untouched.
#[test]
fn analyze_dry_run_sentinel() {
    let test_dir = std::env::temp_dir().join(format!("mu-analyze-test-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&test_dir);
    let sentinel = test_dir.join("sentinel.dat");
    std::fs::write(&sentinel, b"precious data").unwrap();

    let o = mu(&["analyze", "--dry-run", test_dir.to_str().unwrap()]);
    assert_eq!(o.status.code(), Some(0));
    assert!(
        sentinel.exists(),
        "sentinel.dat must not be removed during --dry-run"
    );

    let _ = std::fs::remove_dir_all(&test_dir);
}

/// `mu analyze --help` -> renders usage and options matching golden fixture, exit 0.
#[test]
fn analyze_help_output() {
    let o = mu(&["analyze", "--help"]);
    assert_eq!(o.status.code(), Some(0));
    assert_eq!(stdout(&o), include_str!("golden/help-analyze.txt"));
}

/// `mu analyze` piped without flags -> exit 1 with standard headless TTY error.
#[test]
fn analyze_headless_matches_go_error() {
    let o = mu(&["analyze"]);
    assert_eq!(o.status.code(), Some(1));
    assert!(
        stderr(&o).contains("could not open a new TTY: open /dev/tty"),
        "unexpected stderr: {}",
        stderr(&o)
    );
}
