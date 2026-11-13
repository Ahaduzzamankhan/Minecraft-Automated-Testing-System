//! Domain types shared by every MATS component.
//!
//! These types are the contract between the inspector, the resolver, the
//! runtime builder, the test runner and the report generator.

use std::collections::BTreeMap;
use std::fmt;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// A mod loader MATS knows how to reason about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Loader {
    Vanilla,
    Fabric,
    Quilt,
    Forge,
    NeoForge,
}

impl Loader {
    pub const ALL: [Loader; 5] = [
        Loader::Vanilla,
        Loader::Fabric,
        Loader::Quilt,
        Loader::Forge,
        Loader::NeoForge,
    ];

    /// Loaders that can actually be installed and launched by MATS.
    pub const INSTALLABLE: [Loader; 4] = [
        Loader::Fabric,
        Loader::Quilt,
        Loader::Forge,
        Loader::NeoForge,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Loader::Vanilla => "vanilla",
            Loader::Fabric => "fabric",
            Loader::Quilt => "quilt",
            Loader::Forge => "forge",
            Loader::NeoForge => "neoforge",
        }
    }

    pub fn display_name(self) -> &'static str {
        match self {
            Loader::Vanilla => "Vanilla",
            Loader::Fabric => "Fabric",
            Loader::Quilt => "Quilt",
            Loader::Forge => "Forge",
            Loader::NeoForge => "NeoForge",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "vanilla" => Some(Loader::Vanilla),
            "fabric" => Some(Loader::Fabric),
            "quilt" => Some(Loader::Quilt),
            "forge" => Some(Loader::Forge),
            "neoforge" | "neo-forge" | "neo" => Some(Loader::NeoForge),
            _ => None,
        }
    }

    /// `true` when the loader uses Fabric's `fabric.mod.json`/Knot runtime.
    pub fn is_fabric_like(self) -> bool {
        matches!(self, Loader::Fabric | Loader::Quilt)
    }

    /// Metadata files inspected for this loader, most specific first.
    pub fn metadata_files(self) -> &'static [&'static str] {
        match self {
            Loader::Fabric => &["fabric.mod.json"],
            Loader::Quilt => &["quilt.mod.json", "fabric.mod.json"],
            Loader::Forge => &["META-INF/mods.toml", "META-INF/neoforge.mods.toml"],
            Loader::NeoForge => &["META-INF/neoforge.mods.toml", "META-INF/mods.toml"],
            Loader::Vanilla => &[],
        }
    }
}

impl Default for Loader {
    fn default() -> Self {
        Loader::Vanilla
    }
}

impl fmt::Display for Loader {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for Loader {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        Loader::from_name(s).ok_or_else(|| format!("unknown loader `{s}`"))
    }
}

/// Which game side is being exercised.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Side {
    Client,
    Server,
    Both,
}

impl Side {
    pub fn as_str(self) -> &'static str {
        match self {
            Side::Client => "client",
            Side::Server => "server",
            Side::Both => "both",
        }
    }

    pub fn includes_client(self) -> bool {
        matches!(self, Side::Client | Side::Both)
    }

    pub fn includes_server(self) -> bool {
        matches!(self, Side::Server | Side::Both)
    }
}

impl Default for Side {
    fn default() -> Self {
        Side::Server
    }
}

impl fmt::Display for Side {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for Side {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "client" | "c" => Ok(Side::Client),
            "server" | "s" => Ok(Side::Server),
            "both" | "all" => Ok(Side::Both),
            other => Err(format!(
                "unknown side `{other}` (expected client, server or both)"
            )),
        }
    }
}

/// Diagnostic severity. Ordered from most to least urgent.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, Default,
)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Critical,
    Error,
    Warning,
    #[default]
    Information,
    NotTested,
}

impl Severity {
    pub fn as_str(self) -> &'static str {
        match self {
            Severity::Critical => "critical",
            Severity::Error => "error",
            Severity::Warning => "warning",
            Severity::Information => "information",
            Severity::NotTested => "not-tested",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Severity::Critical => "CRITICAL",
            Severity::Error => "ERROR",
            Severity::Warning => "WARNING",
            Severity::Information => "INFO",
            Severity::NotTested => "NOT TESTED",
        }
    }
}

impl fmt::Display for Severity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Broad area a finding belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FindingCategory {
    Metadata,
    Loader,
    Dependency,
    Mixin,
    Runtime,
    Classpath,
    Conflict,
    Environment,
    Network,
    Graphics,
    Performance,
    Configuration,
}

