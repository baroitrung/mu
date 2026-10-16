//! `scan_snap.go` — disabled snap revision discovery, sizing, and removal.

use std::sync::Arc;

use crate::error::Error;
use crate::oplog;
use crate::runner::CommandSpec;

use super::{CleanTarget, Deps, join_errors};

/// `snapRevision` — a disabled snap's name and revision string.
#[derive(Debug)]
pub(crate) struct SnapRevision {
    pub name: String,
    pub revision: String,
}

/// `parseDisabledSnaps` — parses `snap list --all` output and returns
/// disabled revisions. Columns: `Name Version Rev Tracking Publisher Notes`;
/// `"disabled"` appears in Notes (last column).
pub(crate) fn parse_disabled_snaps(output: &str) -> Vec<SnapRevision> {
    let mut result = Vec::new();
    for (i, line) in output.split('\n').enumerate() {
        if i == 0 {
            continue; // header line
        }
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let fields: Vec<&str> = line.split_whitespace().collect();
        // Expect at least 6 fields: Name Version Rev Tracking Publisher Notes
        if fields.len() < 6 {
            continue;
        }
        let notes = fields[fields.len() - 1];
        if notes.contains("disabled") {
            result.push(SnapRevision {
                name: fields[0].to_string(),
                revision: fields[2].to_string(),
            });
        }
    }
    result
}

