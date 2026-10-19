//! Safe trash deletion — port of `internal/utils/trash.go`.
//!
//! `SafeDelete` prefers `gio trash` and falls back to a FreeDesktop-trash
//! implementation: `~/.local/share/Trash` when the file is on the same device
//! as the data home, otherwise `<mount>/.Trash/<uid>` (sticky shared dir) or
//! `<mount>/.Trash-<uid>`. Renames use `renameat2(RENAME_NOREPLACE)` so an
//! existing trash entry is never overwritten; metadata finalization failure
//! rolls the file back, and a failed rollback surfaces the recovery path via
//! [`crate::error::Error::TrashRecovery`].
//!
//! Go keeps the seam for tests as package-level variable hooks
//! (`trashHomeDir`, `trashDeviceID`, `trashMountFor`, `renamePath`, ...).
//! Rust makes them explicit fields of [`TrashDeps`]; `safe_delete_with` is the
//! injectable core and [`safe_delete`] wires the real environment in.

use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use crate::config;
use crate::error::{Error, Result, msg};
use crate::paths;
use crate::runner::{CommandSpec, ProcessRunner, Runner};
use crate::whitelist;
use crate::{oplog, xdg};

type HomeFn = Box<dyn Fn() -> Result<PathBuf> + Send + Sync>;
type DeviceFn = Box<dyn Fn(&Path) -> Result<u64> + Send + Sync>;
type MountFn = Box<dyn Fn(&Path) -> Result<PathBuf> + Send + Sync>;
type FsFn = Box<dyn Fn(&Path) -> io::Result<()> + Send + Sync>;
type RenameFn = Box<dyn Fn(&Path, &Path) -> io::Result<()> + Send + Sync>;

/// Injectable filesystem/device hooks — Go's `trashHomeDir`, `trashDeviceID`,
/// `trashMountFor`, `renamePath`, `linkPath`, `removePath` variables.
pub struct TrashDeps {
    /// `trashHomeDir`: returns the XDG data home, creating it 0700 if needed.
    pub data_home: HomeFn,
    /// `trashDeviceID`: device id of `path` (`stat -f`-style `st_dev`).
    pub device_id: DeviceFn,
    /// `trashMountFor`: mount point containing `path` (mountinfo lookup).
    pub mount_for: MountFn,
    /// `renamePath`: atomic rename that must NOT replace an existing target.
    pub rename: RenameFn,
    /// `linkPath`: `os.Link`.
    pub link: RenameFn,
    /// `removePath`: `os.Remove`.
    pub remove: FsFn,
}

impl TrashDeps {
    /// The production hooks.
    pub fn real() -> Self {
        Self {
            data_home: Box::new(real_data_home),
            device_id: Box::new(device_id),
            mount_for: Box::new(|p| mount_point_for_path(p, Path::new("/proc/self/mountinfo"))),
            rename: Box::new(rename_no_replace),
            link: Box::new(|a, b| std::fs::hard_link(a, b)),
            // `os.Remove` semantics: file, symlink, or EMPTY directory only.
            remove: Box::new(|p| std::fs::remove_file(p).or_else(|_| std::fs::remove_dir(p))),
        }
    }
}

fn real_data_home() -> Result<PathBuf> {
    let home = xdg::data_home();
    // `os.MkdirAll(dataHome, 0o700)` — mode applies to created dirs only;
    // an existing data home is left untouched (no chmod).
    if !home.exists() {
        std::fs::create_dir_all(&home)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700))?;
        }
    }
    Ok(home)
}

/// `SafeDelete` — whitelist check, lstat, `gio trash`, FreeDesktop fallback.
pub fn safe_delete(path: &Path, dry_run: bool) -> Result<()> {
    safe_delete_with(
        &ProcessRunner,
        &xdg::config_home(),
        &TrashDeps::real(),
        path,
        dry_run,
    )
}

/// Injectable core of [`safe_delete`].
pub fn safe_delete_with(
    runner: &dyn Runner,
    config_home: &Path,
    deps: &TrashDeps,
    path: &Path,
    dry_run: bool,
) -> Result<()> {
    let wl = config::load_whitelist_from(config_home)
        .map_err(|e| Error::Msg(format!("invalid mu configuration: {e}")))?;
    if whitelist::is_whitelisted(path, &wl) {
        return msg(format!("refused: {} is a protected path", path.display()));
    }
    std::fs::symlink_metadata(path)
        .map_err(|e| Error::Msg(format!("inspect {}: {e}", path.display())))?;
    if dry_run {
        oplog::log_outcome("trash", &path.to_string_lossy(), "dry-run");
        return Ok(());
    }

    if runner.look_path("gio").is_ok() {
        let spec = CommandSpec::new("gio", [std::ffi::OsStr::new("trash"), path.as_os_str()]);
        if runner.run(&spec).is_ok() {
            oplog::log_outcome("trash", &path.to_string_lossy(), "success");
            return Ok(());
        }
    }
    match move_to_trash(deps, path) {
        Ok(_) => {
            oplog::log_outcome("trash", &path.to_string_lossy(), "success");
            Ok(())
        }
        Err(e) => {
            oplog::log_outcome("trash", &path.to_string_lossy(), "failure");
            // `fmt.Errorf("trash %s: %w", path, err)` — wraps without losing
            // the TrashRecovery variant (Go callers reach it via errors.As).
            Err(e.with_context(format!("trash {}", path.display())))
        }
    }
}

