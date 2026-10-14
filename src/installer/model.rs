//! Data models for package installer discovery and removal.

use std::path::PathBuf;

/// Package format / installer type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackageKind {
    Deb,
    AppImage,
    Iso,
    TarGz,
    Zip,
}

impl PackageKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            PackageKind::Deb => ".deb",
            PackageKind::AppImage => ".AppImage",
            PackageKind::Iso => ".iso",
            PackageKind::TarGz => ".tar.gz",
            PackageKind::Zip => ".zip",
        }
    }
}

/// A discovered installer package.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallerPackage {
    pub path: PathBuf,
    pub name: String,
    pub kind: PackageKind,
    pub size: u64,
}

/// Options for `mu installer`.
#[derive(Debug, Clone, Default)]
pub struct Options {
    pub paths: Vec<PathBuf>,
    pub dry_run: bool,
    pub auto_yes: bool,
    pub debug: bool,
}
