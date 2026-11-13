//! User configuration, persisted as TOML in the MATS home.

use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::paths::MatsHome;

/// Update channel used for automatic update checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum UpdateChannel {
    #[default]
    Stable,
    Prerelease,
}

impl UpdateChannel {
    pub fn as_str(self) -> &'static str {
        match self {
            UpdateChannel::Stable => "stable",
            UpdateChannel::Prerelease => "prerelease",
        }
    }

    pub fn include_prereleases(self) -> bool {
        matches!(self, UpdateChannel::Prerelease)
    }
}

impl std::str::FromStr for UpdateChannel {
    type Err = String;

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "stable" => Ok(UpdateChannel::Stable),
            "prerelease" | "beta" | "pre" => Ok(UpdateChannel::Prerelease),
            other => Err(format!(
                "unknown update channel `{other}` (expected stable or prerelease)"
            )),
        }
    }
}

/// How client launches obtain a graphical context.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum DisplayMode {
    /// Try the native display, then a virtual X server when available.
    #[default]
    Auto,
    /// Only use the native display.
    Native,
    /// Prefer a virtual framebuffer (Xvfb) with software OpenGL.
    Virtual,
    /// Never launch a graphical client; report client tests as not tested.
    Disabled,
}

impl std::str::FromStr for DisplayMode {
    type Err = String;

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "auto" => Ok(DisplayMode::Auto),
            "native" | "none" => Ok(DisplayMode::Native),
            "virtual" | "xvfb" | "headless" => Ok(DisplayMode::Virtual),
            "disabled" | "off" => Ok(DisplayMode::Disabled),
            other => Err(format!(
                "unknown display mode `{other}` (expected auto, native, virtual or disabled)"
            )),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct UpdateConfig {
    pub enabled: bool,
    pub auto_check: bool,
    pub auto_install: bool,
    pub channel: UpdateChannel,
    pub check_interval_hours: u64,
    /// Hard timeout for the startup update check, in milliseconds.
    pub timeout_ms: u64,
    /// `owner/repo` of the official MATS repository.
    pub repository: String,
}

impl Default for UpdateConfig {
    fn default() -> Self {
        UpdateConfig {
            enabled: true,
            auto_check: true,
            auto_install: false,
            channel: UpdateChannel::Stable,
            check_interval_hours: 24,
            timeout_ms: 2500,
            repository: "Ahaduzzamankhan/Minecraft-Automated-Testing-System".into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RuntimeConfig {
    /// Explicit Java executable to use when no override flag is given.
    pub java: Option<String>,
    /// Force a specific Java feature release, skipping detection heuristics.
    pub java_major: Option<u32>,
    /// Default maximum JVM heap.
    pub max_heap_mb: u32,
    /// Per-launch timeout.
    pub timeout_secs: u64,
    pub max_parallel_downloads: usize,
    /// Reuse the local cache only; never touch the network.
    pub offline: bool,
    /// Download the full asset object set for client runs. Minecraft can start
    /// without assets, so this is off by default to keep downloads small.
    pub download_assets: bool,
    /// Automatically download required mod dependencies (for example Fabric
    /// API) from Modrinth instead of only reporting them as missing.
    pub auto_dependencies: bool,
    pub display: DisplayMode,
    /// Keep prepared environments between runs.
    pub keep_environments: bool,
    pub extra_jvm_args: Vec<String>,
    /// Extra JVM args for client launches only.
    pub extra_client_jvm_args: Vec<String>,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        RuntimeConfig {
            java: None,
            java_major: None,
            max_heap_mb: 1536,
            timeout_secs: 300,
            max_parallel_downloads: 6,
            offline: false,
            download_assets: false,
            auto_dependencies: true,
            display: DisplayMode::Auto,
            keep_environments: true,
            extra_jvm_args: Vec::new(),
            extra_client_jvm_args: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct NetworkConfig {
    pub mojang_manifest: String,
    pub mojang_resources: String,
    pub fabric_meta: String,
    pub fabric_maven: String,
    pub quilt_meta: String,
    pub quilt_maven: String,
    pub neoforge_maven: String,
    pub forge_maven: String,
    pub forge_promotions: String,
    pub adoptium_api: String,
    pub modrinth_api: String,
    pub github_api: String,
    pub github_downloads: String,
    pub timeout_secs: u64,
    pub retries: u32,
}

impl Default for NetworkConfig {
    fn default() -> Self {
        NetworkConfig {
            mojang_manifest: "https://piston-meta.mojang.com/mc/game/version_manifest_v2.json"
                .into(),
            mojang_resources: "https://resources.download.minecraft.net".into(),
            fabric_meta: "https://meta.fabricmc.net".into(),
            fabric_maven: "https://maven.fabricmc.net".into(),
            quilt_meta: "https://meta.quiltmc.org".into(),
            quilt_maven: "https://maven.quiltmc.org/repository/release".into(),
            neoforge_maven: "https://maven.neoforged.net/releases".into(),
            forge_maven: "https://maven.minecraftforge.net".into(),
            forge_promotions:
                "https://files.minecraftforge.net/net/minecraftforge/forge/promotions_slim.json"
                    .into(),
            adoptium_api: "https://api.adoptium.net".into(),
            modrinth_api: "https://api.modrinth.com".into(),
            github_api: "https://api.github.com".into(),
            github_downloads: "https://github.com".into(),
            timeout_secs: 30,
            retries: 3,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ReportsConfig {
    /// Report output directory; `None` means `<home>/reports`.
    pub dir: Option<String>,
    pub formats: Vec<String>,
    /// Copy the raw JDK log files next to the report.
    pub include_logs: bool,
}

impl Default for ReportsConfig {
    fn default() -> Self {
        ReportsConfig {
            dir: None,
            formats: vec!["markdown".into(), "json".into()],
            include_logs: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CacheConfig {
    /// Delete prepared environments older than this during `cache clean`.
    pub max_env_age_hours: u64,
    /// Hint shown by `cache info`; MATS never deletes the cache silently.
    pub warn_above_gb: u64,
}

impl Default for CacheConfig {
    fn default() -> Self {
        CacheConfig {
            max_env_age_hours: 24 * 14,
            warn_above_gb: 10,
        }
    }
}

/// Root configuration document (`config.toml`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub updates: UpdateConfig,
    pub runtime: RuntimeConfig,
    pub network: NetworkConfig,
    pub reports: ReportsConfig,
    pub cache: CacheConfig,
}

impl Config {
    /// Load the configuration, returning defaults when the file is absent.
    pub fn load(home: &MatsHome) -> Result<Self> {
        let path = home.config_file();
        if !path.exists() {
            return Ok(Config::default());
        }
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading configuration {}", path.display()))?;
        let config: Config = toml::from_str(&text)
            .with_context(|| format!("parsing configuration {}", path.display()))?;
        Ok(config)
    }

    /// Persist the configuration, creating the home directory when necessary.
    pub fn save(&self, home: &MatsHome) -> Result<()> {
        home.ensure_layout()?;
        let text = toml::to_string_pretty(self).context("serialising configuration")?;
        crate::fsutil::atomic_write(&home.config_file(), text.as_bytes())
    }

    /// Report output directory, honouring an override in configuration.
    pub fn reports_dir(&self, home: &MatsHome) -> std::path::PathBuf {
        match &self.reports.dir {
            Some(dir) if !dir.trim().is_empty() => {
                let expanded = expand_home(dir);
                std::path::PathBuf::from(expanded)
            }
            _ => home.reports_dir(),
        }
    }

    pub fn has_format(&self, format: &str) -> bool {
        self.reports
            .formats
            .iter()
            .any(|entry| entry.eq_ignore_ascii_case(format))
    }
}

fn expand_home(input: &str) -> String {
    if let Some(rest) = input.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest).to_string_lossy().into_owned();
        }
    }
    input.to_string()
}

/// Load configuration from an explicit path (used by `--config`).
pub fn load_from_path(path: &Path) -> Result<Config> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading configuration {}", path.display()))?;
    toml::from_str(&text).with_context(|| format!("parsing configuration {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_documented_behaviour() {
        let config = Config::default();
        assert!(config.updates.enabled);
        assert!(config.updates.auto_check);
        assert!(!config.updates.auto_install);
        assert_eq!(config.updates.channel, UpdateChannel::Stable);
        assert_eq!(config.runtime.max_parallel_downloads, 6);
        assert!(!config.runtime.offline);
    }

    #[test]
    fn round_trips_through_toml() {
        let mut config = Config::default();
        config.updates.auto_install = true;
        config.runtime.max_heap_mb = 4096;
        config.runtime.java_major = Some(25);
        let text = toml::to_string_pretty(&config).unwrap();
        let parsed: Config = toml::from_str(&text).unwrap();
        assert_eq!(parsed, config);
    }

    #[test]
    fn partial_toml_uses_defaults_for_missing_keys() {
        let parsed: Config = toml::from_str("[updates]\nauto_install = true\n").unwrap();
        assert!(parsed.updates.auto_install);
        assert_eq!(parsed.runtime.timeout_secs, 300);
        assert_eq!(parsed.network.retries, 3);
    }

    #[test]
    fn save_and_load_home_configuration() {
        let temp = tempfile::tempdir().unwrap();
        let home = MatsHome::new(temp.path().join("home"));
        let mut config = Config::default();
        config.reports.dir = Some("custom-reports".into());
        config.save(&home).unwrap();
        let loaded = Config::load(&home).unwrap();
        assert_eq!(loaded.reports.dir.as_deref(), Some("custom-reports"));
    }

    #[test]
    fn channel_and_display_parsing() {
        assert_eq!(
            "prerelease".parse::<UpdateChannel>().unwrap(),
            UpdateChannel::Prerelease
        );
        assert!("nightly".parse::<UpdateChannel>().is_err());
        assert_eq!("xvfb".parse::<DisplayMode>().unwrap(), DisplayMode::Virtual);
    }
}