/// Trashes a verified mu component using default system whitelist protection,
/// allowing self-removal to succeed even when user configuration is malformed.
pub fn safe_delete_component(path: &Path, dry_run: bool) -> Result<()> {
    safe_delete_component_with(&ProcessRunner, &TrashDeps::real(), path, dry_run)
}

/// Injectable core of [`safe_delete_component`].
pub fn safe_delete_component_with(
    runner: &dyn Runner,
    deps: &TrashDeps,
    path: &Path,
    dry_run: bool,
) -> Result<()> {
    let wl = config::default_whitelist();
    if whitelist::is_whitelisted(path, &wl) {
        return msg(format!("refused: {} is a protected path", path.display()));
    }
    std::fs::symlink_metadata(path)
        .map_err(|e| Error::Msg(format!("inspect {}: {e}", path.display())))?;
    if dry_run {
        oplog::log_outcome("trash", &path.to_string_lossy(), "dry-run");
        return Ok(());
    }

    if runner.look_path("gio").is_ok() {
        let spec = CommandSpec::new("gio", [std::ffi::OsStr::new("trash"), path.as_os_str()]);
        if runner.run(&spec).is_ok() {
            oplog::log_outcome("trash", &path.to_string_lossy(), "success");
            return Ok(());
        }
    }
    match move_to_trash(deps, path) {
        Ok(_) => {
            oplog::log_outcome("trash", &path.to_string_lossy(), "success");
            Ok(())
        }
        Err(e) => {
            oplog::log_outcome("trash", &path.to_string_lossy(), "failure");
            Err(e.with_context(format!("trash {}", path.display())))
        }
    }
}

struct TrashLocation {
    files_dir: PathBuf,
    info_dir: PathBuf,
    /// The `Path=` value written into `.trashinfo`: raw bytes, because Go's
    /// `filepath.Base`/`Rel` keep non-UTF8 names intact end-to-end.
    path_info: Vec<u8>,
}

/// `moveToTrash` — returns the destination inside the trash `files/` dir.
/// On unrecoverable rollback the error is [`Error::TrashRecovery`], whose
/// `recovery` field names where the data landed.
fn move_to_trash(deps: &TrashDeps, path: &Path) -> Result<PathBuf> {
    let abs = absolute(path)?;
    let location = locate_trash(deps, &abs)?;
    let uid = nix::unistd::getuid().as_raw() as i32;
    let trash_root = paths::dir(&location.files_dir);
    ensure_private_trash_dir(&trash_root, uid)?;
    ensure_private_trash_dir(&location.files_dir, uid)?;
    ensure_private_trash_dir(&location.info_dir, uid)?;

    let base_name = abs
        .file_name()
        .map(std::ffi::OsStr::to_os_string)
        .unwrap_or_else(|| abs.as_os_str().to_os_string());
    let (base, temp_info) = reserve_trash_name(&location, &base_name)?;
    // `keepTemp` defer: remove the temp metadata on every error exit below.
    let keep_temp = |deps: &TrashDeps| {
        let _ = (deps.remove)(&temp_info);
    };

    let metadata = format!(
        "[Trash Info]\nPath={}\nDeletionDate={}\n",
        percent_encode_path(&location.path_info),
        deletion_date()
    );
    let meta_result = (|| -> io::Result<()> {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&temp_info)?;
        f.write_all(metadata.as_bytes())?;
        f.sync_all()
    })();
    if let Err(e) = meta_result {
        keep_temp(deps);
        return Err(e.into());
    }

    let destination = location.files_dir.join(&base);
    if let Err(e) = (deps.rename)(&abs, &destination) {
        keep_temp(deps);
        return Err(e.into());
    }
    let mut final_name = base.clone();
    final_name.push(".trashinfo");
    let final_info = location.info_dir.join(&final_name);
    if let Err(e) = (deps.link)(&temp_info, &final_info) {
        if let Err(rb) = (deps.rename)(&destination, &abs) {
            keep_temp(deps);
            return Err(Error::TrashRecovery {
                original: abs.to_string_lossy().into_owned(),
                recovery: destination.to_string_lossy().into_owned(),
                metadata: e.to_string(),
                rollback: rb.to_string(),
            });
        }
        keep_temp(deps);
        return msg(format!("finalize trash metadata: {e}"));
    }
    if let Err(e) = (deps.remove)(&temp_info) {
        // Go's `defer` still fires keepTemp here — retry the cleanup.
        keep_temp(deps);
        return Err(Error::Msg(format!(
            "remove temporary trash metadata {}: {e}",
            temp_info.display()
        )));
    }
    Ok(destination)
}

