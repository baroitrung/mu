//! `targets.go` — the built-in clean targets that do not live in a scan_*:
//! `userCache`, `thumbnails`, `fontCache`, `aptCache`, `journalLogs`, plus
//! `JournalSize` and the `fontCacheDirs` helpers. `sandboxFontCache` is
//! post-port behavior with no Go counterpart.
//!
//! Every `*_in` constructor takes [`Deps`] — the Go env/package-var lookups
//! (`XDGCacheHome`, `LoadWhitelist`, `SafeDelete`, `cleanRunner`) as explicit
//! dependencies.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::error::{Error, Result};
use crate::runner::{CommandSpec, Output, Runner};
use crate::{config, oplog, paths, size, trash, whitelist};

use super::{CleanTarget, Deps, scan_f_s};

/// Write a captured stderr buffer through to the process's stderr — Go's
/// `os.Stderr.Write(result.Stderr)` passthrough in the sudo commands.
fn passthrough_stderr(out: &Output) {
    if !out.stderr.is_empty() {
        let _ = std::io::stderr().write_all(&out.stderr);
    }
}

/// `userCacheTarget` — scans `~/.cache` but excludes the `thumbnails` subdir
/// and any path matching `cache_skip` (defaults + user config).
pub(crate) fn user_cache_target_in(deps: &Deps) -> CleanTarget {
    let cache_home = deps.cache_home.clone();
    let thumb_dir = cache_home.join("thumbnails");

    let scan_home = cache_home.clone();
    let scan_thumb = thumb_dir.clone();
    let scan_cfg = deps.config_home.clone();
    let exec_home = cache_home.clone();
    let exec_cfg = deps.config_home.clone();
    let exec_runner = Arc::clone(&deps.trash_runner);
    let exec_deps = deps.trash_deps.clone();

    CleanTarget {
        id: "user-cache",
        label: "User Cache (~/.cache)",
        requires_sudo: false,
        opt_in: false,
        scan: Box::new(move || {
            // Go: `return total, err` — WalkDir aborts at the first error
            // and the partial total rides back with it.
            if let Err(e) = paths::validate_cleanup_root(&scan_home) {
                return (0, Some(e));
            }
            let root_info = match scan_home.symlink_metadata() {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return (0, None),
                Err(e) => return (0, Some(e.into())),
                Ok(info) => info,
            };
            let patterns = match config::load_whitelist_from(&scan_cfg) {
                Ok(wl) => wl.cache_skip.dirs,
                Err(e) => return (0, Some(e)),
            };
            let mut total: i64 = 0;
            if let Err(e) = walk_user_cache(
                &scan_home,
                root_info.is_dir(),
                &scan_thumb,
                &scan_home,
                &patterns,
                &mut total,
            ) {
                return (total, Some(e));
            }
            (total, None)
        }),
        preview: None,
        execute: Box::new(move |dry_run| {
            paths::validate_cleanup_root(&exec_home)?;
            let patterns = config::load_whitelist_from(&exec_cfg)?.cache_skip.dirs;
            // `filepath.Glob(cacheHome/*)` — Glob ignores fs errors, so an
            // unreadable/missing dir yields an empty entry list, and returns
            // matches in lexical order (deterministic error aggregation).
            let mut entries: Vec<PathBuf> = std::fs::read_dir(&exec_home)
                .map(|rd| rd.flatten().map(|e| e.path()).collect())
                .unwrap_or_default();
            entries.sort();
            let mut delete_errors: Vec<Error> = Vec::new();
            for entry in entries {
                let base = entry
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                if base == "thumbnails" {
                    continue;
                }
                if whitelist::should_skip_cache_top_level(&base, &patterns) {
                    continue;
                }
                if whitelist::match_cache_skip(&entry, &exec_home, &patterns) {
                    continue;
                }
                if let Err(e) = paths::validate_cleanup_candidate(&exec_home, &entry) {
                    delete_errors.push(e);
                    continue;
                }
                if let Err(e) =
                    trash::safe_delete_with(&*exec_runner, &exec_cfg, &exec_deps, &entry, dry_run)
                {
                    delete_errors.push(e);
                }
            }
            if delete_errors.is_empty() {
                Ok(())
            } else {
                Err(super::join_errors(&delete_errors))
            }
        }),
        count_fn: None,
    }
}

/// One `filepath.WalkDir` node visit inside `userCacheTarget`'s scan.
/// `is_dir` comes from the parent's `DirEntry` (lstat semantics) — the same
/// value Go's `d.IsDir()` reports without a follow.
fn walk_user_cache(
    path: &Path,
    is_dir: bool,
    thumb_dir: &Path,
    cache_home: &Path,
    patterns: &[String],
    total: &mut i64,
) -> Result<()> {
    // Skip thumbnails entirely to avoid double-counting.
    if path == thumb_dir {
        return Ok(()); // filepath.SkipDir
    }
    if path != cache_home && whitelist::match_cache_skip(path, cache_home, patterns) {
        // dirs → SkipDir; files → skip silently. Same outcome either way.
        return Ok(());
    }
    if !is_dir {
        // `d.Info()` errors are swallowed in Go (`if err == nil`).
        if let Ok(info) = path.symlink_metadata() {
            *total += info.len() as i64;
        }
        return Ok(());
    }
    // `os.ReadDir` order is lexical — sort for parity.
    let mut entries: Vec<_> = std::fs::read_dir(path)?.collect();
    entries.sort_by_key(|e| e.as_ref().map(|x| x.file_name()).unwrap_or_default());
    for entry in entries {
        let entry = entry?; // WalkDir reports entry errors through the callback.
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        // Go's `return filepath.SkipDir` on `path == thumbDir`: on a NON-dir
        // entry SkipDir skips the remaining siblings of the containing dir.
        if !is_dir && entry.path() == thumb_dir {
            break;
        }
        walk_user_cache(
            &entry.path(),
            is_dir,
            thumb_dir,
            cache_home,
            patterns,
            total,
        )?;
    }
    Ok(())
}

