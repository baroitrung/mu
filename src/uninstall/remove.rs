//! `remove.go` — package + remnant removal. The ordering contract is kept
//! bit-faithful: every selected package's source removal (`sudo apt-get
//! purge` / `sudo snap remove`) runs FIRST; a remnant is deleted only after
//! its owning package's removal succeeded, only when it is not `/var/`-rooted
//! and not owned by any remaining installed package, and only through
//! `utils.SafeDelete` (the trash path — never `rm -rf`) gated by
//! `remnantRoot` + `utils.ValidateCleanupCandidate`.
//!
//! `RemoveSelected`'s exact per-remnant sequence:
//!   1. skip when the path was already processed (by an earlier package);
//!   2. retain when `filepath.Clean(remnant)` is `/var/`-prefixed or
//!      `remnantOwnedByRemaining` says another installed package owns it —
//!      retained paths are NOT marked processed;
//!   3. otherwise mark processed BEFORE attempting
//!      `remnantRoot` → `ValidateCleanupCandidate` → `SafeDelete`, so a
//!      failed delete is not retried for a later package sharing the path.

use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result, msg};
use crate::runner::CommandSpec;
use crate::{oplog, paths, trash, xdg};

use super::Deps;
use super::discover::Package;
use super::remnants::known_alias;

/// `RemovalResult` — package and remnant outcomes recorded independently.
#[derive(Debug)]
pub struct RemovalResult {
    pub package: Package,
    pub removed: bool,
    pub dry_run: bool,
    pub remnants_removed: Vec<String>,
    pub remnants_retained: Vec<String>,
    pub err: Option<Error>,
}

/// `RemoveSelected` — remove each source-qualified package independently,
/// then remove only remnants no remaining installed package owns.
///
/// `out`/`err_out` stand in for `os.Stdout`/`os.Stderr`: the dry-run line and
/// the child's captured stdout go to `out`, captured stderr to `err_out`.
pub fn remove_selected(
    selected: &[Package],
    installed: &[Package],
    dry_run: bool,
) -> Vec<RemovalResult> {
    let deps = Deps::real();
    let stdout = std::io::stdout();
    let stderr = std::io::stderr();
    let mut out = stdout.lock();
    let mut err_out = stderr.lock();
    remove_selected_in(&deps, selected, installed, dry_run, &mut out, &mut err_out)
}