/// `filepath.Abs` — join with cwd for relative paths, then lexical clean.
fn absolute(p: &Path) -> Result<PathBuf> {
    let abs = if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir()?.join(p)
    };
    Ok(xdg::clean(&abs))
}

fn locate_trash(deps: &TrashDeps, path: &Path) -> Result<TrashLocation> {
    let data_home = (deps.data_home)()?;
    let path_device = (deps.device_id)(path)?;
    let home_device = (deps.device_id)(&data_home)?;
    if path_device == home_device {
        let root = data_home.join("Trash");
        return Ok(TrashLocation {
            files_dir: root.join("files"),
            info_dir: root.join("info"),
            path_info: path.as_os_str().as_encoded_bytes().to_vec(),
        });
    }

    let mount = (deps.mount_for)(path)?;
    let root = per_filesystem_trash(&mount, nix::unistd::getuid().as_raw() as i32)?;
    let clean_mount = xdg::clean(&mount);
    let rel = path.strip_prefix(&clean_mount).map_err(|_| {
        Error::Msg(format!(
            "path {} is outside mount {}",
            path.display(),
            mount.display()
        ))
    })?;
    if rel.as_os_str().is_empty() {
        return Err(Error::Msg(format!(
            "path {} is outside mount {}",
            path.display(),
            mount.display()
        )));
    }
    Ok(TrashLocation {
        files_dir: root.join("files"),
        info_dir: root.join("info"),
        // filepath.ToSlash is a no-op on Unix — raw rel bytes, not lossy UTF-8.
        path_info: rel.as_os_str().as_encoded_bytes().to_vec(),
    })
}

/// `deviceID` — `os.Stat` (follows symlinks) `st_dev`.
fn device_id(path: &Path) -> Result<u64> {
    use std::os::unix::fs::MetadataExt;
    Ok(std::fs::metadata(path)?.dev())
}

/// `renameNoReplace` — `renameat2(AT_FDCWD, old, AT_FDCWD, new, NOREPLACE)`.
/// `nix` only exposes `renameat2` under `target_env = "gnu"`, and the phase
/// check bans `unsafe` (so no raw `libc::syscall`), so the NOREPLACE
/// semantics are reproduced safely: reserve the destination atomically with
/// a same-type placeholder (`create_new`/`create_dir` fails EEXIST, exactly
/// like the flag), then `rename(2)` over it — a rename may only replace an
/// empty dir or a regular file, never a populated one, so the placeholder
/// type must match the source. On rename failure the placeholder is removed
/// so nothing is left behind.
fn rename_no_replace(old: &Path, new: &Path) -> io::Result<()> {
    let meta = std::fs::symlink_metadata(old)?;
    let reserve = if meta.is_dir() {
        std::fs::create_dir(new)
    } else {
        std::fs::File::create_new(new).map(|_| ())
    };
    reserve?; // EEXIST when `new` is taken — the NOREPLACE guard
    match std::fs::rename(old, new) {
        Ok(()) => Ok(()),
        Err(e) => {
            if meta.is_dir() {
                let _ = std::fs::remove_dir(new);
            } else {
                let _ = std::fs::remove_file(new);
            }
            Err(e)
        }
    }
}

/// `perFilesystemTrash` — prefer `<mount>/.Trash/<uid>` when `.Trash` is a
/// valid sticky shared dir; fall back to `<mount>/.Trash-<uid>`.
fn per_filesystem_trash(mount: &Path, uid: i32) -> Result<PathBuf> {
    let shared = mount.join(".Trash");
    let shared_err = match validate_shared_trash_dir(&shared) {
        Ok(()) => {
            let path = shared.join(uid.to_string());
            match ensure_private_trash_dir(&path, uid) {
                Ok(()) => return Ok(path),
                Err(e) => e,
            }
        }
        Err(e) => e,
    };
    let private = mount.join(format!(".Trash-{uid}"));
    if let Err(e) = ensure_private_trash_dir(&private, uid) {
        return Err(Error::Msg(format!(
            "shared filesystem trash {}: {shared_err}\nprivate filesystem trash {}: {e}",
            shared.display(),
            private.display()
        )));
    }
    Ok(private)
}

/// `validateSharedTrashDir` — must be a real directory (no symlink) with the
/// sticky bit set.
fn validate_shared_trash_dir(path: &Path) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    let info = std::fs::symlink_metadata(path)?;
    if !info.is_dir() || info.file_type().is_symlink() {
        return msg("must be a directory without symlinks".to_string());
    }
    if info.mode() & 0o1000 == 0 {
        return msg("must have the sticky bit set".to_string());
    }
    Ok(())
}

