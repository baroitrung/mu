//! `scan_kernels.go` — APT autoremove simulation parsing, the `kernels`
//! target (kept stable for CLI compatibility while using APT policy for the
//! complete autoremove candidate set), and `RunAutoremove`.
//!
//! The Go `freedSpacePattern` regexp is hand-ported (no regex dependency):
//! it is two alternatives —
//!   A: `after this operation,\s*NUM\s*UNIT\s+of additional disk space will be used`
//!   B: `NUM\s*UNIT\s+disk space will be freed`
//! both case-insensitive, `NUM=[0-9]+(\.[0-9]+)?`, `UNIT=[kmgt]?b`. Go takes
//! the leftmost match; an A-match is not reclaimable.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use crate::error::{Error, Result};
use crate::oplog;
use crate::runner::{CommandSpec, Runner};

use super::{CleanTarget, Deps};

/// `ParseAutoremoveSimulation` — extracts APT's complete removal candidate
/// set and estimated freed bytes from `apt-get -s autoremove --purge`
/// output. Only `Remv`/`Purg` lines name candidates — kernel images are
/// never matched by name; APT policy is the sole source of truth.
pub fn parse_autoremove_simulation(output: &str) -> (Vec<String>, i64) {
    let mut seen = std::collections::HashSet::new();
    let mut packages = Vec::new();
    for line in output.split('\n') {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 2 || (fields[0] != "Remv" && fields[0] != "Purg") {
            continue;
        }
        if seen.insert(fields[1].to_string()) {
            packages.push(fields[1].to_string());
        }
    }

    let mut bytes: i64 = 0;
    for line in output.split('\n') {
        if let Some(Some((value, unit))) = freed_space_match(&line.trim().to_ascii_lowercase()) {
            bytes = parse_apt_size(&value, &unit);
        }
    }
    (packages, bytes)
}

/// `parseAPTSize` — KB/MB/GB/TB are decimal (1000-based), anything else is
/// treated as bare bytes, matching Go's multiplier table exactly.
fn parse_apt_size(value: &str, unit: &str) -> i64 {
    let Ok(number) = value.parse::<f64>() else {
        return 0;
    };
    let multiplier = match unit.to_uppercase().as_str() {
        "KB" => 1_000.0,
        "MB" => 1_000_000.0,
        "GB" => 1_000_000_000.0,
        "TB" => 1_000_000_000_000.0,
        _ => 1.0,
    };
    (number * multiplier) as i64
}

/// The result of trying to match `freedSpacePattern` on one line:
/// `None` = no match; `Some(None)` = matched the "additional disk space will
/// be used" alternative (explicitly NOT reclaimable); `Some(Some((v, u)))` =
/// matched "disk space will be freed" with value+unit (lowercased line).
fn freed_space_match(line: &str) -> Option<Option<(String, String)>> {
    let mut best: Option<(usize, Option<(String, String)>)> = None;
    // Alternative B — match start is the number's start.
    for (i, _) in line.match_indices("disk space will be freed") {
        if let Some((start, _end, value, unit)) = walk_back_num_unit(line, i) {
            let cand = (start, Some((value, unit)));
            if best.as_ref().is_none_or(|(s, _)| start < *s) {
                best = Some(cand);
            }
        }
    }
    // Alternative A — match start is the position of "after this operation,".
    for (i, _) in line.match_indices("of additional disk space will be used") {
        if let Some((num_start, _end, _v, _u)) = walk_back_num_unit(line, i) {
            // \s* then the literal "after this operation," must precede.
            let before = line[..num_start].trim_end();
            if let Some(prefix) = before.strip_suffix("after this operation,") {
                // Match start = position of "after" (prefix is what precedes).
                let cand_start = prefix.len();
                if best.as_ref().is_none_or(|(s, _)| cand_start < *s) {
                    best = Some((cand_start, None));
                }
            }
        }
    }
    best.map(|(_, m)| m)
}

