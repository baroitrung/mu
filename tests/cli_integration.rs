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

/// `mu status --json` emits extended fields `cpu_cores` and `disk_io`.
#[test]
fn status_json_extended_fields() {
    let o = mu(&["status", "--json"]);
    assert_eq!(o.status.code(), Some(0), "stderr: {}", stderr(&o));
    let v: serde_json::Value = serde_json::from_str(&stdout(&o)).expect("status --json parse");
    for key in [
        "cpu_percent",
        "memory",
        "disks",
        "network",
        "health",
        "cpu_cores",
        "disk_io",
    ] {
        assert!(v.get(key).is_some(), "missing {key} in {v}");
    }
}

/// `mu status --watch` streams valid NDJSON lines containing required metrics.
#[test]
fn status_watch_ndjson_streaming() {
    let mu_bin = env!("CARGO_BIN_EXE_mu");
    let mut child = Command::new(mu_bin)
        .args(["status", "--watch", "--interval", "100ms"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn mu status --watch");

    let stdout_pipe = child.stdout.take().expect("stdout pipe");
    let reader = std::io::BufReader::new(stdout_pipe);
    let mut lines = Vec::new();
    for line in std::io::BufRead::lines(reader).take(2) {
        let line = line.expect("read line from watch");
        let v: serde_json::Value = serde_json::from_str(&line).expect("parse NDJSON line");
        for key in ["cpu_percent", "memory", "disks", "network", "health"] {
            assert!(v.get(key).is_some(), "missing {key} in {line}");
        }
        lines.push(v);
    }
    assert_eq!(lines.len(), 2);
    let _ = child.kill();
    let _ = child.wait();
}

/// `mu status --help` renders usage matching golden fixture.
#[test]
fn status_help_output() {
    let o = mu(&["status", "--help"]);
    assert_eq!(o.status.code(), Some(0));
    assert_eq!(stdout(&o), include_str!("golden/help-status.txt"));
}

/// `mu status --interval 0s` and `infs` are rejected with exit code 1.
#[test]
fn status_interval_invalid_rejected() {
    for arg in ["0s", "0ms", "0", "infs", "1e300s", "1e-10s", "1e-12m"] {
        let o = mu(&["status", "--interval", arg]);
        assert_eq!(o.status.code(), Some(1), "for {arg}");
        assert!(
            stderr(&o).contains(&format!("invalid duration \"{arg}\"")),
            "for {arg}: {}",
            stderr(&o)
        );
    }
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
        "history",
        "installer",
        "purge",
        "remove",
        "sudo",
        "update",
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

/// `mu analyze --json` with non-UTF8 filename -> exit 0 and valid JSON report.
#[test]
fn analyze_json_non_utf8_filename() {
    use std::os::unix::ffi::OsStrExt;
    let test_dir = std::env::temp_dir().join(format!("mu-analyze-utf8-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&test_dir);
    let bad_file_name = std::ffi::OsStr::from_bytes(b"bad-\xff-file.dat");
    let bad_file = test_dir.join(bad_file_name);
    let _ = std::fs::write(&bad_file, b"content");

    let o = mu(&["analyze", "--json", test_dir.to_str().unwrap()]);
    assert_eq!(o.status.code(), Some(0));
    let out = stdout(&o);
    assert!(out.contains("\"path\""), "missing path in JSON: {out}");
    assert!(out.contains("bad-"), "missing file in JSON: {out}");

    let _ = std::fs::remove_file(&bad_file);
    let _ = std::fs::remove_dir_all(&test_dir);
}

/// `mu analyze --json` targeting a symlink to directory -> treats target as directory, exit 0.
#[test]
fn analyze_root_symlink_directory() {
    let test_dir = std::env::temp_dir().join(format!("mu-analyze-sym-{}", std::process::id()));
    let real_dir = test_dir.join("real");
    let _ = std::fs::create_dir_all(&real_dir);
    let file = real_dir.join("f.txt");
    std::fs::write(&file, b"hello").unwrap();

    let link_dir = test_dir.join("link");
    std::os::unix::fs::symlink(&real_dir, &link_dir).unwrap();

    let o = mu(&["analyze", "--json", link_dir.to_str().unwrap()]);
    assert_eq!(o.status.code(), Some(0));
    let out = stdout(&o);
    assert!(
        out.contains("\"is_dir\": true"),
        "root symlink should be is_dir: true: {out}"
    );
    assert!(
        out.contains("f.txt"),
        "should scan children of symlinked root: {out}"
    );

    let _ = std::fs::remove_dir_all(&test_dir);
}

/// `mu analyze` with nonexistent target path -> exit 1 with inspect error.
#[test]
fn analyze_nonexistent_target_exits_with_error() {
    let non_existent = format!("/tmp/mu-nonexistent-path-{}", std::process::id());
    let o = mu(&["analyze", "--dry-run", &non_existent]);
    assert_eq!(o.status.code(), Some(1));
    assert!(
        stderr(&o).contains("cannot inspect"),
        "expected 'cannot inspect' in stderr: {}",
        stderr(&o)
    );
}

/// `mu analyze --dry-run` on relative path -> resolves to absolute target path.
#[test]
fn analyze_relative_path_dry_run_reports_absolute_target() {
    let test_dir = std::env::temp_dir().join(format!("mu-analyze-rel-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&test_dir);

    let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_mu"));
    cmd.args(["analyze", "--dry-run", "."]);
    cmd.current_dir(&test_dir);
    let output = cmd.output().expect("failed to execute mu");
    assert_eq!(output.status.code(), Some(0));
    let out = String::from_utf8_lossy(&output.stdout);
    assert!(
        out.contains(&format!("Target: {}", test_dir.display())),
        "target should be absolute: {out}"
    );

    let _ = std::fs::remove_dir_all(&test_dir);
}

/// Phase 2: `mu clean --dry-run` creates preview ledger at $XDG_CONFIG_HOME/mu/clean-list.txt.
#[test]
fn clean_dry_run_creates_preview_ledger() {
    let o = mu(&["clean", "--dry-run"]);
    assert_eq!(o.status.code(), Some(0));
    let out = stdout(&o);
    assert!(out.contains("Potential space to free:"), "{out}");
    assert!(out.contains("DRY RUN"), "{out}");

    let config_home = std::env::var_os("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::env::var_os("HOME")
                .map(|h| std::path::PathBuf::from(h).join(".config"))
                .unwrap()
        });
    let ledger = config_home.join("mu").join("clean-list.txt");
    assert!(
        ledger.exists(),
        "clean-list.txt must exist at {}",
        ledger.display()
    );
    let content = std::fs::read_to_string(&ledger).unwrap();
    assert!(content.contains("# mu clean preview ledger"));
}

/// Phase 2: `mu clean --dry-run --include=system-trash` previews item count and size.
#[test]
fn clean_dry_run_system_trash_previews_count_and_size() {
    let o = mu(&["clean", "--dry-run", "--include=system-trash"]);
    assert_eq!(o.status.code(), Some(0));
    let out = stdout(&o);
    assert!(out.contains("System Trash"), "{out}");
    assert!(out.contains("item"), "{out}");
}

/// Phase 2: `mu clean --include=system-trash < /dev/null` fails closed without --yes.
#[test]
fn clean_include_system_trash_aborts_without_yes() {
    let o = mu(&["clean", "--include=system-trash"]);
    assert_eq!(o.status.code(), Some(0));
    let out = stdout(&o);
    assert!(out.contains("Non-interactive: pass --yes"), "{out}");
    assert!(out.contains("Aborted."), "{out}");
}

/// Phase 2: Sentinel immutability test for `clean --dry-run`.
#[test]
fn clean_dry_run_sentinel_immutability() {
    let test_dir = std::env::temp_dir().join(format!("mu-clean-sentinel-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&test_dir);
    let sentinel = test_dir.join("keep.txt");
    std::fs::write(&sentinel, b"preserve me").unwrap();

    let o = mu(&["clean", "--dry-run"]);
    assert_eq!(o.status.code(), Some(0));
    assert!(
        sentinel.exists(),
        "sentinel keep.txt must not be deleted during clean --dry-run"
    );

    let _ = std::fs::remove_dir_all(&test_dir);
}

/// `mu history --limit 5` -> renders tabular log of recent operations, exit 0.
#[test]
fn history_limit_tabular() {
    let o = mu(&["history", "--limit", "5"]);
    assert_eq!(o.status.code(), Some(0), "stderr: {}", stderr(&o));
    let out = stdout(&o);
    // Either header or no operations message
    assert!(
        out.contains("TIMESTAMP") || out.contains("No recent operations recorded"),
        "unexpected output: {out}"
    );
}

/// `mu history --json` -> valid JSON array with fields timestamp, action, outcome, target.
#[test]
fn history_json_array() {
    let o = mu(&["history", "--json"]);
    assert_eq!(o.status.code(), Some(0), "stderr: {}", stderr(&o));
    let v: serde_json::Value = serde_json::from_str(&stdout(&o)).expect("history JSON parse");
    assert!(v.is_array(), "expected JSON array: {v}");
    if let Some(first) = v.as_array().and_then(|arr| arr.first()) {
        for key in ["timestamp", "action", "outcome", "target"] {
            assert!(first.get(key).is_some(), "missing {key} in entry: {first}");
        }
    }
}

/// `mu history --help` -> renders usage matching golden fixture, exit 0.
#[test]
fn history_help_output() {
    let o = mu(&["history", "--help"]);
    assert_eq!(o.status.code(), Some(0));
    assert_eq!(stdout(&o), include_str!("golden/help-history.txt"));
}

/// `mu purge --dry-run` -> scans search paths and exits 0.
#[test]
fn purge_dry_run_scans() {
    let o = mu(&["purge", "--dry-run"]);
    assert_eq!(o.status.code(), Some(0), "stderr: {}", stderr(&o));
    let out = stdout(&o);
    assert!(
        out.contains("project build artifact")
            || out.contains("No project build artifacts found")
            || out.contains("No project directories found"),
        "unexpected purge dry run output: {out}"
    );
}

/// Sentinel immutability test (purge):
/// `mu purge --dry-run --paths=/tmp/mu-purge-test` leaves sentinel.js intact.
#[test]
fn purge_sentinel_immutability() {
    let test_dir = std::env::temp_dir().join(format!("mu-purge-test-{}", std::process::id()));
    let nm = test_dir.join("node_modules");
    std::fs::create_dir_all(&nm).unwrap();
    let sentinel = nm.join("sentinel.js");
    std::fs::write(&sentinel, b"console.log('sentinel');").unwrap();

    let o = mu(&[
        "purge",
        "--dry-run",
        &format!("--paths={}", test_dir.display()),
    ]);
    assert_eq!(o.status.code(), Some(0), "stderr: {}", stderr(&o));
    assert!(
        sentinel.exists(),
        "purge --dry-run must leave sentinel.js intact"
    );

    let out = stdout(&o);
    assert!(
        out.contains("DRY RUN"),
        "output should mention DRY RUN: {out}"
    );
    assert!(
        out.contains("node_modules"),
        "output should list node_modules: {out}"
    );

    let _ = std::fs::remove_dir_all(&test_dir);
}

/// Phase 2: `mu uninstall --list` lists installed packages with source prefix.
#[test]
fn uninstall_list_outputs_packages() {
    let o = mu(&["uninstall", "--list"]);
    assert_eq!(o.status.code(), Some(0));
    let out = stdout(&o);
    assert!(
        out.contains("apt:") || out.contains("snap:") || out.contains("flatpak:"),
        "output should list packages with source prefix: {out}"
    );
}

/// Phase 2: `mu uninstall --dry-run bash` previews removal plan without executing apt.
#[test]
fn uninstall_dry_run_bash_previews_plan() {
    let o = mu(&["uninstall", "--dry-run", "bash"]);
    assert_eq!(o.status.code(), Some(0));
    let out = stdout(&o);
    assert!(out.contains("Package to remove:"), "{out}");
    assert!(out.contains("bash"), "{out}");
    assert!(out.contains("[dry-run]"), "{out}");
}

/// Phase 2: `mu uninstall --permanent bash` rejected with exit 1 and safety error.
#[test]
fn uninstall_rejects_permanent_flag() {
    let o = mu(&["uninstall", "--permanent", "bash"]);
    assert_eq!(o.status.code(), Some(1));
    let err = stderr(&o);
    assert!(
        err.contains("error: --permanent is rejected: mu enforces SafeDelete via trash only"),
        "unexpected stderr: {err}"
    );
}

/// Phase 2: `mu uninstall --help` renders updated help matching golden fixture.
#[test]
fn uninstall_help_matches_golden() {
    let o = mu(&["uninstall", "--help"]);
    assert_eq!(o.status.code(), Some(0));
    assert_eq!(stdout(&o), include_str!("golden/help-uninstall.txt"));
}

/// Phase 2: `mu clean --external <PATH>` previews external-volume metadata and
/// mutates nothing on a dry run.
#[test]
fn clean_external_dry_run_previews() {
    let dir = std::env::temp_dir().join(format!("mu-external-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join(".Trash-1000")).unwrap();
    std::fs::write(dir.join(".directory"), b"meta").unwrap();

    let path = dir.to_string_lossy().into_owned();
    let o = mu(&["clean", "--dry-run", "--external", &path]);
    assert_eq!(o.status.code(), Some(0), "stderr: {}", stderr(&o));
    let out = stdout(&o);
    assert!(out.contains("External Volume"), "{out}");
    assert!(
        dir.join(".directory").exists(),
        "dry-run must not delete external metadata"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// Phase 3: `mu optimize --dry-run` output contains exact shell command strings.
#[test]
fn optimize_dry_run_exact_commands() {
    let o = mu(&["optimize", "--dry-run"]);
    assert_eq!(o.status.code(), Some(0), "stderr: {}", stderr(&o));
    let out = stdout(&o);
    assert!(
        out.contains("[dry-run] sudo journalctl --vacuum-size=500M"),
        "missing exact journal command: {out}"
    );
    assert!(
        out.contains("[dry-run] sudo resolvectl flush-caches"),
        "missing exact dns command: {out}"
    );
    assert!(
        out.contains("[dry-run] sudo apt-get update && sudo apt-get autoremove --purge -y"),
        "missing exact apt command: {out}"
    );
    assert!(
        out.contains("[dry-run] sudo dpkg --configure -a && sudo apt-get install -f -y"),
        "missing exact packages command: {out}"
    );
    assert!(
        out.contains("[dry-run] sudo systemctl reset-failed && systemctl --user reset-failed"),
        "missing exact systemd command: {out}"
    );
}

/// Phase 3: `mu optimize --dry-run --skip=apt,dns,packages` marks skipped steps with [skip].
#[test]
fn optimize_dry_run_skip_steps() {
    let o = mu(&["optimize", "--dry-run", "--skip=apt,dns,packages"]);
    assert_eq!(o.status.code(), Some(0), "stderr: {}", stderr(&o));
    let out = stdout(&o);
    assert!(out.contains("[skip]  apt"), "missing [skip] apt: {out}");
    assert!(out.contains("[skip]  dns"), "missing [skip] dns: {out}");
    assert!(
        out.contains("[skip]  packages"),
        "missing [skip] packages: {out}"
    );
    assert!(
        out.contains("[dry-run] sudo journalctl --vacuum-size=500M"),
        "unskipped step should show dry-run: {out}"
    );
}

/// Phase 3: Sentinel immutability test on ~/.local/state/sentinel-test/stale.log.
#[test]
fn optimize_dry_run_sentinel_immutability() {
    let home = std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_default();
    let state_dir = home.join(".local/state/sentinel-test");
    let _ = std::fs::create_dir_all(&state_dir);
    let sentinel_file = state_dir.join("stale.log");
    std::fs::write(&sentinel_file, "sentinel stale state data").unwrap();

    let o = mu(&["optimize", "--dry-run"]);
    assert_eq!(o.status.code(), Some(0), "stderr: {}", stderr(&o));
    assert!(
        sentinel_file.exists(),
        "sentinel file was deleted during dry-run!"
    );

    let _ = std::fs::remove_dir_all(&state_dir);
}

/// Phase 3: `mu clean --dry-run --include=dev-cache` output includes "Developer CLI Caches".
#[test]
fn clean_dry_run_include_dev_cache() {
    let o = mu(&["clean", "--dry-run", "--include=dev-cache"]);
    assert_eq!(o.status.code(), Some(0), "stderr: {}", stderr(&o));
    let out = stdout(&o);
    assert!(
        out.contains("Developer CLI Caches"),
        "missing 'Developer CLI Caches' in output: {out}"
    );
}

/// Phase 3: `mu clean --dry-run` with all 4 opt-in targets.
#[test]
fn clean_dry_run_all_opt_in_targets() {
    let o = mu(&[
        "clean",
        "--dry-run",
        "--include=dev-cache,podman,orphaned-app-data,orphaned-services",
    ]);
    assert_eq!(o.status.code(), Some(0), "stderr: {}", stderr(&o));
    let out = stdout(&o);
    assert!(
        out.contains("Developer CLI Caches"),
        "missing dev-cache: {out}"
    );
    assert!(out.contains("Podman System Cache"), "missing podman: {out}");
    assert!(
        out.contains("Orphaned Application Data"),
        "missing orphaned-app-data: {out}"
    );
    assert!(
        out.contains("Orphaned User Services"),
        "missing orphaned-services: {out}"
    );
}

/// Sentinel immutability test (installer):
/// `mu installer --dry-run --paths=/tmp/mu-installer-test` leaves dummy.deb intact.
#[test]
fn installer_sentinel_immutability() {
    let test_dir = std::env::temp_dir().join(format!("mu-installer-test-{}", std::process::id()));
    std::fs::create_dir_all(&test_dir).unwrap();
    let dummy = test_dir.join("dummy.deb");
    std::fs::write(&dummy, b"dummy deb package content").unwrap();

    let o = mu(&[
        "installer",
        "--dry-run",
        &format!("--paths={}", test_dir.display()),
    ]);
    assert_eq!(o.status.code(), Some(0), "stderr: {}", stderr(&o));
    assert!(
        dummy.exists(),
        "installer --dry-run must leave dummy.deb intact"
    );

    let out = stdout(&o);
    assert!(
        out.contains("DRY RUN"),
        "output should mention DRY RUN: {out}"
    );
    assert!(
        out.contains("dummy.deb"),
        "output should list dummy.deb: {out}"
    );

    let _ = std::fs::remove_dir_all(&test_dir);
}

/// `mu purge --help` -> renders usage matching golden fixture, exit 0.
#[test]
fn purge_help_output() {
    let o = mu(&["purge", "--help"]);
    assert_eq!(o.status.code(), Some(0));
    assert_eq!(stdout(&o), include_str!("golden/help-purge.txt"));
}

/// `mu installer --help` -> renders usage matching golden fixture, exit 0.
#[test]
fn installer_help_output() {
    let o = mu(&["installer", "--help"]);
    assert_eq!(o.status.code(), Some(0));
    assert_eq!(stdout(&o), include_str!("golden/help-installer.txt"));
}

/// `mu sudo status` -> reports PAM fingerprint status, exit 0.
#[test]
fn sudo_status_output() {
    let o = mu(&["sudo", "status"]);
    assert_eq!(o.status.code(), Some(0));
    let out = stdout(&o);
    assert!(
        out.contains("Fingerprint authentication for sudo:"),
        "unexpected output: {out}"
    );
}

/// `mu sudo --dry-run enable` -> previews changes without modifying files, exit 0.
#[test]
fn sudo_dry_run_enable() {
    let o = mu(&["sudo", "--dry-run", "enable"]);
    assert_eq!(o.status.code(), Some(0));
    let out = stdout(&o);
    assert!(
        out.contains("[dry-run] Would enable fingerprint authentication for sudo"),
        "unexpected output: {out}"
    );
}

/// `mu update --dry-run` -> reports local version and status, exit 0.
#[test]
fn update_dry_run() {
    let tmp = std::env::temp_dir().join(format!("mu-update-dry-cli-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&tmp);
    let release_json = tmp.join("release.json");
    let json_content = serde_json::json!({
        "tag_name": "v0.3.0",
        "name": "v0.3.0",
        "assets": []
    });
    std::fs::write(&release_json, json_content.to_string()).unwrap();

    let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_mu"));
    cmd.args(["update", "--dry-run"]);
    cmd.env(
        "MU_UPDATE_URL",
        format!("file://{}", release_json.display()),
    );
    let o = cmd.output().expect("run mu update --dry-run");

    assert_eq!(o.status.code(), Some(0));
    let out = String::from_utf8_lossy(&o.stdout);
    assert!(
        out.contains("Checking for updates") || out.contains("up-to-date"),
        "unexpected output: {out}"
    );

    let _ = std::fs::remove_dir_all(&tmp);
}

/// `mu remove --dry-run` sentinel immutability test -> exit 0, sentinel untouched.
#[test]
fn remove_dry_run_sentinel() {
    let test_dir = std::env::temp_dir().join(format!("mu-remove-sentinel-{}", std::process::id()));
    let config_dir = test_dir.join(".config/mu");
    let _ = std::fs::create_dir_all(&config_dir);
    let sentinel = config_dir.join("config.toml");
    std::fs::write(&sentinel, b"# sentinel config").unwrap();

    let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_mu"));
    cmd.args(["remove", "--dry-run"]);
    cmd.env("HOME", &test_dir);
    cmd.env("XDG_CONFIG_HOME", test_dir.join(".config"));
    cmd.env("XDG_CACHE_HOME", test_dir.join(".cache"));
    cmd.env("XDG_DATA_HOME", test_dir.join(".local/share"));
    let output = cmd.output().expect("failed to run mu remove --dry-run");

    assert_eq!(output.status.code(), Some(0));
    let out = String::from_utf8_lossy(&output.stdout);
    assert!(out.contains("[dry-run]"), "expected dry run notice: {out}");
    assert!(
        sentinel.exists(),
        "sentinel config.toml must not be deleted during --dry-run"
    );

    let _ = std::fs::remove_dir_all(&test_dir);
}

/// `mu completion --install bash` -> writes completion script, exit 0.
#[test]
fn completion_install_bash() {
    let test_dir = std::env::temp_dir().join(format!("mu-comp-install-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&test_dir);

    let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_mu"));
    cmd.args(["completion", "--install", "bash"]);
    cmd.env("HOME", &test_dir);
    cmd.env("XDG_DATA_HOME", test_dir.join(".local/share"));
    let output = cmd
        .output()
        .expect("failed to run mu completion --install bash");

    assert_eq!(output.status.code(), Some(0));
    let out = String::from_utf8_lossy(&output.stdout);
    assert!(
        out.contains("Installed completion for bash"),
        "expected install confirmation in output: {out}"
    );

    let expected_file = test_dir.join(".local/share/bash-completion/completions/mu");
    assert!(
        expected_file.exists(),
        "installed completion file must exist at {}",
        expected_file.display()
    );

    let _ = std::fs::remove_dir_all(&test_dir);
}

/// `mu completion --install desktop` -> writes desktop entry, exit 0.
#[test]
fn completion_install_desktop() {
    let test_dir = std::env::temp_dir().join(format!("mu-comp-desktop-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&test_dir);

    let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_mu"));
    cmd.args(["completion", "--install", "desktop"]);
    cmd.env("HOME", &test_dir);
    cmd.env("XDG_DATA_HOME", test_dir.join(".local/share"));
    let output = cmd
        .output()
        .expect("failed to run mu completion --install desktop");

    assert_eq!(output.status.code(), Some(0));
    let expected_file = test_dir.join(".local/share/applications/mu.desktop");
    assert!(
        expected_file.exists(),
        "installed desktop file must exist at {}",
        expected_file.display()
    );
    let content = std::fs::read_to_string(&expected_file).unwrap();
    assert!(content.contains("[Desktop Entry]"));
    assert!(content.contains("Name=mu"));

    let _ = std::fs::remove_dir_all(&test_dir);
}

#[cfg(unix)]
fn assert_unreadable_config_fails(cmd_args: &[&str]) {
    use std::os::unix::fs::PermissionsExt;

    let test_dir = std::env::temp_dir().join(format!(
        "mu-unreadable-cfg-{}-{}-{}",
        cmd_args[0],
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let config_dir = test_dir.join("mu");
    std::fs::create_dir_all(&config_dir).unwrap();
    let cfg_file = config_dir.join("config.toml");
    std::fs::write(&cfg_file, b"[optimize_skip]\nsteps = [\"apt\"]\n").unwrap();

    let mut perms = std::fs::metadata(&cfg_file).unwrap().permissions();
    perms.set_mode(0o000);
    std::fs::set_permissions(&cfg_file, perms).unwrap();

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_mu"));
    cmd.args(cmd_args);
    cmd.stdin(Stdio::null());
    cmd.env("HOME", &test_dir);
    cmd.env("XDG_CONFIG_HOME", &test_dir);
    cmd.env("XDG_CACHE_HOME", test_dir.join("cache"));
    cmd.env("XDG_DATA_HOME", test_dir.join("data"));
    let output = cmd.output().expect("failed to run mu command");

    let mut perms = std::fs::metadata(&cfg_file).unwrap().permissions();
    perms.set_mode(0o600);
    let _ = std::fs::set_permissions(&cfg_file, perms);
    let _ = std::fs::remove_dir_all(&test_dir);

    assert_eq!(
        output.status.code(),
        Some(1),
        "command {:?} must exit 1 on unreadable config",
        cmd_args
    );
    let err = String::from_utf8_lossy(&output.stderr);
    assert!(
        err.contains("invalid mu configuration") || err.contains("Permission denied"),
        "stderr for {:?} should mention config error: {err}",
        cmd_args
    );
}

#[test]
#[cfg(unix)]
fn purge_unreadable_config_fails_closed() {
    assert_unreadable_config_fails(&["purge", "--dry-run"]);
}

#[test]
#[cfg(unix)]
fn installer_unreadable_config_fails_closed() {
    assert_unreadable_config_fails(&["installer", "--dry-run"]);
}

#[test]
#[cfg(unix)]
fn clean_unreadable_config_fails_closed() {
    assert_unreadable_config_fails(&["clean", "--dry-run"]);
}

#[test]
#[cfg(unix)]
fn optimize_unreadable_config_fails_closed() {
    assert_unreadable_config_fails(&["optimize", "--dry-run"]);
}

#[test]
#[cfg(unix)]
fn uninstall_unreadable_config_fails_closed() {
    assert_unreadable_config_fails(&["uninstall", "--dry-run", "bash"]);
}

#[test]
fn remove_non_interactive_without_yes_exits_nonzero() {
    let test_dir = std::env::temp_dir().join(format!("mu-remove-nonint-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&test_dir);

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_mu"));
    cmd.args(["remove"]);
    cmd.stdin(Stdio::null());
    cmd.env("HOME", &test_dir);
    cmd.env("XDG_CONFIG_HOME", test_dir.join(".config"));
    cmd.env("XDG_CACHE_HOME", test_dir.join(".cache"));
    cmd.env("XDG_DATA_HOME", test_dir.join(".local/share"));
    let output = cmd.output().expect("failed to run mu remove");

    assert_eq!(output.status.code(), Some(1));
    let out = String::from_utf8_lossy(&output.stdout);
    assert!(
        out.contains("Non-interactive: pass --yes"),
        "expected non-interactive note: {out}"
    );

    let _ = std::fs::remove_dir_all(&test_dir);
}

#[test]
fn uninstall_non_interactive_without_yes_exits_nonzero() {
    let test_dir = std::env::temp_dir().join(format!("mu-uninst-nonint-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&test_dir);

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_mu"));
    cmd.args(["uninstall", "bash"]);
    cmd.stdin(Stdio::null());
    cmd.env("HOME", &test_dir);
    cmd.env("XDG_CONFIG_HOME", test_dir.join(".config"));
    cmd.env("XDG_CACHE_HOME", test_dir.join(".cache"));
    cmd.env("XDG_DATA_HOME", test_dir.join(".local/share"));
    let output = cmd.output().expect("failed to run mu uninstall");

    assert_eq!(output.status.code(), Some(1));
    let out = String::from_utf8_lossy(&output.stdout);
    assert!(
        out.contains("Non-interactive: pass --yes"),
        "expected non-interactive note: {out}"
    );

    let _ = std::fs::remove_dir_all(&test_dir);
}
