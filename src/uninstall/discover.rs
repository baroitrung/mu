//! `discover.go` — installed-package discovery via `dpkg-query` and
//! `snap list`, plus per-snap on-disk sizing via `du -sk /snap/<name>/current`.
//!
//! Go reads the `uninstallRunner` package var; here the [`Runner`] is a
//! parameter. `populateSnapSizes` fans out with goroutines in Go — the port
//! sizes snaps sequentially; the observable result is identical (each
//! package's size is independent) and invocation order is deterministic.

use crate::error::{Error, Result};
use crate::runner::{CommandSpec, Runner};

/// `Package` — an installed package and its disk footprint.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Package {
    pub name: String,
    pub version: String,
    /// `"apt"` or `"snap"`.
    pub source: String,
    pub installed_kb: i64,
    pub remnants_kb: i64,
    pub remnants_found: Vec<String>,
}

impl Package {
    /// `Key` — the source-qualified identity (`apt:foo` vs `snap:foo`).
    pub fn key(&self) -> String {
        format!("{}:{}", self.source, self.name)
    }
}

/// `DiscoverAPT` — installed APT packages with size info.
pub fn discover_apt(runner: &dyn Runner) -> Result<Vec<Package>> {
    // Go returns the runner error unwrapped; RunError's Display is already
    // byte-compatible with the Go error text ("exit status 1", "exec: ...").
    let out = runner
        .run(&CommandSpec::new(
            "dpkg-query",
            [
                "--show",
                "--showformat=${Status}\t${Installed-Size}\t${Package}\t${Version}\n",
            ],
        ))
        .map_err(|e| Error::Msg(e.to_string()))?;
    Ok(parse_apt_output(&out.stdout_lossy()))
}

/// `parseAPTOutput` — one `${Status}\t${Installed-Size}\t${Package}\t${Version}`
/// record per line; only `install ok installed` rows count.
pub(crate) fn parse_apt_output(output: &str) -> Vec<Package> {
    let mut pkgs = Vec::new();
    for line in output.split('\n') {
        let fields: Vec<&str> = line.splitn(4, '\t').collect();
        if fields.len() < 4 {
            continue;
        }
        if !fields[0].starts_with("install ok installed") {
            continue;
        }
        // `fmt.Sscanf(fields[1], "%d", &sizeKB)`.
        let size_kb = scan_i64(fields[1]);
        pkgs.push(Package {
            name: fields[2].trim().to_string(),
            version: fields[3].trim().to_string(),
            source: "apt".to_string(),
            installed_kb: size_kb,
            ..Default::default()
        });
    }
    pkgs
}

/// `DiscoverSnap` — installed snap packages. Returns an empty list (no error)
/// when `snap` is not on PATH.
pub fn discover_snap(runner: &dyn Runner) -> Result<Vec<Package>> {
    if runner.look_path("snap").is_err() {
        return Ok(Vec::new());
    }
    let out = runner
        .run(&CommandSpec::new("snap", ["list"]))
        .map_err(|e| Error::Msg(format!("snap list: {e}")))?;
    Ok(parse_snap_output(&out.stdout_lossy(), runner))
}

/// `parseSnapOutput` — `snap list` rows (header line skipped), then per-snap
/// sizes via [`snap_installed_kb`] — the Go function reaches the package-var
/// runner inside `populateSnapSizes`, so the runner is a parameter here.
pub(crate) fn parse_snap_output(output: &str, runner: &dyn Runner) -> Vec<Package> {
    let mut pkgs = Vec::new();
    for (i, line) in output.split('\n').enumerate() {
        if i == 0 {
            continue; // skip header
        }
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 2 {
            continue;
        }
        pkgs.push(Package {
            name: fields[0].to_string(),
            version: fields[1].to_string(),
            source: "snap".to_string(),
            ..Default::default()
        });
    }
    // `populateSnapSizes` — sequential rather than per-package goroutines.
    for p in &mut pkgs {
        p.installed_kb = snap_installed_kb(runner, &p.name);
    }
    pkgs
}