/// `ensurePrivateTrashDir` — create (0700, current uid) or validate a private
/// trash directory. Fail-closed on wrong owner/mode/symlink.
fn ensure_private_trash_dir(path: &Path, uid: i32) -> Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let info = match std::fs::symlink_metadata(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            if let Err(e) = std::fs::create_dir(path) {
                if e.kind() == io::ErrorKind::AlreadyExists {
                    return ensure_private_trash_dir(path, uid);
                }
                return Err(Error::Msg(format!("create {}: {e}", path.display())));
            }
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
                .map_err(|e| Error::Msg(format!("chmod {}: {e}", path.display())))?;
            std::fs::symlink_metadata(path)
                .map_err(|e| Error::Msg(format!("inspect {}: {e}", path.display())))?
        }
        Err(e) => return Err(Error::Msg(format!("inspect {}: {e}", path.display()))),
        Ok(i) => i,
    };
    if !info.is_dir() || info.file_type().is_symlink() {
        return Err(Error::Msg(format!(
            "{} must be a directory without symlinks",
            path.display()
        )));
    }
    if info.mode() & 0o777 != 0o700 {
        return Err(Error::Msg(format!(
            "{} permissions are {:04o}, want 0700",
            path.display(),
            info.mode() & 0o777
        )));
    }
    if info.uid() as i32 != uid {
        return Err(Error::Msg(format!(
            "{} owner does not match uid {uid}",
            path.display()
        )));
    }
    Ok(())
}

/// `mountPointForPath` — longest mount point in `mountinfo` containing `path`.
fn mount_point_for_path(path: &Path, mountinfo: &Path) -> Result<PathBuf> {
    let mut contents = String::new();
    std::fs::File::open(mountinfo)?.read_to_string(&mut contents)?;
    let clean_path = xdg::clean(path);
    let mut best = PathBuf::new();
    for line in contents.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 5 {
            continue;
        }
        let mount = PathBuf::from(unescape_mount(fields[4]));
        let mount = xdg::clean(&mount);
        if (clean_path == mount || paths::path_contains(&mount, &clean_path))
            && mount.as_os_str().len() > best.as_os_str().len()
        {
            best = mount;
        }
    }
    if best.as_os_str().is_empty() {
        return Err(Error::Msg(format!(
            "no mount point found for {}",
            path.display()
        )));
    }
    Ok(best)
}

/// `unescapeMount` — mountinfo octal escapes `\040` space, `\011` tab,
/// `\012` newline, `\134` backslash.
fn unescape_mount(value: &str) -> String {
    value
        .replace("\\040", " ")
        .replace("\\011", "\t")
        .replace("\\012", "\n")
        .replace("\\134", "\\")
}

/// `reserveTrashName` — collision-free `files/` and `info/` base plus an
/// exclusively created `info/<base>.trashinfo.tmp` reservation file.
/// `requested`/`base` are `OsString` so non-UTF8 names survive (Go keeps raw
/// bytes; lossy conversion would rename the entry).
fn reserve_trash_name(
    location: &TrashLocation,
    requested: &std::ffi::OsStr,
) -> Result<(std::ffi::OsString, PathBuf)> {
    for attempt in 0..10_000u32 {
        let mut base = requested.to_os_string();
        if attempt > 0 {
            base.push(format!(".{attempt}"));
        }
        match std::fs::symlink_metadata(location.files_dir.join(&base)) {
            Ok(_) => continue,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        let mut info_name = base.clone();
        info_name.push(".trashinfo");
        match std::fs::symlink_metadata(location.info_dir.join(&info_name)) {
            Ok(_) => continue,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        let mut temp_name = base.clone();
        temp_name.push(".trashinfo.tmp");
        let temp = location.info_dir.join(&temp_name);
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600); // O_CREATE mode applies only to a new file
        }
        match opts.open(&temp) {
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e.into()),
            Ok(f) => drop(f),
        }
        return Ok((base, temp));
    }
    Err(Error::Msg(format!(
        "could not reserve a collision-free trash name for {}",
        requested.to_string_lossy()
    )))
}

/// `percentEncodePath` — Go `url.PathEscape` byte-for-byte (verified against
/// the oracle): unescaped set is `[A-Za-z0-9-_.~$&+:=@]`; every other byte
/// becomes `%XX` uppercase — including `/` (`%2F`), `\` (`%5C`), and space
/// (`%20`). Operates on raw bytes so non-UTF8 names round-trip.
fn percent_encode_path(path: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(path.len());
    for &b in path {
        match b {
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'-'
            | b'_'
            | b'.'
            | b'~'
            | b'$'
            | b'&'
            | b'+'
            | b':'
            | b'='
            | b'@' => out.push(b as char),
            _ => {
                out.push('%');
                out.push(HEX[(b >> 4) as usize] as char);
                out.push(HEX[(b & 0xf) as usize] as char);
            }
        }
    }
    out
}