/// `thumbnailsTarget` — `~/.cache/thumbnails`.
pub(crate) fn thumbnails_target_in(deps: &Deps) -> CleanTarget {
    let thumb_dir = deps.cache_home.join("thumbnails");

    let scan_thumb = thumb_dir.clone();
    let exec_thumb = thumb_dir;
    let exec_root = deps.cache_home.clone();
    let exec_cfg = deps.config_home.clone();
    let exec_runner = Arc::clone(&deps.trash_runner);
    let exec_deps = deps.trash_deps.clone();

    CleanTarget {
        id: "thumbnails",
        label: "Thumbnail Cache",
        requires_sudo: false,
        opt_in: false,
        scan: Box::new(move || {
            if !paths::path_exists(&scan_thumb) {
                return (0, None);
            }
            // dir_size already returns Go's `(total, errors.Join)` pair.
            let (bytes, err) = size::dir_size(&scan_thumb);
            (bytes as i64, err)
        }),
        preview: None,
        execute: Box::new(move |dry_run| {
            if !paths::path_exists(&exec_thumb) {
                return Ok(());
            }
            paths::validate_cleanup_candidate(&exec_root, &exec_thumb)?;
            trash::safe_delete_with(&*exec_runner, &exec_cfg, &exec_deps, &exec_thumb, dry_run)
        }),
        count_fn: None,
    }
}

/// `aptCacheTarget` — `/var/cache/apt/archives`.
pub(crate) fn apt_cache_target_in(deps: &Deps) -> CleanTarget {
    let runner = Arc::clone(&deps.runner);
    CleanTarget {
        id: "apt-cache",
        label: "APT Package Cache",
        requires_sudo: true,
        opt_in: false,
        scan: Box::new(move || {
            let (bytes, _) = size::dir_size(Path::new("/var/cache/apt/archives"));
            (bytes as i64, None)
        }),
        preview: None,
        execute: Box::new(move |dry_run| {
            if dry_run {
                oplog::log_outcome("apt-clean", "apt cache", "dry-run");
                return Ok(());
            }
            match runner.run(&CommandSpec::new("sudo", ["apt-get", "clean"])) {
                Ok(out) => {
                    passthrough_stderr(&out);
                    Ok(())
                }
                Err(e) => {
                    if let Some(out) = e.output() {
                        passthrough_stderr(out);
                    }
                    Err(Error::Msg(e.to_string()))
                }
            }
        }),
        count_fn: None,
    }
}

/// `journalLogsTarget` — systemd journal logs (vacuum older than 30 days).
pub(crate) fn journal_logs_target_in(deps: &Deps) -> CleanTarget {
    let scan_runner = Arc::clone(&deps.runner);
    let exec_runner = Arc::clone(&deps.runner);
    CleanTarget {
        id: "journal-logs",
        label: "Journal Logs",
        requires_sudo: true,
        opt_in: false,
        scan: Box::new(move || match journal_size(&*scan_runner) {
            Ok(n) => (n, None),
            Err(e) => (0, Some(e)),
        }),
        preview: None,
        execute: Box::new(move |dry_run| {
            if dry_run {
                oplog::log_outcome("journal-vacuum", "30d", "dry-run");
                return Ok(());
            }
            match exec_runner.run(&CommandSpec::new(
                "sudo",
                ["journalctl", "--vacuum-time=30d"],
            )) {
                Ok(out) => {
                    passthrough_stderr(&out);
                    oplog::log_outcome("journal-vacuum", "30d", "success");
                    Ok(())
                }
                Err(e) => {
                    if let Some(out) = e.output() {
                        passthrough_stderr(out);
                    }
                    Err(Error::Msg(format!("journalctl vacuum: {e}")))
                }
            }
        }),
        count_fn: None,
    }
}

/// `fontCacheTarget` — clears fontconfig caches for every user: the system
/// cache plus `.cache/fontconfig` and legacy `.fontconfig` in each home
/// under `/home` and `/root`. Removal goes through sudo — trashing is
/// impossible for other users' files — and fontconfig regenerates the cache
/// on demand.
pub(crate) fn font_cache_target_in(deps: &Deps) -> CleanTarget {
    let runner = Arc::clone(&deps.runner);
    let cache_home = deps.cache_home.clone();
    let home_root = PathBuf::from("/home");

    let scan_cache = cache_home.clone();
    let scan_root = home_root.clone();
    let prev_cache = cache_home.clone();
    let prev_root = home_root.clone();
    let exec_cache = cache_home;
    let exec_root = home_root;

    CleanTarget {
        id: "font-cache",
        label: "Font Cache (all users)",
        requires_sudo: true,
        opt_in: false,
        scan: Box::new(move || {
            let mut total: i64 = 0;
            for dir in font_cache_dirs_in(&scan_root, &scan_cache) {
                let (bytes, _) = size::dir_size(&dir); // unreadable dirs count as 0
                total += bytes as i64;
            }
            (total, None)
        }),
        preview: Some(Box::new(move || {
            (
                font_cache_dirs_in(&prev_root, &prev_cache)
                    .iter()
                    .map(|d| d.to_string_lossy().into_owned())
                    .collect(),
                None,
            )
        })),
        execute: Box::new(move |dry_run| {
            let dirs = font_cache_dirs_in(&exec_root, &exec_cache);
            if dirs.is_empty() {
                return Ok(());
            }
            if dry_run {
                oplog::log_outcome("font-cache", &format!("{} dirs", dirs.len()), "dry-run");
                return Ok(());
            }
            // Homes with 0700 perms are not traversable unprivileged, so
            // existence is re-checked as root; a failed dir doesn't abort
            // the rest. -mindepth 1 clears contents but keeps each cache dir.
            let mut args: Vec<std::ffi::OsString> = vec![
                "sh".into(),
                "-c".into(),
                r#"rc=0; for d in "$@"; do [ -d "$d" ] || continue; find "$d" -mindepth 1 -delete || rc=1; done; exit $rc"#
                    .into(),
                "mu-font-cache".into(),
            ];
            args.extend(dirs.iter().map(|d| d.as_os_str().to_os_string()));
            match runner.run(&CommandSpec::new("sudo", args)) {
                Ok(out) => {
                    passthrough_stderr(&out);
                    oplog::log_outcome("font-cache", "all users", "success");
                    Ok(())
                }
                Err(e) => {
                    if let Some(out) = e.output() {
                        passthrough_stderr(out);
                    }
                    Err(Error::Msg(format!("font cache clean: {e}")))
                }
            }
        }),
        count_fn: None,
    }
}

