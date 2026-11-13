//! Report rendering.
//!
//! A run produces `report.md`, `report.json`, `test.md`, `error.md`,
//! `environment.json` and a `logs/` directory. The Markdown documents are for
//! humans; the JSON documents are the machine-readable contract.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use mats_core::fsutil;
use mats_core::types::{Finding, ModMetadata, Report, Severity, TestOutcome, TestStatus, Verdict};
use performance_monitor::{GcStats, summarize};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReportPaths {
    pub dir: PathBuf,
    pub report_md: PathBuf,
    pub report_json: PathBuf,
    pub test_md: PathBuf,
    pub error_md: PathBuf,
    pub environment_json: PathBuf,
    pub logs_dir: PathBuf,
}

/// Write every report artefact for a run.
pub fn write_report(
    report: &Report,
    output_root: &Path,
    logs_source: Option<&Path>,
) -> Result<ReportPaths> {
    let slug = sanitize(&format!(
        "{}-{}",
        report
            .metadata
            .as_ref()
            .map(|metadata| metadata.id.as_str())
            .unwrap_or("mod"),
        report.generated_at.format("%Y%m%d-%H%M%S")
    ));
    let dir = output_root.join(slug);
    fs::create_dir_all(&dir)
        .with_context(|| format!("creating report directory {}", dir.display()))?;

    let paths = ReportPaths {
        report_md: dir.join("report.md"),
        report_json: dir.join("report.json"),
        test_md: dir.join("test.md"),
        error_md: dir.join("error.md"),
        environment_json: dir.join("environment.json"),
        logs_dir: dir.join("logs"),
        dir,
    };

    fsutil::atomic_write(&paths.report_md, render_markdown(report, None).as_bytes())?;
    fsutil::atomic_write(&paths.report_json, &serde_json::to_vec_pretty(report)?)?;
    fsutil::atomic_write(&paths.test_md, render_test_markdown(report).as_bytes())?;
    fsutil::atomic_write(&paths.error_md, render_error_markdown(report).as_bytes())?;
    fsutil::atomic_write(
        &paths.environment_json,
        render_environment_json(report).as_bytes(),
    )?;

    if let Some(source) = logs_source {
        if source.is_dir() {
            let _ = fsutil::remove_dir_if_exists(&paths.logs_dir);
            fsutil::copy_dir(source, &paths.logs_dir)?;
        }
    }
    Ok(paths)
}