/// `time.Now().Format("2006-01-02T15:04:05")` — local time, no zone.
fn deletion_date() -> String {
    let now = time::OffsetDateTime::now_local().unwrap_or_else(|_| time::OffsetDateTime::now_utc());
    let fmt = time::format_description::parse_borrowed::<2>(
        "[year]-[month]-[day]T[hour]:[minute]:[second]",
    )
    .unwrap_or_default();
    now.format(&fmt).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::FakeRunner;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicI32, Ordering};

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

    fn uid() -> i32 {
        nix::unistd::getuid().as_raw() as i32
    }

    /// `os.Mkdir(path, 0o700)` — create a dir with exactly 0700 regardless of
    /// umask (Rust's `create_dir` defaults to 0o777 & umask).
    fn mkdir_0700(path: &Path) {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new().mode(0o700).create(path).unwrap();
    }

    // Port of TestLocateTrashUsesPerFilesystemTrashAcrossDevices.
    #[test]
    fn locate_trash_uses_per_filesystem_trash_across_devices() {
        let home = tempdir("tr-home");
        let mount = tempdir("tr-mount");
        let path = mount.join("data.txt");
        fs::write(&path, b"x").unwrap();

        let home_c = home.clone();
        let home_d = home.clone();
        let mount_c = mount.clone();
        let deps = TrashDeps {
            data_home: Box::new(move || Ok(home_c.clone())),
            device_id: Box::new(move |p: &Path| Ok(if p == home_d { 1 } else { 2 })),
            mount_for: Box::new(move |_| Ok(mount_c.clone())),
            ..TrashDeps::real()
        };
        let location = locate_trash(&deps, &path).unwrap();
        let want_root = mount.join(format!(".Trash-{}", uid()));
        assert_eq!(location.files_dir, want_root.join("files"));
        assert_eq!(location.path_info, b"data.txt");
        fs::remove_dir_all(&home).ok();
        fs::remove_dir_all(&mount).ok();
    }

    // Port of TestMoveToTrashCollisionAndEncodedMetadata.
    #[test]
    fn move_to_trash_collision_and_encoded_metadata() {
        let home = tempdir("tr-coll");
        let data_home = home.join(".data");
        let path = home.join("file name");
        fs::write(&path, b"new").unwrap();
        let trash_root = data_home.join("Trash");
        fs::create_dir_all(trash_root.join("files")).unwrap();
        fs::create_dir_all(trash_root.join("info")).unwrap();
        // Go test used MkdirAll(..., 0700); create_dir_all honours umask, so
        // pin the modes to what ensure_private_trash_dir requires.
        {
            use std::os::unix::fs::PermissionsExt;
            for d in [
                &data_home,
                &trash_root,
                &trash_root.join("files"),
                &trash_root.join("info"),
            ] {
                fs::set_permissions(d, fs::Permissions::from_mode(0o700)).unwrap();
            }
        }
        fs::write(trash_root.join("files").join("file name"), b"old").unwrap();

        let data_home_c = data_home.clone();
        let deps = TrashDeps {
            data_home: Box::new(move || Ok(data_home_c.clone())),
            device_id: Box::new(|_| Ok(1)),
            mount_for: Box::new(|_| unreachable!("same device")),
            ..TrashDeps::real()
        };
        let recovery = move_to_trash(&deps, &path).unwrap();
        assert_eq!(
            recovery.file_name().unwrap().to_string_lossy(),
            "file name.1"
        );
        let meta =
            fs::read_to_string(trash_root.join("info").join("file name.1.trashinfo")).unwrap();
        assert!(
            meta.contains("Path=%2F") && meta.contains("%20"),
            "path is not percent encoded: {meta:?}"
        );
        fs::remove_dir_all(&home).ok();
    }

    // Port of TestRenameNoReplaceNeverOverwritesExistingTrashEntry.
    #[test]
    fn rename_no_replace_never_overwrites() {
        let root = tempdir("tr-rnr");
        let source = root.join("source");
        let destination = root.join("destination");
        fs::write(&source, b"source").unwrap();
        fs::write(&destination, b"destination").unwrap();
        assert!(rename_no_replace(&source, &destination).is_err());
        assert_eq!(fs::read_to_string(&destination).unwrap(), "destination");
        assert!(paths::path_exists(&source));
        fs::remove_dir_all(&root).ok();
    }

    // Port of TestMoveToTrashRollsBackWhenMetadataFinalizationFails.
    #[test]
    fn move_to_trash_rolls_back_on_metadata_failure() {
        let home = tempdir("tr-rb");
        let data_home = home.join(".data");
        mkdir_0700(&data_home);
        let path = home.join("rollback");
        fs::write(&path, b"keep").unwrap();

        let data_home_c = data_home.clone();
        let deps = TrashDeps {
            data_home: Box::new(move || Ok(data_home_c.clone())),
            device_id: Box::new(|_| Ok(1)),
            mount_for: Box::new(|_| unreachable!()),
            link: Box::new(|_, _| Err(io::Error::other("metadata full"))),
            ..TrashDeps::real()
        };
        assert!(move_to_trash(&deps, &path).is_err());
        assert!(paths::path_exists(&path), "original was not restored");
        fs::remove_dir_all(&home).ok();
    }

    // Port of TestMoveToTrashReportsRecoveryPathWhenRollbackFails.
    #[test]
    fn move_to_trash_reports_recovery_when_rollback_fails() {
        let home = tempdir("tr-rec");
        let data_home = home.join(".data");
        mkdir_0700(&data_home);
        let path = home.join("recover");
        fs::write(&path, b"keep").unwrap();

        let renames = Arc::new(AtomicI32::new(0));
        let renames_c = renames.clone();
        let data_home_c = data_home.clone();
        let deps = TrashDeps {
            data_home: Box::new(move || Ok(data_home_c.clone())),
            device_id: Box::new(|_| Ok(1)),
            mount_for: Box::new(|_| unreachable!()),
            link: Box::new(|_, _| Err(io::Error::other("metadata full"))),
            rename: Box::new(move |a, b| {
                if renames_c.fetch_add(1, Ordering::SeqCst) == 1 {
                    return Err(io::Error::other("rollback blocked"));
                }
                std::fs::rename(a, b)
            }),
            ..TrashDeps::real()
        };
        let err = move_to_trash(&deps, &path).unwrap_err();
        let Error::TrashRecovery { recovery, .. } = &err else {
            panic!("expected TrashRecovery, got {err:?}");
        };
        assert!(paths::path_exists(Path::new(recovery)));
        fs::remove_dir_all(&home).ok();
    }

    // Port of TestPerFilesystemTrashFallsBackFromInvalidSharedDirectory.
    #[test]
    fn per_filesystem_trash_falls_back_from_invalid_shared() {
        for setup in ["regular-file", "symlink", "no-sticky-bit"] {
            let mount = tempdir("tr-pf");
            let shared = mount.join(".Trash");
            match setup {
                "regular-file" => fs::write(&shared, b"unsafe").unwrap(),
                "symlink" => std::os::unix::fs::symlink(tempdir("tr-target"), &shared).unwrap(),
                _ => {
                    fs::create_dir(&shared).unwrap();
                    use std::os::unix::fs::PermissionsExt;
                    fs::set_permissions(&shared, fs::Permissions::from_mode(0o755)).unwrap();
                }
            }
            let u = uid();
            let got = per_filesystem_trash(&mount, u).unwrap();
            assert_eq!(got, mount.join(format!(".Trash-{u}")));
            ensure_private_trash_dir(&got, u).unwrap();
            fs::remove_dir_all(&mount).ok();
        }
    }

    // Port of TestPerFilesystemTrashRejectsInvalidPrivateDirectory.
    #[test]
    fn per_filesystem_trash_rejects_invalid_private() {
        let u = uid();
        for (name, other_uid) in [
            ("regular-file", u),
            ("symlink", u),
            ("mode-0755", u),
            ("foreign-owner", u + 1),
        ] {
            let mount = tempdir("tr-priv");
            let path = mount.join(format!(".Trash-{other_uid}"));
            match name {
                "regular-file" => fs::write(&path, b"unsafe").unwrap(),
                "symlink" => std::os::unix::fs::symlink(tempdir("tr-t"), &path).unwrap(),
                "mode-0755" => {
                    mkdir_0700(&path);
                    use std::os::unix::fs::PermissionsExt;
                    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
                }
                _ => {
                    mkdir_0700(&path);
                }
            }
            assert!(
                per_filesystem_trash(&mount, other_uid).is_err(),
                "case {name}"
            );
            fs::remove_dir_all(&mount).ok();
        }
    }

    // Port of TestMoveToTrashRejectsUnsafeSubdirectoriesBeforeMovingSource.
    #[test]
    fn move_to_trash_rejects_unsafe_subdirs_before_moving() {
        for subdir in ["files", "info"] {
            for setup in ["regular-file", "symlink", "mode-0755"] {
                let home = tempdir("tr-sub-home");
                let mount = tempdir("tr-sub-mount");
                let source = mount.join("payload");
                fs::write(&source, b"keep").unwrap();

                let root = mount.join(format!(".Trash-{}", uid()));
                mkdir_0700(&root);
                if subdir == "info" {
                    mkdir_0700(&root.join("files"));
                }
                let victim = root.join(subdir);
                match setup {
                    "regular-file" => fs::write(&victim, b"unsafe").unwrap(),
                    "symlink" => std::os::unix::fs::symlink(tempdir("tr-st"), &victim).unwrap(),
                    _ => {
                        mkdir_0700(&victim);
                        use std::os::unix::fs::PermissionsExt;
                        fs::set_permissions(&victim, fs::Permissions::from_mode(0o755)).unwrap();
                    }
                }

                let home_c = home.clone();
                let home_c2 = home.clone();
                let mount_c = mount.clone();
                let deps = TrashDeps {
                    data_home: Box::new(move || Ok(home_c.clone())),
                    device_id: Box::new(move |p| Ok(if p == home_c2 { 1 } else { 2 })),
                    mount_for: Box::new(move |_| Ok(mount_c.clone())),
                    ..TrashDeps::real()
                };
                assert!(
                    move_to_trash(&deps, &source).is_err(),
                    "{subdir}/{setup} expected rejection"
                );
                assert_eq!(fs::read_to_string(&source).unwrap(), "keep");
                fs::remove_dir_all(&home).ok();
                fs::remove_dir_all(&mount).ok();
            }
        }
    }

    // Port of TestSafeDelete_ProtectedPath + TestSafeDelete_DryRun +
    // TestSafeDeleteReloadsWhitelistForEveryOperation (config-home injected).
    #[test]
    fn safe_delete_protected_and_dry_run() {
        let cfg = tempdir("tr-cfg");
        let runner = FakeRunner::new();
        let deps = TrashDeps::real();
        assert!(safe_delete_with(&runner, &cfg, &deps, Path::new("/etc/passwd"), false).is_err());

        let victim = tempdir("tr-sd").join("candidate");
        fs::write(&victim, b"keep").unwrap();
        safe_delete_with(&runner, &cfg, &deps, &victim, true).unwrap();
        assert!(paths::path_exists(&victim), "dry-run must not delete");
    }

    // Port of TestSafeDeleteReloadsWhitelistForEveryOperation +
    // TestSafeDelete_UserProtectedPath.
    #[test]
    fn safe_delete_reloads_whitelist_each_call() {
        let root = tempdir("tr-reload");
        let cfg_dir = root.join("mu");
        fs::create_dir_all(&cfg_dir).unwrap();
        let candidate = root.join("candidate");
        fs::write(&candidate, b"keep").unwrap();

        let runner = FakeRunner::new();
        let deps = TrashDeps::real();
        safe_delete_with(&runner, &root, &deps, &candidate, true).unwrap();

        fs::write(
            cfg_dir.join("config.toml"),
            format!("[protected_paths]\nsystem = [{}]", toml_string(&candidate)),
        )
        .unwrap();
        let err = safe_delete_with(&runner, &root, &deps, &candidate, true).unwrap_err();
        assert!(err.to_string().contains("protected path"), "err={err}");
    }

    // Port of TestSafeDeleteRejectsAncestorOfProtectedPathInDryRunAndRealMode.
    #[test]
    fn safe_delete_rejects_ancestor_of_protected_path() {
        let root = tempdir("tr-anc");
        let config_dir = root.join("cfg");
        let parent = root.join(".cache").join("app");
        let protected = parent.join("keep");
        fs::create_dir_all(&protected).unwrap();
        fs::create_dir_all(config_dir.join("mu")).unwrap();
        fs::write(
            config_dir.join("mu").join("config.toml"),
            format!("[protected_paths]\nsystem = [{}]", toml_string(&protected)),
        )
        .unwrap();

        let runner = FakeRunner::new();
        let deps = TrashDeps::real();
        for dry_run in [true, false] {
            let err = safe_delete_with(&runner, &config_dir, &deps, &parent, dry_run).unwrap_err();
            assert!(err.to_string().contains("protected path"), "err={err}");
            assert!(paths::path_exists(&protected));
        }
    }

    // Port of TestMalformedWhitelistFailsClosedEvenInDryRun.
    #[test]
    fn malformed_whitelist_fails_closed_even_in_dry_run() {
        let root = tempdir("tr-mal");
        fs::create_dir_all(root.join("mu")).unwrap();
        fs::write(root.join("mu").join("config.toml"), "not = [valid").unwrap();
        let candidate = root.join("candidate");
        fs::write(&candidate, b"keep").unwrap();

        let runner = FakeRunner::new();
        let deps = TrashDeps::real();
        let err = safe_delete_with(&runner, &root, &deps, &candidate, true).unwrap_err();
        assert!(
            err.to_string().contains("invalid mu configuration"),
            "err={err}"
        );
        assert!(paths::path_exists(&candidate));
    }

    // Port of TestSafeDelete_MovesToTrash (e2e, FreeDesktop fallback — gio is
    // absent in the FakeRunner so the XDG path must move the file).
    #[test]
    fn safe_delete_moves_to_trash() {
        let root = tempdir("tr-e2e");
        let data_home = root.join(".data");
        let victim = root.join("victim.txt");
        fs::write(&victim, b"bye").unwrap();

        let runner = FakeRunner::new(); // gio not present → fallback
        let deps = TrashDeps {
            data_home: {
                let d = data_home.clone();
                Box::new(move || {
                    std::fs::create_dir_all(&d)?;
                    Ok(d.clone())
                })
            },
            ..TrashDeps::real()
        };
        let cfg = tempdir("tr-e2e-cfg");
        safe_delete_with(&runner, &cfg, &deps, &victim, false).unwrap();
        assert!(!paths::path_exists(&victim), "file should be gone");
        assert!(
            paths::path_exists(&data_home.join("Trash").join("files").join("victim.txt")),
            "file should land in Trash/files"
        );
        assert!(
            paths::path_exists(
                &data_home
                    .join("Trash")
                    .join("info")
                    .join("victim.txt.trashinfo")
            ),
            "trashinfo metadata should exist"
        );
        fs::remove_dir_all(&root).ok();
        fs::remove_dir_all(&cfg).ok();
    }

    // Port of TestSafeDeleteFallbackAndMissingPath (coverage_test.go:24) —
    // missing source must error before any trash attempt.
    #[test]
    fn safe_delete_missing_path_errors() {
        let root = tempdir("tr-miss");
        let cfg = tempdir("tr-miss-cfg");
        let runner = FakeRunner::new();
        let deps = TrashDeps::real();
        let missing = root.join("nope");
        let err = safe_delete_with(&runner, &cfg, &deps, &missing, false).unwrap_err();
        assert!(err.to_string().contains("inspect"), "err={err}");
        fs::remove_dir_all(&root).ok();
        fs::remove_dir_all(&cfg).ok();
    }

    // Port of TestSafeDeleteFallsBackWhenGioFails (coverage_test.go:46) —
    // gio present but exits nonzero → FreeDesktop fallback still succeeds.
    #[test]
    fn safe_delete_falls_back_when_gio_fails() {
        let root = tempdir("tr-gio");
        let data_home = root.join(".data");
        let victim = root.join("victim.txt");
        fs::write(&victim, b"bye").unwrap();

        let runner = FakeRunner::new();
        runner.set_look_path("gio", "/usr/bin/gio");
        runner.push_response(Err(crate::runner::RunError::Failed(
            crate::runner::Output {
                stderr: b"trash failed".to_vec(),
                exit_code: 1,
                ..Default::default()
            },
        )));
        let deps = TrashDeps {
            data_home: {
                let d = data_home.clone();
                Box::new(move || {
                    std::fs::create_dir_all(&d)?;
                    Ok(d.clone())
                })
            },
            ..TrashDeps::real()
        };
        let cfg = tempdir("tr-gio-cfg");
        safe_delete_with(&runner, &cfg, &deps, &victim, false).unwrap();
        assert!(!paths::path_exists(&victim));
        assert!(paths::path_exists(
            &data_home.join("Trash").join("files").join("victim.txt")
        ));
        fs::remove_dir_all(&root).ok();
        fs::remove_dir_all(&cfg).ok();
    }

    // Port of TestMountPointAndSharedFilesystemTrash (coverage_test.go:106):
    // mountinfo longest-prefix + \040 unescape; positive .Trash/<uid> path.
    #[test]
    fn mount_point_parsing_and_shared_trash() {
        let root = tempdir("tr-mi");
        let mountinfo = root.join("mountinfo");
        fs::write(
            &mountinfo,
            concat!(
                "22 1 8:1 / / rw,relatime - ext4 /dev/sda1 rw\n",
                "30 22 0:20 / /mnt/with\\040space rw - tmpfs tmpfs rw\n",
                "31 22 0:21 / /mnt rw - tmpfs tmpfs rw\n",
            ),
        )
        .unwrap();
        // Longest prefix wins.
        assert_eq!(
            mount_point_for_path(Path::new("/mnt/sub/file"), &mountinfo).unwrap(),
            PathBuf::from("/mnt")
        );
        // \040 decodes to a space.
        assert_eq!(
            mount_point_for_path(Path::new("/mnt/with space/x"), &mountinfo).unwrap(),
            PathBuf::from("/mnt/with space")
        );
        // /nomount is covered by the root mount — resolves to "/".
        assert_eq!(
            mount_point_for_path(Path::new("/nomount"), &mountinfo).unwrap(),
            PathBuf::from("/")
        );

        // Valid sticky shared dir → <mount>/.Trash/<uid>.
        let mount = tempdir("tr-shared");
        let shared = mount.join(".Trash");
        {
            use std::os::unix::fs::DirBuilderExt;
            std::fs::DirBuilder::new()
                .mode(0o1777)
                .create(&shared)
                .unwrap();
        }
        let u = uid();
        let got = per_filesystem_trash(&mount, u).unwrap();
        assert_eq!(got, shared.join(u.to_string()));
        assert!(paths::path_exists(&got));

        // device_id smoke: real impl returns some id for an existing path.
        assert!(device_id(&mount).is_ok());
        fs::remove_dir_all(&mount).ok();
        fs::remove_dir_all(&root).ok();
    }

    // Port of TestSafeDelete_MovesToTrash's gio-present fast path: gio
    // succeeds → no filesystem fallback, invocation recorded verbatim.
    #[test]
    fn safe_delete_uses_gio_when_available() {
        let root = tempdir("tr-gio-ok");
        let cfg = tempdir("tr-gio-ok-cfg");
        let victim = root.join("victim.txt");
        fs::write(&victim, b"bye").unwrap();

        let runner = FakeRunner::new();
        runner.set_look_path("gio", "/usr/bin/gio");
        // Default handler answers Ok — file stays put (gio would handle it).
        let deps = TrashDeps::real();
        safe_delete_with(&runner, &cfg, &deps, &victim, false).unwrap();
        let invocations = runner.invocations();
        assert_eq!(invocations.len(), 1);
        assert_eq!(invocations[0].program, std::ffi::OsStr::new("gio"));
        assert_eq!(
            invocations[0].args,
            vec![
                std::ffi::OsString::from("trash"),
                victim.as_os_str().to_os_string()
            ]
        );
        fs::remove_dir_all(&root).ok();
        fs::remove_dir_all(&cfg).ok();
    }

    fn toml_string(p: &Path) -> String {
        format!("{:?}", p.to_string_lossy())
    }
}