/// Walks back over `\s+ UNIT \s* NUM` immediately before `phrase_start` in a
/// lowercased line. Returns `(num_start, num_end, value_str, unit_str)`.
/// The number is the longest suffix of the `[0-9.]*` run matching
/// `^[0-9]+(\.[0-9]+)?$` — the same substring the Go regexp captures, since
/// the leftmost match begins at the earliest valid number start.
fn walk_back_num_unit(line: &str, phrase_start: usize) -> Option<(usize, usize, String, String)> {
    let b = line.as_bytes();
    let mut j = phrase_start;
    // `\s+` — at least one whitespace is required before the phrase.
    while j > 0 && b[j - 1].is_ascii_whitespace() {
        j -= 1;
    }
    if j == phrase_start {
        return None;
    }
    // `[kmgt]?b` — mandatory 'b', optional single scale char.
    if j == 0 || b[j - 1] != b'b' {
        return None;
    }
    let unit_end = j;
    j -= 1;
    if j > 0 && matches!(b[j - 1], b'k' | b'm' | b'g' | b't') {
        j -= 1;
    }
    let unit = line[j..unit_end].to_string();
    // `\s*` between number and unit.
    while j > 0 && b[j - 1].is_ascii_whitespace() {
        j -= 1;
    }
    let num_end = j;
    // Maximal `[0-9.]*` run, then its longest `^[0-9]+(\.[0-9]+)?$` suffix.
    let mut run_start = j;
    while run_start > 0 && (b[run_start - 1].is_ascii_digit() || b[run_start - 1] == b'.') {
        run_start -= 1;
    }
    for s in run_start..num_end {
        if !b[s].is_ascii_digit() {
            continue;
        }
        let cand = &line[s..num_end];
        if is_apt_number(cand) {
            return Some((s, num_end, cand.to_string(), unit));
        }
    }
    None
}

/// `^[0-9]+(\.[0-9]+)?$`
fn is_apt_number(s: &str) -> bool {
    let mut parts = s.split('.');
    let digits = |p: &str| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit());
    match (parts.next(), parts.next(), parts.next()) {
        (Some(a), None, None) => digits(a),
        (Some(a), Some(b), None) => digits(a) && digits(b),
        _ => false,
    }
}

/// `simulateAutoremove` — `apt-get -s -o Debug::NoLocking=1 autoremove
/// --purge` (no sudo). Go wraps the command in a 30s context at every call
/// site, so the spec carries that timeout.
fn simulate_autoremove(runner: &dyn Runner) -> Result<(Vec<String>, i64)> {
    let spec = CommandSpec::new(
        "apt-get",
        ["-s", "-o", "Debug::NoLocking=1", "autoremove", "--purge"],
    )
    .timeout(Duration::from_secs(30));
    match runner.run(&spec) {
        Err(e) => {
            let stderr = e
                .output()
                .map(|o| o.stderr_lossy().trim().to_string())
                .unwrap_or_default();
            Err(Error::Msg(format!(
                "apt-get autoremove preview: {e}: {stderr}"
            )))
        }
        Ok(out) => Ok(parse_autoremove_simulation(&out.stdout_lossy())),
    }
}

/// `RunAutoremove` — executes APT's own autoremove policy. The dry-run path
/// only previews (30s timeout); the real transaction intentionally has no
/// short timeout.
pub fn run_autoremove(runner: &dyn Runner, dry_run: bool) -> Result<()> {
    if dry_run {
        let (packages, _bytes) = simulate_autoremove(runner)?;
        oplog::log_outcome("apt-autoremove", &packages.join(","), "dry-run");
        return Ok(());
    }
    match runner.run(&CommandSpec::new(
        "sudo",
        ["apt-get", "autoremove", "--purge", "-y"],
    )) {
        Err(e) => {
            oplog::log_outcome("apt-autoremove", "apt policy", "failure");
            let stderr = e
                .output()
                .map(|o| o.stderr_lossy().trim().to_string())
                .unwrap_or_default();
            Err(Error::Msg(format!("apt-get autoremove: {e}: {stderr}")))
        }
        Ok(_) => {
            oplog::log_outcome("apt-autoremove", "apt policy", "success");
            Ok(())
        }
    }
}

/// (packages, bytes, error) loaded once. The error text is stored rather
/// than the `Error` so both closures can hand out identical copies — Go
/// reuses the same `scanErr` value.
type SimCache = Rc<RefCell<Option<(Vec<String>, i64, Option<String>)>>>;