/// `snapInstalledKB` — on-disk size of a snap's current revision in KB;
/// any failure or unparseable output yields 0.
fn snap_installed_kb(runner: &dyn Runner, name: &str) -> i64 {
    let Ok(out) = runner.run(&CommandSpec::new(
        "du",
        ["-sk", format!("/snap/{name}/current").as_str()],
    )) else {
        return 0;
    };
    let stdout = out.stdout_lossy();
    let Some(first) = stdout.split_whitespace().next() else {
        return 0;
    };
    // `fmt.Sscan(fields[0], &kb)`.
    scan_i64(first)
}

/// Go `fmt.Sscanf(_, "%d", &v)` core: skip leading whitespace, read an
/// optional sign and the longest digit run; unparseable input leaves 0.
fn scan_i64(s: &str) -> i64 {
    let t = s.trim_start();
    let (neg, rest) = match t.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, t.strip_prefix('+').unwrap_or(t)),
    };
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return 0;
    }
    let v = digits.parse::<i64>().unwrap_or(0);
    if neg { -v } else { v }
}

/// `DiscoverFlatpak` — installed Flatpak application packages.
pub fn discover_flatpak(runner: &dyn Runner) -> Result<Vec<Package>> {
    if runner.look_path("flatpak").is_err() {
        return Ok(Vec::new());
    }
    let out = runner
        .run(&CommandSpec::new(
            "flatpak",
            ["list", "--app", "--columns=application,version,size,origin"],
        ))
        .map_err(|e| Error::Msg(format!("flatpak list: {e}")))?;
    Ok(parse_flatpak_output(&out.stdout_lossy()))
}

/// `parseFlatpakOutput` — parses rows of `flatpak list --app`.
pub(crate) fn parse_flatpak_output(output: &str) -> Vec<Package> {
    let mut pkgs = Vec::new();
    for line in output.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let fields: Vec<&str> = line.split('\t').collect();
        let fields: Vec<&str> = if fields.len() >= 2 {
            fields
        } else {
            line.split_whitespace().collect()
        };
        if fields.is_empty() {
            continue;
        }
        let name = fields[0].trim().to_string();
        let version = fields
            .get(1)
            .map(|v| v.trim().to_string())
            .unwrap_or_default();
        let size_str = fields.get(2).map(|s| s.trim()).unwrap_or("");
        let installed_kb = parse_size_to_kb(size_str);
        pkgs.push(Package {
            name,
            version,
            source: "flatpak".to_string(),
            installed_kb,
            ..Default::default()
        });
    }
    pkgs
}

fn parse_size_to_kb(s: &str) -> i64 {
    let s = s.trim();
    if s.is_empty() {
        return 0;
    }
    let mut num_str = String::new();
    let mut unit_str = String::new();
    for c in s.chars() {
        if c.is_ascii_digit() || c == '.' {
            num_str.push(c);
        } else if !c.is_whitespace() {
            unit_str.push(c);
        }
    }
    let val: f64 = num_str.parse().unwrap_or(0.0);
    let unit = unit_str.to_uppercase();
    if unit.starts_with('G') {
        (val * 1024.0 * 1024.0) as i64
    } else if unit.starts_with('M') {
        (val * 1024.0) as i64
    } else if unit.starts_with('K') {
        val as i64
    } else if unit.starts_with('B') {
        (val / 1024.0) as i64
    } else {
        val as i64
    }
}

