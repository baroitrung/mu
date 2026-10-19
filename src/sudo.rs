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
    pub enabled_direct: bool,
    pub enabled_included: bool,
    pub module_installed: bool,
    pub pam_file_exists: bool,
    pub pam_path: PathBuf,
}

pub fn is_module_installed() -> bool {
    if let Ok(val) = std::env::var("MU_FPRINTD_INSTALLED") {
        return val == "1" || val == "true";
    }
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
    let mut enabled_direct = false;
    let mut enabled_included = false;

    if pam_file_exists && let Ok(content) = fs::read_to_string(pam_path) {
        for line in content.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with('#') {
                continue;
            }
            if trimmed.contains("pam_fprintd.so") {
                enabled_direct = true;
            } else if trimmed.starts_with("@include") {
                let parts: Vec<&str> = trimmed.split_whitespace().collect();
                if parts.len() >= 2 {
                    let inc_name = parts[1];
                    let inc_path = pam_path
                        .parent()
                        .unwrap_or_else(|| Path::new("/etc/pam.d"))
                        .join(inc_name);
                    if inc_path.exists()
                        && let Ok(inc_content) = fs::read_to_string(&inc_path)
                    {
                        for inc_line in inc_content.lines() {
                            let inc_trimmed = inc_line.trim();
                            if !inc_trimmed.starts_with('#')
                                && inc_trimmed.contains("pam_fprintd.so")
                            {
                                enabled_included = true;
                                break;
                            }
                        }
                    }
                }
            }
        }
    }

    StatusInfo {
        enabled: enabled_direct || enabled_included,
        enabled_direct,
        enabled_included,
        module_installed,
        pam_file_exists,
        pam_path: pam_path.to_path_buf(),
    }
}

fn backup_pam(pam_path: &Path, content: &str) -> Result<PathBuf, String> {
    let parent = pam_path.parent().unwrap_or_else(|| Path::new("."));
    let file_name = pam_path
        .file_name()
        .and_then(|f| f.to_str())
        .unwrap_or("sudo");
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let backup_path = parent.join(format!(".{file_name}.mu-bak-{ts}"));
    fs::write(&backup_path, content)
        .map_err(|e| format!("failed to create backup {}: {}", backup_path.display(), e))?;
    Ok(backup_path)
}

fn atomic_write_pam(
    pam_path: &Path,
    new_content: &str,
    backup_path: &Path,
    original_content: &str,
    expected_token: &str,
    require_token: bool,
) -> Result<(), String> {
    let parent = pam_path.parent().unwrap_or_else(|| Path::new("."));
    let file_name = pam_path
        .file_name()
        .and_then(|f| f.to_str())
        .unwrap_or("sudo");
    let temp_path = parent.join(format!(".{file_name}.mu-tmp-{}", std::process::id()));

    if let Err(e) = fs::write(&temp_path, new_content) {
        return Err(format!(
            "cannot write temporary PAM file {} (permission denied? run with sudo): {}",
            temp_path.display(),
            e
        ));
    }

    if let Ok(meta) = fs::metadata(pam_path) {
        let _ = fs::set_permissions(&temp_path, meta.permissions());
    }

    if let Ok(file) = fs::File::open(&temp_path) {
        let _ = file.sync_all();
    }

    if let Err(e) = fs::rename(&temp_path, pam_path) {
        let _ = fs::remove_file(&temp_path);
        let _ = fs::write(pam_path, original_content);
        return Err(format!(
            "failed to replace PAM file {}: {}",
            pam_path.display(),
            e
        ));
    }

    let verified = match fs::read_to_string(pam_path) {
        Ok(s) => s,
        Err(_) => {
            let _ = fs::copy(backup_path, pam_path);
            return Err(
                "verification failed: cannot read back PAM file; rolled back changes".to_string(),
            );
        }
    };

    if require_token && !verified.contains(expected_token) {
        let _ = fs::copy(backup_path, pam_path);
        return Err(
            "verification failed: missing expected token after edit; rolled back changes"
                .to_string(),
        );
    }
    if !require_token && verified.contains(expected_token) {
        let _ = fs::copy(backup_path, pam_path);
        return Err(
            "verification failed: token still present after removal; rolled back changes"
                .to_string(),
        );
    }

    Ok(())
}