impl FindingCategory {
    pub fn as_str(self) -> &'static str {
        match self {
            FindingCategory::Metadata => "metadata",
            FindingCategory::Loader => "loader",
            FindingCategory::Dependency => "dependency",
            FindingCategory::Mixin => "mixin",
            FindingCategory::Runtime => "runtime",
            FindingCategory::Classpath => "classpath",
            FindingCategory::Conflict => "conflict",
            FindingCategory::Environment => "environment",
            FindingCategory::Network => "network",
            FindingCategory::Graphics => "graphics",
            FindingCategory::Performance => "performance",
            FindingCategory::Configuration => "configuration",
        }
    }
}

impl fmt::Display for FindingCategory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A single classified diagnostic, optionally attached to a test.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finding {
    /// Stable machine-readable identifier, e.g. `runtime.no-class-def-found`.
    pub code: String,
    pub severity: Severity,
    pub category: FindingCategory,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Verbatim log lines or metadata fragments that justify the finding.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<String>,
}

impl Finding {
    pub fn new(
        code: impl Into<String>,
        severity: Severity,
        category: FindingCategory,
        message: impl Into<String>,
    ) -> Self {
        Finding {
            code: code.into(),
            severity,
            category,
            message: message.into(),
            detail: None,
            evidence: Vec::new(),
        }
    }

    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }

    pub fn with_evidence<I, S>(mut self, evidence: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.evidence = evidence.into_iter().map(Into::into).collect();
        self
    }
}

/// Outcome of one executed (or deliberately not executed) test.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TestStatus {
    Passed,
    Failed,
    Skipped,
    NotTested,
    TimedOut,
}

impl TestStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            TestStatus::Passed => "passed",
            TestStatus::Failed => "failed",
            TestStatus::Skipped => "skipped",
            TestStatus::NotTested => "not-tested",
            TestStatus::TimedOut => "timed-out",
        }
    }

    /// Only `Passed` means the behaviour was actually observed working.
    pub fn is_success(self) -> bool {
        matches!(self, TestStatus::Passed)
    }

    pub fn is_failure(self) -> bool {
        matches!(self, TestStatus::Failed | TestStatus::TimedOut)
    }
}

impl fmt::Display for TestStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TestOutcome {
    pub id: String,
    pub name: String,
    pub side: Side,
    pub status: TestStatus,
    pub duration_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub details: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub findings: Vec<Finding>,
}

impl TestOutcome {
    pub fn new(id: impl Into<String>, name: impl Into<String>, side: Side) -> Self {
        TestOutcome {
            id: id.into(),
            name: name.into(),
            side,
            status: TestStatus::NotTested,
            duration_ms: 0,
            summary: None,
            details: Vec::new(),
            findings: Vec::new(),
        }
    }

    pub fn status(mut self, status: TestStatus) -> Self {
        self.status = status;
        self
    }

    pub fn summary(mut self, summary: impl Into<String>) -> Self {
        self.summary = Some(summary.into());
        self
    }

    pub fn duration(mut self, duration_ms: u64) -> Self {
        self.duration_ms = duration_ms;
        self
    }

    pub fn detail(mut self, detail: impl Into<String>) -> Self {
        self.details.push(detail.into());
        self
    }

    pub fn finding(mut self, finding: Finding) -> Self {
        self.findings.push(finding);
        self
    }
}

/// What a declared dependency refers to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DependencyKind {
    Minecraft,
    Loader,
    Java,
    Mod,
}

impl DependencyKind {
    pub fn as_str(self) -> &'static str {
        match self {
            DependencyKind::Minecraft => "minecraft",
            DependencyKind::Loader => "loader",
            DependencyKind::Java => "java",
            DependencyKind::Mod => "mod",
        }
    }
}

