//! Whitelist/config TOML loading and merging — port of `internal/utils/whitelist.go`
//! `Whitelist` struct, `defaultWhitelist`, `LoadWhitelist`, and
//! `validateWhitelistOverride`.
//!
//! Fail-closed contract (identical to Go): a malformed or semantically invalid
//! `~/.config/mu/config.toml` returns an error that blocks destructive
//! commands, including `--dry-run`. Unknown keys are rejected via
//! `deny_unknown_fields` (the serde equivalent of BurntSushi's `Undecoded()`).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result, msg};
use crate::{paths, xdg};

const DEFAULT_WHITELIST_TOML: &str = include_str!("default-whitelist.toml");

/// Holds path protection rules and skip lists.
#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Whitelist {
    #[serde(default)]
    pub protected_paths: ProtectedPaths,
    #[serde(default)]
    pub cache_skip: CacheSkip,
    #[serde(default)]
    pub optimize_skip: OptimizeSkip,
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProtectedPaths {
    #[serde(default)]
    pub system: Vec<String>,
    #[serde(default)]
    pub protect_running_kernel: bool,
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CacheSkip {
    #[serde(default)]
    pub dirs: Vec<String>,
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OptimizeSkip {
    #[serde(default)]
    pub steps: Vec<String>,
}

/// `defaultWhitelist` — embedded defaults, with the same safe-minimum
/// fallback Go uses if the TOML were somehow corrupt.
pub fn default_whitelist() -> Whitelist {
    toml::from_str::<Whitelist>(DEFAULT_WHITELIST_TOML).unwrap_or_else(|_| Whitelist {
        protected_paths: ProtectedPaths {
            system: [
                "/", "/boot", "/etc", "/usr", "/lib", "/lib64", "/bin", "/sbin", "/proc", "/sys",
                "/dev", "/run",
            ]
            .iter()
            .map(|s| s.to_string())
            .collect(),
            protect_running_kernel: true,
        },
        cache_skip: CacheSkip::default(),
        optimize_skip: OptimizeSkip::default(),
    })
}

/// `LoadWhitelist` — defaults merged with user overrides from
/// `$XDG_CONFIG_HOME/mu/config.toml`. Missing file → defaults.
pub fn load_whitelist() -> Result<Whitelist> {
    load_whitelist_from(&xdg::config_home())
}

/// Testable core: `config_home` is the directory that would contain `mu/`.
pub fn load_whitelist_from(config_home: &Path) -> Result<Whitelist> {
    let mut wl = default_whitelist();
    let user_cfg = config_home.join("mu").join("config.toml");
    if !paths::path_exists(&user_cfg) {
        return Ok(wl);
    }
    let text = std::fs::read_to_string(&user_cfg).map_err(Error::Io)?;
    let override_cfg: Whitelist = toml::from_str(&text).map_err(|e| {
        // BurntSushi reports the first undecoded key; toml's parse error text
        // differs but preserves the fail-closed signal.
        Error::Msg(format!("{}", e))
    })?;
    validate_whitelist_override(&override_cfg)?;
    if !override_cfg.protected_paths.system.is_empty() {
        wl.protected_paths
            .system
            .extend(override_cfg.protected_paths.system);
    }
    if !override_cfg.cache_skip.dirs.is_empty() {
        wl.cache_skip.dirs.extend(override_cfg.cache_skip.dirs);
    }
    if !override_cfg.optimize_skip.steps.is_empty() {
        wl.optimize_skip.steps = override_cfg.optimize_skip.steps;
    }
    Ok(wl)
}

/// Saves whitelist overrides to `$XDG_CONFIG_HOME/mu/config.toml`.
pub fn save_whitelist(wl: &Whitelist) -> Result<PathBuf> {
    save_whitelist_to(&xdg::config_home(), wl)
}

/// Testable core: saves whitelist to `<config_home>/mu/config.toml`.
pub fn save_whitelist_to(config_home: &Path, wl: &Whitelist) -> Result<PathBuf> {
    let mu_dir = config_home.join("mu");
    if !mu_dir.exists() {
        std::fs::create_dir_all(&mu_dir)?;
    }
    let user_cfg = mu_dir.join("config.toml");
    let serialized = toml::to_string_pretty(wl)
        .map_err(|e| Error::Msg(format!("failed to serialize config: {e}")))?;
    std::fs::write(&user_cfg, serialized)?;
    Ok(user_cfg)
}

fn validate_whitelist_override(override_cfg: &Whitelist) -> Result<()> {
    for path in &override_cfg.protected_paths.system {
        if !Path::new(path).is_absolute() {
            return msg(format!("protected path must be absolute: {path:?}"));
        }
    }
    for pattern in &override_cfg.cache_skip.dirs {
        // Go applies filepath.ToSlash here — a no-op on Unix (separator is
        // already '/'), so the pattern is used verbatim.
        let pat = pattern.trim();
        let p = Path::new(pat);
        if pat.is_empty() || p.is_absolute() || pat == ".." || pat.starts_with("../") {
            return msg(format!(
                "cache_skip pattern must stay relative to the cache root: {pat:?}"
            ));
        }
        if glob_validate(pat).is_err() {
            return msg(format!("invalid cache_skip pattern {pat:?}"));
        }
    }
    Ok(())
}

/// `filepath.Match(pattern, pattern)` validity check — a pattern is valid if
/// it scans without a syntax error (unclosed `[`, trailing `\`, bad range).
fn glob_validate(pattern: &str) -> std::result::Result<(), ()> {
    glob_match(pattern, pattern).map(|_| ())
}

/// Minimal `filepath.Match` port: `*` (any non-separator run), `?` (one
/// non-separator char), `[^class]` negated class (note: `^` only — Go treats
/// `!` as a literal class member), `\` escape incl. inside classes.
/// Patterns here never contain separators beyond `/`, which both `*` and `?`
/// refuse to cross — matching Go's `filepath.Match` rule that they don't
/// match the separator.
pub fn glob_match(pattern: &str, name: &str) -> std::result::Result<bool, ()> {
    let p: Vec<char> = pattern.chars().collect();
    let n: Vec<char> = name.chars().collect();
    let mut pi = 0;
    let mut ni = 0;
    while pi < p.len() {
        match p[pi] {
            '*' => {
                // collapse consecutive stars
                while pi < p.len() && p[pi] == '*' {
                    pi += 1;
                }
                if pi == p.len() {
                    // trailing star matches anything without '/'
                    return Ok(!n[ni..].contains(&'/'));
                }
                // try every split where the matched run has no '/'
                let mut k = ni;
                loop {
                    if glob_match(
                        &p[pi..].iter().collect::<String>(),
                        &n[k..].iter().collect::<String>(),
                    )? {
                        return Ok(true);
                    }
                    if k >= n.len() || n[k] == '/' {
                        return Ok(false);
                    }
                    k += 1;
                }
            }
            '?' => {
                if ni >= n.len() || n[ni] == '/' {
                    return Ok(false);
                }
                pi += 1;
                ni += 1;
            }
            '[' => {
                if ni >= n.len() {
                    return Ok(false);
                }
                let c = n[ni];
                if c == '/' {
                    return Ok(false);
                }
                let mut j = pi + 1;
                // Go negation is `^` only; `!` is a literal class member.
                let mut neg = false;
                if j < p.len() && p[j] == '^' {
                    neg = true;
                    j += 1;
                }
                let mut matched = false;
                let mut nrange = 0u32;
                let mut closed = false;
                while j < p.len() {
                    if p[j] == ']' && nrange > 0 {
                        closed = true;
                        j += 1;
                        break;
                    }
                    // getEsc: backslash escapes a literal char (incl. '-',
                    // ']', '\'); a trailing backslash is ErrBadPattern.
                    let lo = if p[j] == '\\' {
                        j += 1;
                        if j >= p.len() {
                            return Err(());
                        }
                        let c0 = p[j];
                        j += 1;
                        c0
                    } else {
                        let c0 = p[j];
                        j += 1;
                        c0
                    };
                    let mut hi = lo;
                    if j < p.len() && p[j] == '-' {
                        j += 1;
                        if j >= p.len() {
                            return Err(());
                        }
                        if p[j] == '\\' {
                            j += 1;
                            if j >= p.len() {
                                return Err(());
                            }
                        }
                        hi = p[j];
                        j += 1;
                    }
                    if lo > hi {
                        return Err(()); // bad range, e.g. [a-] (hi=']')
                    }
                    nrange += 1;
                    if c >= lo && c <= hi {
                        matched = true;
                    }
                }
                if !closed {
                    return Err(()); // unclosed class
                }
                if matched == neg {
                    return Ok(false);
                }
                pi = j;
                ni += 1;
            }
            '\\' => {
                if pi + 1 >= p.len() {
                    return Err(()); // trailing escape
                }
                if ni >= n.len() || n[ni] != p[pi + 1] {
                    return Ok(false);
                }
                pi += 2;
                ni += 1;
            }
            c => {
                if ni >= n.len() || n[ni] != c {
                    return Ok(false);
                }
                pi += 1;
                ni += 1;
            }
        }
    }
    Ok(ni == n.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

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

    fn write_config(root: &Path, body: &str) {
        let dir = root.join("mu");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("config.toml"), body).unwrap();
    }

    // Port of TestLoadWhitelist_NoUserConfig.
    #[test]
    fn load_whitelist_no_user_config() {
        let tmp = tempdir("wl-none");
        let wl = load_whitelist_from(&tmp).expect("load_whitelist_from");
        assert!(
            crate::whitelist::is_whitelisted(Path::new("/etc"), &wl),
            "default whitelist must protect /etc"
        );
        fs::remove_dir_all(&tmp).ok();
    }

    // Port of TestLoadWhitelist_UserOverride.
    #[test]
    fn load_whitelist_user_override() {
        let tmp = tempdir("wl-user");
        write_config(&tmp, "[optimize_skip]\nsteps = [\"apt\"]\n");
        let wl = load_whitelist_from(&tmp).expect("load with override");
        assert_eq!(wl.optimize_skip.steps, vec!["apt".to_string()]);
        fs::remove_dir_all(&tmp).ok();
    }

    // Port of TestLoadWhitelistRejectsUnknownAndSemanticInvalidKeys.
    #[test]
    fn load_whitelist_rejects_unknown_and_invalid() {
        for (name, config) in [
            ("unknown", "[protectd_paths]\nsystem = [\"/safe\"]\n"),
            (
                "relative-path",
                "[protected_paths]\nsystem = [\"relative\"]\n",
            ),
            ("escaping-skip", "[cache_skip]\ndirs = [\"../outside\"]\n"),
        ] {
            let root = tempdir("wl-bad");
            write_config(&root, config);
            assert!(
                load_whitelist_from(&root).is_err(),
                "expected invalid configuration rejection for {name}"
            );
            fs::remove_dir_all(&root).ok();
        }
    }

    // Port of TestDefaultWhitelist_HasGoBuildSkip.
    #[test]
    fn default_whitelist_has_go_build_skip() {
        let wl = default_whitelist();
        assert!(
            wl.cache_skip.dirs.iter().any(|d| d == "go-build"),
            "default cache_skip must include go-build"
        );
    }

    // Port of TestMalformedWhitelistFailsClosedEvenInDryRun (config side —
    // the SafeDelete side lives in trash.rs tests).
    #[test]
    fn malformed_whitelist_fails_closed() {
        let root = tempdir("wl-mal");
        write_config(&root, "not = [valid");
        assert!(load_whitelist_from(&root).is_err());
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn glob_match_basics() {
        assert_eq!(glob_match("go-build", "go-build"), Ok(true));
        assert_eq!(glob_match("go-*", "go-build"), Ok(true));
        assert_eq!(glob_match("*", "a/b"), Ok(false)); // * doesn't cross /
        assert_eq!(glob_match("*/startupCache", "ff/startupCache"), Ok(true));
        assert_eq!(
            glob_match("mozilla/*/startupCache", "mozilla/a/startupCache"),
            Ok(true)
        );
        assert_eq!(glob_match("Session*", "Session Restore"), Ok(true));
        assert_eq!(glob_match("[abc]", "b"), Ok(true));
        // Go semantics (verified against the oracle): `!` is literal, `^`
        // negates, `[a-]` is ErrBadPattern, `\-` escapes a literal '-'.
        assert_eq!(glob_match("[!abc]", "d"), Ok(false));
        assert_eq!(glob_match("[!abc]", "!"), Ok(true));
        assert_eq!(glob_match("[^abc]", "d"), Ok(true));
        assert_eq!(glob_match("[a-]", "-"), Err(()));
        assert_eq!(glob_match("[a\\-c]", "-"), Ok(true));
        assert_eq!(glob_match("[abc", "x"), Err(())); // unclosed class
        assert_eq!(glob_match("a\\", "a"), Err(())); // trailing escape
    }
}
