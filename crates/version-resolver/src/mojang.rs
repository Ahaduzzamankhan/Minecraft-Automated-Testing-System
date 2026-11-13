//! Mojang version manifest and per-version metadata.

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use mats_core::config::NetworkConfig;
use resource_manager::ResourceManager;
use serde::{Deserialize, Serialize};

const MANIFEST_TTL: Duration = Duration::from_secs(60 * 60);
const VERSION_TTL: Duration = Duration::from_secs(60 * 60 * 24 * 7);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LatestVersions {
    #[serde(default)]
    pub release: String,
    #[serde(default)]
    pub snapshot: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestEntry {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub url: String,
    #[serde(default)]
    pub time: String,
    #[serde(default, rename = "releaseTime")]
    pub release_time: String,
    #[serde(default)]
    pub sha1: String,
    #[serde(default, rename = "complianceLevel")]
    pub compliance_level: u32,
}

impl ManifestEntry {
    /// Stable releases are the only versions MATS selects automatically.
    pub fn is_release(&self) -> bool {
        self.kind == "release"
    }

    pub fn is_snapshot(&self) -> bool {
        matches!(self.kind.as_str(), "snapshot" | "old_beta" | "old_alpha")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionManifest {
    pub latest: LatestVersions,
    pub versions: Vec<ManifestEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct LibraryDownloads {
    #[serde(default)]
    pub artifact: Option<DownloadInfo>,
    #[serde(default)]
    pub classifiers: BTreeMap<String, DownloadInfo>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DownloadInfo {
    pub path: Option<String>,
    pub sha1: Option<String>,
    pub size: Option<u64>,
    pub url: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Library {
    pub name: String,
    #[serde(default)]
    pub downloads: LibraryDownloads,
    #[serde(default)]
    pub rules: Vec<Rule>,
    #[serde(default)]
    pub natives: BTreeMap<String, String>,
    #[serde(default)]
    pub extract: Option<ExtractRule>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ExtractRule {
    #[serde(default)]
    pub exclude: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rule {
    pub action: String,
    #[serde(default)]
    pub os: Option<OsRule>,
    #[serde(default)]
    pub features: Option<BTreeMap<String, bool>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct OsRule {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub arch: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct AssetIndex {
    pub id: String,
    pub sha1: Option<String>,
    pub size: Option<u64>,
    pub total_size: Option<u64>,
    pub url: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct VersionDownloads {
    #[serde(default)]
    pub client: Option<DownloadInfo>,
    #[serde(default)]
    pub server: Option<DownloadInfo>,
    #[serde(default)]
    pub client_mappings: Option<DownloadInfo>,
    #[serde(default)]
    pub server_mappings: Option<DownloadInfo>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct JavaVersion {
    #[serde(default, rename = "majorVersion")]
    pub major_version: u32,
    #[serde(default, rename = "component")]
    pub component: String,
}

/// One argument entry: a plain string or a rule-gated string list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ArgumentEntry {
    Plain(String),
    Gated {
        rules: Vec<Rule>,
        value: ArgumentValue,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ArgumentValue {
    Single(String),
    Many(Vec<String>),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct VersionArguments {
    #[serde(default)]
    pub game: Vec<ArgumentEntry>,
    #[serde(default)]
    pub jvm: Vec<ArgumentEntry>,
}

/// Parsed Mojang `versions/<id>/<id>.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MojangVersion {
    pub id: String,
    #[serde(rename = "type", default)]
    pub kind: String,
    #[serde(default, rename = "mainClass")]
    pub main_class: String,
    #[serde(default, rename = "inheritsFrom")]
    pub inherits_from: Option<String>,
    #[serde(default)]
    pub arguments: VersionArguments,
    #[serde(default, rename = "minecraftArguments")]
    pub minecraft_arguments: Option<String>,
    #[serde(default)]
    pub libraries: Vec<Library>,
    #[serde(default, rename = "assetIndex")]
    pub asset_index: Option<AssetIndex>,
    #[serde(default)]
    pub assets: Option<String>,
    #[serde(default)]
    pub downloads: VersionDownloads,
    #[serde(default, rename = "javaVersion")]
    pub java_version: Option<JavaVersion>,
    #[serde(default, rename = "complianceLevel")]
    pub compliance_level: u32,
    #[serde(default, rename = "releaseTime")]
    pub release_time: Option<String>,
    #[serde(default, rename = "minimumLauncherVersion")]
    pub minimum_launcher_version: Option<u32>,
}

/// Client for Mojang's official version metadata.
#[derive(Debug, Clone)]
pub struct MojangClient<'a> {
    manager: &'a ResourceManager,
    network: &'a NetworkConfig,
}

impl<'a> MojangClient<'a> {
    pub fn new(manager: &'a ResourceManager, network: &'a NetworkConfig) -> Self {
        MojangClient { manager, network }
    }

    pub fn manifest(&self) -> Result<VersionManifest> {
        let value = self
            .manager
            .fetch_json(&self.network.mojang_manifest, "mojang-version-manifest", MANIFEST_TTL)
            .context(
                "could not read Mojang's version manifest; check network connectivity or run with a warm cache",
            )?;
        let manifest: VersionManifest = serde_json::from_value(value)
            .context("Mojang's version manifest has an unexpected shape")?;
        if manifest.versions.is_empty() {
            bail!("Mojang's version manifest lists no versions");
        }
        Ok(manifest)
    }

    pub fn version_json(&self, entry: &ManifestEntry) -> Result<MojangVersion> {
        let cache_key = format!("mojang-version-{}", entry.id);
        let value = self
            .manager
            .fetch_json(&entry.url, &cache_key, VERSION_TTL)
            .with_context(|| format!("fetching metadata for Minecraft {}", entry.id))?;
        let version: MojangVersion = serde_json::from_value(value).with_context(|| {
            format!(
                "metadata for Minecraft {} has an unexpected shape",
                entry.id
            )
        })?;
        Ok(version)
    }

    pub fn version_json_by_id(&self, id: &str) -> Result<MojangVersion> {
        let manifest = self.manifest()?;
        let entry = manifest
            .versions
            .iter()
            .find(|entry| entry.id == id)
            .with_context(|| {
                format!("Minecraft {id} is not listed in Mojang's version manifest")
            })?;
        self.version_json(entry)
    }

    /// Stable releases, newest first (the manifest is already ordered).
    pub fn releases(&self) -> Result<Vec<ManifestEntry>> {
        Ok(self
            .manifest()?
            .versions
            .into_iter()
            .filter(ManifestEntry::is_release)
            .collect())
    }

    pub fn latest_release(&self) -> Result<ManifestEntry> {
        let manifest = self.manifest()?;
        manifest
            .versions
            .iter()
            .find(|entry| entry.is_release() && entry.id == manifest.latest.release)
            .or_else(|| manifest.versions.iter().find(|entry| entry.is_release()))
            .cloned()
            .context("Mojang's version manifest contains no release versions")
    }

    pub fn find(&self, id: &str) -> Result<Option<ManifestEntry>> {
        Ok(self
            .manifest()?
            .versions
            .into_iter()
            .find(|entry| entry.id == id))
    }
}

/// Java feature release required by a Minecraft version, derived from the
/// per-version metadata when it is unavailable.
///
/// Mojang's metadata is authoritative; this table is only used for offline
/// inspection and as a cross-check.
pub fn fallback_java_major(mc_version: &mats_core::McVersion) -> u32 {
    let parsed = mc_version.as_str();
    if mc_version.is_calendar() {
        return 25;
    }
    let mut parts = parsed.split('.');
    let major = parts.next().and_then(|part| part.parse::<u32>().ok());
    let minor = parts.next().and_then(|part| part.parse::<u32>().ok());
    let patch = parts.next().and_then(|part| part.parse::<u32>().ok());
    // This table only covers versions whose Mojang JSON could not be read.
    // It is expressed as ranges so every past release is covered without
    // listing each one, and future calendar versions are handled above.
    match (major, minor, patch) {
        (Some(1), Some(minor), _) if minor >= 21 => 21,
        (Some(1), Some(20), Some(patch)) if patch >= 5 => 21,
        (Some(1), Some(20), _) => 17,
        (Some(1), Some(18..=19), _) => 17,
        (Some(1), Some(17), _) => 16,
        (Some(1), Some(0..=16), _) => 8,
        // Unknown or very old versions: assume the current runtime rather than
        // failing; Mojang metadata normally supplies the exact requirement.
        _ => 25,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_argument_entries() {
        let json = serde_json::json!({
            "game": [
                "--username",
                "${auth_player_name}",
                {"rules": [{"action": "allow", "features": {"is_demo_user": true}}], "value": "--demo"}
            ],
            "jvm": [{"rules": [{"action": "allow", "os": {"name": "osx"}}], "value": ["-XstartOnFirstThread"]}]
        });
        let arguments: VersionArguments = serde_json::from_value(json).unwrap();
        assert_eq!(arguments.game.len(), 3);
        assert_eq!(arguments.jvm.len(), 1);
    }

    #[test]
    fn java_requirement_table_covers_both_eras() {
        use mats_core::McVersion;
        assert_eq!(fallback_java_major(&McVersion::new("26.3")), 25);
        assert_eq!(fallback_java_major(&McVersion::new("26.1.2")), 25);
        assert_eq!(fallback_java_major(&McVersion::new("1.21.11")), 21);
        assert_eq!(fallback_java_major(&McVersion::new("1.20.5")), 21);
        assert_eq!(fallback_java_major(&McVersion::new("1.20.4")), 17);
        assert_eq!(fallback_java_major(&McVersion::new("1.18.2")), 17);
        assert_eq!(fallback_java_major(&McVersion::new("1.17.1")), 16);
        assert_eq!(fallback_java_major(&McVersion::new("1.12.2")), 8);
        // Unlisted legacy releases are covered by ranges, not a fixed list.
        assert_eq!(fallback_java_major(&McVersion::new("1.19.4")), 17);
        assert_eq!(fallback_java_major(&McVersion::new("1.16.5")), 8);
        assert_eq!(fallback_java_major(&McVersion::new("1.7.10")), 8);
    }

    #[test]
    fn manifest_entry_types() {
        let release = ManifestEntry {
            id: "26.3".into(),
            kind: "release".into(),
            url: "u".into(),
            time: String::new(),
            release_time: String::new(),
            sha1: String::new(),
            compliance_level: 1,
        };
        assert!(release.is_release());
        assert!(!release.is_snapshot());
        let snapshot = ManifestEntry {
            kind: "snapshot".into(),
            ..release
        };
        assert!(snapshot.is_snapshot());
        assert!(!snapshot.is_release());
    }
}
