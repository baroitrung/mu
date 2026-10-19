//! `remnants.go` — leftover file detection for removed packages: the
//! `knownAliases` name map, `FindRemnants` (the per-name candidate scan over
//! the XDG homes and `/var/lib`), and `RemnantSize` (user-owned remnant
//! sizing, `/var/` skipped).

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::paths;
use crate::size;

use super::Deps;

/// `knownAliases` — package name → the folder name it actually uses in
/// `~/.config` etc.
pub(crate) const KNOWN_ALIASES: &[(&str, &str)] = &[
    ("code", "Code"),
    ("google-chrome-stable", "google-chrome"),
    ("google-chrome", "google-chrome"),
    ("chromium-browser", "chromium"),
    ("vscode", "Code"),
    ("slack-desktop", "Slack"),
    ("discord", "discord"),
    ("spotify-client", "spotify"),
];

/// `knownAliases[name]` lookup.
pub(crate) fn known_alias(name: &str) -> Option<&'static str> {
    KNOWN_ALIASES
        .iter()
        .find(|(k, _)| *k == name)
        .map(|(_, v)| *v)
}

/// `FindRemnants` — production wrapper reading the real environment.
pub fn find_remnants(name: &str) -> Vec<String> {
    find_remnants_in(&Deps::real(), name)
}

/// `FindRemnants` — existing remnant directories for a package under
/// `$XDG_CONFIG_HOME`, `$XDG_DATA_HOME`, `$XDG_CACHE_HOME`, and `/var/lib`,
/// for the package name and its alias. Returns nothing when the home
/// directory cannot be resolved (Go's `os.UserHomeDir` error → nil).
pub(crate) fn find_remnants_in(deps: &Deps, name: &str) -> Vec<String> {
    if deps.home.as_os_str().is_empty() {
        return Vec::new();
    }

    // Collect candidate names (original + alias).
    let mut names = vec![name.to_string()];
    if let Some(alias) = known_alias(name)
        && alias != name
    {
        names.push(alias.to_string());
    }

    let mut found: Vec<String> = Vec::new();
    let mut seen: HashSet<PathBuf> = HashSet::new();

    for n in &names {
        let candidates = vec![
            deps.config_home.join(n),
            deps.data_home.join(n),
            deps.cache_home.join(n),
            Path::new("/var/lib").join(n),
            // systemd user units
            deps.config_home
                .join("systemd")
                .join("user")
                .join(format!("{n}.service")),
            // desktop entries
            deps.data_home
                .join("applications")
                .join(format!("{n}.desktop")),
            deps.config_home
                .join("autostart")
                .join(format!("{n}.desktop")),
        ];

        for p in candidates {
            if !seen.insert(p.clone()) {
                continue;
            }
            if paths::path_exists(&p) {
                found.push(p.to_string_lossy().into_owned());
            }
        }
    }
    found
}

/// `RemnantSize` — total size in bytes of user-owned remnant dirs.
/// `/var/` paths are skipped (would need sudo); `DirSize` errors skip the
/// entry (Go adds the size only when `err == nil`, dropping partial totals).
pub fn remnant_size(paths: &[String]) -> i64 {
    let mut total: i64 = 0;
    for p in paths {
        if p.starts_with("/var/") {
            continue; // skip root-owned
        }
        let (size, err) = size::dir_size(Path::new(p));
        if err.is_none() {
            total += size as i64;
        }
    }
    total
}

/// One installed Steam application discovered from an `appmanifest_*.acf`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SteamGame {
    pub appid: String,
    pub name: String,
}

/// `acf_field` — value of `"key"\t\t"value"` in a Steam appmanifest.
fn acf_field(content: &str, key: &str) -> Option<String> {
    for line in content.lines() {
        let line = line.trim();
        let Some(rest) = line.strip_prefix('"') else {
            continue;
        };
        let mut it = rest.splitn(2, '"');
        let k = it.next()?;
        if k != key {
            continue;
        }
        let after = it.next()?.trim_start().strip_prefix('"')?;
        let end = after.find('"')?;
        return Some(after[..end].to_string());
    }
    None
}

/// `steamGames` — Steam apps installed under `$XDG_DATA_HOME/Steam/steamapps`.
/// The manifests are read (never modified): uninstalling a game is delegated
/// to Steam rather than trashing Steam's own database.
pub(crate) fn steam_games_in(deps: &Deps) -> Vec<SteamGame> {
    let mut games = Vec::new();
    let steamapps = deps.data_home.join("Steam").join("steamapps");
    let Ok(entries) = std::fs::read_dir(&steamapps) else {
        return games;
    };
    let mut files: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|f| f.to_str())
                .is_some_and(|f| f.starts_with("appmanifest_") && f.ends_with(".acf"))
        })
        .collect();
    files.sort();
    for p in files {
        let Ok(content) = std::fs::read_to_string(&p) else {
            continue;
        };
        let Some(appid) = acf_field(&content, "appid") else {
            continue;
        };
        let Some(name) = acf_field(&content, "name") else {
            continue;
        };
        games.push(SteamGame { appid, name });
    }
    games
}