/// `fontCacheDirs` — fontconfig cache directories to clean. Candidates that
/// definitely don't exist are dropped; unreadable ones are kept — the
/// sudo-side `[ -d ]` gate decides at execute time. The current user's
/// `~/.cache/fontconfig` is skipped: the user-cache target already owns it
/// and would double-count its size.
pub(crate) fn font_cache_dirs(cache_home: &Path) -> Vec<PathBuf> {
    font_cache_dirs_in(Path::new("/home"), cache_home)
}

/// `fontCacheDirsIn(homeRoot)` — the testable core: `home_root` replaces the
/// hardcoded `/home`, `cache_home` is the current user's `XDGCacheHome()`.
pub(crate) fn font_cache_dirs_in(home_root: &Path, cache_home: &Path) -> Vec<PathBuf> {
    let mut candidates: Vec<PathBuf> = vec![
        PathBuf::from("/var/cache/fontconfig"),
        PathBuf::from("/root/.cache/fontconfig"),
        PathBuf::from("/root/.fontconfig"),
    ];
    if let Ok(entries) = std::fs::read_dir(home_root) {
        // `os.ReadDir` returns entries sorted by filename — sort for the
        // same deterministic preview/argv order on multi-home systems.
        let mut homes: Vec<_> = entries.flatten().collect();
        homes.sort_by_key(|e| e.file_name());
        for e in homes {
            // `e.IsDir()` — readdir type bits, a symlink is NOT a dir.
            if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                let home = home_root.join(e.file_name());
                candidates.push(home.join(".cache").join("fontconfig"));
                candidates.push(home.join(".fontconfig"));
            }
        }
    }
    let own_font_dir = cache_home.join("fontconfig");
    let mut dirs = Vec::new();
    for dir in candidates {
        if dir == own_font_dir {
            continue;
        }
        match dir.symlink_metadata() {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => dirs.push(dir), // unreadable — kept for the sudo-side gate
            Ok(info) => {
                if !info.is_dir() || info.file_type().is_symlink() {
                    continue;
                }
                dirs.push(dir);
            }
        }
    }
    dirs
}

/// Sandboxed app homes whose fontconfig caches `mu` cleans, as
/// `(root under home, cache path relative to the app dir)`.
const SANDBOX_CACHE_ROOTS: &[(&str, &str)] = &[
    ("snap", "common/.cache/fontconfig"),
    (".var/app", "cache/fontconfig"),
];

/// `sandboxFontCacheTarget` — fontconfig caches inside sandboxed app homes:
/// `~/snap/*/common/.cache/fontconfig` (snap) and
/// `~/.var/app/*/cache/fontconfig` (flatpak). These are the current user's
/// own files, so unlike `fontCacheTarget` removal goes through `SafeDelete`
/// (trash) and needs no sudo.
pub(crate) fn sandbox_font_cache_target_in(deps: &Deps) -> CleanTarget {
    let scan_home = deps.home.clone();
    let prev_home = deps.home.clone();
    let exec_home = deps.home.clone();
    let exec_cfg = deps.config_home.clone();
    let exec_runner = Arc::clone(&deps.trash_runner);
    let exec_deps = deps.trash_deps.clone();

    CleanTarget {
        id: "sandbox-font-cache",
        label: "Sandboxed Font Caches (snap/flatpak)",
        requires_sudo: false,
        opt_in: false,
        scan: Box::new(move || {
            let mut total: i64 = 0;
            for (_, dir) in sandbox_font_cache_dirs_in(&scan_home) {
                let (bytes, _) = size::dir_size(&dir); // unreadable dirs count as 0
                total += bytes as i64;
            }
            (total, None)
        }),
        preview: Some(Box::new(move || {
            (
                sandbox_font_cache_dirs_in(&prev_home)
                    .into_iter()
                    .map(|(_, dir)| dir.to_string_lossy().into_owned())
                    .collect(),
                None,
            )
        })),
        execute: Box::new(move |dry_run| {
            let mut delete_errors: Vec<Error> = Vec::new();
            for (root, dir) in sandbox_font_cache_dirs_in(&exec_home) {
                // Each candidate is validated against the boundary it was
                // discovered under, so a symlinked `~/snap` or `~/.var/app`
                // fails closed instead of deleting outside it.
                if let Err(e) = paths::validate_cleanup_candidate(&root, &dir) {
                    delete_errors.push(e);
                    continue;
                }
                if let Err(e) =
                    trash::safe_delete_with(&*exec_runner, &exec_cfg, &exec_deps, &dir, dry_run)
                {
                    delete_errors.push(e);
                }
            }
            if delete_errors.is_empty() {
                Ok(())
            } else {
                Err(super::join_errors(&delete_errors))
            }
        }),
        count_fn: None,
    }
}