/// A dependency declared by the mod, normalised across loaders.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DependencySpec {
    pub mod_id: String,
    pub kind: DependencyKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub constraint: Option<String>,
    pub mandatory: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub side: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ordering: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl DependencySpec {
    pub fn new(mod_id: impl Into<String>, kind: DependencyKind, mandatory: bool) -> Self {
        DependencySpec {
            mod_id: mod_id.into(),
            kind,
            constraint: None,
            mandatory,
            side: None,
            ordering: None,
            reason: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entrypoint {
    pub kind: String,
    pub value: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub side: Option<String>,
}

/// Normalised mod metadata produced by [`metadata_parser`](../../metadata_parser).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ModMetadata {
    pub id: String,
    pub name: String,
    pub version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub authors: Vec<String>,
    pub loader: Option<Loader>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata_file: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mixins: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub access_wideners: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub entrypoints: Vec<Entrypoint>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub depends: Vec<DependencySpec>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub breaks: Vec<DependencySpec>,
    /// Raw Minecraft version constraints exactly as declared.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mc_constraints: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub java_constraint: Option<String>,
    /// `client`, `server` or `*` as declared by the mod.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub environment: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub license: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub contacts: BTreeMap<String, String>,
    /// Jar-in-jar dependencies shipped inside the mod.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub nested_jars: Vec<String>,
    /// Non-fatal metadata problems noticed while parsing.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

impl ModMetadata {
    pub fn mc_constraint_joined(&self) -> Option<String> {
        if self.mc_constraints.is_empty() {
            None
        } else {
            Some(self.mc_constraints.join(" | "))
        }
    }

    pub fn declares_side(&self, side: Side) -> bool {
        match (self.environment.as_deref(), side) {
            (None, _) | (Some("*"), _) | (Some("both"), _) => true,
            (Some("client"), Side::Client) => true,
            (Some("server"), Side::Server) => true,
            (Some("client"), _) => false,
            (Some("server"), _) => false,
            _ => true,
        }
    }

    pub fn entrypoints_for(&self, side: Side) -> Vec<&Entrypoint> {
        self.entrypoints
            .iter()
            .filter(|entry| match entry.side.as_deref() {
                None => true,
                Some(s) if s == side.as_str() => true,
                Some("both") => true,
                _ => false,
            })
            .collect()
    }
}

/// Identity of the inspected JAR.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JarFingerprint {
    pub path: String,
    pub sha1: String,
    pub sha256: String,
    pub size_bytes: u64,
    pub entries: usize,
}

/// Resource kinds cached by the resource manager.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ResourceKind {
    MinecraftClient,
    MinecraftServer,
    Library,
    Native,
    Loader,
    Installer,
    AssetIndex,
    Asset,
    Java,
    /// A mod file, including automatically resolved dependencies.
    Mod,
}

impl ResourceKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ResourceKind::MinecraftClient => "minecraft-client",
            ResourceKind::MinecraftServer => "minecraft-server",
            ResourceKind::Library => "library",
            ResourceKind::Native => "native",
            ResourceKind::Loader => "loader",
            ResourceKind::Installer => "installer",
            ResourceKind::AssetIndex => "asset-index",
            ResourceKind::Asset => "asset",
            ResourceKind::Java => "java",
            ResourceKind::Mod => "mod",
        }
    }
}

/// One resource that was fetched (or reused from cache) for a test run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceRecord {
    pub name: String,
    pub kind: ResourceKind,
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sha1: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size_bytes: Option<u64>,
    pub cache_path: String,
    pub from_cache: bool,
}

/// Metrics collected while the JVM ran. Absent values are genuinely unknown,
/// never zeroed placeholders.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Metrics {
    pub total_duration_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub startup_duration_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu_avg_percent: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu_peak_percent: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub peak_rss_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub peak_heap_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub heap_max_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread_peak: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_signal: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub samples: Option<u32>,
    /// `true` when the process was terminated by the MATS timeout.
    #[serde(default, skip_serializing_if = "is_false")]
    pub timed_out: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
}

/// Overall verdict for a run. `Incomplete` means at least one relevant test
/// could not be executed, and nothing failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Verdict {
    Passed,
    Failed,
    Incomplete,
    NotTested,
}

impl Verdict {
    pub fn as_str(self) -> &'static str {
        match self {
            Verdict::Passed => "passed",
            Verdict::Failed => "failed",
            Verdict::Incomplete => "incomplete",
            Verdict::NotTested => "not-tested",
        }
    }
}

impl fmt::Display for Verdict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunSummary {
    pub total: usize,
    pub passed: usize,
    pub failed: usize,
    pub skipped: usize,
    pub not_tested: usize,
    pub critical: usize,
    pub errors: usize,
    pub warnings: usize,
    pub verdict: Option<Verdict>,
}

impl RunSummary {
    pub fn from_parts(tests: &[TestOutcome], findings: &[Finding]) -> Self {
        let mut summary = RunSummary {
            total: tests.len(),
            ..RunSummary::default()
        };
        for test in tests {
            match test.status {
                TestStatus::Passed => summary.passed += 1,
                TestStatus::Failed | TestStatus::TimedOut => summary.failed += 1,
                TestStatus::Skipped => summary.skipped += 1,
                TestStatus::NotTested => summary.not_tested += 1,
            }
        }
        for finding in findings {
            match finding.severity {
                Severity::Critical => summary.critical += 1,
                Severity::Error => summary.errors += 1,
                Severity::Warning => summary.warnings += 1,
                Severity::Information | Severity::NotTested => {}
            }
        }
        summary.verdict = Some(if summary.failed > 0 || summary.critical > 0 {
            Verdict::Failed
        } else if summary.not_tested > 0 || summary.skipped > 0 {
            Verdict::Incomplete
        } else if summary.passed > 0 {
            Verdict::Passed
        } else {
            Verdict::NotTested
        });
        summary
    }