/// `snapTarget` — CleanTarget for disabled snap revisions. `snap`'s presence
/// is probed once at construction (Go captures `snapErr` the same way).
pub(crate) fn snap_target_in(deps: &Deps) -> CleanTarget {
    let runner = Arc::clone(&deps.runner);
    let snap_missing = runner.look_path("snap").is_err();

    let scan_runner = Arc::clone(&runner);
    let exec_runner = runner;

    CleanTarget {
        id: "snap",
        label: "Snap Disabled Revisions",
        requires_sudo: true,
        opt_in: false,
        scan: Box::new(move || {
            if snap_missing {
                return (0, None);
            }
            let out = match scan_runner.run(&CommandSpec::new("snap", ["list", "--all"])) {
                Ok(out) => out,
                Err(e) => return (0, Some(Error::Msg(format!("snap list: {e}")))),
            };
            let revs = parse_disabled_snaps(&out.stdout_lossy());
            let mut total: i64 = 0;
            let mut scan_errors: Vec<Error> = Vec::new();
            for r in &revs {
                let snap_path = format!("/var/lib/snapd/snaps/{}_{}.snap", r.name, r.revision);
                match scan_runner.run(&CommandSpec::new("du", ["-sb", snap_path.as_str()])) {
                    Err(e) => scan_errors.push(Error::Msg(format!("size {snap_path}: {e}"))),
                    Ok(du) => {
                        if let Some(first) = du.stdout_lossy().split_whitespace().next()
                            && let Ok(sz) = first.parse::<i64>()
                        {
                            total += sz;
                        }
                    }
                }
            }
            // Go: `return total, errors.Join(scanErrors...)` — partial total
            // rides back with the error.
            if scan_errors.is_empty() {
                (total, None)
            } else {
                (total, Some(join_errors(&scan_errors)))
            }
        }),
        preview: None,
        execute: Box::new(move |dry_run| {
            if snap_missing {
                return Ok(());
            }
            let out = exec_runner
                .run(&CommandSpec::new("snap", ["list", "--all"]))
                .map_err(|e| Error::Msg(format!("snap list: {e}")))?;
            let revs = parse_disabled_snaps(&out.stdout_lossy());
            let mut remove_errors: Vec<Error> = Vec::new();
            for r in &revs {
                let target = format!("snap {} rev {}", r.name, r.revision);
                if dry_run {
                    oplog::log_outcome("snap-remove", &target, "dry-run");
                    continue;
                }
                match exec_runner.run(&CommandSpec::new(
                    "sudo",
                    ["snap", "remove", "--revision", &r.revision, &r.name],
                )) {
                    Err(e) => {
                        oplog::log_outcome("snap-remove", &target, "failure");
                        remove_errors.push(Error::Msg(format!(
                            "remove snap {} rev {}: {e}",
                            r.name, r.revision
                        )));
                    }
                    Ok(_) => oplog::log_outcome("snap-remove", &target, "success"),
                }
            }
            if remove_errors.is_empty() {
                Ok(())
            } else {
                Err(join_errors(&remove_errors))
            }
        }),
        count_fn: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clean::test_support::*;
    use crate::runner::{FakeRunner, Output, RunError};
    use std::fs;

    fn snap_deps(root: &std::path::Path, runner: Arc<FakeRunner>) -> Deps {
        runner.set_look_path("snap", "/usr/bin/snap");
        deps_for_test(root, runner)
    }

    // scan_snap_test.go: TestSnapScanSizesArchiveInsteadOfMountedRevision
    #[test]
    fn snap_scan_sizes_archive_instead_of_mounted_revision() {
        let tmp = tempdir("snapscan");
        let runner = Arc::new(FakeRunner::new());
        runner.set_handler(|spec| match spec.program.to_string_lossy().as_ref() {
            "snap" => Ok(Output {
                stdout: SNAP_LIST_FIXTURE.as_bytes().to_vec(),
                ..Default::default()
            }),
            "du" => {
                let path = spec.args.last().unwrap().to_string_lossy().into_owned();
                match path.as_str() {
                    "/var/lib/snapd/snaps/lxd_27183.snap" => Ok(Output {
                        stdout: format!("100 {path}\n").into_bytes(),
                        ..Default::default()
                    }),
                    "/var/lib/snapd/snaps/vlc_3078.snap" => Ok(Output {
                        stdout: format!("200 {path}\n").into_bytes(),
                        ..Default::default()
                    }),
                    _ => Err(RunError::Spawn(std::io::Error::other(format!(
                        "unexpected snap size path: {path}"
                    )))),
                }
            }
            other => Err(RunError::Spawn(std::io::Error::other(format!(
                "unexpected command: {other}"
            )))),
        });
        let deps = snap_deps(&tmp, runner);
        let target = snap_target_in(&deps);
        let size = (target.scan)().0;
        assert_eq!(size, 300, "size={size} want 300");
        fs::remove_dir_all(&tmp).ok();
    }

    // safety_test.go: TestSnapTargetPropagatesRevisionFailure
    #[test]
    fn snap_target_propagates_revision_failure() {
        let tmp = tempdir("snapfail");
        let runner = Arc::new(FakeRunner::new());
        runner.set_handler(|spec| {
            let joined = format!(
                "{} {}",
                spec.program.to_string_lossy(),
                spec.args
                    .iter()
                    .map(|a| a.to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
                    .join(" ")
            );
            if joined.trim_end() == "snap list --all" {
                return Ok(Output {
                    stdout: SNAP_LIST_FIXTURE.as_bytes().to_vec(),
                    ..Default::default()
                });
            }
            if joined.starts_with("sudo snap remove") {
                return Err(RunError::Spawn(std::io::Error::other("snapd unavailable")));
            }
            Ok(Output::default())
        });
        let deps = snap_deps(&tmp, runner);
        let target = snap_target_in(&deps);
        let err = (target.execute)(false).expect_err("expected revision failure");
        assert!(
            err.to_string().contains("snapd unavailable"),
            "expected revision failure, got {err}"
        );
        fs::remove_dir_all(&tmp).ok();
    }

    // safety_test.go: TestThumbnailAndSnapDryRunPaths (snap half)
    #[test]
    fn snap_dry_run_path() {
        let tmp = tempdir("snapdry");
        let runner = Arc::new(FakeRunner::new());
        runner.set_handler(|spec| match spec.program.to_string_lossy().as_ref() {
            "snap" => Ok(Output {
                stdout: SNAP_LIST_FIXTURE.as_bytes().to_vec(),
                ..Default::default()
            }),
            "du" => Ok(Output {
                stdout: b"10 /snap/x/1\n".to_vec(),
                ..Default::default()
            }),
            _ => Ok(Output::default()),
        });
        let deps = snap_deps(&tmp, runner);
        let snap = snap_target_in(&deps);
        let size = (snap.scan)().0;
        assert_eq!(size, 20, "snap size={size}");
        (snap.execute)(true).expect("snap dry-run execute");
        fs::remove_dir_all(&tmp).ok();
    }

    // scan_snap_test.go: TestParseDisabledSnaps_basic
    #[test]
    fn parse_disabled_snaps_basic() {
        let revs = parse_disabled_snaps(SNAP_LIST_FIXTURE);
        assert_eq!(revs.len(), 2, "expected 2 disabled revisions: {revs:?}");
        assert_eq!(revs[0].name, "lxd");
        assert_eq!(revs[0].revision, "27183");
        assert_eq!(revs[1].name, "vlc");
        assert_eq!(revs[1].revision, "3078");
    }

    // scan_snap_test.go: TestParseDisabledSnaps_enabled_not_included
    #[test]
    fn parse_disabled_snaps_enabled_not_included() {
        let revs = parse_disabled_snaps(SNAP_LIST_FIXTURE);
        for r in &revs {
            assert!(
                r.name != "core20" && r.name != "firefox",
                "enabled snap {} should not be in disabled list",
                r.name
            );
        }
    }

    // scan_snap_test.go: TestParseDisabledSnaps_empty
    #[test]
    fn parse_disabled_snaps_empty() {
        assert_eq!(parse_disabled_snaps("").len(), 0);
    }

    // scan_snap_test.go: TestParseDisabledSnaps_headerOnly
    #[test]
    fn parse_disabled_snaps_header_only() {
        let revs =
            parse_disabled_snaps("Name    Version   Rev   Tracking       Publisher   Notes\n");
        assert_eq!(revs.len(), 0);
    }
}
