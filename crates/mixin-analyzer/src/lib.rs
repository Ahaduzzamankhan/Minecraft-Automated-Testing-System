//! Mixin analysis: static configuration checks plus runtime failure parsing.

use std::collections::BTreeSet;

use jar_inspector::JarInspection;
use mats_core::types::{Finding, FindingCategory, Severity};
use metadata_parser::ParsedMod;
use regex::Regex;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MixinAnalysis {
    pub config_count: usize,
    pub mixin_class_count: usize,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub targets: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub findings: Vec<Finding>,
}

/// Check declared mixin configurations against what is actually in the JAR.
pub fn analyze_static(parsed: &ParsedMod, inspection: &JarInspection) -> MixinAnalysis {
    let mut analysis = MixinAnalysis {
        config_count: parsed.mixin_configs.len(),
        mixin_class_count: parsed
            .mixin_configs
            .iter()
            .map(|config| config.mixin_count())
            .sum(),
        ..MixinAnalysis::default()
    };
    let entries: BTreeSet<&str> = inspection
        .entries
        .iter()
        .map(|entry| entry.name.as_str())
        .collect();

    for config in &parsed.mixin_configs {
        if let Some(refmap) = &config.refmap {
            let present = entries
                .iter()
                .any(|name| *name == refmap.as_str() || name.ends_with(&format!("/{refmap}")));
            if !present {
                analysis.findings.push(
                    Finding::new(
                        "mixin.refmap-missing",
                        Severity::Error,
                        FindingCategory::Mixin,
                        format!(
                            "Mixin configuration `{}` declares refmap `{refmap}` but the file is not in the JAR",
                            config.path
                        ),
                    )
                    .with_detail(
                        "production remapping will fail unless the build produces the refmap",
                    ),
                );
            }
        }
        if let Some(plugin) = &config.plugin {
            let path = format!("{}.class", plugin.replace('.', "/"));
            if !entries.contains(path.as_str()) {
                analysis.findings.push(Finding::new(
                    "mixin.plugin-missing",
                    Severity::Error,
                    FindingCategory::Mixin,
                    format!(
                        "Mixin plugin `{plugin}` declared by `{}` is not present in the JAR",
                        config.path
                    ),
                ));
            }
        }
        for qualified in config.qualified_classes() {
            let path = format!("{}.class", qualified.replace('.', "/"));
            if !entries.contains(path.as_str()) {
                analysis.findings.push(
                    Finding::new(
                        "mixin.class-missing",
                        Severity::Warning,
                        FindingCategory::Mixin,
                        format!(
                            "Mixin class `{qualified}` referenced by `{}` has no matching class file",
                            config.path
                        ),
                    )
                    .with_detail("the loader will fail if this mixin is applied"),
                );
            }
        }
        if config.required && config.mixin_count() == 0 {
            analysis.findings.push(Finding::new(
                "mixin.empty-required-config",
                Severity::Warning,
                FindingCategory::Mixin,
                format!(
                    "Mixin configuration `{}` is required but declares no mixins",
                    config.path
                ),
            ));
        }
        for warning in &config.warnings {
            if warning.contains("nested mixin config reference") {
                analysis.findings.push(Finding::new(
                    "mixin.nested-config",
                    Severity::Information,
                    FindingCategory::Mixin,
                    format!("`{}`: {warning}", config.path),
                ));
            }
        }
        analysis.targets.extend(config.qualified_classes());
    }

    for warning in &parsed.warnings {
        if warning.contains("declared but missing from the archive") {
            analysis.findings.push(
                Finding::new(
                    "mixin.config-declared-missing",
                    Severity::Error,
                    FindingCategory::Mixin,
                    warning.clone(),
                )
                .with_detail("the loader will abort when it cannot find a declared mixin config"),
            );
        }
    }

    if !parsed.metadata.mixins.is_empty() && parsed.mixin_configs.is_empty() {
        analysis.findings.push(
            Finding::new(
                "mixin.no-configs-found",
                Severity::Error,
                FindingCategory::Mixin,
                "Metadata declares mixin configurations but none could be read from the JAR",
            )
            .with_evidence(parsed.metadata.mixins.iter().cloned()),
        );
    }

    analysis.targets.sort();
    analysis.targets.dedup();
    analysis
}