    pub fn verdict(&self) -> Verdict {
        self.verdict.unwrap_or(Verdict::NotTested)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolInfo {
    pub name: String,
    pub version: String,
    pub os: String,
    pub arch: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub features: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvironmentReport {
    pub mc_version: String,
    pub loader: Loader,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub loader_version: Option<String>,
    pub side: Side,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub java_major: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub java_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub java_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub env_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub env_dir: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub launch_command: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub jvm_args: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory_mb: Option<u32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

/// Everything a report needs about the run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Report {
    pub schema_version: u32,
    pub generated_at: DateTime<Utc>,
    pub tool: ToolInfo,
    pub jar: JarFingerprint,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<ModMetadata>,
    pub inspection: crate::types::InspectionSummary,
    pub environment: EnvironmentReport,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub resources: Vec<ResourceRecord>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tests: Vec<TestOutcome>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub findings: Vec<Finding>,
    #[serde(default)]
    pub metrics: Metrics,
    pub summary: RunSummary,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub logs_dir: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

impl Report {
    pub const SCHEMA_VERSION: u32 = 1;
}

/// Lightweight summary of the JAR inspection, embedded in reports.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InspectionSummary {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub loaders_detected: Vec<Loader>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub primary_loader: Option<Loader>,
    pub class_count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub java_major_min: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub java_major_max: Option<u32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mixin_configs: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub access_wideners: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub nested_jars: Vec<String>,
    pub has_refmap: bool,
    pub uses_minecraft_classes: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn failing_test() -> TestOutcome {
        TestOutcome::new("startup", "Startup", Side::Server)
            .status(TestStatus::Failed)
            .summary("crashed")
    }

    #[test]
    fn summary_counts_statuses_and_findings() {
        let tests = vec![
            failing_test(),
            TestOutcome::new("init", "Init", Side::Server).status(TestStatus::Passed),
            TestOutcome::new("client", "Client", Side::Client).status(TestStatus::NotTested),
        ];
        let findings = vec![
            Finding::new(
                "runtime.crash",
                Severity::Critical,
                FindingCategory::Runtime,
                "crash",
            ),
            Finding::new(
                "dep.missing",
                Severity::Warning,
                FindingCategory::Dependency,
                "missing",
            ),
        ];
        let summary = RunSummary::from_parts(&tests, &findings);
        assert_eq!(summary.total, 3);
        assert_eq!(summary.passed, 1);
        assert_eq!(summary.failed, 1);
        assert_eq!(summary.not_tested, 1);
        assert_eq!(summary.critical, 1);
        assert_eq!(summary.warnings, 1);
        assert_eq!(summary.verdict(), Verdict::Failed);
    }

    #[test]
    fn incomplete_when_nothing_failed_but_tests_were_not_run() {
        let tests = vec![
            TestOutcome::new("server", "Server", Side::Server).status(TestStatus::Passed),
            TestOutcome::new("client", "Client", Side::Client).status(TestStatus::NotTested),
        ];
        assert_eq!(
            RunSummary::from_parts(&tests, &[]).verdict(),
            Verdict::Incomplete
        );
    }

    #[test]
    fn loader_parsing_accepts_aliases() {
        assert_eq!(Loader::from_name("NeoForge"), Some(Loader::NeoForge));
        assert_eq!(Loader::from_name(" neo "), Some(Loader::NeoForge));
        assert_eq!(Loader::from_name("quilt"), Some(Loader::Quilt));
        assert_eq!(Loader::from_name("bukkit"), None);
        assert!("fabric".parse::<Loader>().is_ok());
    }

    #[test]
    fn metadata_side_filtering() {
        let mut meta = ModMetadata {
            environment: Some("client".into()),
            ..ModMetadata::default()
        };
        assert!(meta.declares_side(Side::Client));
        assert!(!meta.declares_side(Side::Server));
        meta.environment = None;
        assert!(meta.declares_side(Side::Server));
    }

    #[test]
    fn severity_ordering_is_most_urgent_first() {
        let mut severities = vec![Severity::Information, Severity::Critical, Severity::Warning];
        severities.sort();
        assert_eq!(severities[0], Severity::Critical);
    }
}