/// `sandboxFontCacheDirs` — the testable core: `home` replaces
/// `os.UserHomeDir()`. Returns `(root, dir)` pairs so the execute path can
/// validate each candidate against the boundary it was discovered under.
/// Only real directories qualify — a missing or symlinked cache is skipped.
pub(crate) fn sandbox_font_cache_dirs_in(home: &Path) -> Vec<(PathBuf, PathBuf)> {
    let mut found = Vec::new();
    for &(root_rel, cache_rel) in SANDBOX_CACHE_ROOTS {
        let root = home.join(root_rel);
        let Ok(entries) = std::fs::read_dir(&root) else {
            continue; // no snap/flatpak apps for this user
        };
        // `os.ReadDir` returns entries sorted by filename — sort for the
        // same deterministic preview order on multi-app systems.
        let mut apps: Vec<_> = entries.flatten().collect();
        apps.sort_by_key(|e| e.file_name());
        for app in apps {
            // `e.IsDir()` — readdir type bits, a symlink is NOT a dir.
            if !app.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let dir = root.join(app.file_name()).join(cache_rel);
            match dir.symlink_metadata() {
                Ok(info) if info.is_dir() && !info.file_type().is_symlink() => {
                    found.push((root.clone(), dir));
                }
                _ => {}
            }
        }
    }
    found
}

/// `count_trash_items` — counts entries in `$XDG_DATA_HOME/Trash/files`.
pub(crate) fn count_trash_items(data_home: &Path) -> usize {
    let files_dir = data_home.join("Trash").join("files");
    let Ok(entries) = std::fs::read_dir(&files_dir) else {
        return 0;
    };
    entries.flatten().count()
}

/// `system_trash_size` — total bytes in `$XDG_DATA_HOME/Trash`.
pub(crate) fn system_trash_size(data_home: &Path) -> i64 {
    let trash_dir = data_home.join("Trash");
    if !trash_dir.exists() {
        return 0;
    }
    let (sz, _) = size::dir_size(&trash_dir);
    sz as i64
}

/// `systemTrashTarget` — CleanTarget for system trash (opt-in).
/// Previews item count and size; executes strictly via `gio trash --empty`.
pub(crate) fn system_trash_target_in(deps: &Deps) -> CleanTarget {
    let scan_data = deps.data_home.clone();
    let exec_runner = Arc::clone(&deps.runner);
    let count_data = deps.data_home.clone();

    CleanTarget {
        id: "system-trash",
        label: "System Trash",
        requires_sudo: false,
        opt_in: true,
        scan: Box::new(move || {
            let sz = system_trash_size(&scan_data);
            (sz, None)
        }),
        preview: None,
        execute: Box::new(move |dry_run| {
            if dry_run {
                return Ok(());
            }
            // Fail closed: without `gio` the trash cannot be emptied, so this
            // must be a failure — not a silent no-op that `execute` would
            // still credit as freed space.
            if exec_runner.look_path("gio").is_err() {
                return Err(Error::Msg(
                    "gio not found: cannot empty system trash".to_string(),
                ));
            }
            exec_runner
                .run(&CommandSpec::new("gio", ["trash", "--empty"]))
                .map(|_| ())
                .map_err(|e| Error::Msg(format!("empty trash: {e}")))
        }),
        count_fn: Some(Box::new(move || {
            let count = count_trash_items(&count_data);
            format!(
                "System Trash ({count} item{})",
                if count == 1 { "" } else { "s" }
            )
        })),
    }
}

/// The kind of an external-volume candidate.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum ExternalCandidate {
    /// `<mount>/.Trash-<uid>` — a per-volume trash directory. It cannot be
    /// trashed into itself; its contents are emptied (FreeDesktop traversal,
    /// the same permanent semantics as `gio trash --empty`).
    TrashDir,
    /// `.directory` — plain metadata, recoverable via `SafeDelete`.
    Metadata,
}

fn classify_external(name: &str) -> Option<ExternalCandidate> {
    if name.starts_with(".Trash-") {
        Some(ExternalCandidate::TrashDir)
    } else if name == ".directory" {
        Some(ExternalCandidate::Metadata)
    } else {
        None
    }
}

/// `externalVolumeEntries` — removable metadata directly under an external
/// mount: `.Trash-<uid>` directories and `.directory` files.
pub(crate) fn external_volume_entries(mount: &Path) -> Vec<(PathBuf, ExternalCandidate)> {
    let mut found = Vec::new();
    let Ok(entries) = std::fs::read_dir(mount) else {
        return found;
    };
    let mut entries: Vec<_> = entries.flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let name = e.file_name();
        let name = name.to_string_lossy();
        if let Some(kind) = classify_external(&name) {
            found.push((e.path(), kind));
        }
    }
    found
}

/// `emptyTrashDir` — permanently remove the entries under a FreeDesktop trash
/// directory's `files/`, `info/`, and `expunged/` children. The trash directory
/// itself is never removed.
///
/// The volume is untrusted, so each subdirectory is lstat'd first: a symlinked
/// (or non-directory) `files/`/`info/`/`expunged/` is refused, never traversed.
/// `read_dir` would otherwise follow `<mount>/.Trash-<uid>/files -> $HOME` and
/// permanently delete everything outside the volume. Entries *inside* a real
/// subdir are unlinked individually (symlinks are removed, not followed).
fn empty_trash_dir(trash_root: &Path) -> Result<()> {
    let mut errors: Vec<Error> = Vec::new();
    for sub in ["files", "info", "expunged"] {
        let dir = trash_root.join(sub);
        match std::fs::symlink_metadata(&dir) {
            Ok(m) if m.file_type().is_symlink() => {
                errors.push(Error::Msg(format!(
                    "refusing symlinked trash subdir {}",
                    dir.display()
                )));
                continue;
            }
            // A non-directory where a subdir should be is not traversed.
            Ok(m) if !m.is_dir() => continue,
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                errors.push(Error::Msg(format!("inspect {}: {e}", dir.display())));
                continue;
            }
        }
        // `read_dir` on `dir` is now safe: `dir` is a real directory (lstat).
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in entries.flatten() {
            let p = e.path();
            // DirEntry::file_type never follows symlinks.
            let res = match e.file_type() {
                Ok(t) if t.is_dir() => std::fs::remove_dir_all(&p),
                Ok(_) => std::fs::remove_file(&p),
                Err(err) => Err(err),
            };
            if let Err(err) = res {
                errors.push(Error::Msg(format!("empty {}: {err}", p.display())));
            }
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(super::join_errors(&errors))
    }
}