/// Games whose display name matches `pkg_name` exactly (case-insensitive).
/// Substring matching is deliberately avoided: short package names (`git`,
/// `ui`) would otherwise match unrelated manifests.
pub(crate) fn steam_delegations(deps: &Deps, pkg_name: &str) -> Vec<SteamGame> {
    let needle = pkg_name.to_lowercase();
    steam_games_in(deps)
        .into_iter()
        .filter(|g| g.name.to_lowercase() == needle)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::FakeRunner;
    use crate::uninstall::test_support::{deps_for_test, tempdir};
    use std::fs;
    use std::sync::Arc;

    // remnants_test.go: TestFindRemnants_FindsExistingDirs — Go overrides
    // the XDG dirs via env; the port injects them through Deps.
    #[test]
    fn find_remnants_finds_existing_dirs() {
        let tmp = tempdir("remnants");
        let deps = deps_for_test(&tmp, Arc::new(FakeRunner::new()));

        // Create a fake remnant dir (Go: tmp/config/testpkg).
        let config_dir = tmp.join("config").join("testpkg");
        fs::create_dir_all(&config_dir).unwrap();

        let deps = Deps {
            config_home: tmp.join("config"),
            data_home: tmp.join("data"),
            cache_home: tmp.join("cache"),
            ..deps
        };
        let remnants = find_remnants_in(&deps, "testpkg");
        let config_dir = config_dir.to_string_lossy().into_owned();
        assert!(
            remnants.contains(&config_dir),
            "expected {config_dir} in remnants, got {remnants:?}"
        );
        fs::remove_dir_all(&tmp).ok();
    }

    // remnants_test.go: TestRemnantSize_SkipsVarLib
    #[test]
    fn remnant_size_skips_var_lib() {
        let paths = vec![
            "/var/lib/somepackage".to_string(),
            "/tmp/mu-test-remnant".to_string(),
        ];
        // /var/lib should be skipped, /tmp path doesn't exist so also 0.
        assert_eq!(remnant_size(&paths), 0);
    }

    // Finding 3: a Steam appmanifest whose *content* merely contains the
    // package name as a substring (e.g. "digital" ~ "git") must never be
    // treated as a removable remnant.
    #[test]
    fn steam_manifest_is_not_a_remnant() {
        let tmp = tempdir("steam-remnant");
        let deps = deps_for_test(&tmp, Arc::new(FakeRunner::new()));
        let steamapps = deps.data_home.join("Steam").join("steamapps");
        fs::create_dir_all(&steamapps).unwrap();
        fs::write(
            steamapps.join("appmanifest_1.acf"),
            "\"appid\"\t\t\"220\"\n\"name\"\t\t\"Half-Life\"\n\"path\"\t\t\"digital\"\n",
        )
        .unwrap();
        let remnants = find_remnants_in(&deps, "git");
        assert!(
            remnants.iter().all(|p| !p.ends_with(".acf")),
            "Steam manifest leaked into remnants: {remnants:?}"
        );
        fs::remove_dir_all(&tmp).ok();
    }

    // Finding 3: Steam games are discovered and matched exactly (never by
    // substring) so uninstallation delegates to `steam steam://uninstall/<id>`.
    #[test]
    fn steam_games_match_exactly() {
        let tmp = tempdir("steam-games");
        let deps = deps_for_test(&tmp, Arc::new(FakeRunner::new()));
        let steamapps = deps.data_home.join("Steam").join("steamapps");
        fs::create_dir_all(&steamapps).unwrap();
        fs::write(
            steamapps.join("appmanifest_220.acf"),
            "\"appid\"\t\t\"220\"\n\"name\"\t\t\"Half-Life\"\n",
        )
        .unwrap();
        assert_eq!(
            steam_games_in(&deps),
            vec![SteamGame {
                appid: "220".to_string(),
                name: "Half-Life".to_string(),
            }]
        );
        assert_eq!(steam_delegations(&deps, "half-life").len(), 1);
        assert!(
            steam_delegations(&deps, "half").is_empty(),
            "substring matched"
        );
        fs::remove_dir_all(&tmp).ok();
    }
}