/// `kernelsTarget` — the stable `kernels` target ID backed by APT policy.
/// Scan and Preview share one memoized simulation (Go's `sync.Once`).
pub(crate) fn kernels_target_in(deps: &Deps) -> CleanTarget {
    let runner = Arc::clone(&deps.runner);
    let cache: SimCache = Rc::new(RefCell::new(None));
    let load = {
        let cache = Rc::clone(&cache);
        let runner = Arc::clone(&runner);
        move || {
            let mut c = cache.borrow_mut();
            if c.is_none() {
                *c = Some(match simulate_autoremove(&*runner) {
                    Ok((packages, bytes)) => (packages, bytes, None),
                    Err(e) => (Vec::new(), 0, Some(e.to_string())),
                });
            }
        }
    };

    let scan_load = load.clone();
    let scan_cache = Rc::clone(&cache);
    let prev_load = load.clone();
    let prev_cache = Rc::clone(&cache);

    CleanTarget {
        id: "kernels",
        label: "APT Autoremove Candidates",
        requires_sudo: true,
        opt_in: false,
        // APT autoremove candidates are managed via apt-get; no static directory root.
        roots: Vec::new(),
        scan: Box::new(move || {
            scan_load();
            let c = scan_cache.borrow();
            let (_, bytes, err) = c.as_ref().expect("load fills the cache");
            // Go: `return m.bytes, m.scanErr` — bytes ride back with the error.
            (*bytes, err.as_ref().map(|e| Error::Msg(e.clone())))
        }),
        preview: Some(Box::new(move || {
            prev_load();
            let c = prev_cache.borrow();
            let (packages, _, err) = c.as_ref().expect("load fills the cache");
            // Go: `return append([]string(nil), packages...), scanErr` —
            // the items are returned alongside the error, not dropped.
            (
                packages.clone(),
                err.as_ref().map(|e| Error::Msg(e.clone())),
            )
        })),
        execute: Box::new(move |dry_run| run_autoremove(&*runner, dry_run)),
        count_fn: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::{FakeRunner, Output};

    // scan_kernels_test.go: TestParseAutoremoveSimulationUsesOnlyAPTCandidates
    #[test]
    fn parse_autoremove_simulation_uses_only_apt_candidates() {
        let fixture = "The following packages will be REMOVED:\n  linux-image-6.8.0-50-generic linux-modules-6.8.0-50-generic\nRemv linux-image-6.8.0-50-generic [6.8.0-50]\nPurg linux-modules-6.8.0-50-generic [6.8.0-50]\nRemv obsolete-lib [1.0]\nRemv obsolete-lib [1.0]\nAfter this operation, 240 MB disk space will be freed.\n";
        let (packages, bytes) = parse_autoremove_simulation(fixture);
        assert_eq!(
            packages,
            vec![
                "linux-image-6.8.0-50-generic",
                "linux-modules-6.8.0-50-generic",
                "obsolete-lib"
            ]
        );
        assert!(
            !packages.iter().any(|p| p == "linux-generic"),
            "meta-package not selected by APT must be preserved"
        );
        assert_eq!(bytes, 240_000_000, "bytes = {bytes}, want 240000000");
    }

    // scan_kernels_test.go: TestParseAutoremoveSimulationNoCandidates
    #[test]
    fn parse_autoremove_simulation_no_candidates() {
        let (packages, bytes) =
            parse_autoremove_simulation("0 upgraded, 0 newly installed, 0 to remove.\n");
        assert!(
            packages.is_empty() && bytes == 0,
            "got {packages:?} bytes={bytes}"
        );
    }

    // scan_kernels_test.go: TestParseAutoremoveSimulationDoesNotTreatAdditionalUsageAsFreed
    #[test]
    fn parse_autoremove_simulation_does_not_treat_additional_usage_as_freed() {
        let (_, bytes) = parse_autoremove_simulation(
            "After this operation, 10 MB of additional disk space will be used.\n",
        );
        assert_eq!(bytes, 0, "bytes = {bytes}, want 0");
    }

    // safety_test.go: TestRunAutoremoveUsesPreviewAndAPTPolicyCommand
    #[test]
    fn run_autoremove_uses_preview_and_apt_policy_command() {
        let runner = FakeRunner::new();
        runner.set_handler(|spec| {
            if spec.program == "apt-get" {
                return Ok(Output {
                    stdout: b"Remv old-lib [1]\n10 MB disk space will be freed.\n".to_vec(),
                    ..Default::default()
                });
            }
            Ok(Output::default())
        });
        run_autoremove(&runner, true).expect("dry-run autoremove");
        run_autoremove(&runner, false).expect("real autoremove");
        let calls = runner.invocations();
        assert_eq!(calls.len(), 2, "calls = {calls:?}");
        assert_eq!(calls[0].program, "apt-get");
        assert_eq!(
            calls[0].args,
            ["-s", "-o", "Debug::NoLocking=1", "autoremove", "--purge"]
                .map(std::ffi::OsString::from)
        );
        // The preview runs under a 30s deadline (context.WithTimeout).
        assert_eq!(calls[0].timeout, Some(Duration::from_secs(30)));
        assert_eq!(calls[1].program, "sudo");
        assert_eq!(
            calls[1].args,
            ["apt-get", "autoremove", "--purge", "-y"].map(std::ffi::OsString::from)
        );
        // Active APT transaction must not use a short generic deadline.
        assert!(calls[1].timeout.is_none());
    }
}