/// `externalVolumeTarget` — clean target added by `mu clean --external <PATH>`:
/// `.directory` metadata is trashed via `SafeDelete`; `.Trash-*` directories
/// are emptied in place (their contents are permanently removed, never trashed
/// into themselves).
pub(crate) fn external_volume_target_in(deps: &Deps, mount: &Path) -> CleanTarget {
    let scan_mount = mount.to_path_buf();
    let prev_mount = mount.to_path_buf();
    let exec_mount = mount.to_path_buf();
    let exec_cfg = deps.config_home.clone();
    let exec_runner = Arc::clone(&deps.trash_runner);
    let exec_deps = deps.trash_deps.clone();

    CleanTarget {
        id: "external-volume",
        label: "External Volume Metadata",
        requires_sudo: false,
        // Only ever present when `--external` names the mount.
        opt_in: true,
        scan: Box::new(move || {
            let mut total = 0i64;
            let mut scan_errors: Vec<Error> = Vec::new();
            for (p, _) in external_volume_entries(&scan_mount) {
                let (sz, err) = match std::fs::symlink_metadata(&p) {
                    Ok(m) if m.is_dir() => {
                        let (sz, err) = size::dir_size(&p);
                        (sz as i64, err)
                    }
                    Ok(m) => (m.len() as i64, None),
                    Err(e) => (0, Some(e.into())),
                };
                match err {
                    None => total += sz,
                    Some(e) => scan_errors.push(e),
                }
            }
            if scan_errors.is_empty() {
                (total, None)
            } else {
                (total, Some(super::join_errors(&scan_errors)))
            }
        }),
        preview: Some(Box::new(move || {
            (
                external_volume_entries(&prev_mount)
                    .into_iter()
                    .map(|(p, _)| p.to_string_lossy().into_owned())
                    .collect(),
                None,
            )
        })),
        execute: Box::new(move |dry_run| {
            paths::validate_cleanup_root(&exec_mount)?;
            let mut delete_errors: Vec<Error> = Vec::new();
            for (p, kind) in external_volume_entries(&exec_mount) {
                if let Err(e) = paths::validate_cleanup_candidate(&exec_mount, &p) {
                    delete_errors.push(e);
                    continue;
                }
                match kind {
                    ExternalCandidate::Metadata => {
                        if let Err(e) = trash::safe_delete_with(
                            &*exec_runner,
                            &exec_cfg,
                            &exec_deps,
                            &p,
                            dry_run,
                        ) {
                            delete_errors.push(e);
                        }
                    }
                    ExternalCandidate::TrashDir => {
                        if dry_run {
                            oplog::log_outcome("empty-trash", &p.to_string_lossy(), "dry-run");
                        } else {
                            match empty_trash_dir(&p) {
                                Ok(()) => oplog::log_outcome(
                                    "empty-trash",
                                    &p.to_string_lossy(),
                                    "success",
                                ),
                                Err(e) => {
                                    oplog::log_outcome(
                                        "empty-trash",
                                        &p.to_string_lossy(),
                                        "failure",
                                    );
                                    delete_errors.push(e);
                                }
                            }
                        }
                    }
                }
            }
            if delete_errors.is_empty() {
                Ok(())
            } else {
                Err(super::join_errors(&delete_errors))
            }
        }),
        count_fn: None,
    }
}

/// `JournalSize` — parses `journalctl --disk-usage` to get journal size in
/// bytes. A missing journalctl is optional; command and parse failures are
/// returned.
pub fn journal_size(runner: &dyn Runner) -> Result<i64> {
    if runner.look_path("journalctl").is_err() {
        return Ok(0);
    }
    let out = runner
        .run(&CommandSpec::new(
            "env",
            ["LC_ALL=C", "journalctl", "--disk-usage"],
        ))
        .map_err(|e| Error::Msg(e.to_string()))?;
    let (val, unit) = parse_journal_disk_usage(&out.stdout_lossy())
        .map_err(|e| Error::Msg(format!("parse journal disk usage: {e}")))?;
    match unit.as_bytes().first() {
        Some(b'G') => Ok((val * 1024.0 * 1024.0 * 1024.0) as i64),
        Some(b'M') => Ok((val * 1024.0 * 1024.0) as i64),
        Some(b'K') => Ok((val * 1024.0) as i64),
        // Go: fmt.Errorf("unknown journal size unit: %q", TrimSpace(unit))
        _ => Err(Error::Msg(format!(
            "unknown journal size unit: {:?}",
            unit.trim()
        ))),
    }
}

