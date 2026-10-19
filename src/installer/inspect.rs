//! Archive content inspector for ZIP and tar.gz installer detection.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::time::Duration;

use crate::runner::{CommandSpec, ProcessRunner, Runner};

/// Package/container extensions that identify an installer by themselves.
const PACKAGE_EXTS: &[&str] = &["deb", "appimage", "iso", "exe", "pkg", "rpm", "msi"];

/// Extensions an `install*`/`setup*` script or binary may carry.
const SCRIPT_EXTS: &[&str] = &["sh", "bin", "run", "exe"];

/// Check if an entry name within an archive indicates an installer or
/// executable binary.
///
/// Review F3: an archive used to count as an installer when *any* entry ended
/// in `.sh`/`.bin`/`.run`, so a project archive holding `build.sh` matched.
/// Now a script entry must itself be an installer (`install*`/`setup*` with a
/// script/binary extension), and Python's `setup.py` no longer matches.
pub fn is_installer_entry(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    let basename = Path::new(&lower)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(&lower);

    let (stem, ext) = match basename.rsplit_once('.') {
        Some((stem, ext)) => (stem, Some(ext)),
        None => (basename, None),
    };

    // `package.deb`, `app.AppImage`, `setup.exe` are installers by extension.
    if let Some(ext) = ext
        && PACKAGE_EXTS.contains(&ext)
    {
        return true;
    }

    // A bare script/binary (or one with a script extension) named install*/setup*.
    let is_script = match ext {
        None => true,
        Some(ext) => SCRIPT_EXTS.contains(&ext),
    };
    is_script && (stem.starts_with("install") || stem.starts_with("setup") || stem == "configure")
}

/// Inspect a ZIP archive's central directory to determine if it contains installer files.
/// Pure standard library implementation (no heavy crate dependencies).
pub fn inspect_zip(path: &Path) -> bool {
    let mut file = match File::open(path) {
        Ok(f) => f,
        Err(_) => return false,
    };

    let file_len = match file.metadata() {
        Ok(m) => m.len(),
        Err(_) => return false,
    };

    if file_len < 22 {
        return false;
    }

    // Search backwards for the End of Central Directory (EOCD) signature: 0x06054b50
    let search_len = std::cmp::min(file_len, 65536 + 22) as usize;
    let mut buffer = vec![0u8; search_len];
    let offset = file_len - search_len as u64;

    if file.seek(SeekFrom::Start(offset)).is_err() {
        return false;
    }
    if file.read_exact(&mut buffer).is_err() {
        return false;
    }

    // Find EOCD marker: PK\x05\x06
    let mut eocd_rel_pos = None;
    for i in (0..search_len.saturating_sub(3)).rev() {
        if buffer[i] == 0x50
            && buffer[i + 1] == 0x4b
            && buffer[i + 2] == 0x05
            && buffer[i + 3] == 0x06
        {
            eocd_rel_pos = Some(i);
            break;
        }
    }

    let eocd_pos = match eocd_rel_pos {
        Some(pos) => pos,
        None => return false,
    };

    if eocd_pos + 22 > search_len {
        return false;
    }

    let total_entries = u16::from_le_bytes([buffer[eocd_pos + 10], buffer[eocd_pos + 11]]) as usize;
    let cd_offset = u32::from_le_bytes([
        buffer[eocd_pos + 16],
        buffer[eocd_pos + 17],
        buffer[eocd_pos + 18],
        buffer[eocd_pos + 19],
    ]) as u64;

    if file.seek(SeekFrom::Start(cd_offset)).is_err() {
        return false;
    }

    // Read central directory records
    let mut header = [0u8; 46];
    for _ in 0..total_entries {
        if file.read_exact(&mut header).is_err() {
            break;
        }

        // Check Central Directory Header magic: PK\x01\x02
        if header[0] != 0x50 || header[1] != 0x4b || header[2] != 0x01 || header[3] != 0x02 {
            break;
        }

        let name_len = u16::from_le_bytes([header[28], header[29]]) as usize;
        let extra_len = u16::from_le_bytes([header[30], header[31]]) as usize;
        let comment_len = u16::from_le_bytes([header[32], header[33]]) as usize;

        let mut name_buf = vec![0u8; name_len];
        if file.read_exact(&mut name_buf).is_err() {
            break;
        }

        let entry_name = String::from_utf8_lossy(&name_buf);
        if is_installer_entry(&entry_name) {
            return true;
        }

        // Skip extra field and comment
        if file
            .seek(SeekFrom::Current((extra_len + comment_len) as i64))
            .is_err()
        {
            break;
        }
    }

    false
}