/// Injectable core of [`remove_selected`].
pub(crate) fn remove_selected_in(
    deps: &Deps,
    selected: &[Package],
    installed: &[Package],
    dry_run: bool,
    out: &mut dyn Write,
    err_out: &mut dyn Write,
) -> Vec<RemovalResult> {
    let mut results: Vec<RemovalResult> = Vec::with_capacity(selected.len());
    let mut removed: HashSet<String> = HashSet::with_capacity(selected.len());
    for pkg in selected {
        let mut result = RemovalResult {
            package: pkg.clone(),
            removed: false,
            dry_run,
            remnants_removed: Vec::new(),
            remnants_retained: Vec::new(),
            err: None,
        };
        match remove_package(deps, pkg, dry_run, out, err_out) {
            Err(e) => {
                result.err = Some(e);
                oplog::log_outcome(&format!("{}-remove", pkg.source), &pkg.name, "failure");
            }
            Ok(()) => {
                result.removed = true;
                removed.insert(pkg.key());
                oplog::log_outcome(
                    &format!("{}-remove", pkg.source),
                    &pkg.name,
                    if dry_run { "dry-run" } else { "success" },
                );
            }
        }
        results.push(result);
    }

    let mut processed_remnants: HashSet<String> = HashSet::new();
    for result in &mut results {
        if !result.removed {
            result
                .remnants_retained
                .extend(result.package.remnants_found.iter().cloned());
            continue;
        }
        // Clone the list: the loop mutates sibling fields of `result`.
        for remnant in result.package.remnants_found.clone() {
            if processed_remnants.contains(&remnant) {
                continue;
            }
            let clean = xdg::clean(Path::new(&remnant));
            if clean.to_string_lossy().starts_with("/var/")
                || remnant_owned_by_remaining(&remnant, &result.package, installed, &removed)
            {
                result.remnants_retained.push(remnant);
                continue;
            }
            processed_remnants.insert(remnant.clone());

            // If remnant is a systemd user unit, stop and disable it before trashing
            if remnant.ends_with(".service") {
                let unit = Path::new(&remnant)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("");
                if !unit.is_empty() {
                    if dry_run {
                        let _ =
                            writeln!(out, "  [dry-run] would run: systemctl --user stop {unit}");
                        let _ = writeln!(
                            out,
                            "  [dry-run] would run: systemctl --user disable {unit}"
                        );
                    } else {
                        let _ = deps
                            .runner
                            .run(&CommandSpec::new("systemctl", ["--user", "stop", unit]));
                        let _ = deps
                            .runner
                            .run(&CommandSpec::new("systemctl", ["--user", "disable", unit]));
                    }
                }
            }

            // `err = remnantRoot(...); if err == nil { ValidateCleanupCandidate };
            //  if err == nil { SafeDelete }` — the first failing step wins.
            let attempt = remnant_root(deps, &remnant)
                .and_then(|root| paths::validate_cleanup_candidate(&root, Path::new(&remnant)))
                .and_then(|()| {
                    trash::safe_delete_with(
                        deps.trash_runner.as_ref(),
                        &deps.config_home,
                        &deps.trash_deps,
                        Path::new(&remnant),
                        dry_run,
                    )
                });
            if let Err(e) = attempt {
                let wrapped = Error::Msg(format!("remove remnant {remnant}: {e}"));
                // `errors.Join(result.Err, wrapped)` — newline-separated text.
                result.err = Some(match result.err.take() {
                    Some(prev) => Error::Msg(format!("{prev}\n{wrapped}")),
                    None => wrapped,
                });
                result.remnants_retained.push(remnant.clone());
                oplog::log_outcome("remnant-remove", &remnant, "failure");
                continue;
            }
            result.remnants_removed.push(remnant.clone());
            oplog::log_outcome(
                "remnant-remove",
                &remnant,
                if dry_run { "dry-run" } else { "success" },
            );
        }
    }
    results
}

/// `removePackage` — `sudo apt-get purge -y <name>` or `sudo snap remove
/// <name>`; the dry-run line is printed for `--dry-run` instead. Captured
/// child output is relayed before the error is inspected, exactly like Go.
/// A `steam` source delegates to Steam's own uninstaller rather than touching
/// the appmanifest.
pub(crate) fn remove_package(
    deps: &Deps,
    pkg: &Package,
    dry_run: bool,
    out: &mut dyn Write,
    err_out: &mut dyn Write,
) -> Result<()> {
    if pkg.source == "steam" {
        let games = super::remnants::steam_delegations(deps, &pkg.name);
        let Some(game) = games.first() else {
            return msg(format!("steam game '{}' not found", pkg.name));
        };
        if dry_run {
            let _ = writeln!(
                out,
                "  [dry-run] would run: steam steam://uninstall/{}",
                game.appid
            );
            return Ok(());
        }
        if deps.runner.look_path("steam").is_err() {
            return msg("steam not found: cannot uninstall Steam games".to_string());
        }
        return deps
            .runner
            .run(&CommandSpec::new(
                "steam",
                [format!("steam://uninstall/{}", game.appid)],
            ))
            .map(|_| ())
            .map_err(|e| Error::Msg(format!("steam uninstall {}: {e}", game.appid)));
    }
    let (prog, args) = match pkg.source.as_str() {
        "apt" => ("sudo", vec!["apt-get", "purge", "-y", pkg.name.as_str()]),
        "snap" => ("sudo", vec!["snap", "remove", pkg.name.as_str()]),
        "flatpak" => ("flatpak", vec!["uninstall", "-y", pkg.name.as_str()]),
        _ => {
            return msg(format!(
                "unsupported package source {:?} for {}",
                pkg.source, pkg.name
            ));
        }
    };
    if dry_run {
        let cmd_str = if prog == "sudo" {
            format!("sudo {}", args.join(" "))
        } else {
            format!("{prog} {}", args.join(" "))
        };
        let _ = writeln!(out, "  [dry-run] would run: {cmd_str}");
        return Ok(());
    }
    let res = deps.runner.run(&CommandSpec::new(prog, &args));
    // Go writes result.Stdout→os.Stdout and result.Stderr→os.Stderr
    // unconditionally; RunError keeps the partial output for the same cases.
    let output = match &res {
        Ok(o) => Some(o),
        Err(e) => e.output(),
    };
    if let Some(o) = output {
        let _ = out.write_all(&o.stdout);
        let _ = err_out.write_all(&o.stderr);
    }
    res.map(|_| ())
        .map_err(|e| Error::Msg(format!("{} remove {}: {}", pkg.source, pkg.name, e)))
}