/// Render the primary Markdown report.
pub fn render_markdown(report: &Report, gc: Option<&GcStats>) -> String {
    let mut out = String::new();
    let title = report
        .metadata
        .as_ref()
        .map(|metadata| metadata.name.clone())
        .unwrap_or_else(|| {
            report
                .jar
                .path
                .rsplit(['/', '\\'])
                .next()
                .unwrap_or("mod")
                .to_string()
        });
    out.push_str(&format!("# MATS test report — {title}\n\n"));
    out.push_str(&format!(
        "**Verdict:** {}  \n**Generated:** {}  \n**MATS:** {} ({} {})\n\n",
        verdict_label(report.summary.verdict()),
        report.generated_at.format("%Y-%m-%d %H:%M:%S UTC"),
        report.tool.version,
        report.tool.os,
        report.tool.arch
    ));

    out.push_str("## Summary\n\n");
    out.push_str("| Metric | Value |\n|---|---|\n");
    out.push_str(&format!(
        "| Tests passed | {} |\n| Tests failed | {} |\n| Tests skipped | {} |\n| Tests not tested | {} |\n",
        report.summary.passed,
        report.summary.failed,
        report.summary.skipped,
        report.summary.not_tested
    ));
    out.push_str(&format!(
        "| Critical findings | {} |\n| Errors | {} |\n| Warnings | {} |\n",
        report.summary.critical, report.summary.errors, report.summary.warnings
    ));
    out.push_str(&format!(
        "| Minecraft | {} |\n| Loader | {} {} |\n| Side | {} |\n| Java | {} |\n",
        report.environment.mc_version,
        report.environment.loader.display_name(),
        report
            .environment
            .loader_version
            .as_deref()
            .unwrap_or("(unresolved)"),
        report.environment.side,
        report
            .environment
            .java_major
            .map(|major| format!("Java {major}"))
            .unwrap_or_else(|| "unknown".to_string())
    ));
    if let Some(java_version) = &report.environment.java_version {
        out.push_str(&format!("| Java runtime | {java_version} |\n"));
    }
    out.push('\n');

    let unexecuted = report
        .tests
        .iter()
        .any(|test| test.status == TestStatus::NotTested);
    if report.summary.verdict() == Verdict::Incomplete || unexecuted {
        out.push_str(
            "> **INCOMPLETE:** at least one relevant test could not be executed. \
             Untested behaviour is not reported as passing.\n\n",
        );
    }

    out.push_str("## Mod\n\n");
    out.push_str(&format!(
        "- **File:** `{}`\n- **SHA-1:** `{}`\n- **SHA-256:** `{}`\n- **Size:** {}\n",
        report.jar.path,
        report.jar.sha1,
        report.jar.sha256,
        fsutil::human_bytes(report.jar.size_bytes)
    ));
    if let Some(metadata) = &report.metadata {
        render_mod_metadata(&mut out, metadata);
    } else {
        out.push_str("- No loader metadata could be parsed.\n");
    }
    if !report.inspection.warnings.is_empty() {
        out.push_str("\n### Inspection warnings\n\n");
        for warning in &report.inspection.warnings {
            out.push_str(&format!("- {warning}\n"));
        }
    }
    out.push('\n');

    out.push_str("## Environment\n\n");
    out.push_str(&format!(
        "- **Minecraft:** {}\n- **Loader:** {} {}\n- **Side:** {}\n",
        report.environment.mc_version,
        report.environment.loader.display_name(),
        report
            .environment
            .loader_version
            .as_deref()
            .unwrap_or("n/a"),
        report.environment.side
    ));
    if let Some(path) = &report.environment.java_path {
        out.push_str(&format!("- **Java executable:** `{path}`\n"));
    }
    if let Some(id) = &report.environment.env_id {
        out.push_str(&format!("- **Environment id:** `{id}`\n"));
    }
    if let Some(dir) = &report.environment.env_dir {
        out.push_str(&format!("- **Environment directory:** `{dir}`\n"));
    }
    if let Some(memory) = report.environment.memory_mb {
        out.push_str(&format!("- **JVM heap limit:** {memory} MiB\n"));
    }
    if !report.environment.notes.is_empty() {
        out.push_str("\n**Environment notes**\n\n");
        for note in &report.environment.notes {
            out.push_str(&format!("- {note}\n"));
        }
    }
    if let Some(command) = &report.environment.launch_command {
        out.push_str("\n<details><summary>Launch command</summary>\n\n```\n");
        out.push_str(command);
        out.push_str("\n```\n\n</details>\n");
    }
    out.push('\n');

    out.push_str("## Tests\n\n");
    if report.tests.is_empty() {
        out.push_str("No tests were executed.\n\n");
    } else {
        out.push_str("| Test | Side | Status | Duration | Notes |\n|---|---|---|---|---|\n");
        for test in &report.tests {
            out.push_str(&format!(
                "| {} | {} | {} | {} | {} |\n",
                escape_table(&test.name),
                test.side,
                status_label(test.status),
                fsutil::human_duration(test.duration_ms),
                escape_table(test.summary.as_deref().unwrap_or(""))
            ));
        }
        out.push('\n');
    }

    out.push_str("## Diagnostics\n\n");
    if report.findings.is_empty() {
        out.push_str("No errors or warnings were detected.\n\n");
    } else {
        for severity in [
            Severity::Critical,
            Severity::Error,
            Severity::Warning,
            Severity::Information,
        ] {
            let group: Vec<&Finding> = report
                .findings
                .iter()
                .filter(|finding| finding.severity == severity)
                .collect();
            if group.is_empty() {
                continue;
            }
            out.push_str(&format!("### {} ({})\n\n", severity.label(), group.len()));
            for finding in group {
                out.push_str(&format!("- **{}** — {}\n", finding.code, finding.message));
                if let Some(detail) = &finding.detail {
                    out.push_str(&format!("  - {detail}\n"));
                }
                for line in finding.evidence.iter().take(3) {
                    out.push_str(&format!("  - `{}`\n", line.replace('`', "'")));
                }
            }
            out.push('\n');
        }
    }

    out.push_str("## Performance\n\n");
    out.push_str("| Metric | Value | Note |\n|---|---|---|\n");
    for row in summarize(&report.metrics, gc) {
        out.push_str(&format!(
            "| {} | {} | {} |\n",
            escape_table(&row.label),
            escape_table(row.value.as_deref().unwrap_or("not measured")),
            escape_table(row.note.as_deref().unwrap_or(""))
        ));
    }
    out.push('\n');

    if !report.resources.is_empty() {
        out.push_str("## Downloaded resources\n\n");
        out.push_str("| Resource | Kind | Size | Source |\n|---|---|---|---|\n");
        for resource in &report.resources {
            out.push_str(&format!(
                "| {} | {} | {} | {} |\n",
                escape_table(&resource.name),
                resource.kind.as_str(),
                resource
                    .size_bytes
                    .map(fsutil::human_bytes)
                    .unwrap_or_else(|| "unknown".into()),
                if resource.from_cache {
                    "cache"
                } else {
                    "network"
                }
            ));
        }
        out.push('\n');
    }

    if !report.notes.is_empty() {
        out.push_str("## Notes\n\n");
        for note in &report.notes {
            out.push_str(&format!("- {note}\n"));
        }
        out.push('\n');
    }

    out.push_str("## Reproducing this run\n\n");
    if let Some(command) = &report.command {
        out.push_str(&format!("```\n{command}\n```\n\n"));
    }
    out.push_str(&format!(
        "- MATS version: `{}`\n- Platform: `{} {}`\n- Report schema: `{}`\n",
        report.tool.version, report.tool.os, report.tool.arch, report.schema_version
    ));
    if let Some(logs) = &report.logs_dir {
        out.push_str(&format!("- Raw logs: `{logs}`\n"));
    }
    out.push_str("\nSee [test.md](test.md), [error.md](error.md) and [environment.json](environment.json).\n");
    out
}

