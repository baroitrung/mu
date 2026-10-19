//! XDG Base Directory resolution — port of `internal/utils` xdg helpers.
//!
//! Relative XDG env values are ignored (fail closed), matching Go's
//! `filepath.IsAbs` gate in `xdgHome`.

use std::env;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// Lexical path cleanup equivalent to Go's `filepath.Clean` on Unix:
/// collapses slashes, drops `.`, resolves `..` lexically (`..` at the root is
/// a no-op; `..` in a relative path with nothing to pop is preserved).
pub fn clean(p: &Path) -> PathBuf {
    use std::path::Component;
    let mut comps: Vec<Component> = Vec::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => match comps.last() {
                Some(Component::Normal(_)) => {
                    comps.pop();
                }
                Some(Component::RootDir) => {} // ".." at root stays at root
                _ => comps.push(c),            // relative path: preserve ".."
            },
            other => comps.push(other),
        }
    }
    if comps.is_empty() {
        PathBuf::from(".")
    } else {
        comps.iter().map(|c| c.as_os_str()).collect()
    }
}

/// Equivalent of `os.UserHomeDir()` on Unix: `$HOME`.
pub fn home_dir() -> PathBuf {
    env::var_os("HOME").map(PathBuf::from).unwrap_or_default()
}

/// Core of Go's `xdgHome`: use the env value only when absolute.
fn resolve(env_val: Option<&OsStr>, fallback: PathBuf) -> PathBuf {
    if let Some(v) = env_val {
        let p = PathBuf::from(v);
        if p.is_absolute() {
            return clean(&p);
        }
    }
    clean(&fallback)
}

pub fn cache_home() -> PathBuf {
    resolve(
        env::var_os("XDG_CACHE_HOME").as_deref(),
        home_dir().join(".cache"),
    )
}

pub fn config_home() -> PathBuf {
    resolve(
        env::var_os("XDG_CONFIG_HOME").as_deref(),
        home_dir().join(".config"),
    )
}

pub fn data_home() -> PathBuf {
    resolve(
        env::var_os("XDG_DATA_HOME").as_deref(),
        home_dir().join(".local").join("share"),
    )
}

/// `$XDG_STATE_HOME` or `~/.local/state`. Relative values are ignored like
/// the other XDG roots (fail closed).
pub fn state_home() -> PathBuf {
    resolve(
        env::var_os("XDG_STATE_HOME").as_deref(),
        home_dir().join(".local").join("state"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    // Port of TestXDGRelativePathsAreIgnored — relative env values fall back.
    #[test]
    fn xdg_relative_paths_are_ignored() {
        let home = Path::new("/tmp/mu-test-home");
        let rel = Some(OsStr::new("relative/cache"));
        assert_eq!(
            resolve(rel, home.join(".cache")),
            home.join(".cache"),
            "relative XDG_CACHE_HOME must be ignored"
        );
        assert_eq!(
            resolve(rel, home.join(".config")),
            home.join(".config"),
            "relative XDG_CONFIG_HOME must be ignored"
        );
        assert_eq!(
            resolve(rel, home.join(".local").join("share")),
            home.join(".local").join("share"),
            "relative XDG_DATA_HOME must be ignored"
        );
        assert_eq!(
            resolve(rel, home.join(".local").join("state")),
            home.join(".local").join("state"),
            "relative XDG_STATE_HOME must be ignored"
        );
    }

    #[test]
    fn xdg_absolute_env_value_wins_and_is_cleaned() {
        let home = Path::new("/tmp/mu-test-home");
        let val = OsString::from("/opt/custom/../cache/");
        assert_eq!(
            resolve(Some(&val), home.join(".cache")),
            PathBuf::from("/opt/cache")
        );
    }

    #[test]
    fn xdg_unset_env_uses_fallback() {
        assert_eq!(
            resolve(None, PathBuf::from("/h/.cache")),
            PathBuf::from("/h/.cache")
        );
    }

    #[test]
    fn clean_normalizes_dot_segments() {
        assert_eq!(clean(Path::new("/a//b/./c/../d")), PathBuf::from("/a/b/d"));
        assert_eq!(clean(Path::new("/")), PathBuf::from("/"));
    }
}
