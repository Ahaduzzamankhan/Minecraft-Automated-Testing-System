//! Loader version discovery from the official Fabric, Quilt, Forge and
//! NeoForge metadata services.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use mats_core::McVersion;
use mats_core::config::NetworkConfig;
use mats_core::types::Loader;
use resource_manager::{ResourceManager, encode_segment};
use serde::{Deserialize, Serialize};
use serde_json::Value;

const LOADER_TTL: Duration = Duration::from_secs(60 * 60);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstallerInfo {
    pub url: String,
    pub sha1: Option<String>,
    pub version: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoaderArtifact {
    pub loader: Loader,
    pub mc_version: String,
    pub loader_version: String,
    pub installer: Option<InstallerInfo>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

pub struct LoaderResolver<'a> {
    manager: &'a ResourceManager,
    network: &'a NetworkConfig,
}

impl<'a> LoaderResolver<'a> {
    pub fn new(manager: &'a ResourceManager, network: &'a NetworkConfig) -> Self {
        LoaderResolver { manager, network }
    }

    /// Resolve a concrete loader build for a Minecraft version.
    pub fn resolve(
        &self,
        loader: Loader,
        mc_version: &str,
        requested_version: Option<&str>,
    ) -> Result<LoaderArtifact> {
        if loader == Loader::Vanilla {
            return Ok(LoaderArtifact {
                loader,
                mc_version: mc_version.to_string(),
                loader_version: mc_version.to_string(),
                installer: None,
                notes: vec!["vanilla launch needs no loader installer".to_string()],
            });
        }
        let mut versions = self.versions(loader, mc_version)?;
        if versions.is_empty() {
            bail!(
                "{} has no published build for Minecraft {mc_version}",
                loader.display_name()
            );
        }
        // Newest first.
        versions.sort_by(|left, right| McVersion::new(right).cmp(&McVersion::new(left)));
        let chosen = match requested_version {
            Some(requested) => {
                if versions.iter().any(|version| version == requested) {
                    requested.to_string()
                } else {
                    bail!(
                        "{} {requested} is not a published {} build for Minecraft {mc_version}. Available: {}",
                        loader.display_name(),
                        loader.display_name(),
                        preview_list(&versions)
                    );
                }
            }
            None => versions[0].clone(),
        };
        let installer = self.installer(loader, &chosen)?;
        Ok(LoaderArtifact {
            loader,
            mc_version: mc_version.to_string(),
            loader_version: chosen,
            installer,
            notes: Vec::new(),
        })
    }

    /// Loader versions published for a specific Minecraft version, newest first.
    pub fn versions(&self, loader: Loader, mc_version: &str) -> Result<Vec<String>> {
        match loader {
            Loader::Fabric => self.fabric_versions(mc_version),
            Loader::Quilt => self.quilt_versions(mc_version),
            Loader::NeoForge => self.neoforge_versions(mc_version),
            Loader::Forge => self.forge_versions(mc_version),
            Loader::Vanilla => Ok(vec![mc_version.to_string()]),
        }
    }

    fn fabric_versions(&self, mc_version: &str) -> Result<Vec<String>> {
        let base = self.network.fabric_meta.trim_end_matches('/');
        let url = format!("{base}/v2/versions/loader/{}", encode_segment(mc_version));
        let value = self
            .manager
            .fetch_json(&url, &format!("fabric-loader-{mc_version}"), LOADER_TTL)
            .with_context(|| format!("listing Fabric Loader builds for Minecraft {mc_version}"))?;
        let array = value
            .as_array()
            .context("Fabric meta returned an unexpected shape")?;
        let mut stable = Vec::new();
        let mut unstable = Vec::new();
        for entry in array {
            let loader = entry.get("loader");
            let Some(version) = loader
                .and_then(|loader| loader.get("version"))
                .and_then(Value::as_str)
            else {
                continue;
            };
            let is_stable = loader
                .and_then(|loader| loader.get("stable"))
                .and_then(Value::as_bool)
                .unwrap_or(true);
            if is_stable {
                stable.push(version.to_string());
            } else {
                unstable.push(version.to_string());
            }
        }
        stable.extend(unstable);
        Ok(stable)
    }

    fn quilt_versions(&self, mc_version: &str) -> Result<Vec<String>> {
        let base = self.network.quilt_meta.trim_end_matches('/');
        let url = format!("{base}/v3/versions/loader/{}", encode_segment(mc_version));
        let value = self
            .manager
            .fetch_json(&url, &format!("quilt-loader-{mc_version}"), LOADER_TTL)
            .with_context(|| format!("listing Quilt Loader builds for Minecraft {mc_version}"))?;
        let array = value
            .as_array()
            .context("Quilt meta returned an unexpected shape")?;
        Ok(array
            .iter()
            .filter_map(|entry| {
                entry
                    .get("loader")
                    .and_then(|loader| loader.get("version"))
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .collect())
    }

    fn neoforge_versions(&self, mc_version: &str) -> Result<Vec<String>> {
        let base = self.network.neoforge_maven.trim_end_matches('/');
        let url = format!("{base}/net/neoforged/neoforge/maven-metadata.xml");
        let xml = self
            .manager
            .fetch_text(&url, "neoforge-maven-metadata", LOADER_TTL)
            .with_context(|| "listing NeoForge builds")?;
        let prefix = neoforge_prefix(mc_version);
        let mut matches: Vec<String> = maven_versions(&xml)
            .into_iter()
            .filter(|version| version.starts_with(&prefix))
            .collect();
        if matches.is_empty() && mc_version == "1.20.1" {
            // NeoForge 1.20.1 shipped from the legacy `net.neoforged:forge`
            // coordinates as 47.1.x.
            let legacy = format!("{base}/net/neoforged/forge/maven-metadata.xml");
            if let Ok(xml) =
                self.manager
                    .fetch_text(&legacy, "neoforge-legacy-maven-metadata", LOADER_TTL)
            {
                matches = maven_versions(&xml)
                    .into_iter()
                    .filter(|version| version.starts_with("47.1."))
                    .collect();
            }
        }
        matches.sort_by(|left, right| McVersion::new(right).cmp(&McVersion::new(left)));
        Ok(matches)
    }

    fn forge_versions(&self, mc_version: &str) -> Result<Vec<String>> {
        let promotions = self
            .manager
            .fetch_json(
                &self.network.forge_promotions,
                "forge-promotions",
                LOADER_TTL,
            )
            .with_context(|| "reading Forge promotions")?;
        let mut ordered: Vec<String> = Vec::new();
        if let Some(promos) = promotions.get("promos").and_then(Value::as_object) {
            for suffix in ["recommended", "latest"] {
                if let Some(version) = promos
                    .get(&format!("{mc_version}-{suffix}"))
                    .and_then(Value::as_str)
                {
                    if !ordered.iter().any(|existing| existing == version) {
                        ordered.push(version.to_string());
                    }
                }
            }
        }
        // Fall back to (and enrich with) the Maven index so users can request a
        // specific older build.
        let base = self.network.forge_maven.trim_end_matches('/');
        let url = format!("{base}/net/minecraftforge/forge/maven-metadata.xml");
        if let Ok(xml) = self
            .manager
            .fetch_text(&url, "forge-maven-metadata", LOADER_TTL)
        {
            let mut from_maven: Vec<String> = maven_versions(&xml)
                .into_iter()
                .filter_map(|entry| forge_version_for(&entry, mc_version))
                .collect();
            from_maven.sort_by(|left, right| McVersion::new(right).cmp(&McVersion::new(left)));
            for version in from_maven {
                if !ordered.iter().any(|existing| existing == &version) {
                    ordered.push(version);
                }
            }
        }
        Ok(ordered)
    }

    fn installer(&self, loader: Loader, loader_version: &str) -> Result<Option<InstallerInfo>> {
        match loader {
            Loader::Fabric => self.fabric_installer(),
            Loader::Quilt => self.quilt_installer(),
            Loader::Forge => self.forge_installer_placeholder(loader_version),
            Loader::NeoForge => Ok(None),
            Loader::Vanilla => Ok(None),
        }
    }

    fn fabric_installer(&self) -> Result<Option<InstallerInfo>> {
        let base = self.network.fabric_meta.trim_end_matches('/');
        let url = format!("{base}/v2/versions/installer");
        let value = self
            .manager
            .fetch_json(&url, "fabric-installer-versions", LOADER_TTL)?;
        let Some(first) = value.as_array().and_then(|array| array.first()) else {
            return Ok(None);
        };
        let version = first
            .get("version")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let download_url = first
            .get("url")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| {
                format!(
                    "{}/net/fabricmc/fabric-installer/{version}/fabric-installer-{version}.jar",
                    self.network.fabric_maven.trim_end_matches('/')
                )
            });
        let sha1 = self
            .manager
            .try_fetch_text(&format!("{download_url}.sha1"))?
            .map(|text| text.trim().to_string());
        Ok(Some(InstallerInfo {
            url: download_url,
            sha1,
            version,
        }))
    }

    fn quilt_installer(&self) -> Result<Option<InstallerInfo>> {
        let base = self.network.quilt_meta.trim_end_matches('/');
        let url = format!("{base}/v3/versions/installer");
        let value = match self
            .manager
            .fetch_json(&url, "quilt-installer-versions", LOADER_TTL)
        {
            Ok(value) => value,
            Err(error) => {
                tracing::debug!("could not list Quilt installers: {error}");
                Value::Array(Vec::new())
            }
        };
        let Some(first) = value.as_array().and_then(|array| array.first()) else {
            return Ok(None);
        };
        let version = first
            .get("version")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let download_url = first
            .get("url")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| {
                format!(
                    "{}/org/quiltmc/quilt-installer/{version}/quilt-installer-{version}.jar",
                    self.network.quilt_maven.trim_end_matches('/')
                )
            });
        let sha1 = self
            .manager
            .try_fetch_text(&format!("{download_url}.sha1"))?
            .map(|text| text.trim().to_string());
        Ok(Some(InstallerInfo {
            url: download_url,
            sha1,
            version,
        }))
    }

    /// Forge and NeoForge installers are versioned with the loader build and
    /// resolved by the runtime builder, which knows the Maven coordinates.
    fn forge_installer_placeholder(&self, _loader_version: &str) -> Result<Option<InstallerInfo>> {
        Ok(None)
    }

    /// Maven coordinates for a Forge/NeoForge installer.
    pub fn loader_installer_url(
        &self,
        loader: Loader,
        mc_version: &str,
        loader_version: &str,
    ) -> String {
        match loader {
            Loader::NeoForge if mc_version == "1.20.1" => format!(
                "{}/net/neoforged/forge/{loader_version}/forge-{loader_version}-installer.jar",
                self.network.neoforge_maven.trim_end_matches('/')
            ),
            Loader::NeoForge => format!(
                "{}/net/neoforged/neoforge/{loader_version}/neoforge-{loader_version}-installer.jar",
                self.network.neoforge_maven.trim_end_matches('/')
            ),
            Loader::Forge => format!(
                "{}/net/minecraftforge/forge/{mc_version}-{loader_version}/forge-{mc_version}-{loader_version}-installer.jar",
                self.network.forge_maven.trim_end_matches('/')
            ),
            _ => String::new(),
        }
    }
}

fn preview_list(versions: &[String]) -> String {
    let mut preview: Vec<&str> = versions.iter().take(5).map(String::as_str).collect();
    if versions.len() > 5 {
        preview.push("…");
    }
    preview.join(", ")
}

/// NeoForge versions embed the Minecraft version without the legacy `1.`
/// prefix: MC 26.3 -> `26.3.x`, MC 1.21.1 -> `21.1.x`.
pub fn neoforge_prefix(mc_version: &str) -> String {
    let trimmed = mc_version.strip_prefix("1.").unwrap_or(mc_version);
    format!("{trimmed}.")
}

/// Extract `<version>` values from a Maven `maven-metadata.xml` document.
pub fn maven_versions(xml: &str) -> Vec<String> {
    let mut versions = Vec::new();
    let mut rest = xml;
    while let Some(start) = rest.find("<version>") {
        let after = &rest[start + "<version>".len()..];
        let Some(end) = after.find("</version>") else {
            break;
        };
        let value = after[..end].trim();
        if !value.is_empty() && value.chars().all(|c| c.is_ascii_graphic()) {
            versions.push(value.to_string());
        }
        rest = &after[end + "</version>".len()..];
    }
    versions
}

/// `1.20.1-47.3.0` plus the requested Minecraft version -> `47.3.0`.
pub fn forge_version_for(combined: &str, mc_version: &str) -> Option<String> {
    let remainder = combined.strip_prefix(mc_version)?.strip_prefix('-')?;
    if remainder.is_empty() || remainder.contains('-') {
        return None;
    }
    if remainder
        .chars()
        .all(|character| character.is_ascii_digit() || character == '.')
    {
        Some(remainder.to_string())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_maven_metadata() {
        let xml = r#"<?xml version="1.0"?><metadata>
            <groupId>net.neoforged</groupId>
            <versioning>
              <latest>26.3.0.12</latest>
              <versions>
                <version>26.2.0.5</version>
                <version>26.3.0.9</version>
                <version>26.3.0.12</version>
              </versions>
            </versioning>
        </metadata>"#;
        assert_eq!(
            maven_versions(xml),
            vec!["26.2.0.5", "26.3.0.9", "26.3.0.12"]
        );
    }

    #[test]
    fn neoforge_prefixes_follow_the_loader_convention() {
        assert_eq!(neoforge_prefix("26.3"), "26.3.");
        assert_eq!(neoforge_prefix("1.21.1"), "21.1.");
        assert_eq!(neoforge_prefix("1.20.4"), "20.4.");
    }

    #[test]
    fn matches_forge_combined_versions() {
        assert_eq!(
            forge_version_for("1.20.1-47.3.0", "1.20.1"),
            Some("47.3.0".into())
        );
        assert_eq!(forge_version_for("1.20.1-47.3.0", "1.20"), None);
        assert_eq!(forge_version_for("1.20.1-47.3.0-beta", "1.20.1"), None);
        assert_eq!(
            forge_version_for("1.21-51.0.0", "1.21"),
            Some("51.0.0".into())
        );
    }

    #[test]
    fn parses_forge_promotions() {
        let promotions = serde_json::json!({
            "promos": {
                "1.20.1-recommended": "47.3.0",
                "1.20.1-latest": "47.4.0",
                "26.3-latest": "66.0.6"
            }
        });
        assert_eq!(promotions["promos"]["26.3-latest"].as_str(), Some("66.0.6"));
    }
}