/// `remnantOwnedByRemaining` — true when another installed (not removed)
/// package has the same app identity as `owner` or lists the same path in
/// its own remnants.
fn remnant_owned_by_remaining(
    path: &str,
    owner: &Package,
    installed: &[Package],
    removed: &HashSet<String>,
) -> bool {
    let owner_identity = app_identity(&owner.name);
    for candidate in installed {
        if candidate.key() == owner.key() || removed.contains(&candidate.key()) {
            continue;
        }
        if app_identity(&candidate.name) == owner_identity {
            return true;
        }
        for candidate_path in &candidate.remnants_found {
            if xdg::clean(Path::new(candidate_path)) == xdg::clean(Path::new(path)) {
                return true;
            }
        }
    }
    false
}

/// `appIdentity` — the aliased (or original) name lowercased, so `code`,
/// `vscode`, and `snap`'s `code` share one identity.
fn app_identity(name: &str) -> String {
    match known_alias(name) {
        Some(alias) => alias.to_lowercase(),
        None => name.to_lowercase(),
    }
}

/// `remnantRoot` — the managed user root (`config`/`data`/`cache` home)
/// containing `path`, or an error when it lives outside all three. Go's
/// `root + separator` prefix check can never match a `/` root — the
/// component-wise `starts_with` below guards that case the same way.
fn remnant_root(deps: &Deps, path: &str) -> Result<PathBuf> {
    let clean = xdg::clean(Path::new(path));
    for root in [&deps.config_home, &deps.data_home, &deps.cache_home] {
        if clean == *root || (root.as_os_str() != "/" && clean.starts_with(root)) {
            return Ok(root.clone());
        }
    }
    msg(format!(
        "remnant is outside managed user roots: {}",
        clean.display()
    ))
}

