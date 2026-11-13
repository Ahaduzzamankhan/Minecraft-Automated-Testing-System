//! Conflict detection across the mods in a test environment.
//!
//! MATS is deliberately conservative here: two mods touching the same game
//! class is normal and is *not* reported as a conflict. Only provable
//! collisions are flagged as problems (duplicate mod ids, the same mixin class
//! shipped twice), while softer overlaps are reported as information.

use std::collections::{BTreeMap, BTreeSet};

use jar_inspector::JarInspection;
use mats_core::types::{Finding, FindingCategory, ModMetadata, Severity};
use metadata_parser::MixinConfig;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModSnapshot {
    pub mod_id: String,
    pub name: String,
    pub version: String,
    pub mixin_classes: Vec<String>,
    pub mixin_configs: Vec<String>,
    pub classes: Vec<String>,
    pub entrypoints: Vec<String>,
    pub access_wideners: Vec<String>,
}

impl ModSnapshot {
    pub fn build(
        metadata: &ModMetadata,
        configs: &[MixinConfig],
        inspection: &JarInspection,
    ) -> Self {
        let mut mixin_classes: Vec<String> = configs
            .iter()
            .flat_map(MixinConfig::qualified_classes)
            .collect();
        mixin_classes.sort();
        mixin_classes.dedup();
        let classes: Vec<String> = inspection
            .entries
            .iter()
            .filter(|entry| !entry.is_dir && entry.name.ends_with(".class"))
            .map(|entry| entry.name.clone())
            .collect();
        ModSnapshot {
            mod_id: metadata.id.clone(),
            name: metadata.name.clone(),
            version: metadata.version.clone(),
            mixin_classes,
            mixin_configs: configs.iter().map(|config| config.path.clone()).collect(),
            classes,
            entrypoints: metadata
                .entrypoints
                .iter()
                .map(|entry| entry.value.clone())
                .collect(),
            access_wideners: metadata.access_wideners.clone(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ConflictReport {
    pub findings: Vec<Finding>,
    pub duplicate_mod_ids: Vec<String>,
    pub shared_mixin_classes: Vec<String>,
    /// Mods that were compared.
    pub mods_compared: usize,
}

impl ConflictReport {
    pub fn has_conflicts(&self) -> bool {
        !self.findings.is_empty()
    }
}

/// Compare every mod in the environment against every other mod.
pub fn analyze(mods: &[ModSnapshot]) -> ConflictReport {
    let mut report = ConflictReport {
        mods_compared: mods.len(),
        ..ConflictReport::default()
    };
    if mods.len() < 2 {
        return report;
    }

    // Duplicate mod ids.
    let mut by_id: BTreeMap<String, Vec<&ModSnapshot>> = BTreeMap::new();
    for module in mods {
        by_id
            .entry(module.mod_id.to_ascii_lowercase())
            .or_default()
            .push(module);
    }
    for (id, owners) in &by_id {
        if owners.len() > 1 {
            report.duplicate_mod_ids.push(id.clone());
            report.findings.push(
                Finding::new(
                    "conflict.duplicate-mod-id",
                    Severity::Critical,
                    FindingCategory::Conflict,
                    format!("{} mods declare the same mod id `{id}`", owners.len()),
                )
                .with_detail(format!(
                    "files: {}",
                    owners
                        .iter()
                        .map(|owner| format!("{} {}", owner.name, owner.version))
                        .collect::<Vec<_>>()
                        .join(", ")
                )),
            );
        }
    }

    // Identical mixin classes shipped by more than one mod.
    let mut mixin_owners: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for module in mods {
        for class in &module.mixin_classes {
            mixin_owners
                .entry(class.as_str())
                .or_default()
                .insert(module.mod_id.as_str());
        }
    }
    for (class, owners) in &mixin_owners {
        if owners.len() > 1 {
            report.shared_mixin_classes.push((*class).to_string());
            report.findings.push(
                Finding::new(
                    "conflict.shared-mixin-class",
                    Severity::Critical,
                    FindingCategory::Conflict,
                    format!("Mixin class `{class}` is shipped by multiple mods"),
                )
                .with_detail(format!(
                    "mods: {} — the loader can apply only one and will abort",
                    owners.iter().copied().collect::<Vec<_>>().join(", ")
                )),
            );
        }
    }

    // Same mixin configuration file name in multiple mods.
    let mut config_owners: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for module in mods {
        for config in &module.mixin_configs {
            config_owners
                .entry(config.as_str())
                .or_default()
                .insert(module.mod_id.as_str());
        }
    }
    for (config, owners) in &config_owners {
        if owners.len() > 1 {
            report.findings.push(
                Finding::new(
                    "conflict.shared-mixin-config",
                    Severity::Warning,
                    FindingCategory::Conflict,
                    format!("Mixin configuration `{config}` is declared by multiple mods"),
                )
                .with_detail(format!(
                    "mods: {} — each is loaded separately, but the shared name makes diagnostics harder",
                    owners.iter().copied().collect::<Vec<_>>().join(", ")
                )),
            );
        }
    }

    // Bundled class overlap. Many mods legitimately bundle the same library,
    // so this is information, not a conflict claim.
    let mut class_owners: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for module in mods {
        for class in &module.classes {
            class_owners
                .entry(class.as_str())
                .or_default()
                .insert(module.mod_id.as_str());
        }
    }
    let mut overlaps: Vec<(&str, &BTreeSet<&str>)> = class_owners
        .iter()
        .filter(|(_, owners)| owners.len() > 1)
        .map(|(class, owners)| (*class, owners))
        .collect();
    overlaps.sort_by_key(|(class, _)| *class);
    if !overlaps.is_empty() {
        let examples: Vec<String> = overlaps
            .iter()
            .take(5)
            .map(|(class, owners)| {
                format!(
                    "{class} ({})",
                    owners.iter().copied().collect::<Vec<_>>().join(", ")
                )
            })
            .collect();
        report.findings.push(
            Finding::new(
                "conflict.bundled-class-overlap",
                Severity::Information,
                FindingCategory::Conflict,
                format!(
                    "{} class path(s) are bundled by more than one mod",
                    overlaps.len()
                ),
            )
            .with_detail(
                "identical class paths are common for bundled libraries and are not by themselves a conflict",
            )
            .with_evidence(examples),
        );
    }

    // Shared entrypoint classes.
    let mut entrypoint_owners: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for module in mods {
        for entrypoint in &module.entrypoints {
            entrypoint_owners
                .entry(entrypoint.as_str())
                .or_default()
                .insert(module.mod_id.as_str());
        }
    }
    for (entrypoint, owners) in &entrypoint_owners {
        if owners.len() > 1 {
            report.findings.push(
                Finding::new(
                    "conflict.shared-entrypoint",
                    Severity::Warning,
                    FindingCategory::Conflict,
                    format!("Entrypoint `{entrypoint}` is declared by multiple mods"),
                )
                .with_detail(format!(
                    "mods: {} — usually an unintended copy",
                    owners.iter().copied().collect::<Vec<_>>().join(", ")
                )),
            );
        }
    }

    // Access wideners with the same file name but different content cannot be
    // distinguished statically; note the overlap only.
    let mut widener_owners: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for module in mods {
        for widener in &module.access_wideners {
            widener_owners
                .entry(widener.as_str())
                .or_default()
                .insert(module.mod_id.as_str());
        }
    }
    for (widener, owners) in &widener_owners {
        if owners.len() > 1 {
            report.findings.push(
                Finding::new(
                    "conflict.shared-access-widener",
                    Severity::Warning,
                    FindingCategory::Conflict,
                    format!("Access widener `{widener}` is declared by multiple mods"),
                )
                .with_detail(format!(
                    "mods: {}",
                    owners.iter().copied().collect::<Vec<_>>().join(", ")
                )),
            );
        }
    }

    report
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(id: &str, mixin: &str, classes: &[&str], entrypoint: &str) -> ModSnapshot {
        ModSnapshot {
            mod_id: id.into(),
            name: id.into(),
            version: "1.0.0".into(),
            mixin_classes: vec![mixin.into()],
            mixin_configs: vec![format!("{id}.mixins.json")],
            classes: classes.iter().map(|class| class.to_string()).collect(),
            entrypoints: vec![entrypoint.into()],
            access_wideners: Vec::new(),
        }
    }

    #[test]
    fn single_mod_is_never_a_conflict() {
        let report = analyze(&[snapshot(
            "a",
            "com.example.mixin.A",
            &["com/example/A.class"],
            "com.example.A",
        )]);
        assert!(!report.has_conflicts());
        assert_eq!(report.mods_compared, 1);
    }

    #[test]
    fn detects_duplicate_mod_ids() {
        let report = analyze(&[
            snapshot(
                "a",
                "com.example.mixin.One",
                &["com/example/A.class"],
                "com.example.A",
            ),
            snapshot(
                "A",
                "com.example.mixin.Two",
                &["com/example/B.class"],
                "com.example.B",
            ),
        ]);
        assert_eq!(report.duplicate_mod_ids.len(), 1);
        assert!(
            report
                .findings
                .iter()
                .any(|finding| finding.code == "conflict.duplicate-mod-id"
                    && finding.severity == Severity::Critical)
        );
    }

    #[test]
    fn detects_identical_mixin_classes() {
        let report = analyze(&[
            snapshot(
                "a",
                "com.example.mixin.Shared",
                &["com/example/A.class"],
                "com.example.A",
            ),
            snapshot(
                "b",
                "com.example.mixin.Shared",
                &["com/example/B.class"],
                "com.example.B",
            ),
        ]);
        assert_eq!(
            report.shared_mixin_classes,
            vec!["com.example.mixin.Shared".to_string()]
        );
    }

    #[test]
    fn shared_game_targets_are_not_conflicts() {
        // Two mods that mix into the same target class (same class file path)
        // by shipping different mixin classes must not be reported as a
        // conflict; only the bundled-class overlap information is recorded.
        let report = analyze(&[
            snapshot(
                "a",
                "com.a.mixin.MinecraftServerMixin",
                &["net/minecraft/server/MinecraftServer.class"],
                "com.a.A",
            ),
            snapshot(
                "b",
                "com.b.mixin.MinecraftServerMixin",
                &["net/minecraft/server/MinecraftServer.class"],
                "com.b.B",
            ),
        ]);
        assert!(report.shared_mixin_classes.is_empty());
        let overlap = report
            .findings
            .iter()
            .find(|finding| finding.code == "conflict.bundled-class-overlap")
            .expect("overlap finding");
        assert_eq!(overlap.severity, Severity::Information);
        assert!(
            !report
                .findings
                .iter()
                .any(|finding| finding.severity == Severity::Critical)
        );
    }

    #[test]
    fn reports_shared_entrypoints_as_warnings() {
        let report = analyze(&[
            snapshot("a", "com.a.mixin.A", &["com/a/A.class"], "com.shared.Entry"),
            snapshot("b", "com.b.mixin.B", &["com/b/B.class"], "com.shared.Entry"),
        ]);
        assert!(
            report
                .findings
                .iter()
                .any(|finding| finding.code == "conflict.shared-entrypoint"
                    && finding.severity == Severity::Warning)
        );
    }
}
