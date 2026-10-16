//! `scan_docker.go` — the opt-in Docker build-cache target, present only
//! when the Docker socket exists.

use std::path::Path;
use std::sync::Arc;

use serde::Deserialize;

use crate::error::Error;
use crate::oplog;
use crate::paths;
use crate::runner::CommandSpec;

use super::{CleanTarget, Deps, scan_f_s};

const DOCKER_SOCKET: &str = "/var/run/docker.sock";

/// `dockerDfLine` — one line of `docker system df --format '{{json .}}'`.
#[derive(Debug, Default, Deserialize)]
struct DockerDfLine {
    #[serde(default, rename = "Type")]
    type_: String,
    #[serde(default, rename = "Reclaimable")]
    reclaimable: String,
}

/// `parseDockerBuildCacheSize` — the first `Build Cache` line's
/// `Reclaimable` value in bytes; non-JSON and non-matching lines skipped.
fn parse_docker_build_cache_size(json_lines: &str) -> i64 {
    for line in json_lines.split('\n') {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(entry) = serde_json::from_str::<DockerDfLine>(line) else {
            continue;
        };
        if entry.type_ != "Build Cache" {
            continue;
        }
        return parse_docker_size(&entry.reclaimable);
    }
    0
}

/// `parseDockerSize` — Docker size strings like "1.2GB", "512MB", "0B" to
/// bytes. Go's `fmt.Sscanf(s, "%f%s", ...)` result is used unconditionally —
/// a failed scan leaves `0`/`""`, which no switch arm matches. Shared with the
/// Podman target (`podman system df` reports the same size strings).
pub(crate) fn parse_docker_size(s: &str) -> i64 {
    let s = s.trim();
    if s.is_empty() || s == "0B" {
        return 0;
    }
    let (val, unit, _n) = scan_f_s(s);
    let unit = unit.to_uppercase();
    if unit.starts_with("GB") || unit.starts_with('G') {
        return (val * 1024.0 * 1024.0 * 1024.0) as i64;
    }
    if unit.starts_with("MB") || unit.starts_with('M') {
        return (val * 1024.0 * 1024.0) as i64;
    }
    if unit.starts_with("KB") || unit.starts_with('K') {
        return (val * 1024.0) as i64;
    }
    if unit.starts_with('B') {
        return val as i64;
    }
    0
}

/// `newDockerTarget` — the Docker build-cache CleanTarget (always OptIn).
pub(crate) fn new_docker_target_in(deps: &Deps) -> CleanTarget {
    let scan_runner = Arc::clone(&deps.runner);
    let exec_runner = Arc::clone(&deps.runner);
    CleanTarget {
        id: "docker",
        label: "Docker Build Cache",
        requires_sudo: false,
        opt_in: true,
        scan: Box::new(move || {
            match scan_runner.run(&CommandSpec::new(
                "docker",
                ["system", "df", "--format", "{{json .}}"],
            )) {
                Err(e) => (0, Some(Error::Msg(e.to_string()))),
                Ok(out) => (parse_docker_build_cache_size(&out.stdout_lossy()), None),
            }
        }),
        preview: None,
        execute: Box::new(move |dry_run| {
            if dry_run {
                oplog::log_outcome("docker-builder-prune", "build cache", "dry-run");
                return Ok(());
            }
            match exec_runner.run(&CommandSpec::new("docker", ["builder", "prune", "-f"])) {
                Err(e) => Err(Error::Msg(format!("docker builder prune: {e}"))),
                Ok(_) => {
                    oplog::log_outcome("docker-builder-prune", "build cache", "success");
                    Ok(())
                }
            }
        }),
        count_fn: None,
    }
}

/// `dockerTarget` — `None` when the Docker socket is absent.
pub(crate) fn docker_target_in(deps: &Deps) -> Option<CleanTarget> {
    if !paths::path_exists(Path::new(DOCKER_SOCKET)) {
        return None;
    }
    Some(new_docker_target_in(deps))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clean::test_support::*;
    use crate::runner::{FakeRunner, Output, RunError};
    use std::fs;
    use std::sync::atomic::{AtomicBool, Ordering};

    // scan_docker_test.go: TestDockerTarget_isOptIn
    #[test]
    fn docker_target_is_opt_in() {
        let tmp = tempdir("dockeroptin");
        let deps = deps_for_test(&tmp, Arc::new(FakeRunner::new()));
        let target = new_docker_target_in(&deps);
        assert!(target.opt_in, "docker cache target should be opt-in");
        assert_eq!(target.id, "docker", "unexpected id {}", target.id);
        fs::remove_dir_all(&tmp).ok();
    }

    // scan_docker_test.go: TestParseDockerBuildCacheSizes
    #[test]
    fn parse_docker_build_cache_sizes() {
        let fixture = "not-json\n\
{\"Type\":\"Images\",\"TotalCount\":\"4\",\"Active\":\"2\",\"Size\":\"12.3GB\",\"Reclaimable\":\"9GB (73%)\"}\n\
{\"Type\":\"Build Cache\",\"TotalCount\":\"8\",\"Active\":\"0\",\"Size\":\"1.5GB\",\"Reclaimable\":\"1.5GB\"}\n";
        let got = parse_docker_build_cache_size(fixture);
        assert_eq!(got, (1.5 * 1024.0 * 1024.0 * 1024.0) as i64, "size={got}");
        for (input, want) in [
            ("512MB", 512 * 1024 * 1024i64),
            ("2KB", 2048),
            ("7B", 7),
            ("0B", 0),
            ("bad", 0),
        ] {
            assert_eq!(
                parse_docker_size(input),
                want,
                "parse_docker_size({input:?})"
            );
        }
    }

    // scan_docker_test.go: TestDockerTargetPropagatesScanAndExecuteFailures
    #[test]
    fn docker_target_propagates_scan_and_execute_failures() {
        let tmp = tempdir("dockerfail");
        let runner = Arc::new(FakeRunner::new());
        let failed = Arc::new(AtomicBool::new(false));
        let failed2 = Arc::clone(&failed);
        runner.set_handler(move |spec| {
            if spec
                .args
                .first()
                .is_some_and(|a| a == std::ffi::OsStr::new("system"))
            {
                return Ok(Output {
                    stdout: b"{\"Type\":\"Build Cache\",\"TotalCount\":\"1\",\"Active\":\"0\",\"Size\":\"2MB\",\"Reclaimable\":\"2MB\"}"
                        .to_vec(),
                    ..Default::default()
                });
            }
            if failed2.load(Ordering::SeqCst) {
                return Err(RunError::Spawn(std::io::Error::other("docker failed")));
            }
            Ok(Output::default())
        });
        let deps = deps_for_test(&tmp, runner);
        let target = new_docker_target_in(&deps);
        let size = (target.scan)().0;
        assert_eq!(size, 2 * 1024 * 1024, "size={size}");
        (target.execute)(true).expect("dry-run execute");
        failed.store(true, Ordering::SeqCst);
        assert!((target.execute)(false).is_err(), "expected prune failure");
        fs::remove_dir_all(&tmp).ok();
    }
}