/// `RemovalErrors` — every failed package/remnant outcome joined as
/// `<key>: <err>` lines (`errors.Join` semantics); `None` when all succeeded.
pub fn removal_errors(results: &[RemovalResult]) -> Option<Error> {
    let errs: Vec<String> = results
        .iter()
        .filter_map(|r| {
            r.err
                .as_ref()
                .map(|e| format!("{}: {}", r.package.key(), e))
        })
        .collect();
    if errs.is_empty() {
        None
    } else {
        Some(Error::Msg(errs.join("\n")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::{FakeRunner, RunError};
    use crate::uninstall::test_support::{deps_for_test, tempdir};
    use std::ffi::OsString;
    use std::fs;
    use std::io;
    use std::sync::Arc;

    fn argv(spec: &CommandSpec) -> Vec<String> {
        std::iter::once(spec.program.to_string_lossy().into_owned())
            .chain(spec.args.iter().map(|a| a.to_string_lossy().into_owned()))
            .collect()
    }

    // remove_test.go: TestRemoveSelectedRetainsSharedRemnantAfterAPTFails
    #[test]
    fn remove_selected_retains_shared_remnant_after_apt_fails() {
        let tmp = tempdir("shared");
        let runner = Arc::new(FakeRunner::new());
        runner.set_handler(|spec| {
            if spec.args == ["apt-get", "purge", "-y", "shared"].map(OsString::from) {
                return Err(RunError::Spawn(io::Error::other("dpkg locked")));
            }
            Ok(crate::runner::Output::default())
        });
        let deps = deps_for_test(&tmp, runner.clone());
        let shared = tmp
            .join("config")
            .join("shared")
            .to_string_lossy()
            .into_owned();
        let selected = vec![
            Package {
                name: "shared".to_string(),
                source: "apt".to_string(),
                remnants_found: vec![shared.clone()],
                ..Default::default()
            },
            Package {
                name: "shared".to_string(),
                source: "snap".to_string(),
                remnants_found: vec![shared.clone()],
                ..Default::default()
            },
        ];
        let mut out = Vec::new();
        let mut err_out = Vec::new();
        let results =
            remove_selected_in(&deps, &selected, &selected, false, &mut out, &mut err_out);
        assert_eq!(results.len(), 2, "unexpected results: {results:?}");
        assert!(results[0].err.is_some(), "expected apt removal failure");
        assert!(results[1].removed, "expected snap removal success");
        assert!(
            results[0].remnants_retained.contains(&shared)
                && results[1].remnants_retained.contains(&shared),
            "shared remnant was not retained: {results:?}"
        );
        // `len(calls) != 2 || !slices.Equal(calls[0], ...) || !slices.Equal(...)`
        let calls: Vec<Vec<String>> = runner.invocations().iter().map(argv).collect();
        assert_eq!(calls.len(), 2, "source-scoped calls = {calls:?}");
        assert_eq!(
            calls[0],
            ["sudo", "apt-get", "purge", "-y", "shared"].map(String::from)
        );
        assert_eq!(
            calls[1],
            ["sudo", "snap", "remove", "shared"].map(String::from)
        );
        let err = removal_errors(&results).expect("expected aggregate package error");
        assert!(
            err.to_string().contains("apt:shared"),
            "expected aggregate package error, got {err}"
        );
        fs::remove_dir_all(&tmp).ok();
    }

    // remove_test.go: TestRemoveSelectedDryRunRemovesOnlyManagedUniqueRemnant
    #[test]
    fn remove_selected_dry_run_removes_only_managed_unique_remnant() {
        let home = tempdir("dryrem");
        // Go: HOME + XDG_{CONFIG,DATA,CACHE}_HOME under the tempdir.
        let deps = deps_for_test(&home, Arc::new(FakeRunner::new()));
        let remnant = home.join(".config").join("solo");
        fs::create_dir_all(&remnant).unwrap();
        let remnant_str = remnant.to_string_lossy().into_owned();
        let pkg = Package {
            name: "solo".to_string(),
            source: "apt".to_string(),
            remnants_found: vec![remnant_str.clone()],
            ..Default::default()
        };
        let mut out = Vec::new();
        let mut err_out = Vec::new();
        let results = remove_selected_in(
            &deps,
            std::slice::from_ref(&pkg),
            std::slice::from_ref(&pkg),
            true,
            &mut out,
            &mut err_out,
        );
        if let Some(e) = removal_errors(&results) {
            panic!("unexpected removal errors: {e}");
        }
        assert!(
            results[0].remnants_removed.contains(&remnant_str) && paths::path_exists(&remnant),
            "dry-run result={:?} exists={}",
            results[0],
            paths::path_exists(&remnant)
        );
        // The dry-run preview line Go prints via fmt.Printf.
        let text = String::from_utf8_lossy(&out);
        assert!(
            text.contains("[dry-run] would run: sudo apt-get purge -y solo"),
            "missing dry-run line in {text:?}"
        );
        // `removePackage(Package{Name: "bad", Source: "unknown"}, false)`.
        let bad = Package {
            name: "bad".to_string(),
            source: "unknown".to_string(),
            ..Default::default()
        };
        assert!(
            remove_package(&deps, &bad, false, &mut out, &mut err_out).is_err(),
            "expected unknown source error"
        );
        fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn flatpak_remove_and_systemd_unit_termination() {
        let home = tempdir("flatpak-systemd");
        let runner = Arc::new(FakeRunner::new());
        let deps = deps_for_test(&home, runner.clone());
        let unit_remnant = deps
            .config_home
            .join("systemd")
            .join("user")
            .join("flat-app.service");
        fs::create_dir_all(unit_remnant.parent().unwrap()).unwrap();
        fs::write(&unit_remnant, b"[Unit]\nDescription=Test\n").unwrap();

        let pkg = Package {
            name: "org.example.App".to_string(),
            source: "flatpak".to_string(),
            remnants_found: vec![unit_remnant.to_string_lossy().into_owned()],
            ..Default::default()
        };

        let mut out = Vec::new();
        let mut err_out = Vec::new();
        let results = remove_selected_in(
            &deps,
            std::slice::from_ref(&pkg),
            std::slice::from_ref(&pkg),
            true,
            &mut out,
            &mut err_out,
        );
        assert!(
            removal_errors(&results).is_none(),
            "errors: {:?}",
            removal_errors(&results)
        );
        let text = String::from_utf8_lossy(&out);
        assert!(text.contains("[dry-run] would run: flatpak uninstall -y org.example.App"));
        assert!(text.contains("[dry-run] would run: systemctl --user stop flat-app.service"));
        assert!(text.contains("[dry-run] would run: systemctl --user disable flat-app.service"));

        fs::remove_dir_all(&home).ok();
    }

    // Finding 3 (r1): a real Steam source delegates to Steam's own uninstaller
    // and never trashes the appmanifest.
    #[test]
    fn steam_game_uninstall_delegates_to_steam() {
        let home = tempdir("steam-delegate");
        let runner = Arc::new(FakeRunner::new());
        let deps = deps_for_test(&home, runner.clone());
        let steamapps = deps.data_home.join("Steam").join("steamapps");
        fs::create_dir_all(&steamapps).unwrap();
        fs::write(
            steamapps.join("appmanifest_220.acf"),
            "\"appid\"\t\t\"220\"\n\"name\"\t\t\"Half-Life\"\n",
        )
        .unwrap();
        let pkg = Package {
            name: "Half-Life".to_string(),
            source: "steam".to_string(),
            ..Default::default()
        };

        let mut out = Vec::new();
        let mut err_out = Vec::new();
        let results = remove_selected_in(
            &deps,
            std::slice::from_ref(&pkg),
            std::slice::from_ref(&pkg),
            true,
            &mut out,
            &mut err_out,
        );
        assert!(
            removal_errors(&results).is_none(),
            "errors: {:?}",
            removal_errors(&results)
        );
        let text = String::from_utf8_lossy(&out);
        assert!(
            text.contains("steam steam://uninstall/220"),
            "missing dry-run delegation: {text}"
        );
        assert!(
            steamapps.join("appmanifest_220.acf").exists(),
            "manifest must never be trashed"
        );

        let runner2 = Arc::new(FakeRunner::new());
        runner2.set_look_path("steam", "/usr/bin/steam");
        let deps2 = deps_for_test(&home, runner2.clone());
        let mut out2 = Vec::new();
        let mut err2 = Vec::new();
        let results = remove_selected_in(
            &deps2,
            std::slice::from_ref(&pkg),
            std::slice::from_ref(&pkg),
            false,
            &mut out2,
            &mut err2,
        );
        assert!(removal_errors(&results).is_none());
        let calls: Vec<Vec<String>> = runner2.invocations().iter().map(argv).collect();
        assert!(
            calls
                .iter()
                .any(|c| c == &["steam", "steam://uninstall/220"].map(String::from)),
            "steam not invoked: {calls:?}"
        );
        fs::remove_dir_all(&home).ok();
    }
}