/// Inspect a tar.gz / tgz archive by listing filenames using `tar -tf`.
pub fn inspect_targz(path: &Path) -> bool {
    let runner = ProcessRunner;
    let mut spec = CommandSpec::new("tar", ["-tf".as_ref(), path.as_os_str()]);
    spec.timeout = Some(Duration::from_secs(2));

    let output = match runner.run(&spec) {
        Ok(out) => out,
        Err(_) => return false,
    };

    let stdout_str = String::from_utf8_lossy(&output.stdout);
    for line in stdout_str.lines() {
        let trimmed = line.trim();
        if !trimmed.is_empty() && is_installer_entry(trimmed) {
            return true;
        }
    }

    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_installer_entry() {
        assert!(is_installer_entry("setup.sh"));
        assert!(is_installer_entry("sub/dir/install.bin"));
        assert!(is_installer_entry("package.deb"));
        assert!(is_installer_entry("app.AppImage"));
        assert!(is_installer_entry("setup.exe"));
        assert!(is_installer_entry("installer"));
        assert!(!is_installer_entry("readme.txt"));
        assert!(!is_installer_entry("image.png"));
        assert!(!is_installer_entry("document.pdf"));
    }

    // Review F3: a project archive's build scripts and Python's setup.py must
    // not make the archive an "installer".
    #[test]
    fn installer_entry_rejects_build_script_and_python_setup() {
        assert!(!is_installer_entry("build.sh"));
        assert!(!is_installer_entry("project/scripts/build.sh"));
        assert!(!is_installer_entry("setup.py"));
        assert!(!is_installer_entry("project/install.md"));
        // Real installers still match.
        assert!(is_installer_entry("configure"));
        assert!(is_installer_entry("project/install.sh"));
        assert!(is_installer_entry("nested/setup.run"));
    }

    fn tempdir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!(
            "mu-test-inspect-{}-{}-{}",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// Build a `.tar.gz` fixture with `tar` (the same tool `inspect_targz`
    /// shells out to).
    fn targz_fixture(dir: &Path, name: &str, entries: &[(&str, &str)]) -> std::path::PathBuf {
        let src = dir.join(format!("{name}-src"));
        for (rel, body) in entries {
            let p = src.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, body).unwrap();
        }
        let out = dir.join(name);
        let status = std::process::Command::new("tar")
            .arg("czf")
            .arg(&out)
            .arg("-C")
            .arg(&src)
            .arg(".")
            .status()
            .expect("tar must be available");
        assert!(status.success(), "tar failed for {name}");
        out
    }

    // Review F3: a project archive holding `build.sh` is not an installer;
    // one holding `install.sh` is.
    #[test]
    fn installer_ignores_project_archive_with_build_script() {
        let dir = tempdir("targz");
        let project = targz_fixture(
            &dir,
            "backup-2025.tar.gz",
            &[("project/build.sh", "#!/bin/sh\nmake\n")],
        );
        let real = targz_fixture(
            &dir,
            "tool-1.0.tar.gz",
            &[("tool/install.sh", "#!/bin/sh\ncp app /usr/local/bin\n")],
        );

        assert!(
            !inspect_targz(&project),
            "a project archive with build.sh must not count as an installer"
        );
        assert!(
            inspect_targz(&real),
            "an archive with install.sh is an installer"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