/// Extract mixin failures reported during a run.
pub fn analyze_output(text: &str) -> Vec<Finding> {
    let apply_failure = Regex::new(
        r"Mixin apply (?:for mod [^\s]+ )?failed[:\s]+(?P<config>\S+?)(?::(?P<mixin>[^\s]+))?\s*->\s*(?P<target>[^\s:]+)",
    )
    .expect("valid mixin apply pattern");
    let injection = Regex::new(r"(?P<error>InvalidInjectionException|InjectionError|Critical injection failure|InvalidMixinException|MixinTransformerError)")
        .expect("valid injection pattern");
    let missing_refmap =
        Regex::new(r"(?i)missing refmap|refmap .* not found|Failed to read refmap")
            .expect("valid refmap pattern");

    let mut findings = Vec::new();
    let mut seen: BTreeSet<(String, String)> = BTreeSet::new();
    for line in text.lines() {
        if let Some(captures) = apply_failure.captures(line) {
            let config = captures["config"].to_string();
            let mixin = captures
                .name("mixin")
                .map(|value| value.as_str().to_string());
            let target = captures["target"].to_string();
            let detail = format!(
                "config {config}{} could not be applied to {target}",
                mixin
                    .as_ref()
                    .map(|mixin| format!(", mixin {mixin}"))
                    .unwrap_or_default()
            );
            if seen.insert(("mixin.apply-failed".into(), detail.clone())) {
                findings.push(
                    Finding::new(
                        "mixin.apply-failed",
                        Severity::Error,
                        FindingCategory::Mixin,
                        "A mixin could not be applied to its target class",
                    )
                    .with_detail(detail)
                    .with_evidence([line.chars().take(400).collect::<String>()]),
                );
            }
        }
        if let Some(captures) = injection.captures(line) {
            let error = &captures["error"];
            let detail = format!("{error} reported by the mixin transformer");
            if seen.insert(("mixin.injection-error".into(), detail.clone())) {
                findings.push(
                    Finding::new(
                        "mixin.injection-error",
                        Severity::Error,
                        FindingCategory::Mixin,
                        "A mixin injection failed",
                    )
                    .with_detail(detail)
                    .with_evidence([line.chars().take(400).collect::<String>()]),
                );
            }
        }
        if missing_refmap.is_match(line) {
            let detail = line.chars().take(300).collect::<String>();
            if seen.insert(("mixin.refmap-runtime".into(), detail.clone())) {
                findings.push(
                    Finding::new(
                        "mixin.refmap-runtime",
                        Severity::Error,
                        FindingCategory::Mixin,
                        "The loader could not read a mixin refmap",
                    )
                    .with_detail(detail)
                    .with_evidence([line.chars().take(400).collect::<String>()]),
                );
            }
        }
    }
    findings
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed_with(config_json: &str, mixins: &[&str]) -> ParsedMod {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("mod.jar");
        let mut entries: Vec<(&str, &[u8])> = vec![
            (
                "fabric.mod.json",
                br#"{"schemaVersion":1,"id":"example","version":"1.0.0"}"#.as_slice(),
            ),
            ("example.mixins.json", config_json.as_bytes()),
        ];
        entries.push(("com/example/mixin/CommonMixin.class", b"\xCA\xFE\xBA\xBE"));
        jar_inspector::write_jar(&path, &entries).unwrap();
        let inspection = JarInspection::inspect(&path).unwrap();
        let mut parsed = metadata_parser::parse_inspection(&inspection).unwrap();
        parsed.metadata.mixins = mixins.iter().map(|value| value.to_string()).collect();
        parsed
    }

    #[test]
    fn detects_missing_refmap_and_mixin_class() {
        let parsed = parsed_with(
            r#"{"package":"com.example.mixin","refmap":"example.refmap.json","mixins":["CommonMixin","AbsentMixin"]}"#,
            &["example.mixins.json"],
        );
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("mod2.jar");
        jar_inspector::write_jar(
            &path,
            &[
                ("fabric.mod.json", br#"{"schemaVersion":1,"id":"example","version":"1.0.0"}"#),
                (
                    "example.mixins.json",
                    br#"{"package":"com.example.mixin","refmap":"example.refmap.json","mixins":["CommonMixin","AbsentMixin"]}"#,
                ),
                ("com/example/mixin/CommonMixin.class", b"\xCA\xFE\xBA\xBE"),
            ],
        )
        .unwrap();
        let inspection = JarInspection::inspect(&path).unwrap();
        let analysis = analyze_static(&parsed, &inspection);
        assert!(
            analysis
                .findings
                .iter()
                .any(|finding| finding.code == "mixin.refmap-missing")
        );
        assert!(
            analysis
                .findings
                .iter()
                .any(|finding| finding.code == "mixin.class-missing")
        );
        assert_eq!(analysis.mixin_class_count, 2);
        assert!(
            analysis
                .targets
                .contains(&"com.example.mixin.CommonMixin".to_string())
        );
    }

    #[test]
    fn reports_missing_plugin_class() {
        let parsed = parsed_with(
            r#"{"package":"com.example.mixin","plugin":"com.example.AbsentPlugin","mixins":["CommonMixin"]}"#,
            &["example.mixins.json"],
        );
        let temp = tempfile::tempdir().unwrap();
        let inspection = {
            let path = temp.path().join("m.jar");
            jar_inspector::write_jar(
                &path,
                &[
                    ("fabric.mod.json", br#"{"schemaVersion":1,"id":"example","version":"1.0.0"}"#),
                    (
                        "example.mixins.json",
                        br#"{"package":"com.example.mixin","plugin":"com.example.AbsentPlugin","mixins":["CommonMixin"]}"#,
                    ),
                    ("com/example/mixin/CommonMixin.class", b"\xCA\xFE\xBA\xBE"),
                ],
            )
            .unwrap();
            JarInspection::inspect(&path).unwrap()
        };
        let analysis = analyze_static(&parsed, &inspection);
        assert!(
            analysis
                .findings
                .iter()
                .any(|finding| finding.code == "mixin.plugin-missing")
        );
    }

    #[test]
    fn parses_mixin_failures_from_output() {
        let log = "\
[10:00:01] [main/ERROR]: Mixin apply failed example.mixins.json:ServerMixin -> net.minecraft.server.MinecraftServer: org.spongepowered.asm.mixin.injection.throwables.InvalidInjectionException
[10:00:01] [main/ERROR]: Critical injection failure: @Inject annotation could not find target
[10:00:02] [main/ERROR]: Missing refmap example.refmap.json
";
        let findings = analyze_output(log);
        assert!(
            findings
                .iter()
                .any(|finding| finding.code == "mixin.apply-failed")
        );
        assert!(
            findings
                .iter()
                .any(|finding| finding.code == "mixin.injection-error")
        );
        assert!(
            findings
                .iter()
                .any(|finding| finding.code == "mixin.refmap-runtime")
        );
    }

    #[test]
    fn clean_output_produces_nothing() {
        assert!(analyze_output("Done (2.345s)! For help, type \"help\"").is_empty());
    }
}
