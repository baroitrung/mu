//! Biometric sudo configurator — port of Mole's Touch ID manager mapped to Ubuntu.
//!
//! Configures Linux PAM via `pam_fprintd.so` in `/etc/pam.d/sudo`.
//! Supports `status`, `enable`, and `disable` with `--dry-run` and automatic
//! backup generation (`<pam_file>.mu-bak`) to prevent lockout.

use std::fs;
use std::path::{Path, PathBuf};

use crate::oplog;

const DEFAULT_PAM_SUDO: &str = "/etc/pam.d/sudo";
const FPRINTD_PAM_LINE: &str = "auth       sufficient pam_fprintd.so";

const PAM_FPRINTD_PATHS: &[&str] = &[
    "/usr/lib/x86_64-linux-gnu/security/pam_fprintd.so",
    "/lib/x86_64-linux-gnu/security/pam_fprintd.so",
    "/usr/lib/aarch64-linux-gnu/security/pam_fprintd.so",
    "/lib/aarch64-linux-gnu/security/pam_fprintd.so",
    "/usr/lib/security/pam_fprintd.so",
    "/lib/security/pam_fprintd.so",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SudoCommand {
    Enable,
    Disable,
    Status,
}

#[derive(Clone, Debug)]
pub struct Options {
    pub command: SudoCommand,
    pub dry_run: bool,
    pub debug: bool,
    pub pam_path: Option<PathBuf>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StatusInfo {
    pub enabled: bool,
    pub module_installed: bool,
    pub pam_file_exists: bool,
    pub pam_path: PathBuf,
}

pub fn is_module_installed() -> bool {
    PAM_FPRINTD_PATHS.iter().any(|p| Path::new(p).exists())
}

pub fn resolve_pam_path(custom: Option<&Path>) -> PathBuf {
    if let Some(p) = custom {
        return p.to_path_buf();
    }
    if let Ok(env_path) = std::env::var("MU_PAM_FILE") {
        return PathBuf::from(env_path);
    }
    PathBuf::from(DEFAULT_PAM_SUDO)
}

pub fn get_status(pam_path: &Path) -> StatusInfo {
    let pam_file_exists = pam_path.exists();
    let module_installed = is_module_installed();
    let mut enabled = false;

    if pam_file_exists && let Ok(content) = fs::read_to_string(pam_path) {
        for line in content.lines() {
            let trimmed = line.trim();
            if !trimmed.starts_with('#') && trimmed.contains("pam_fprintd.so") {
                enabled = true;
                break;
            }
        }
    }

    StatusInfo {
        enabled,
        module_installed,
        pam_file_exists,
        pam_path: pam_path.to_path_buf(),
    }
}

pub fn enable(pam_path: &Path, dry_run: bool) -> Result<(), String> {
    let status = get_status(pam_path);
    if !status.pam_file_exists {
        return Err(format!("PAM file does not exist: {}", pam_path.display()));
    }
    if status.enabled {
        println!("Fingerprint authentication for sudo is already enabled.");
        return Ok(());
    }

    let content = fs::read_to_string(pam_path)
        .map_err(|e| format!("cannot read PAM file {}: {}", pam_path.display(), e))?;

    // Determine insertion position: before `@include common-auth` if present,
    // otherwise before first auth or at the end.
    let mut new_lines = Vec::new();
    let mut inserted = false;

    for line in content.lines() {
        if !inserted && line.trim().starts_with("@include common-auth") {
            new_lines.push(FPRINTD_PAM_LINE.to_string());
            inserted = true;
        }
        new_lines.push(line.to_string());
    }

    if !inserted {
        new_lines.insert(0, FPRINTD_PAM_LINE.to_string());
    }

    let new_content = new_lines.join("\n") + "\n";
    let backup_path = pam_path.with_extension("mu-bak");

    if dry_run {
        println!("[dry-run] Would enable fingerprint authentication for sudo");
        println!("[dry-run] Target: {}", pam_path.display());
        println!("[dry-run] Backup: {}", backup_path.display());
        println!("[dry-run] + {FPRINTD_PAM_LINE}");
        if !status.module_installed {
            println!("[dry-run] Note: pam_fprintd.so is not currently installed on this system");
        }
        oplog::log_outcome("sudo", "enable", "dry-run");
        return Ok(());
    }

    // Backup original
    fs::write(&backup_path, &content)
        .map_err(|e| format!("failed to create backup {}: {}", backup_path.display(), e))?;

    // Write new content
    if let Err(e) = fs::write(pam_path, &new_content) {
        return Err(format!(
            "cannot write to {} (permission denied? run with sudo): {}",
            pam_path.display(),
            e
        ));
    }

    // Safety verification: file must not be empty and must be readable
    match fs::read_to_string(pam_path) {
        Ok(verified) if verified.contains("pam_fprintd.so") => {
            println!("Fingerprint authentication enabled for sudo.");
            println!("Backup created at: {}", backup_path.display());
            oplog::log_outcome("sudo", "enable", "success");
            Ok(())
        }
        _ => {
            // Rollback
            let _ = fs::write(pam_path, content);
            Err("verification failed after editing PAM file; rolled back changes".to_string())
        }
    }
}

pub fn disable(pam_path: &Path, dry_run: bool) -> Result<(), String> {
    let status = get_status(pam_path);
    if !status.pam_file_exists {
        return Err(format!("PAM file does not exist: {}", pam_path.display()));
    }
    if !status.enabled {
        println!("Fingerprint authentication for sudo is already disabled.");
        return Ok(());
    }

    let content = fs::read_to_string(pam_path)
        .map_err(|e| format!("cannot read PAM file {}: {}", pam_path.display(), e))?;

    let mut new_lines = Vec::new();
    let mut removed_lines = Vec::new();

    for line in content.lines() {
        if line.contains("pam_fprintd.so") {
            removed_lines.push(line.to_string());
        } else {
            new_lines.push(line.to_string());
        }
    }

    let new_content = new_lines.join("\n") + "\n";
    let backup_path = pam_path.with_extension("mu-bak");

    if dry_run {
        println!("[dry-run] Would disable fingerprint authentication for sudo");
        println!("[dry-run] Target: {}", pam_path.display());
        println!("[dry-run] Backup: {}", backup_path.display());
        for line in &removed_lines {
            println!("[dry-run] - {line}");
        }
        oplog::log_outcome("sudo", "disable", "dry-run");
        return Ok(());
    }

    // Backup original
    fs::write(&backup_path, &content)
        .map_err(|e| format!("failed to create backup {}: {}", backup_path.display(), e))?;

    // Write new content
    if let Err(e) = fs::write(pam_path, &new_content) {
        return Err(format!(
            "cannot write to {} (permission denied? run with sudo): {}",
            pam_path.display(),
            e
        ));
    }

    println!("Fingerprint authentication disabled for sudo.");
    println!("Backup created at: {}", backup_path.display());
    oplog::log_outcome("sudo", "disable", "success");
    Ok(())
}

pub fn run(opts: &Options) -> i32 {
    let pam_path = resolve_pam_path(opts.pam_path.as_deref());

    match opts.command {
        SudoCommand::Status => {
            let status = get_status(&pam_path);
            let state = if status.enabled {
                "enabled"
            } else {
                "disabled"
            };
            println!("Fingerprint authentication for sudo: {state}");
            println!("PAM target: {}", status.pam_path.display());
            if status.module_installed {
                println!("Module pam_fprintd.so: installed");
            } else {
                println!(
                    "Module pam_fprintd.so: not found (install with: sudo apt install libpam-fprintd)"
                );
            }
            0
        }
        SudoCommand::Enable => match enable(&pam_path, opts.dry_run) {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("error: {e}");
                1
            }
        },
        SudoCommand::Disable => match disable(&pam_path, opts.dry_run) {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("error: {e}");
                1
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_PAM: &str = "\
#%PAM-1.0
session    required   pam_limits.so
session    required   pam_env.so readenv=1 user_readenv=0

@include common-auth
@include common-account
@include common-session-noninteractive
";

    #[test]
    fn test_status_disabled_on_sample() {
        let tmp = std::env::temp_dir().join(format!("mu-sudo-test-{}", std::process::id()));
        let _ = fs::create_dir_all(&tmp);
        let pam_file = tmp.join("sudo");
        fs::write(&pam_file, SAMPLE_PAM).unwrap();

        let status = get_status(&pam_file);
        assert!(!status.enabled);
        assert!(status.pam_file_exists);

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_enable_and_disable_lifecycle() {
        let tmp = std::env::temp_dir().join(format!("mu-sudo-lifecycle-{}", std::process::id()));
        let _ = fs::create_dir_all(&tmp);
        let pam_file = tmp.join("sudo");
        fs::write(&pam_file, SAMPLE_PAM).unwrap();

        // 1. Enable dry-run
        let res = enable(&pam_file, true);
        assert!(res.is_ok());
        let status = get_status(&pam_file);
        assert!(!status.enabled, "dry-run must not modify PAM file");

        // 2. Enable real
        let res = enable(&pam_file, false);
        assert!(res.is_ok());
        let status = get_status(&pam_file);
        assert!(status.enabled, "real enable must enable fingerprint auth");

        // Backup must exist
        let backup = pam_file.with_extension("mu-bak");
        assert!(backup.exists(), "backup file must exist");
        let backup_content = fs::read_to_string(&backup).unwrap();
        assert!(!backup_content.contains("pam_fprintd.so"));

        // Content verification: line inserted before @include common-auth
        let modified = fs::read_to_string(&pam_file).unwrap();
        let fprintd_idx = modified.find("pam_fprintd.so").unwrap();
        let common_auth_idx = modified.find("@include common-auth").unwrap();
        assert!(fprintd_idx < common_auth_idx);

        // 3. Re-enable when already enabled -> no-op Ok
        let res = enable(&pam_file, false);
        assert!(res.is_ok());

        // 4. Disable dry-run
        let res = disable(&pam_file, true);
        assert!(res.is_ok());
        assert!(get_status(&pam_file).enabled, "dry run must not disable");

        // 5. Disable real
        let res = disable(&pam_file, false);
        assert!(res.is_ok());
        assert!(!get_status(&pam_file).enabled);

        let _ = fs::remove_dir_all(&tmp);
    }
}