fn render_mod_metadata(out: &mut String, metadata: &ModMetadata) {
    out.push_str(&format!(
        "- **Mod id:** `{}`\n- **Version:** {}\n",
        metadata.id, metadata.version
    ));
    if !metadata.authors.is_empty() {
        out.push_str(&format!("- **Authors:** {}\n", metadata.authors.join(", ")));
    }
    if let Some(license) = &metadata.license {
        out.push_str(&format!("- **License:** {license}\n"));
    }
    if let Some(description) = &metadata.description {
        let trimmed: String = description.chars().take(400).collect();
        out.push_str(&format!(
            "- **Description:** {}\n",
            trimmed.replace('\n', " ")
        ));
    }
    if let Some(environment) = &metadata.environment {
        out.push_str(&format!("- **Declared environment:** {environment}\n"));
    }
    if !metadata.mc_constraints.is_empty() {
        out.push_str(&format!(
            "- **Minecraft constraint(s):** `{}`\n",
            metadata.mc_constraint_joined().unwrap_or_default()
        ));
    }
    if let Some(java) = &metadata.java_constraint {
        out.push_str(&format!("- **Java constraint:** `{java}`\n"));
    }
    if !metadata.entrypoints.is_empty() {
        out.push_str(&format!(
            "- **Entrypoints:** {}\n",
            metadata
                .entrypoints
                .iter()
                .take(8)
                .map(|entry| format!("{}={}", entry.kind, entry.value))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if !metadata.mixins.is_empty() {
        out.push_str(&format!(
            "- **Mixin configs:** {}\n",
            metadata.mixins.join(", ")
        ));
    }
    if !metadata.access_wideners.is_empty() {
        out.push_str(&format!(
            "- **Access wideners:** {}\n",
            metadata.access_wideners.join(", ")
        ));
    }
    let required: Vec<&mats_core::types::DependencySpec> = metadata
        .depends
        .iter()
        .filter(|dependency| dependency.mandatory)
        .collect();
    if !required.is_empty() {
        out.push_str("- **Required dependencies:**\n");
        for dependency in required.iter().take(20) {
            out.push_str(&format!(
                "  - `{}`{}{}\n",
                dependency.mod_id,
                dependency
                    .constraint
                    .as_ref()
                    .map(|constraint| format!(" {constraint}"))
                    .unwrap_or_default(),
                if dependency.kind == mats_core::types::DependencyKind::Mod {
                    ""
                } else {
                    " (loader/platform)"
                }
            ));
        }
    }
}

/// Render the detailed test document.
pub fn render_test_markdown(report: &Report) -> String {
    let mut out = String::new();
    out.push_str("# Test detail\n\n");
    out.push_str(&format!(
        "Mod: `{}`  \nMinecraft: `{}` · Loader: `{}` {} · Side: `{}`\n\n",
        report
            .metadata
            .as_ref()
            .map(|metadata| metadata.id.as_str())
            .unwrap_or("unknown"),
        report.environment.mc_version,
        report.environment.loader.display_name(),
        report
            .environment
            .loader_version
            .as_deref()
            .unwrap_or("n/a"),
        report.environment.side
    ));
    if report.tests.is_empty() {
        out.push_str("No tests were executed.\n");
        return out;
    }
    for test in &report.tests {
        out.push_str(&format!(
            "## {} — {}\n\n",
            test.name,
            status_label(test.status)
        ));
        out.push_str(&format!(
            "- **Test id:** `{}`\n- **Side:** {}\n- **Duration:** {}\n",
            test.id,
            test.side,
            fsutil::human_duration(test.duration_ms)
        ));
        if let Some(summary) = &test.summary {
            out.push_str(&format!("- **Summary:** {summary}\n"));
        }
        if !test.details.is_empty() {
            out.push_str("\n**Details**\n\n");
            for detail in &test.details {
                out.push_str(&format!("- {detail}\n"));
            }
        }
        if !test.findings.is_empty() {
            out.push_str("\n**Findings**\n\n");
            for finding in &test.findings {
                out.push_str(&format!(
                    "- `{}` [{}] {}\n",
                    finding.code, finding.severity, finding.message
                ));
                if let Some(detail) = &finding.detail {
                    out.push_str(&format!("  - {detail}\n"));
                }
            }
        }
        if matches!(test.status, TestStatus::NotTested | TestStatus::Skipped) {
            out.push_str("\n> This test did not run. Its behaviour is unknown, not passing.\n");
        }
        out.push('\n');
    }
    out
}

/// Render the error document, which only contains actionable failures.
pub fn render_error_markdown(report: &Report) -> String {
    let mut out = String::new();
    out.push_str("# Errors and blocking diagnostics\n\n");
    let blocking: Vec<&Finding> = report
        .findings
        .iter()
        .filter(|finding| matches!(finding.severity, Severity::Critical | Severity::Error))
        .collect();
    if blocking.is_empty() {
        out.push_str("No critical findings or errors were detected.\n\n");
        if report.summary.failed == 0 && report.summary.not_tested > 0 {
            out.push_str(&format!(
                "{} test(s) were not executed; see `report.md` for the reason.\n\n",
                report.summary.not_tested
            ));
        }
        return out;
    }
    for finding in blocking {
        out.push_str(&format!(
            "## {} — {}\n\n- **Severity:** {}\n- **Category:** {}\n",
            finding.code,
            finding.message,
            finding.severity.label(),
            finding.category
        ));
        if let Some(detail) = &finding.detail {
            out.push_str(&format!("- **Detail:** {detail}\n"));
        }
        if !finding.evidence.is_empty() {
            out.push_str("\n```\n");
            for line in finding.evidence.iter().take(10) {
                out.push_str(line);
                out.push('\n');
            }
            out.push_str("```\n");
        }
        out.push('\n');
    }
    let failed: Vec<&TestOutcome> = report
        .tests
        .iter()
        .filter(|test| test.status.is_failure())
        .collect();
    if !failed.is_empty() {
        out.push_str("## Failed tests\n\n");
        for test in failed {
            out.push_str(&format!(
                "- **{}** ({}) — {}\n",
                test.name,
                test.side,
                test.summary.as_deref().unwrap_or("failed")
            ));
        }
    }
    out
}

/// Render `environment.json`.
pub fn render_environment_json(report: &Report) -> String {
    let document = serde_json::json!({
        "schema_version": report.schema_version,
        "generated_at": report.generated_at,
        "tool": report.tool,
        "environment": report.environment,
        "jar": {
            "path": report.jar.path,
            "sha1": report.jar.sha1,
            "sha256": report.jar.sha256,
            "size_bytes": report.jar.size_bytes,
        },
        "resources": report.resources,
        "metrics": report.metrics,
        "notes": report.notes,
        "command": report.command,
    });
    serde_json::to_string_pretty(&document).unwrap_or_else(|_| "{}".to_string())
}

fn verdict_label(verdict: Verdict) -> &'static str {
    match verdict {
        Verdict::Passed => "PASSED",
        Verdict::Failed => "FAILED",
        Verdict::Incomplete => "INCOMPLETE (some tests not executed)",
        Verdict::NotTested => "NOT TESTED",
    }
}

fn status_label(status: TestStatus) -> &'static str {
    match status {
        TestStatus::Passed => "PASSED",
        TestStatus::Failed => "FAILED",
        TestStatus::Skipped => "SKIPPED",
        TestStatus::NotTested => "NOT TESTED",
        TestStatus::TimedOut => "TIMED OUT",
    }
}

fn escape_table(input: &str) -> String {
    input.replace('|', "\\|").replace('\n', " ")
}

fn sanitize(input: &str) -> String {
    let mut output: String = input
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.') {
                character
            } else {
                '-'
            }
        })
        .collect();
    while output.contains("--") || output.contains("..") {
        output = output.replace("--", "-").replace("..", "-");
    }
    output.trim_matches(['-', '.']).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use mats_core::Loader as L;
    use mats_core::types::{
        EnvironmentReport, FindingCategory, InspectionSummary, JarFingerprint, Loader, Metrics,
        RunSummary, Side, TestStatus, ToolInfo,
    };

    fn sample_report() -> Report {
        let tests = vec![
            TestOutcome::new("startup", "Startup", Side::Server)
                .status(TestStatus::Passed)
                .duration(4200)
                .summary("server reached Done"),
            TestOutcome::new("client", "Client startup", Side::Client)
                .status(TestStatus::NotTested)
                .summary("no display available"),
        ];
        let findings = vec![
            Finding::new(
                "runtime.no-class-def-found",
                Severity::Critical,
                FindingCategory::Classpath,
                "A class that the mod references could not be loaded",
            )
            .with_detail("java.lang.NoClassDefFoundError: net/minecraft/class_1234")
            .with_evidence(["at com.example.ExampleMod.init(ExampleMod.java:12)"]),
            Finding::new(
                "metadata.warning",
                Severity::Warning,
                FindingCategory::Metadata,
                "no licence declared",
            ),
        ];
        let summary = RunSummary::from_parts(&tests, &findings);
        Report {
            schema_version: Report::SCHEMA_VERSION,
            generated_at: chrono::Utc::now(),
            tool: ToolInfo {
                name: "mats".into(),
                version: "1.0.0".into(),
                os: std::env::consts::OS.into(),
                arch: std::env::consts::ARCH.into(),
                features: Vec::new(),
            },
            jar: JarFingerprint {
                path: "/tmp/mod.jar".into(),
                sha1: "a".repeat(40),
                sha256: "b".repeat(64),
                size_bytes: 4096,
                entries: 10,
            },
            metadata: Some(ModMetadata {
                id: "example".into(),
                name: "Example".into(),
                version: "1.0.0".into(),
                loader: Some(Loader::Fabric),
                mc_constraints: vec![">=26.1".into()],
                mixins: vec!["example.mixins.json".into()],
                ..ModMetadata::default()
            }),
            inspection: InspectionSummary {
                primary_loader: Some(L::Fabric),
                class_count: 5,
                ..InspectionSummary::default()
            },
            environment: EnvironmentReport {
                mc_version: "26.3".into(),
                loader: Loader::Fabric,
                loader_version: Some("0.19.5".into()),
                side: Side::Server,
                java_major: Some(25),
                java_version: Some("25.0.2".into()),
                java_path: Some("/usr/bin/java".into()),
                env_id: Some("abc".into()),
                env_dir: Some("/tmp/env".into()),
                launch_command: Some("java -jar server.jar nogui".into()),
                jvm_args: Vec::new(),
                memory_mb: Some(1024),
                notes: vec!["server.properties written".into()],
            },
            resources: Vec::new(),
            tests,
            findings,
            metrics: Metrics {
                total_duration_ms: 4200,
                startup_duration_ms: Some(4000),
                exit_code: Some(0),
                ..Metrics::default()
            },
            summary,
            command: Some("mats test mod.jar".into()),
            logs_dir: Some("/tmp/logs".into()),
            notes: Vec::new(),
        }
    }

    #[test]
    fn writes_every_report_artefact() {
        let temp = tempfile::tempdir().unwrap();
        let logs = temp.path().join("logs");
        std::fs::create_dir_all(&logs).unwrap();
        std::fs::write(logs.join("server.log"), "Done (4.2s)!").unwrap();
        let paths = write_report(&sample_report(), temp.path(), Some(&logs)).unwrap();
        for path in [
            &paths.report_md,
            &paths.report_json,
            &paths.test_md,
            &paths.error_md,
            &paths.environment_json,
        ] {
            assert!(path.is_file(), "missing {}", path.display());
        }
        assert!(paths.logs_dir.join("server.log").is_file());
    }

    #[test]
    fn markdown_reports_not_tested_separately_from_passing() {
        let report = sample_report();
        let markdown = render_markdown(&report, None);
        assert!(markdown.contains("INCOMPLETE"));
        assert!(markdown.contains("NOT TESTED"));
        assert!(markdown.contains("Untested behaviour is not reported as passing"));
        assert!(markdown.contains("runtime.no-class-def-found"));
    }

    #[test]
    fn error_document_contains_only_blocking_findings() {
        let report = sample_report();
        let errors = render_error_markdown(&report);
        assert!(errors.contains("runtime.no-class-def-found"));
        assert!(!errors.contains("metadata.warning"));
        let clean = render_error_markdown(&Report {
            findings: Vec::new(),
            ..report
        });
        assert!(clean.contains("No critical findings"));
    }

    #[test]
    fn test_document_lists_each_test_with_status() {
        let markdown = render_test_markdown(&sample_report());
        assert!(markdown.contains("## Startup — PASSED"));
        assert!(markdown.contains("## Client startup — NOT TESTED"));
        assert!(markdown.contains("This test did not run"));
    }

    #[test]
    fn json_reports_round_trip() {
        let report = sample_report();
        let json = serde_json::to_string_pretty(&report).unwrap();
        let parsed: Report = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.summary, report.summary);
        assert_eq!(parsed.environment.mc_version, "26.3");
        let environment: serde_json::Value =
            serde_json::from_str(&render_environment_json(&report)).unwrap();
        assert_eq!(environment["environment"]["mc_version"], "26.3");
    }

    #[test]
    fn report_directory_names_are_sanitised() {
        assert_eq!(
            sanitize("example-mod-20261113-101010"),
            "example-mod-20261113-101010"
        );
        assert_eq!(sanitize("a/b c"), "a-b-c");
        assert!(!sanitize("../evil").contains(".."));
    }

    #[test]
    fn performance_section_marks_unknown_values() {
        let markdown = render_markdown(&sample_report(), None);
        assert!(markdown.contains("not measured"));
        assert!(markdown.contains("FPS"));
    }
}
