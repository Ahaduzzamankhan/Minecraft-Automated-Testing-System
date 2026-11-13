//! Turning mod constraints into a concrete, testable plan.

use anyhow::{Result, bail};
use mats_core::config::NetworkConfig;
use mats_core::types::{Loader, Side};
use mats_core::{McVersion, VersionPredicate};
use resource_manager::ResourceManager;
use serde::{Deserialize, Serialize};

use crate::loaders::{InstallerInfo, LoaderArtifact, LoaderResolver};
use crate::mojang::{ManifestEntry, MojangClient, fallback_java_major};

/// Input to planning, derived from CLI overrides and mod metadata.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PlanRequest {
    pub loader: Loader,
    pub side: Side,
    pub mc_version: Option<String>,
    pub loader_version: Option<String>,
    pub mc_constraints: Vec<String>,
    /// Include snapshots when no release satisfies the constraints.
    pub allow_snapshots: bool,
}

impl PlanRequest {
    pub fn new(loader: Loader, side: Side) -> Self {
        PlanRequest {
            loader,
            side,
            ..PlanRequest::default()
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedPlan {
    pub loader: Loader,
    pub side: Side,
    pub mc_version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub loader_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub installer: Option<InstallerInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub java_major: Option<u32>,
    pub java_major_source: String,
    /// Candidate versions considered, newest first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub candidates: Vec<String>,
    /// `true` when no version was declared and MATS picked the newest release.
    pub assumed_version: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct CandidateSelection {
    pub versions: Vec<String>,
    pub assumed: bool,
    pub notes: Vec<String>,
}

/// Select candidate Minecraft versions from declared constraints.
///
/// Only stable releases are selected automatically. When the mod declares no
/// version at all, the newest releases are returned with `assumed = true` so
/// callers can report the assumption instead of hiding it.
pub fn select_candidates(
    releases: &[ManifestEntry],
    all_versions: &[ManifestEntry],
    constraints: &[String],
    limit: usize,
    allow_snapshots: bool,
) -> Result<CandidateSelection> {
    let mut notes = Vec::new();
    let predicates: Vec<VersionPredicate> = constraints
        .iter()
        .filter(|constraint| !constraint.trim().is_empty() && constraint.trim() != "*")
        .map(|constraint| VersionPredicate::parse(constraint))
        .collect();
    for predicate in &predicates {
        if let Some(error) = predicate.parse_error() {
            notes.push(format!(
                "could not interpret Minecraft constraint `{}`: {error}",
                predicate.raw()
            ));
        }
    }
    let usable: Vec<&VersionPredicate> = predicates
        .iter()
        .filter(|predicate| predicate.parse_error().is_none())
        .collect();

    if usable.is_empty() {
        let versions: Vec<String> = releases
            .iter()
            .take(limit.max(1))
            .map(|entry| entry.id.clone())
            .collect();
        if versions.is_empty() {
            bail!("Mojang's version manifest contains no stable releases");
        }
        notes.push(
            "the mod does not declare a usable Minecraft version range; the newest stable releases were considered"
                .to_string(),
        );
        return Ok(CandidateSelection {
            versions,
            assumed: true,
            notes,
        });
    }

    let matches = |entry: &ManifestEntry| -> bool {
        let version = McVersion::new(entry.id.clone());
        usable.iter().all(|predicate| predicate.matches(&version))
    };

    let mut matching: Vec<String> = releases
        .iter()
        .filter(|entry| matches(entry))
        .map(|entry| entry.id.clone())
        .collect();
    matching.sort_by(|left, right| McVersion::new(right).cmp(&McVersion::new(left)));
    matching.truncate(limit.max(1));

    if matching.is_empty() {
        if allow_snapshots {
            // A snapshot such as `26.4-snapshot-5` belongs to the `26.4`
            // version line, so a declared range that covers 26.4 also covers
            // that snapshot once snapshots are explicitly allowed.
            let matches_snapshot_base = |entry: &ManifestEntry| -> bool {
                let base = entry
                    .id
                    .split('-')
                    .next()
                    .unwrap_or(entry.id.as_str())
                    .to_string();
                usable
                    .iter()
                    .all(|predicate| predicate.matches(&McVersion::new(base.clone())))
            };
            let mut snapshots: Vec<String> = all_versions
                .iter()
                .filter(|entry| {
                    entry.is_snapshot() && (matches(entry) || matches_snapshot_base(entry))
                })
                .map(|entry| entry.id.clone())
                .collect();
            snapshots.sort_by(|left, right| McVersion::new(right).cmp(&McVersion::new(left)));
            snapshots.truncate(limit.max(1));
            if !snapshots.is_empty() {
                notes.push(
                    "no stable release satisfies the declared range; matching snapshots are listed instead"
                        .to_string(),
                );
                return Ok(CandidateSelection {
                    versions: snapshots,
                    assumed: false,
                    notes,
                });
            }
        }
        let declared = constraints.join(", ");
        bail!(
            "no stable Minecraft release satisfies the declared range: {declared}. \
             Pass --mc <version> to test a specific version (snapshots require --allow-snapshots)."
        );
    }

    if matching.len() > 1 {
        notes.push(format!(
            "{} stable releases satisfy the declared range; {} will be preferred and the rest are listed as candidates",
            matching.len(),
            matching[0]
        ));
    }

    Ok(CandidateSelection {
        versions: matching,
        assumed: false,
        notes,
    })
}

/// Resolve the full plan: Minecraft version, loader build and Java release.
pub fn resolve_plan(
    manager: &ResourceManager,
    network: &NetworkConfig,
    request: &PlanRequest,
) -> Result<ResolvedPlan> {
    let mojang = MojangClient::new(manager, network);
    let loader_resolver = LoaderResolver::new(manager, network);
    let manifest = mojang.manifest()?;
    let releases: Vec<ManifestEntry> = manifest
        .versions
        .iter()
        .filter(|entry| entry.is_release())
        .cloned()
        .collect();

    let selection = match &request.mc_version {
        Some(explicit) => {
            let mut notes = Vec::new();
            if !manifest.versions.iter().any(|entry| &entry.id == explicit) {
                notes.push(format!(
                    "Minecraft {explicit} is not listed in Mojang's manifest; trying it anyway"
                ));
            }
            CandidateSelection {
                versions: vec![explicit.clone()],
                assumed: false,
                notes,
            }
        }
        None => select_candidates(
            &releases,
            &manifest.versions,
            &request.mc_constraints,
            // Walk deeply through the manifest: a loader may have no build for
            // the newest matches while older (or newer) versions remain usable.
            if request.allow_snapshots { 64 } else { 32 },
            request.allow_snapshots,
        )?,
    };

    let mut notes = selection.notes.clone();
    let candidates = selection.versions.clone();
    let mut last_error = None;

    for (index, mc_version) in candidates.iter().enumerate() {
        let loader_result: Result<LoaderArtifact> = match request.loader {
            Loader::Vanilla => Ok(LoaderArtifact {
                loader: Loader::Vanilla,
                mc_version: mc_version.clone(),
                loader_version: mc_version.clone(),
                installer: None,
                notes: Vec::new(),
            }),
            _ => loader_resolver.resolve(
                request.loader,
                mc_version,
                request.loader_version.as_deref(),
            ),
        };
        let artifact = match loader_result {
            Ok(artifact) => artifact,
            Err(error) => {
                notes.push(format!(
                    "Minecraft {mc_version} cannot be tested with {}: {error}",
                    request.loader.display_name()
                ));
                last_error = Some(error);
                continue;
            }
        };

        // Java requirement comes from Mojang's metadata when reachable.
        let (java_major, java_major_source) = match mojang.version_json_by_id(mc_version) {
            Ok(version) => match version.java_version {
                Some(java) if java.major_version > 0 => {
                    (Some(java.major_version), "mojang metadata".to_string())
                }
                _ => (
                    Some(fallback_java_major(&McVersion::new(mc_version.clone()))),
                    "MATS compatibility table".to_string(),
                ),
            },
            Err(error) => {
                tracing::debug!("could not read Java requirement for {mc_version}: {error}");
                (
                    Some(fallback_java_major(&McVersion::new(mc_version.clone()))),
                    "MATS compatibility table (Mojang metadata unavailable)".to_string(),
                )
            }
        };

        let _ = index;
        return Ok(ResolvedPlan {
            loader: request.loader,
            side: request.side,
            mc_version: mc_version.clone(),
            loader_version: Some(artifact.loader_version),
            installer: artifact.installer,
            java_major,
            java_major_source,
            candidates: candidates.clone(),
            assumed_version: selection.assumed,
            notes,
        });
    }

    match last_error {
        Some(error) => bail!(
            "no candidate Minecraft version could be prepared for {}: {error}",
            request.loader.display_name()
        ),
        None => bail!("no candidate Minecraft version could be prepared"),
    }
}

/// One row of `mats matrix` output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MatrixRow {
    pub mc_version: String,
    pub loader_version: Option<String>,
    pub supported: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub java_major: Option<u32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

/// Build a loader support matrix for the candidate Minecraft versions.
pub fn build_matrix(
    manager: &ResourceManager,
    network: &NetworkConfig,
    loader: Loader,
    releases: &[ManifestEntry],
    all_versions: &[ManifestEntry],
    constraints: &[String],
    limit: usize,
    allow_snapshots: bool,
) -> Result<Vec<MatrixRow>> {
    let selection = select_candidates(releases, all_versions, constraints, limit, allow_snapshots)?;
    let resolver = LoaderResolver::new(manager, network);
    let mut rows = Vec::new();
    for mc_version in selection.versions {
        let mut notes = Vec::new();
        let resolved = resolver.resolve(loader, &mc_version, None);
        let (loader_version, supported) = match resolved {
            Ok(artifact) => (Some(artifact.loader_version), true),
            Err(error) => {
                notes.push(format!("{error}"));
                (None, false)
            }
        };
        rows.push(MatrixRow {
            java_major: Some(fallback_java_major(&McVersion::new(mc_version.clone()))),
            mc_version,
            loader_version,
            supported,
            notes,
        });
    }
    if selection.assumed {
        if let Some(first) = rows.first_mut() {
            first
                .notes
                .push("version was assumed because the mod declares no range".to_string());
        }
    }
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, kind: &str) -> ManifestEntry {
        ManifestEntry {
            id: id.to_string(),
            kind: kind.to_string(),
            url: format!("https://example.invalid/{id}.json"),
            time: String::new(),
            release_time: String::new(),
            sha1: String::new(),
            compliance_level: 1,
        }
    }

    fn releases() -> Vec<ManifestEntry> {
        [
            "26.3", "26.2", "26.1.2", "26.1", "1.21.11", "1.21.1", "1.20.4", "1.20.1",
        ]
        .into_iter()
        .map(|id| entry(id, "release"))
        .collect()
    }

    #[test]
    fn selects_latest_release_without_constraints_and_flags_assumption() {
        let selection = select_candidates(&releases(), &releases(), &[], 3, false).unwrap();
        assert_eq!(selection.versions, vec!["26.3", "26.2", "26.1.2"]);
        assert!(selection.assumed);
        assert!(
            selection
                .notes
                .iter()
                .any(|note| note.contains("newest stable"))
        );
    }

    #[test]
    fn filters_by_declared_calendar_range() {
        let selection =
            select_candidates(&releases(), &releases(), &[">=26.1 <26.3".into()], 5, false)
                .unwrap();
        assert_eq!(selection.versions, vec!["26.2", "26.1.2", "26.1"]);
        assert!(!selection.assumed);
    }

    #[test]
    fn filters_by_legacy_range() {
        let selection = select_candidates(
            &releases(),
            &releases(),
            &["[1.20.1,1.21)".into()],
            5,
            false,
        )
        .unwrap();
        assert_eq!(selection.versions, vec!["1.20.4", "1.20.1"]);
    }

    #[test]
    fn reports_multiple_matches() {
        let selection =
            select_candidates(&releases(), &releases(), &[">=1.20".into()], 10, false).unwrap();
        assert!(selection.versions.len() > 1);
        assert!(
            selection
                .notes
                .iter()
                .any(|note| note.contains("satisfy the declared range"))
        );
    }

    #[test]
    fn errors_when_nothing_matches() {
        // An exact version that no manifest entry provides. A `>=` bound would
        // legitimately match newer calendar releases, so it cannot be used here.
        let error =
            select_candidates(&releases(), &releases(), &["1.99".into()], 5, false).unwrap_err();
        let message = error.to_string();
        assert!(message.contains("no stable Minecraft release"));
    }

    #[test]
    fn snapshots_only_when_allowed() {
        let mut all = releases();
        all.insert(0, entry("26.4-snapshot-5", "snapshot"));
        let error = select_candidates(&releases(), &all, &[">=26.4".into()], 5, false).unwrap_err();
        assert!(error.to_string().contains("no stable Minecraft release"));
        let selection = select_candidates(&releases(), &all, &[">=26.4".into()], 5, true).unwrap();
        assert_eq!(selection.versions, vec!["26.4-snapshot-5"]);
        assert!(!selection.assumed);
    }

    #[test]
    fn unparseable_constraints_fall_back_to_latest_with_a_note() {
        let selection =
            select_candidates(&releases(), &releases(), &[">=".into()], 2, false).unwrap();
        assert!(selection.assumed);
        assert!(
            selection
                .notes
                .iter()
                .any(|note| note.contains("could not interpret"))
        );
    }
}