pub fn enable(pam_path: &Path, dry_run: bool) -> Result<(), String> {
    let status = get_status(pam_path);
    if !status.pam_file_exists {
        return Err(format!("PAM file does not exist: {}", pam_path.display()));
    }
    if status.enabled_included {
        println!(
            "Fingerprint authentication for sudo is already enabled via system PAM configuration (common-auth)."
        );
        return Ok(());
    }
    if status.enabled_direct {
        println!("Fingerprint authentication for sudo is already enabled.");
        return Ok(());
    }

    if dry_run {
        println!("[dry-run] Would enable fingerprint authentication for sudo");
        println!("[dry-run] Target: {}", pam_path.display());
        println!("[dry-run] Backup: .sudo.mu-bak-<timestamp>");
        println!("[dry-run] + {FPRINTD_PAM_LINE}");
        if !status.module_installed {
            println!("[dry-run] Note: pam_fprintd.so is not currently installed on this system");
        }
        oplog::log_outcome("sudo", "enable", "dry-run");
        return Ok(());
    }

    if !status.module_installed {
        return Err(
            "pam_fprintd.so is not installed on this system (install libpam-fprintd first)"
                .to_string(),
        );
    }

    let content = fs::read_to_string(pam_path)
        .map_err(|e| format!("cannot read PAM file {}: {}", pam_path.display(), e))?;

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
    let backup_path = backup_pam(pam_path, &content)?;

    atomic_write_pam(
        pam_path,
        &new_content,
        &backup_path,
        &content,
        "pam_fprintd.so",
        true,
    )?;

    println!("Fingerprint authentication enabled for sudo.");
    println!("Backup created at: {}", backup_path.display());
    oplog::log_outcome("sudo", "enable", "success");
    Ok(())
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
    if status.enabled_included && !status.enabled_direct {
        return Err(
            "Fingerprint authentication is managed globally via pam-auth-update (common-auth). To disable system-wide, run: sudo pam-auth-update"
                .to_string(),
        );
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

    if dry_run {
        println!("[dry-run] Would disable fingerprint authentication for sudo");
        println!("[dry-run] Target: {}", pam_path.display());
        println!("[dry-run] Backup: .sudo.mu-bak-<timestamp>");
        for line in &removed_lines {
            println!("[dry-run] - {line}");
        }
        oplog::log_outcome("sudo", "disable", "dry-run");
        return Ok(());
    }

    let backup_path = backup_pam(pam_path, &content)?;

    atomic_write_pam(
        pam_path,
        &new_content,
        &backup_path,
        &content,
        "pam_fprintd.so",
        false,
    )?;

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
            let state = if status.enabled_direct {
                "enabled"
            } else if status.enabled_included {
                "enabled (via common-auth)"
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

        // Ensure module check passes in test
        unsafe {
            std::env::set_var("MU_FPRINTD_INSTALLED", "1");
        }

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
        assert!(status.enabled_direct);

        // Backup must exist and be hidden (starts with .)
        let mut found_backup = false;
        for entry in fs::read_dir(&tmp).unwrap().flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with(".sudo.mu-bak-") {
                found_backup = true;
                let backup_content = fs::read_to_string(entry.path()).unwrap();
                assert!(!backup_content.contains("pam_fprintd.so"));
            }
        }
        assert!(found_backup, "hidden timestamped backup file must exist");

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

        unsafe {
            std::env::remove_var("MU_FPRINTD_INSTALLED");
        }
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_status_and_enable_with_common_auth_include() {
        let tmp = std::env::temp_dir().join(format!("mu-sudo-inc-{}", std::process::id()));
        let _ = fs::create_dir_all(&tmp);
        let pam_file = tmp.join("sudo");
        fs::write(&pam_file, SAMPLE_PAM).unwrap();

        let common_auth = tmp.join("common-auth");
        fs::write(
            &common_auth,
            "auth [success=1 default=ignore] pam_fprintd.so\nauth requisite pam_deny.so\n",
        )
        .unwrap();

        let status = get_status(&pam_file);
        assert!(status.enabled);
        assert!(status.enabled_included);
        assert!(!status.enabled_direct);

        // Enable should detect it's already enabled via common-auth and not modify sudo
        let res = enable(&pam_file, false);
        assert!(res.is_ok());
        let sudo_content = fs::read_to_string(&pam_file).unwrap();
        assert!(
            !sudo_content.contains("pam_fprintd.so"),
            "must not add duplicate pam_fprintd.so when already in common-auth"
        );

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_enable_refuses_when_module_not_installed() {
        let tmp = std::env::temp_dir().join(format!("mu-sudo-nomod-{}", std::process::id()));
        let _ = fs::create_dir_all(&tmp);
        let pam_file = tmp.join("sudo");
        fs::write(&pam_file, SAMPLE_PAM).unwrap();

        unsafe {
            std::env::set_var("MU_FPRINTD_INSTALLED", "0");
        }

        let res = enable(&pam_file, false);
        assert!(res.is_err(), "must refuse when module not installed");
        assert!(res.unwrap_err().contains("not installed"));
        assert!(
            !get_status(&pam_file).enabled,
            "PAM file must not be modified"
        );

        unsafe {
            std::env::remove_var("MU_FPRINTD_INSTALLED");
        }
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_atomic_write_rollback_on_verification_failure() {
        let tmp = std::env::temp_dir().join(format!("mu-sudo-rollback-{}", std::process::id()));
        let _ = fs::create_dir_all(&tmp);
        let pam_file = tmp.join("sudo");
        fs::write(&pam_file, SAMPLE_PAM).unwrap();

        let backup_file = tmp.join(".sudo.mu-bak-test");
        fs::write(&backup_file, SAMPLE_PAM).unwrap();

        // Force verification failure by requiring a token that is not in the new content
        let res = atomic_write_pam(
            &pam_file,
            "corrupted pam content",
            &backup_file,
            SAMPLE_PAM,
            "nonexistent_token_to_force_failure",
            true,
        );
        assert!(res.is_err());
        assert!(
            res.unwrap_err().contains("rolled back"),
            "must report rollback"
        );

        // Verify pam_file was restored to original SAMPLE_PAM
        let restored = fs::read_to_string(&pam_file).unwrap();
        assert_eq!(restored, SAMPLE_PAM);

        let _ = fs::remove_dir_all(&tmp);
    }
}