/// `fmt.Sscanf(out, "Archived and active journals take up %f%s")` — the
/// literal matches from position 0 with each format space accepting any
/// whitespace run; then a float and a whitespace-free token. Errors carry
/// the Go scanner's texts: `input does not match format`,
/// `strconv.ParseFloat: parsing "": invalid syntax`, `EOF`.
fn parse_journal_disk_usage(out: &str) -> std::result::Result<(f64, String), String> {
    const LIT: &str = "Archived and active journals take up";
    let bytes = out.as_bytes();
    let mut i = 0usize;
    for ch in LIT.chars() {
        if ch == ' ' {
            // A space in the format consumes any whitespace (including none).
            while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                i += 1;
            }
        } else {
            if i >= bytes.len() {
                return Err("input does not match format".to_string());
            }
            if bytes[i] != ch as u8 {
                return Err("input does not match format".to_string());
            }
            i += 1;
        }
    }
    let rest = &out[i..];
    if rest.trim_start().is_empty() {
        return Err("EOF".to_string());
    }
    let (val, unit, n) = scan_f_s(rest);
    match n {
        0 => Err("strconv.ParseFloat: parsing \"\": invalid syntax".to_string()),
        1 => Err("EOF".to_string()),
        _ => Ok((val, unit)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clean::test_support::*;
    use crate::oplog;
    use crate::runner::{FakeRunner, Output};
    use std::fs;
    use std::os::unix::fs::symlink;

    // targets_cache_test.go: TestUserCacheTarget_skipsDenylisted
    #[test]
    fn user_cache_target_skips_denylisted() {
        let tmp = tempdir("ucache");
        let cache_home = tmp.join(".cache");

        // Safe-to-delete junk.
        let junk = cache_home.join("some-app");
        fs::create_dir_all(&junk).unwrap();
        fs::write(junk.join("a.bin"), "junk data here").unwrap();

        // Denylisted go-build (must not be counted or deleted).
        let go_build = cache_home.join("go-build");
        fs::create_dir_all(&go_build).unwrap();
        fs::write(go_build.join("pkg"), "important build cache").unwrap();

        let deps = deps_for_test(&tmp, Arc::new(FakeRunner::new()));
        let target = user_cache_target_in(&deps);
        let sz = (target.scan)().0;
        // Only the junk file size should count.
        assert!(
            sz >= "junk data here".len() as i64,
            "expected scan to include junk size, got {sz}"
        );
        assert!(
            sz <= "junk data here".len() as i64 + 64,
            "scan size {sz} looks like it included go-build"
        );

        (target.execute)(true).expect("dry-run execute");
        assert!(
            paths::path_exists(&go_build),
            "go-build must remain after dry-run"
        );
        assert!(paths::path_exists(&junk), "junk must remain after dry-run");

        // Non-dry-run: junk trashed, go-build kept.
        (target.execute)(false).expect("execute");
        assert!(
            paths::path_exists(&go_build),
            "go-build must not be deleted"
        );
        assert!(
            !paths::path_exists(&junk),
            "junk cache entry should have been trashed"
        );
        fs::remove_dir_all(&tmp).ok();
    }

    // safety_test.go: TestUserCacheRejectsTopLevelSymlinkInDryRun
    #[test]
    fn user_cache_rejects_top_level_symlink_in_dry_run() {
        let tmp = tempdir("usymlink");
        let cache_root = tmp.join(".cache");
        fs::create_dir_all(&cache_root).unwrap();
        let outside = tmp.join("keep");
        fs::write(&outside, "keep").unwrap();
        symlink(&outside, cache_root.join("ambiguous")).unwrap();

        let deps = deps_for_test(&tmp, Arc::new(FakeRunner::new()));
        let err =
            (user_cache_target_in(&deps).execute)(true).expect_err("expected symlink failure");
        assert!(
            err.to_string().contains("symlink"),
            "expected symlink failure, got {err}"
        );
        assert!(paths::path_exists(&outside), "symlink target changed");
        fs::remove_dir_all(&tmp).ok();
    }

    // safety_test.go: TestUserCacheScanAllowsMissingRoot
    #[test]
    fn user_cache_scan_allows_missing_root() {
        let tmp = tempdir("umissing");
        let deps = deps_for_test(&tmp, Arc::new(FakeRunner::new()));
        let mut deps_missing = deps;
        deps_missing.cache_home = tmp.join("missing-cache");
        let size = (user_cache_target_in(&deps_missing).scan)().0;
        assert_eq!(size, 0, "size={size}");
        fs::remove_dir_all(&tmp).ok();
    }

    // targets_fontcache_test.go: TestFontCacheDirs_enumeratesHomes
    #[test]
    fn font_cache_dirs_enumerates_homes() {
        let tmp = tempdir("fontdirs");
        let homes = tmp.join("home");

        // alice: real font cache (excluded — it is the current user's XDG
        // cache) plus a legacy .fontconfig dir (included).
        let alice_cache = homes.join("alice").join(".cache");
        let alice_font = alice_cache.join("fontconfig");
        let alice_legacy = homes.join("alice").join(".fontconfig");
        for d in [&alice_font, &alice_legacy] {
            fs::create_dir_all(d).unwrap();
        }
        fs::write(alice_font.join("x.cache"), "data").unwrap();

        // bob: no font dirs — contributes nothing.
        fs::create_dir_all(homes.join("bob")).unwrap();

        // carol: fontconfig is a symlink — must be skipped.
        let carol_cache = homes.join("carol").join(".cache");
        fs::create_dir_all(&carol_cache).unwrap();
        symlink(&alice_font, carol_cache.join("fontconfig")).unwrap();

        // link: symlinked home dir — not enumerated.
        symlink(homes.join("alice"), homes.join("link")).unwrap();

        let dirs = font_cache_dirs_in(&homes, &alice_cache);

        assert!(
            dirs.contains(&alice_legacy),
            "expected legacy dir {alice_legacy:?} in {dirs:?}"
        );
        for excluded in [
            alice_font, // owned by user-cache target
            homes.join("bob").join(".cache").join("fontconfig"),
            homes.join("bob").join(".fontconfig"),
            carol_cache.join("fontconfig"), // symlink
            homes.join("link").join(".cache").join("fontconfig"),
        ] {
            assert!(
                !dirs.contains(&excluded),
                "unexpected dir {excluded:?} in {dirs:?}"
            );
        }
        fs::remove_dir_all(&tmp).ok();
    }

    // targets_fontcache_test.go: TestFontCacheTarget_dryRunKeepsDirs
    #[test]
    fn font_cache_target_dry_run_keeps_dirs() {
        let tmp = tempdir("fontdry");
        let deps = deps_for_test(&tmp, Arc::new(FakeRunner::new()));
        // Go's test runs utils.InitLogger so LogOutcome has somewhere to
        // write; the ported logger takes the data home explicitly.
        oplog::init_logger_at(&deps.data_home).expect("init logger");
        let target = font_cache_target_in(&deps);
        (target.execute)(true).expect("dry-run execute");
        oplog::close_logger();
        // Go then checks PathExists("/var/cache/fontconfig") and skips when
        // absent — presence on this host is informational either way.
        let _ = paths::path_exists(Path::new("/var/cache/fontconfig"));
        fs::remove_dir_all(&tmp).ok();
    }

    // Post-port behavior (no Go oracle): sandboxed app font caches.
    #[test]
    fn sandbox_font_cache_dirs_enumerates_snap_and_flatpak() {
        let tmp = tempdir("sandboxdirs");
        let home = tmp.join("home");

        let snap_firefox = home.join("snap/firefox/common/.cache/fontconfig");
        let snap_thunderbird = home.join("snap/thunderbird/common/.cache/fontconfig");
        let flatpak_telegram = home.join(".var/app/org.telegram.desktop/cache/fontconfig");
        for d in [&snap_firefox, &snap_thunderbird, &flatpak_telegram] {
            fs::create_dir_all(d).unwrap();
        }

        // Noise: a plain file in ~/snap, an app without the cache path, a
        // symlinked cache dir, and a symlinked app dir.
        fs::write(home.join("snap/README"), "not an app").unwrap();
        fs::create_dir_all(home.join("snap/snapd/common")).unwrap();
        fs::create_dir_all(home.join("snap/gimp/common/.cache")).unwrap();
        let outside = tmp.join("outside-fontconfig");
        fs::create_dir_all(&outside).unwrap();
        symlink(&outside, home.join("snap/gimp/common/.cache/fontconfig")).unwrap();
        fs::create_dir_all(home.join(".var/app/org.example.App/cache")).unwrap();
        symlink(
            &snap_firefox,
            home.join(".var/app/org.example.App/cache/fontconfig"),
        )
        .unwrap();

        let found = sandbox_font_cache_dirs_in(&home);
        let dirs: Vec<_> = found.iter().map(|(_, d)| d.clone()).collect();

        assert_eq!(
            dirs,
            vec![
                snap_firefox.clone(),
                snap_thunderbird.clone(),
                flatpak_telegram.clone()
            ],
            "unexpected discovery: {dirs:?}"
        );
        // Each candidate carries the boundary it was discovered under.
        assert_eq!(found[0].0, home.join("snap"), "snap root");
        assert_eq!(found[2].0, home.join(".var/app"), "flatpak root");
        fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn sandbox_font_cache_target_trashes_caches_and_keeps_siblings() {
        let tmp = tempdir("sandboxtrash");
        // `deps_for_test` roots home at the tempdir itself.
        let home = tmp.clone();
        let snap_cache = home.join("snap/firefox/common/.cache/fontconfig");
        let flatpak_cache = home.join(".var/app/org.telegram.desktop/cache/fontconfig");
        for d in [&snap_cache, &flatpak_cache] {
            fs::create_dir_all(d).unwrap();
            fs::write(d.join("f.cache-7"), "font cache data").unwrap();
        }
        // Sibling data in the same sandbox home must survive.
        let snap_state = home.join("snap/firefox/common/.local/share/keep/state");
        fs::create_dir_all(snap_state.parent().unwrap()).unwrap();
        fs::write(&snap_state, "keep me").unwrap();

        let deps = deps_for_test(&tmp, Arc::new(FakeRunner::new()));
        oplog::init_logger_at(&deps.data_home).expect("init logger");
        let target = sandbox_font_cache_target_in(&deps);

        let scanned = (target.scan)().0;
        assert!(
            scanned >= 2 * "font cache data".len() as i64,
            "scan size {scanned} missed the caches"
        );
        let preview = target.preview.as_ref().expect("preview");
        assert_eq!((preview)().0.len(), 2, "preview should list both caches");

        (target.execute)(true).expect("dry-run execute");
        assert!(
            paths::path_exists(&snap_cache),
            "dry-run must keep the snap cache"
        );
        assert!(
            paths::path_exists(&flatpak_cache),
            "dry-run must keep the flatpak cache"
        );

        (target.execute)(false).expect("execute");
        oplog::close_logger();
        assert!(
            !paths::path_exists(&snap_cache),
            "snap font cache should have been trashed"
        );
        assert!(
            !paths::path_exists(&flatpak_cache),
            "flatpak font cache should have been trashed"
        );
        assert!(
            paths::path_exists(&snap_state),
            "sibling sandbox data must survive"
        );
        fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn sandbox_font_cache_scan_allows_missing_roots() {
        let tmp = tempdir("sandboxmissing");
        let deps = deps_for_test(&tmp, Arc::new(FakeRunner::new()));
        let target = sandbox_font_cache_target_in(&deps);
        assert_eq!((target.scan)().0, 0, "no sandboxed apps means no size");
        (target.execute)(true).expect("dry-run with no sandboxed apps");
        (target.execute)(false).expect("execute with no sandboxed apps");
        fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn sandbox_font_cache_rejects_symlinked_root() {
        let tmp = tempdir("sandboxsymlink");
        let real = tmp.join("real-snap");
        let cache = real.join("firefox/common/.cache/fontconfig");
        fs::create_dir_all(&cache).unwrap();
        fs::write(cache.join("f.cache-7"), "font cache data").unwrap();
        // `deps_for_test` roots home at the tempdir, so ~/snap is tmp/snap.
        symlink(&real, tmp.join("snap")).unwrap();

        let deps = deps_for_test(&tmp, Arc::new(FakeRunner::new()));
        let err = (sandbox_font_cache_target_in(&deps).execute)(false)
            .expect_err("a symlinked root must fail closed");
        assert!(
            err.to_string().contains("symlink"),
            "expected symlink refusal, got {err}"
        );
        assert!(
            paths::path_exists(&cache.join("f.cache-7")),
            "cache outside the boundary must survive"
        );
        fs::remove_dir_all(&tmp).ok();
    }

    // safety_test.go: TestJournalSizeOptionalAndParseErrors
    #[test]
    fn journal_size_optional_and_parse_errors() {
        // journalctl missing → optional, (0, nil).
        let runner = FakeRunner::new();
        runner.set_handler(|_| Ok(Output::default()));
        let size = journal_size(&runner).expect("optional journal size");
        assert_eq!(size, 0, "optional journal size={size}");

        // Unparseable output → parse error.
        let runner = FakeRunner::new();
        runner.set_look_path("journalctl", "/usr/bin/journalctl");
        runner.set_handler(|_| {
            Ok(Output {
                stdout: b"unparseable".to_vec(),
                ..Default::default()
            })
        });
        assert!(journal_size(&runner).is_err(), "expected parse error");
    }

    // safety_test.go: TestJournalTargetUsesSudoOnlyForRealVacuum
    #[test]
    fn journal_target_uses_sudo_only_for_real_vacuum() {
        let tmp = tempdir("jsudo");
        let runner = Arc::new(FakeRunner::new());
        runner.set_handler(|_| Ok(Output::default()));
        let deps = deps_for_test(&tmp, Arc::clone(&runner) as Arc<dyn Runner>);
        let target = journal_logs_target_in(&deps);
        assert!(target.requires_sudo, "journal target must advertise sudo");
        (target.execute)(true).expect("dry-run execute");
        assert!(
            runner.invocations().is_empty(),
            "dry-run executed command: {:?}",
            runner.invocations()
        );
        (target.execute)(false).expect("real execute");
        let calls = runner.invocations();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].program, "sudo");
        assert_eq!(
            calls[0].args,
            ["journalctl", "--vacuum-time=30d"].map(std::ffi::OsString::from)
        );
        fs::remove_dir_all(&tmp).ok();
    }

    // safety_test.go: TestJournalSizeForcesCLocale
    #[test]
    fn journal_size_forces_c_locale() {
        let runner = FakeRunner::new();
        runner.set_look_path("journalctl", "/usr/bin/journalctl");
        runner.set_handler(|_| {
            Ok(Output {
                stdout: b"Archived and active journals take up 2.0M in the file system.\n".to_vec(),
                ..Default::default()
            })
        });
        let size = journal_size(&runner).expect("journal size");
        assert_eq!(size, 2 * 1024 * 1024, "size={size}");
        let calls = runner.invocations();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].program, "env");
        assert_eq!(
            calls[0].args,
            ["LC_ALL=C", "journalctl", "--disk-usage"].map(std::ffi::OsString::from)
        );
    }

    // Finding 5: without `gio` the trash target must fail instead of reporting
    // a silent no-op success that `execute` credits as freed space.
    #[test]
    fn system_trash_fails_closed_without_gio() {
        let tmp = tempdir("trash-nogio");
        let deps = deps_for_test(&tmp, Arc::new(FakeRunner::new())); // gio missing
        let target = system_trash_target_in(&deps);
        let err = (target.execute)(false).expect_err("expected gio-missing failure");
        assert!(
            err.to_string().contains("gio not found"),
            "unexpected error: {err}"
        );
        fs::remove_dir_all(&tmp).ok();
    }

    // Finding 2 (review r1): `--external <PATH>` must empty `.Trash-*`
    // in place (never trash the trash into itself) and SafeDelete `.directory`.
    #[test]
    fn external_volume_target_cleans_metadata() {
        let tmp = tempdir("external");
        let mount = tmp.join("mnt");
        let trash = mount.join(".Trash-1000");
        fs::create_dir_all(trash.join("files")).unwrap();
        fs::create_dir_all(trash.join("info")).unwrap();
        fs::write(trash.join("files").join("junk"), "x").unwrap();
        fs::write(trash.join("info").join("junk.trashinfo"), "meta").unwrap();
        fs::write(mount.join(".directory"), "meta").unwrap();
        let runner = Arc::new(FakeRunner::new());
        let deps = deps_for_test(&tmp, runner);
        let target = external_volume_target_in(&deps, &mount);
        let (sz, err) = (target.scan)();
        assert!(err.is_none(), "scan error: {err:?}");
        assert!(sz > 0, "expected non-zero scan size, got {sz}");

        (target.execute)(true).expect("dry-run execute");
        assert!(mount.join(".directory").exists(), "dry-run deleted a file");
        assert!(
            trash.join("files").join("junk").exists(),
            "dry-run emptied trash"
        );

        (target.execute)(false).expect("real execute");
        assert!(!mount.join(".directory").exists(), ".directory not removed");
        assert!(trash.exists(), "trash dir itself must be kept");
        assert!(
            !trash.join("files").join("junk").exists(),
            "trash files/ contents not emptied"
        );
        assert!(
            !trash.join("info").join("junk.trashinfo").exists(),
            "trash info/ contents not emptied"
        );
        fs::remove_dir_all(&tmp).ok();
    }

    // Finding 1 (review r2): a symlinked `files/`/`info/`/`expunged/` on an
    // untrusted external volume must never be followed out of the mount.
    #[test]
    fn external_volume_refuses_symlinked_trash_subdir() {
        for sub in ["files", "info", "expunged"] {
            let tmp = tempdir("ext-symlink");
            let mount = tmp.join("mnt");
            let trash = mount.join(".Trash-1000");
            fs::create_dir_all(&trash).unwrap();
            // A directory OUTSIDE the mount that must never be touched.
            let outside = tmp.join("outside");
            fs::create_dir_all(&outside).unwrap();
            fs::write(outside.join("victim"), "keep").unwrap();
            // <mount>/.Trash-1000/<sub> -> ../outside (absolute symlink)
            std::os::unix::fs::symlink(&outside, trash.join(sub)).unwrap();

            let deps = deps_for_test(&tmp, Arc::new(FakeRunner::new()));
            let target = external_volume_target_in(&deps, &mount);
            let err = (target.execute)(false).expect_err("expected refusal");
            assert!(
                err.to_string().contains("symlinked trash subdir"),
                "sub={sub} unexpected error: {err}"
            );
            assert!(
                outside.join("victim").exists(),
                "sub={sub}: file outside the mount was deleted"
            );
            fs::remove_dir_all(&tmp).ok();
        }
    }
}