/// `Discover` — all installed packages (APT + Snap) sorted by name, plus the
/// joined per-source errors. Partial results are returned alongside the error
/// exactly like Go's `(all, errors.Join(aptErr, snapErr))` pair. Rust's sort
/// is stable; Go's `sort.Slice` is not, but the less-func compares name only
/// so ties were already arbitrary there.
pub fn discover(runner: &dyn Runner) -> (Vec<Package>, Option<Error>) {
    let (apt, apt_err) = match discover_apt(runner) {
        Ok(pkgs) => (pkgs, None),
        Err(e) => (Vec::new(), Some(e)),
    };
    let (snap, snap_err) = match discover_snap(runner) {
        Ok(pkgs) => (pkgs, None),
        Err(e) => (Vec::new(), Some(e)),
    };
    let (flatpak, flatpak_err) = match discover_flatpak(runner) {
        Ok(pkgs) => (pkgs, None),
        Err(e) => (Vec::new(), Some(e)),
    };
    let mut all = apt;
    all.extend(snap);
    all.extend(flatpak);
    all.sort_by(|a, b| a.name.cmp(&b.name));

    // `errors.Join(apt discovery: %w, snap discovery: %w, flatpak discovery: %w)`
    let mut errs: Vec<String> = Vec::new();
    if let Some(e) = apt_err {
        errs.push(format!("apt discovery: {e}"));
    }
    if let Some(e) = snap_err {
        errs.push(format!("snap discovery: {e}"));
    }
    if let Some(e) = flatpak_err {
        errs.push(format!("flatpak discovery: {e}"));
    }
    let err = if errs.is_empty() {
        None
    } else {
        Some(Error::Msg(errs.join("\n")))
    };
    (all, err)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::{FakeRunner, RunError};
    use std::io;

    const APT_ROW: &str = "install ok installed\t2\tapt-app\t1\n";
    const SNAP_LIST: &str =
        "Name Version Rev Tracking Publisher Notes\nsnap-app 2 1 stable pub -\n";

    fn spawn_err(text: &str) -> RunError {
        RunError::Spawn(io::Error::other(text.to_string()))
    }

    // discover_test.go: TestParseAPTOutput_FilterInstalled
    #[test]
    fn parse_apt_output_filters_installed() {
        let fixture = "install ok installed\t1234\tcurl\t7.88.1\n\
             deinstall ok config-files\t0\told-pkg\t1.0\n\
             install ok installed\t5678\twget\t1.21.3\n";
        let pkgs = parse_apt_output(fixture);
        assert_eq!(pkgs.len(), 2, "expected 2 installed packages, got {pkgs:?}");
        assert_eq!(pkgs[0].name, "curl");
        assert_eq!(pkgs[0].installed_kb, 1234);
        assert_eq!(pkgs[1].name, "wget");
    }

    // discover_test.go: TestParseSnapOutput_SkipsHeader — the Go test runs
    // `du` via the default ExecRunner (fails → 0 KB); FakeRunner's default
    // empty Ok parses to 0 the same way.
    #[test]
    fn parse_snap_output_skips_header() {
        let fixture = "Name    Version   Rev   Tracking  Publisher  Notes\n\
             firefox  124.0    456   latest/stable  mozilla  -\n\
             vlc      3.0.21   2345  latest/stable  videolan -\n";
        let runner = FakeRunner::new();
        let pkgs = parse_snap_output(fixture, &runner);
        assert_eq!(pkgs.len(), 2, "expected 2 snap packages, got {pkgs:?}");
        assert_eq!(pkgs[0].name, "firefox");
        assert_eq!(pkgs[0].source, "snap");
    }

    #[test]
    fn parse_flatpak_output_parses_rows() {
        let fixture = "org.mozilla.firefox\t120.0\t250 MB\tflathub\n\
             com.spotify.Client\t1.2.0\t1.5 GB\tflathub\n";
        let pkgs = parse_flatpak_output(fixture);
        assert_eq!(pkgs.len(), 2);
        assert_eq!(pkgs[0].name, "org.mozilla.firefox");
        assert_eq!(pkgs[0].source, "flatpak");
        assert_eq!(pkgs[0].installed_kb, 250 * 1024);
        assert_eq!(pkgs[1].name, "com.spotify.Client");
        assert_eq!(pkgs[1].installed_kb, (1.5 * 1024.0 * 1024.0) as i64);
    }

    // discover_test.go: TestDiscoverReturnsPartialPackagesAndJoinedErrors
    #[test]
    fn discover_returns_partial_packages_and_joined_errors() {
        for fail_command in ["dpkg-query", "snap"] {
            let runner = FakeRunner::new();
            // The Go stub's LookPath always succeeds.
            runner.set_look_path("snap", "/usr/bin/snap");
            runner.set_handler(move |spec| {
                if spec.program == fail_command {
                    return Err(spawn_err("source unavailable"));
                }
                match spec.program.to_string_lossy().as_ref() {
                    "dpkg-query" => Ok(crate::runner::Output {
                        stdout: APT_ROW.as_bytes().to_vec(),
                        ..Default::default()
                    }),
                    "snap" => Ok(crate::runner::Output {
                        stdout: SNAP_LIST.as_bytes().to_vec(),
                        ..Default::default()
                    }),
                    _ => Ok(crate::runner::Output::default()),
                }
            });
            let (packages, err) = discover(&runner);
            let err = err.expect("expected discovery error");
            assert!(
                err.to_string().contains("source unavailable"),
                "expected 'source unavailable' in {err} (fail={fail_command})"
            );
            assert_eq!(
                packages.len(),
                1,
                "expected 1 partial package (fail={fail_command}), got {packages:?}"
            );
        }
    }

    // remove_test.go: TestDiscoverUsesSourceSpecificCommandsAndSorts — the
    // Discover coverage lives in remove_test.go in the oracle suite.
    #[test]
    fn discover_uses_source_specific_commands_and_sorts() {
        let runner = FakeRunner::new();
        runner.set_look_path("snap", "/usr/bin/snap");
        runner.set_handler(|spec| match spec.program.to_string_lossy().as_ref() {
            "dpkg-query" => Ok(crate::runner::Output {
                stdout: b"install ok installed\t2\tzeta\t1\n".to_vec(),
                ..Default::default()
            }),
            "snap" => Ok(crate::runner::Output {
                stdout: b"Name Version Rev Tracking Publisher Notes\nalpha 2 1 stable pub -\n"
                    .to_vec(),
                ..Default::default()
            }),
            "du" => Ok(crate::runner::Output {
                stdout: b"8 /snap/alpha/current\n".to_vec(),
                ..Default::default()
            }),
            _ => Ok(crate::runner::Output::default()),
        });
        let (packages, err) = discover(&runner);
        assert!(err.is_none(), "unexpected error: {err:?}");
        assert!(
            packages.len() == 2
                && packages[0].name == "alpha"
                && packages[0].installed_kb == 8
                && packages[1].name == "zeta",
            "packages={packages:?}"
        );

        // `DiscoverSnap` with LookPath failing → empty, no error (the Go
        // stub also errors `run`, which is never reached).
        let runner = FakeRunner::new();
        runner.set_handler(|_| Err(spawn_err("snap missing")));
        let snaps = discover_snap(&runner).expect("discover_snap");
        assert!(snaps.is_empty(), "snaps={snaps:?}");
    }

    // discover_test.go: TestDiscoverReportsBothSourceFailures
    #[test]
    fn discover_reports_both_source_failures() {
        let runner = FakeRunner::new();
        runner.set_look_path("snap", "/usr/bin/snap");
        runner.set_handler(|spec| {
            Err(spawn_err(&format!(
                "{} unavailable",
                spec.program.to_string_lossy()
            )))
        });
        let (packages, err) = discover(&runner);
        let err = err.expect("expected discovery error");
        assert!(packages.is_empty(), "packages={packages:?}");
        assert!(
            err.to_string().contains("dpkg-query unavailable"),
            "missing dpkg-query failure in {err}"
        );
        assert!(
            err.to_string().contains("snap unavailable"),
            "missing snap failure in {err}"
        );
    }
}
