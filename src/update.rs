//! Self-update subsystem — queries GitHub Releases, verifies SHA-256 checksums,
//! and atomically replaces the running binary.
//!
//! Follows fail-closed security invariant: missing or mismatched checksum aborts
//! before modifying the binary. Supports `--dry-run` and `--force`.

use std::fs;
use std::path::{Path, PathBuf};

use crate::oplog;
use crate::runner::{CommandSpec, ProcessRunner, Runner};

const DEFAULT_REPO: &str = "baroitrung/mu";
const DEFAULT_API_URL: &str = "https://api.github.com/repos/baroitrung/mu/releases/latest";
const BINARY_NAME: &str = "mu";

#[derive(Clone, Debug, Default)]
pub struct Options {
    pub dry_run: bool,
    pub force: bool,
    pub debug: bool,
    pub target_bin: Option<PathBuf>,
    pub api_url: Option<String>,
}

#[derive(Clone, Debug, serde::Deserialize)]
pub struct ReleaseInfo {
    pub tag_name: String,
    pub name: Option<String>,
    pub body: Option<String>,
    #[serde(default)]
    pub assets: Vec<ReleaseAsset>,
}

#[derive(Clone, Debug, serde::Deserialize)]
pub struct ReleaseAsset {
    pub name: String,
    pub browser_download_url: String,
    pub size: Option<u64>,
}

/// Compute SHA-256 hex string of bytes.
pub fn compute_sha256(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

/// Parse GNU sha256sum formatted `checksums.txt` (`<hash>  <filename>`).
pub fn parse_checksum(content: &str, binary_name: &str) -> Option<String> {
    for line in content.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() >= 2 {
            let hash = parts[0];
            let name = parts[1].trim_start_matches('*');
            if name == binary_name {
                return Some(hash.to_lowercase());
            }
        }
    }
    None
}

/// Fetches bytes from a URL using `Runner` (`curl`) or local `file://` scheme.
pub fn fetch_bytes(url: &str, runner: &dyn Runner) -> Result<Vec<u8>, String> {
    if let Some(file_path) = url.strip_prefix("file://") {
        return fs::read(file_path)
            .map_err(|e| format!("cannot read local file {}: {}", file_path, e));
    }

    let spec = CommandSpec::new("curl", ["-fsSL", url]);
    match runner.run(&spec) {
        Ok(out) => Ok(out.stdout),
        Err(e) => Err(format!("failed to fetch {url}: {e}")),
    }
}

/// Query release info from GitHub API or custom/file URL.
pub fn fetch_release_info(api_url: &str, runner: &dyn Runner) -> Result<ReleaseInfo, String> {
    let bytes = fetch_bytes(api_url, runner)?;
    serde_json::from_slice::<ReleaseInfo>(&bytes)
        .map_err(|e| format!("invalid release JSON from {api_url}: {e}"))
}

pub fn resolve_target_bin(custom: Option<&Path>) -> Result<PathBuf, String> {
    if let Some(p) = custom {
        return Ok(p.to_path_buf());
    }
    std::env::current_exe().map_err(|e| format!("cannot resolve current executable path: {e}"))
}

pub fn current_version() -> &'static str {
    match option_env!("MU_VERSION") {
        Some(v) => v,
        None => "0.4.0",
    }
}

pub fn parse_version_tuple(v: &str) -> Option<(u64, u64, u64, u64)> {
    let clean = v.trim_start_matches('v');
    if clean == "dev" {
        return None;
    }
    let mut parts = clean.splitn(2, '-');
    let core = parts.next()?;
    let suffix = parts.next();
    let nums: Vec<&str> = core.split('.').collect();
    if nums.len() < 3 {
        return None;
    }
    let major = nums[0].parse::<u64>().ok()?;
    let minor = nums[1].parse::<u64>().ok()?;
    let patch = nums[2].parse::<u64>().ok()?;
    let commits_ahead = if let Some(s) = suffix {
        s.split('-')
            .next()
            .and_then(|c| c.parse::<u64>().ok())
            .unwrap_or(0)
    } else {
        0
    };
    Some((major, minor, patch, commits_ahead))
}

pub fn is_newer_version(latest_tag: &str, current: &str) -> bool {
    let clean_curr = current.trim_start_matches('v');
    if clean_curr == "dev" {
        return false;
    }
    match (
        parse_version_tuple(latest_tag),
        parse_version_tuple(current),
    ) {
        (Some(latest), Some(curr)) => {
            if latest.0 != curr.0 {
                return latest.0 > curr.0;
            }
            if latest.1 != curr.1 {
                return latest.1 > curr.1;
            }
            if latest.2 != curr.2 {
                return latest.2 > curr.2;
            }
            latest.3 > curr.3
        }
        _ => latest_tag.trim_start_matches('v') != current.trim_start_matches('v'),
    }
}

