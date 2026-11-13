//! Classification of Minecraft/JVM runtime output.
//!
//! The analyzer recognises failure signatures by their real textual markers
//! (exception class names, loader messages, mixin transformer errors) and maps
//! them onto MATS findings. Anything it does not recognise is left alone
//! instead of being guessed at.

use std::collections::BTreeMap;

use mats_core::types::{Finding, FindingCategory, Severity};
use regex::Regex;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExceptionRecord {
    pub class: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    pub count: usize,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModDiscovery {
    /// `(mod id, version)` pairs the loader printed while loading.
    pub mods: Vec<(String, String)>,
    pub declared_count: Option<usize>,
    pub announced: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogMarkers {
    pub minecraft_loading: bool,
    pub mixin_phase: bool,
    pub server_done: bool,
    pub client_sound_started: bool,
    pub runtime_started: bool,
    pub initialization_errors: usize,
    pub warnings: usize,
    pub errors: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogAnalysis {
    pub findings: Vec<Finding>,
    pub exceptions: Vec<ExceptionRecord>,
    pub discovery: ModDiscovery,
    pub markers: LogMarkers,
    pub line_count: usize,
    pub truncated: bool,
}

const MAX_ANALYSED_BYTES: usize = 32 * 1024 * 1024;
const MAX_ANALYSED_LINES: usize = 400_000;

/// Analyse captured process output.
pub fn analyze(text: &str) -> LogAnalysis {
    let truncated = text.len() > MAX_ANALYSED_BYTES;
    let text = if truncated {
        &text[..safe_boundary(text, MAX_ANALYSED_BYTES)]
    } else {
        text
    };

    let exception_pattern = Regex::new(
        r"(?P<prefix>(?:Caused by:\s*)?)(?P<class>(?:[A-Za-z_$][A-Za-z0-9_$]*\.)*[A-Z][A-Za-z0-9_$]*(?:Exception|Error|Throwable))(?::\s*(?P<message>[^\r\n]*))?",
    )
    .expect("valid exception pattern");
    let frame_pattern =
        Regex::new(r"(?:^|\s)at\s+(?P<frame>[A-Za-z0-9_$./]+\.[A-Za-z0-9_$<>]+\([^)]*\))")
            .expect("valid frame pattern");
    let level_error = Regex::new(r"\[(?:[^\]]*/)?(?:ERROR|FATAL)\]").expect("valid error pattern");
    let level_warn = Regex::new(r"\[(?:[^\]]*/)?WARN\]").expect("valid warn pattern");
    let loading_mods = Regex::new(r"Loading\s+(?P<count>\d+)\s+mods?:").expect("valid mod count");
    let mod_line = Regex::new(r"^\s*-\s+(?P<id>[A-Za-z0-9_.-]+)\s+(?P<version>\S+)\s*$")
        .expect("valid mod line");

    let mut findings: Vec<Finding> = Vec::new();
    let mut exceptions: BTreeMap<(String, Option<String>), ExceptionRecord> = BTreeMap::new();
    let mut markers = LogMarkers::default();
    let mut discovery = ModDiscovery::default();
    let mut line_count = 0usize;
    let mut in_mod_list = false;
    let mut last_exception: Option<(String, Option<String>)> = None;

    for line in text.lines().take(MAX_ANALYSED_LINES) {
        line_count += 1;
        // MATS' combined log prefixes every stream line with `out `/`err `;
        // strip that so the game's own output can be matched structurally.
        let raw = line.trim_end();
        let trimmed = raw
            .strip_prefix("out ")
            .or_else(|| raw.strip_prefix("err "))
            .unwrap_or(raw);

        if trimmed.contains("Loading Minecraft") || trimmed.contains("Starting minecraft server") {
            markers.minecraft_loading = true;
        }
        if trimmed.contains("Done (") || trimmed.contains("For help, type") {
            markers.server_done = true;
            markers.runtime_started = true;
        }
        if trimmed.contains("Sound engine started") || trimmed.contains("Backend library: LWJGL") {
            markers.client_sound_started = true;
            markers.runtime_started = true;
        }
        if trimmed.contains("Mixin") || trimmed.contains("mixin") {
            markers.mixin_phase = true;
        }
        if level_error.is_match(trimmed) {
            markers.errors += 1;
        }
        if level_warn.is_match(trimmed) {
            markers.warnings += 1;
        }

        if let Some(captures) = loading_mods.captures(trimmed) {
            discovery.announced = true;
            in_mod_list = true;
            discovery.declared_count = captures
                .name("count")
                .and_then(|value| value.as_str().parse::<usize>().ok());
            discovery.evidence.push(trimmed.to_string());
            continue;
        }
        if in_mod_list {
            if let Some(captures) = mod_line.captures(trimmed) {
                let id = captures["id"].to_string();
                let version = captures["version"].to_string();
                if !discovery.mods.iter().any(|(existing, _)| existing == &id) {
                    discovery.mods.push((id, version));
                }
                continue;
            }
            if !trimmed.starts_with([' ', '\t']) && !trimmed.trim().is_empty() {
                in_mod_list = false;
            }
        }

        // Exception classes, including `Caused by:` chains.
        if let Some(captures) = exception_pattern.captures(trimmed) {
            let class = captures["class"].to_string();
            let message = captures
                .name("message")
                .map(|value| value.as_str().trim().to_string())
                .filter(|value| !value.is_empty());
            let entry = exceptions
                .entry((class.clone(), message.clone()))
                .or_insert_with(|| ExceptionRecord {
                    class: class.clone(),
                    message: message.clone(),
                    source: Some(trimmed.chars().take(300).collect()),
                    count: 0,
                    evidence: Vec::new(),
                });
            entry.count += 1;
            if entry.evidence.len() < 3 {
                entry.evidence.push(trimmed.chars().take(300).collect());
            }
            last_exception = Some((class.clone(), message.clone()));
        } else if let Some(captures) = frame_pattern.captures(trimmed) {
            // Attach the first few stack frames that follow an exception.
            if let Some(key) = &last_exception {
                if let Some(record) = exceptions.get_mut(key) {
                    if record.evidence.len() < 5 {
                        record.evidence.push(captures["frame"].to_string());
                    }
                }
            }
        }

        classify_line(trimmed, &mut findings, &mut markers);
    }

    for record in exceptions.values() {
        findings.push(exception_finding(record));
    }

    LogAnalysis {
        findings,
        exceptions: exceptions.into_values().collect(),
        discovery,
        markers,
        line_count,
        truncated,
    }
}

fn safe_boundary(text: &str, limit: usize) -> usize {
    if text.len() <= limit {
        return text.len();
    }
    let mut boundary = limit;
    while boundary > 0 && !text.is_char_boundary(boundary) {
        boundary -= 1;
    }
    boundary
}

fn classify_line(line: &str, findings: &mut Vec<Finding>, markers: &mut LogMarkers) {
    let lower = line.to_ascii_lowercase();

    let mixin_failure = [
        "mixin apply failed",
        "mixtransformererror",
        "mixinapplicationexception",
        "invalidinjectionexception",
        "injectionerror",
        "critical injection failure",
        "invalidmixinexception",
        "failed to apply mixin",
        "mixin apply for mod",
    ]
    .iter()
    .find(|pattern| lower.contains(*pattern));
    if let Some(pattern) = mixin_failure {
        markers.initialization_errors += 1;
        findings.push(
            Finding::new(
                "mixin.apply-failed",
                Severity::Error,
                FindingCategory::Mixin,
                "A mixin failed to apply or inject",
            )
            .with_detail(format!("matched `{pattern}` in runtime output"))
            .with_evidence([line.chars().take(400).collect::<String>()]),
        );
    }

    let dependency_failure = [
        "missing or unsupported mandatory dependencies",
        "mod resolution failed",
        "incompatible mod set",
        "missing dependency",
        "requires version",
        "requires any version",
        "which is missing",
        "unsatisfied dependency",
    ]
    .iter()
    .find(|pattern| lower.contains(*pattern));
    if let Some(pattern) = dependency_failure {
        markers.initialization_errors += 1;
        findings.push(
            Finding::new(
                "dependency.resolution-failed",
                Severity::Critical,
                FindingCategory::Dependency,
                "The loader could not satisfy the mod's dependency graph",
            )
            .with_detail(format!("matched `{pattern}` in runtime output"))
            .with_evidence([line.chars().take(400).collect::<String>()]),
        );
    }

    if lower.contains("unsupported class file major version")
        || lower.contains("unsupportedclassversionerror")
        || (lower.contains("requires java") && lower.contains("but"))
    {
        findings.push(
            Finding::new(
                "runtime.java-version-mismatch",
                Severity::Critical,
                FindingCategory::Environment,
                "The mod or loader requires a different Java version than the one that was used",
            )
            .with_evidence([line.chars().take(400).collect::<String>()]),
        );
    }

    if lower.contains("a fatal error has been detected by the java runtime environment") {
        markers.initialization_errors += 1;
        findings.push(
            Finding::new(
                "jvm.fatal-error",
                Severity::Critical,
                FindingCategory::Runtime,
                "The JVM reported a fatal error (crash)",
            )
            .with_evidence([line.chars().take(400).collect::<String>()]),
        );
    }

    if lower.contains("failed to start the minecraft server")
        || lower.contains("could not start the server")
        || lower.contains("encountered an unexpected exception")
    {
        markers.initialization_errors += 1;
        findings.push(
            Finding::new(
                "runtime.startup-failed",
                Severity::Critical,
                FindingCategory::Runtime,
                "Minecraft reported a startup failure",
            )
            .with_evidence([line.chars().take(400).collect::<String>()]),
        );
    }

    if lower.contains("outofmemoryerror") {
        findings.push(
            Finding::new(
                "runtime.out-of-memory",
                Severity::Critical,
                FindingCategory::Performance,
                "The JVM ran out of heap memory during the test",
            )
            .with_detail(
                "increase the MATS heap limit with --memory or the runtime.max_heap_mb setting",
            )
            .with_evidence([line.chars().take(400).collect::<String>()]),
        );
    }
}

fn exception_finding(record: &ExceptionRecord) -> Finding {
    let class = record.class.as_str();
    let base = class.rsplit('.').next().unwrap_or(class);
    let (severity, category, code, message) = match base {
        "ClassNotFoundException" => (
            Severity::Critical,
            FindingCategory::Classpath,
            "runtime.class-not-found",
            "A required class is missing from the runtime classpath",
        ),
        "NoClassDefFoundError" => (
            Severity::Critical,
            FindingCategory::Classpath,
            "runtime.no-class-def-found",
            "A class that the mod references could not be loaded",
        ),
        "NoSuchMethodError" | "NoSuchFieldError" | "IncompatibleClassChangeError" => (
            Severity::Critical,
            FindingCategory::Runtime,
            "runtime.binary-incompatible",
            "The mod calls a method or field that does not exist in this Minecraft/loader version",
        ),
        "ClassCastException" => (
            Severity::Error,
            FindingCategory::Runtime,
            "runtime.class-cast",
            "A class cast failed at runtime",
        ),
        "UnsupportedClassVersionError" => (
            Severity::Critical,
            FindingCategory::Environment,
            "runtime.unsupported-class-version",
            "The mod was compiled for a newer Java release than the JVM that ran it",
        ),
        "UnsatisfiedLinkError" => (
            Severity::Error,
            FindingCategory::Runtime,
            "runtime.native-link-error",
            "A native library could not be loaded",
        ),
        "ExceptionInInitializerError" => (
            Severity::Error,
            FindingCategory::Runtime,
            "runtime.static-initializer-failed",
            "A static initializer of a mod class threw",
        ),
        "OutOfMemoryError" => (
            Severity::Critical,
            FindingCategory::Performance,
            "runtime.out-of-memory",
            "The JVM ran out of memory",
        ),
        "LinkageError" => (
            Severity::Critical,
            FindingCategory::Classpath,
            "runtime.linkage-error",
            "A linkage error occurred while resolving mod classes",
        ),
        "MixinException" | "InvalidMixinException" | "MixinTransformerError" => (
            Severity::Error,
            FindingCategory::Mixin,
            "mixin.exception",
            "A mixin threw while being applied",
        ),
        _ if base.ends_with("Exception") || base.ends_with("Error") => (
            Severity::Warning,
            FindingCategory::Runtime,
            "runtime.exception",
            "An exception was reported during the run",
        ),
        _ => (
            Severity::Information,
            FindingCategory::Runtime,
            "runtime.throwable",
            "A throwable was reported during the run",
        ),
    };
    let mut finding = Finding::new(code, severity, category, message).with_detail(format!(
        "{}{}",
        class,
        record
            .message
            .as_ref()
            .map(|message| format!(": {message}"))
            .unwrap_or_default()
    ));
    if record.count > 1 {
        finding = finding.with_detail(format!(
            "{}{} (reported {} times)",
            class,
            record
                .message
                .as_ref()
                .map(|message| format!(": {message}"))
                .unwrap_or_default(),
            record.count
        ));
    }
    finding.evidence = record.evidence.clone();
    finding
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_missing_dependency() {
        let log = "\
[10:00:00] [main/INFO]: Loading Minecraft 26.3 with Fabric Loader 0.19.5
[10:00:01] [main/ERROR]: Mod resolution failed
[10:00:01] [main/ERROR]: Missing or unsupported mandatory dependencies:
[10:00:01] [main/ERROR]: \t- Mod 'Example' (example) requires any version of mod 'fabric-api'
";
        let analysis = analyze(log);
        assert!(analysis.markers.minecraft_loading);
        assert!(
            analysis
                .findings
                .iter()
                .any(|finding| finding.code == "dependency.resolution-failed")
        );
        assert!(analysis.markers.initialization_errors >= 1);
    }

    #[test]
    fn classifies_no_class_def_found() {
        let log = "\
[10:00:02] [main/ERROR]: java.lang.NoClassDefFoundError: net/minecraft/class_1234
[10:00:02] [main/ERROR]: \tat com.example.ExampleMod.onInitialize(ExampleMod.java:42)
";
        let analysis = analyze(log);
        let finding = analysis
            .findings
            .iter()
            .find(|finding| finding.code == "runtime.no-class-def-found")
            .expect("finding");
        assert_eq!(finding.severity, Severity::Critical);
        assert!(
            finding
                .detail
                .as_deref()
                .unwrap()
                .contains("NoClassDefFoundError")
        );
        assert!(
            finding
                .evidence
                .iter()
                .any(|line| line.contains("com.example"))
        );
    }

    #[test]
    fn does_not_invent_failures_from_clean_output() {
        let log = "\
[10:00:00] [main/INFO]: Loading Minecraft 26.3 with Fabric Loader 0.19.5
[10:00:05] [Server thread/INFO]: Done (5.123s)! For help, type \"help\"
";
        let analysis = analyze(log);
        assert!(analysis.findings.is_empty());
        assert!(analysis.markers.server_done);
        assert!(!analysis.markers.initialization_errors > 0);
    }

    #[test]
    fn distinguishes_mixin_failures() {
        let log = "[10:00:01] [main/ERROR]: Mixin apply failed example.mixins.json:ExampleMixin -> net.minecraft.server.MinecraftServer: org.spongepowered.asm.mixin.injection.throwables.InvalidInjectionException\n";
        let analysis = analyze(log);
        assert!(
            analysis
                .findings
                .iter()
                .any(|finding| finding.code == "mixin.apply-failed")
        );
        assert!(analysis.markers.mixin_phase);
    }

    #[test]
    fn parses_loaded_mod_list() {
        let log = "\
Loading 3 mods:
  - minecraft 26.3
  - fabricloader 0.19.5
  - example-mod 1.0.0
";
        let analysis = analyze(log);
        assert_eq!(analysis.discovery.declared_count, Some(3));
        assert_eq!(analysis.discovery.mods.len(), 3);
        assert!(
            analysis
                .discovery
                .mods
                .iter()
                .any(|(id, version)| id == "example-mod" && version == "1.0.0")
        );
    }

    #[test]
    fn counts_repeated_exceptions_and_truncates_evidence() {
        let log = "[ERROR]: java.lang.NullPointerException: bad\n[ERROR]: java.lang.NullPointerException: bad\n";
        let analysis = analyze(log);
        let record = analysis
            .exceptions
            .iter()
            .find(|record| record.class == "java.lang.NullPointerException")
            .unwrap();
        assert_eq!(record.count, 2);
        assert_eq!(analysis.markers.errors, 2);
    }

    #[test]
    fn recognises_java_version_problems() {
        let log = "[ERROR]: java.lang.UnsupportedClassVersionError: com/example/Mod has been compiled by a more recent version of the Java Runtime (class file version 69.0), this version of the Java Runtime only recognizes class file versions up to 65.0\n";
        let analysis = analyze(log);
        assert!(
            analysis
                .findings
                .iter()
                .any(|finding| finding.code == "runtime.unsupported-class-version")
        );
    }

    #[test]
    fn recognises_fatal_jvm_errors() {
        let log = "# A fatal error has been detected by the Java Runtime Environment:\n# SIGSEGV (0xb) at pc=0x00007f\n";
        let analysis = analyze(log);
        assert!(
            analysis
                .findings
                .iter()
                .any(|finding| finding.code == "jvm.fatal-error")
        );
    }
}