pub fn target_arch_asset_names() -> Vec<String> {
    target_arch_asset_names_for(std::env::consts::ARCH)
}

pub fn target_arch_asset_names_for(arch: &str) -> Vec<String> {
    let mut names = vec![
        format!("mu-{arch}-unknown-linux-gnu"),
        format!("mu-linux-{arch}"),
    ];
    if arch == "x86_64" {
        names.push(BINARY_NAME.to_string());
    }
    names
}

pub fn check_background_update_silently() {
    let cache_dir = crate::xdg::cache_home().join("mu");
    let cache_file = cache_dir.join("last_version_check.json");
    if let Ok(meta) = fs::metadata(&cache_file)
        && let Ok(modified) = meta.modified()
        && let Ok(elapsed) = modified.elapsed()
        && elapsed.as_secs() < 86400
    {
        return;
    }
    let _ = std::thread::Builder::new()
        .name("mu-update-check".to_string())
        .spawn(move || {
            let runner = ProcessRunner;
            let url =
                std::env::var("MU_UPDATE_URL").unwrap_or_else(|_| DEFAULT_API_URL.to_string());
            if let Ok(rel) = fetch_release_info(&url, &runner) {
                let ts = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                let payload = serde_json::json!({
                    "latest": rel.tag_name,
                    "checked_at": ts,
                });
                let _ = fs::create_dir_all(&cache_dir);
                let tmp_file = cache_dir.join(format!(".check.tmp-{}", std::process::id()));
                if fs::write(&tmp_file, payload.to_string()).is_ok() {
                    let _ = fs::rename(&tmp_file, &cache_file);
                }
            }
        });
}

pub fn run_with(opts: &Options, runner: &dyn Runner) -> i32 {
    let env_url = std::env::var("MU_UPDATE_URL").ok();
    let api_url = opts
        .api_url
        .as_deref()
        .or(env_url.as_deref())
        .unwrap_or(DEFAULT_API_URL);

    let target_bin = match resolve_target_bin(opts.target_bin.as_deref()) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("error: {e}");
            return 1;
        }
    };

    let curr_ver = current_version();
    let ver_display = if curr_ver.starts_with('v') {
        curr_ver.to_string()
    } else {
        format!("v{curr_ver}")
    };
    println!("Checking for updates (current version: {ver_display})...");

    let release = match fetch_release_info(api_url, runner) {
        Ok(rel) => rel,
        Err(e) => {
            eprintln!("error: could not query GitHub release API: {e}");
            return 1;
        }
    };

    let latest_tag = &release.tag_name;
    let has_update = is_newer_version(latest_tag, curr_ver) || opts.force;

    if !has_update {
        println!("mu is already up-to-date ({latest_tag}).");
        return 0;
    }

    println!("Found newer release: {latest_tag}");

    let candidate_names = target_arch_asset_names();
    let bin_asset = candidate_names
        .iter()
        .find_map(|name| release.assets.iter().find(|a| &a.name == name));
    let bin_asset = match bin_asset {
        Some(a) => a,
        None => {
            let arch = std::env::consts::ARCH;
            eprintln!("error: no release binary for {arch}");
            return 1;
        }
    };
    let resolved_bin_name = &bin_asset.name;
    let checksum_asset = release.assets.iter().find(|a| a.name == "checksums.txt");

    if opts.dry_run {
        println!("[dry-run] Would update mu to {latest_tag}");
        println!("[dry-run] Target executable: {}", target_bin.display());
        println!("[dry-run] Binary asset: {}", bin_asset.browser_download_url);
        if let Some(c) = checksum_asset {
            println!("[dry-run] Checksum asset: {}", c.browser_download_url);
        } else {
            println!("[dry-run] Checksum asset: default release asset 'checksums.txt'");
        }
        oplog::log_outcome("update", latest_tag, "dry-run");
        return 0;
    }

    // Download checksums.txt
    let fallback_checksum_url;
    let checksum_url = match checksum_asset {
        Some(a) => a.browser_download_url.as_str(),
        None => {
            fallback_checksum_url = format!(
                "https://github.com/{DEFAULT_REPO}/releases/download/{latest_tag}/checksums.txt"
            );
            &fallback_checksum_url
        }
    };

    println!("Downloading checksums.txt...");
    let checksum_bytes = match fetch_bytes(checksum_url, runner) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("error: fail-closed: cannot download checksums: {e}");
            return 1;
        }
    };

    let checksum_text = String::from_utf8_lossy(&checksum_bytes);
    let expected_hash = match parse_checksum(&checksum_text, resolved_bin_name)
        .or_else(|| parse_checksum(&checksum_text, BINARY_NAME))
    {
        Some(h) => h,
        None => {
            eprintln!("error: fail-closed: checksums.txt missing entry for {resolved_bin_name}");
            return 1;
        }
    };

    // Download binary
    let bin_url = bin_asset.browser_download_url.as_str();

    println!("Downloading {resolved_bin_name} {latest_tag}...");

    let new_bin_bytes = match fetch_bytes(bin_url, runner) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("error: failed to download binary: {e}");
            return 1;
        }
    };

    // Verify SHA-256
    let actual_hash = compute_sha256(&new_bin_bytes);
    if actual_hash != expected_hash {
        eprintln!("error: SHA-256 checksum mismatch for {resolved_bin_name}!");
        eprintln!("  expected: {expected_hash}");
        eprintln!("  actual:   {actual_hash}");
        eprintln!("Update aborted (fail-closed). Current binary was NOT modified.");
        oplog::log_outcome("update", latest_tag, "failure-checksum-mismatch");
        return 1;
    }

    println!("SHA-256 checksum verified: {actual_hash}");

    // Atomic replacement
    let target_dir = target_bin.parent().unwrap_or_else(|| Path::new("."));
    let temp_target = target_dir.join(format!(".mu-update-{}", std::process::id()));

    if let Err(e) = fs::write(&temp_target, &new_bin_bytes) {
        eprintln!(
            "error: cannot write to {} (permission denied? run with sudo): {}",
            temp_target.display(),
            e
        );
        return 1;
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(e) = fs::set_permissions(&temp_target, fs::Permissions::from_mode(0o755)) {
            let _ = fs::remove_file(&temp_target);
            eprintln!("error: failed to set executable permissions: {e}");
            return 1;
        }
    }

    if let Ok(file) = fs::File::open(&temp_target) {
        let _ = file.sync_all();
    }
    if let Err(e) = fs::rename(&temp_target, &target_bin) {
        let _ = fs::remove_file(&temp_target);
        eprintln!("error: failed to replace {}: {}", target_bin.display(), e);
        return 1;
    }

    println!("Successfully updated mu to {latest_tag}!");
    oplog::log_outcome("update", latest_tag, "success");
    0
}

pub fn run(opts: &Options) -> i32 {
    run_with(opts, &ProcessRunner)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compute_sha256() {
        let data = b"hello world\n";
        let hash = compute_sha256(data);
        assert_eq!(
            hash,
            "a948904f2f0f479b8f8197694b30184b0d2ed1c1cd2a1ec0fb85d299a192a447"
        );
    }

    #[test]
    fn test_parse_checksum() {
        let content = "\
a948904f2f0f479b8f8197694b30184b0d2ed1c1cd2a1ec0fb85d299a192a447  mu
e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855 *other
";
        let hash = parse_checksum(content, "mu");
        assert_eq!(
            hash.unwrap(),
            "a948904f2f0f479b8f8197694b30184b0d2ed1c1cd2a1ec0fb85d299a192a447"
        );

        let other = parse_checksum(content, "other");
        assert_eq!(
            other.unwrap(),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );

        assert!(parse_checksum(content, "nonexistent").is_none());
    }

    #[test]
    fn test_offline_update_verification() {
        let tmp = std::env::temp_dir().join(format!("mu-update-test-{}", std::process::id()));
        let _ = fs::create_dir_all(&tmp);

        let target_bin = tmp.join("mu");
        fs::write(&target_bin, b"original-mu").unwrap();

        let new_bin_content = b"brand-new-mu-v0.4.0";
        let new_bin_hash = compute_sha256(new_bin_content);

        let new_bin_file = tmp.join("new-mu");
        fs::write(&new_bin_file, new_bin_content).unwrap();

        let checksum_file = tmp.join("checksums.txt");
        fs::write(&checksum_file, format!("{new_bin_hash}  mu\n")).unwrap();

        let release_json = tmp.join("release.json");
        let json_content = serde_json::json!({
            "tag_name": "v0.4.0",
            "name": "v0.4.0",
            "assets": [
                {
                    "name": "mu",
                    "browser_download_url": format!("file://{}", new_bin_file.display())
                },
                {
                    "name": "checksums.txt",
                    "browser_download_url": format!("file://{}", checksum_file.display())
                }
            ]
        });
        fs::write(&release_json, json_content.to_string()).unwrap();

        // Dry-run
        let dry_opts = Options {
            dry_run: true,
            force: true,
            target_bin: Some(target_bin.clone()),
            api_url: Some(format!("file://{}", release_json.display())),
            ..Default::default()
        };
        let code = run_with(&dry_opts, &ProcessRunner);
        assert_eq!(code, 0);
        assert_eq!(fs::read(&target_bin).unwrap(), b"original-mu");

        // Real update
        let real_opts = Options {
            dry_run: false,
            force: true,
            target_bin: Some(target_bin.clone()),
            api_url: Some(format!("file://{}", release_json.display())),
            ..Default::default()
        };
        let code = run_with(&real_opts, &ProcessRunner);
        assert_eq!(code, 0);
        assert_eq!(fs::read(&target_bin).unwrap(), new_bin_content);

        // Mismatched checksum fails closed
        fs::write(
            &checksum_file,
            "0000000000000000000000000000000000000000000000000000000000000000  mu\n",
        )
        .unwrap();
        let fail_opts = Options {
            dry_run: false,
            force: true,
            target_bin: Some(target_bin.clone()),
            api_url: Some(format!("file://{}", release_json.display())),
            ..Default::default()
        };
        let code = run_with(&fail_opts, &ProcessRunner);
        assert_eq!(code, 1);
        // Target binary remains unchanged
        assert_eq!(fs::read(&target_bin).unwrap(), new_bin_content);

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_semver_ordering() {
        assert!(is_newer_version("v0.4.0", "0.3.0"));
        assert!(is_newer_version("0.3.1", "0.3.0"));
        assert!(!is_newer_version("v0.2.0", "0.3.0"));
        assert!(!is_newer_version("v0.3.0", "0.3.0"));
        // Git checkout ahead of tag must NOT be downgraded to tag
        assert!(!is_newer_version("v0.3.0", "0.3.0-9-gccc0649"));
        // Higher tag is newer than git checkout of older base
        assert!(is_newer_version("v0.4.0", "0.3.0-9-gccc0649"));
        // Dev is never updated
        assert!(!is_newer_version("v1.0.0", "dev"));
    }

    #[test]
    fn test_dry_run_api_failure_fails_closed() {
        let dry_opts = Options {
            dry_run: true,
            force: false,
            api_url: Some("file:///nonexistent/path/to/release.json".to_string()),
            ..Default::default()
        };
        let code = run_with(&dry_opts, &ProcessRunner);
        assert_eq!(code, 1, "update dry-run must fail when release API fails");
    }

    #[test]
    fn test_arch_asset_preference() {
        let x86_names = target_arch_asset_names_for("x86_64");
        assert!(x86_names.contains(&"mu".to_string()));
        assert!(x86_names.contains(&"mu-x86_64-unknown-linux-gnu".to_string()));

        let arm_names = target_arch_asset_names_for("aarch64");
        assert!(!arm_names.contains(&"mu".to_string()));
        assert!(arm_names.contains(&"mu-aarch64-unknown-linux-gnu".to_string()));

        let release = ReleaseInfo {
            tag_name: "v0.4.0".to_string(),
            name: None,
            body: None,
            assets: vec![
                ReleaseAsset {
                    name: "mu".to_string(),
                    browser_download_url: "https://example.com/mu".to_string(),
                    size: None,
                },
                ReleaseAsset {
                    name: "mu-x86_64-unknown-linux-gnu".to_string(),
                    browser_download_url: "https://example.com/mu-x86".to_string(),
                    size: None,
                },
            ],
        };

        let picked = x86_names
            .iter()
            .find_map(|name| release.assets.iter().find(|a| &a.name == name))
            .unwrap();
        assert_eq!(picked.name, "mu-x86_64-unknown-linux-gnu");
    }

    #[test]
    fn test_non_x86_rejects_generic_mu_asset() {
        let arm_names = target_arch_asset_names_for("aarch64");
        let release = ReleaseInfo {
            tag_name: "v0.4.0".to_string(),
            name: None,
            body: None,
            assets: vec![ReleaseAsset {
                name: "mu".to_string(),
                browser_download_url: "https://example.com/mu".to_string(),
                size: None,
            }],
        };
        let picked = arm_names
            .iter()
            .find_map(|name| release.assets.iter().find(|a| &a.name == name));
        assert!(picked.is_none(), "aarch64 must reject generic x86 mu asset");
    }
}
