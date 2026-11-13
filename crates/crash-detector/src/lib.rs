//! Detection of crashes, abnormal exits and JVM-level failures.

use std::fs;
use std::path::{Path, PathBuf};

use mats_core::types::{Finding, FindingCategory, Severity};
use regex::Regex;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CrashKind {
    None,
    /// The process exited with a non-zero code without a crash artefact.
    AbnormalExit,
    /// MATS killed the process after the timeout.
    Timeout,
    /// Minecraft wrote a crash report.
    MinecraftCrashReport,
    /// The JVM wrote an `hs_err_pid` fatal error log.
    JvmFatalError,
    /// The process exited cleanly but never reached the expected startup marker.
    StartupIncomplete,
}

impl CrashKind {
    pub fn as_str(self) -> &'static str {
        match self {
            CrashKind::None => "none",
            CrashKind::AbnormalExit => "abnormal-exit",
            CrashKind::Timeout => "timeout",
            CrashKind::MinecraftCrashReport => "minecraft-crash-report",
            CrashKind::JvmFatalError => "jvm-fatal-error",
            CrashKind::StartupIncomplete => "startup-incomplete",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CrashReportFile {
    pub path: PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cause: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stack: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CrashDetection {
    pub kind: Option<CrashKind>,
    pub crashed: bool,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub crash_reports: Vec<CrashReportFile>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hs_err_files: Vec<PathBuf>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub findings: Vec<Finding>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
}

impl CrashDetection {
    pub fn kind(&self) -> CrashKind {
        self.kind.unwrap_or(CrashKind::None)
    }
}

/// Inspect a finished run for crash artefacts and abnormal termination.
pub fn detect(
    environment_dir: &Path,
    log: &str,
    exit_code: Option<i32>,
    timed_out: bool,
    startup_complete: bool,
) -> CrashDetection {
    let mut detection = CrashDetection {
        exit_code,
        timed_out,
        ..CrashDetection::default()
    };

    detection.crash_reports = find_crash_reports(environment_dir);
    detection.hs_err_files = find_hs_err_files(environment_dir);

    if !detection.crash_reports.is_empty() {
        detection.crashed = true;
        detection.kind = Some(CrashKind::MinecraftCrashReport);
        for report in &detection.crash_reports {
            let detail = match (&report.description, &report.cause) {
                (Some(description), Some(cause)) => format!("{description} ({cause})"),
                (Some(description), None) => description.clone(),
                (None, Some(cause)) => cause.clone(),
                _ => "crash report without a description".to_string(),
            };
            detection.findings.push(
                Finding::new(
                    "crash.minecraft-report",
                    Severity::Critical,
                    FindingCategory::Runtime,
                    "Minecraft wrote a crash report",
                )
                .with_detail(detail)
                .with_evidence(report.stack.iter().cloned()),
            );
        }
        detection.summary = detection
            .crash_reports
            .first()
            .and_then(|report| report.description.clone());
    }

    if !detection.hs_err_files.is_empty() {
        detection.crashed = true;
        if detection.kind.is_none() {
            detection.kind = Some(CrashKind::JvmFatalError);
        }
        detection.findings.push(
            Finding::new(
                "crash.jvm-fatal",
                Severity::Critical,
                FindingCategory::Runtime,
                "The JVM wrote a fatal error log (hs_err)",
            )
            .with_detail(format!(
                "{} file(s) in the test environment",
                detection.hs_err_files.len()
            )),
        );
    }

    if timed_out {
        detection.kind = Some(CrashKind::Timeout);
        detection.findings.push(
            Finding::new(
                "crash.timeout",
                Severity::Error,
                FindingCategory::Runtime,
                "The test exceeded the configured timeout and was terminated",
            )
            .with_detail("increase --timeout for slow machines or heavy mod packs"),
        );
    }

    if detection.kind.is_none() {
        match exit_code {
            Some(0) if !startup_complete => {
                detection.kind = Some(CrashKind::StartupIncomplete);
                detection.findings.push(Finding::new(
                    "crash.startup-incomplete",
                    Severity::Critical,
                    FindingCategory::Runtime,
                    "The process exited before Minecraft finished starting",
                ));
            }
            Some(0) => {}
            Some(code) => {
                detection.crashed = true;
                detection.kind = Some(CrashKind::AbnormalExit);
                detection.findings.push(
                    Finding::new(
                        "crash.abnormal-exit",
                        Severity::Critical,
                        FindingCategory::Runtime,
                        "The JVM exited with a non-zero status",
                    )
                    .with_detail(format!("exit code {code}")),
                );
            }
            None => {
                detection.kind = Some(CrashKind::AbnormalExit);
                detection.findings.push(Finding::new(
                    "crash.terminated",
                    Severity::Critical,
                    FindingCategory::Runtime,
                    "The JVM was terminated by a signal",
                ));
            }
        }
    }

    if log.contains("# A fatal error has been detected by the Java Runtime Environment") {
        detection.crashed = true;
        detection.kind = Some(CrashKind::JvmFatalError);
    }

    detection
}

fn find_crash_reports(root: &Path) -> Vec<CrashReportFile> {
    let mut reports = Vec::new();
    for dir in [root.join("crash-reports"), root.to_path_buf()] {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("txt") {
                continue;
            }
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or_default();
            let is_crash_report = dir.ends_with("crash-reports") || name.starts_with("crash-");
            if !is_crash_report {
                continue;
            }
            let text = fs::read_to_string(&path).unwrap_or_default();
            if !text.contains("---- Minecraft Crash Report ----") {
                continue;
            }
            reports.push(parse_crash_report(&path, &text));
        }
    }
    reports.sort_by(|left, right| left.path.cmp(&right.path));
    reports
}

fn parse_crash_report(path: &Path, text: &str) -> CrashReportFile {
    let description = text
        .lines()
        .find_map(|line| line.trim().strip_prefix("Description:"))
        .map(|value| value.trim().to_string());
    let cause = text
        .lines()
        .find_map(|line| line.trim().strip_prefix("Caused by:"))
        .map(|value| value.trim().to_string());
    let stack: Vec<String> = text
        .lines()
        .skip_while(|line| !line.contains("Description:"))
        .skip(1)
        .take(40)
        .map(|line| line.trim().to_string())
        .filter(|line| !line.is_empty())
        .collect();
    CrashReportFile {
        path: path.to_path_buf(),
        description,
        cause,
        stack,
    }
}

fn find_hs_err_files(root: &Path) -> Vec<PathBuf> {
    let pattern = Regex::new(r"^hs_err_pid\d+\.log$").expect("valid hs_err pattern");
    let mut files = Vec::new();
    let Ok(entries) = fs::read_dir(root) else {
        return files;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default();
        if pattern.is_match(name) {
            files.push(path);
        }
    }
    files.sort();
    files
}

#[cfg(test)]
mod tests {
    use super::*;

    const CRASH_REPORT: &str = "\
---- Minecraft Crash Report ----
// Uh oh.

Time: 2026-11-13 10:00:00
Description: Initializing game

java.lang.NoSuchMethodError: 'void net.minecraft.server.MinecraftServer.tick()'
\tat com.example.ExampleMod.onInitialize(ExampleMod.java:42)
\tat net.fabricmc.loader.impl.FabricLoaderImpl.invokeEntrypoints(FabricLoaderImpl.java:1)

A detailed walkthrough of the error is at https://example.invalid
";

    #[test]
    fn detects_minecraft_crash_reports() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir_all(temp.path().join("crash-reports")).unwrap();
        fs::write(
            temp.path().join("crash-reports/crash-2026-11-13.txt"),
            CRASH_REPORT,
        )
        .unwrap();
        let detection = detect(temp.path(), "", Some(0), false, true);
        assert!(detection.crashed);
        assert_eq!(detection.kind(), CrashKind::MinecraftCrashReport);
        assert_eq!(
            detection.crash_reports[0].description.as_deref(),
            Some("Initializing game")
        );
        assert!(
            detection
                .findings
                .iter()
                .any(|finding| finding.code == "crash.minecraft-report")
        );
        assert_eq!(detection.summary.as_deref(), Some("Initializing game"));
    }

    #[test]
    fn detects_hs_err_files() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(
            temp.path().join("hs_err_pid12345.log"),
            "# A fatal error has been detected by the Java Runtime Environment:\n# SIGSEGV",
        )
        .unwrap();
        let detection = detect(temp.path(), "", None, false, false);
        assert!(detection.crashed);
        assert_eq!(detection.kind(), CrashKind::JvmFatalError);
        assert_eq!(detection.hs_err_files.len(), 1);
    }

    #[test]
    fn reports_timeout_as_crash_kind() {
        let temp = tempfile::tempdir().unwrap();
        let detection = detect(temp.path(), "", None, true, false);
        assert_eq!(detection.kind(), CrashKind::Timeout);
        assert!(
            detection
                .findings
                .iter()
                .any(|finding| finding.code == "crash.timeout")
        );
    }

    #[test]
    fn clean_startup_is_not_a_crash() {
        let temp = tempfile::tempdir().unwrap();
        let detection = detect(temp.path(), "", Some(0), false, true);
        assert!(!detection.crashed);
        assert_eq!(detection.kind(), CrashKind::None);
        assert!(detection.findings.is_empty());
    }

    #[test]
    fn non_zero_exit_is_abnormal() {
        let temp = tempfile::tempdir().unwrap();
        let detection = detect(temp.path(), "", Some(1), false, false);
        assert!(detection.crashed);
        assert_eq!(detection.kind(), CrashKind::AbnormalExit);
    }

    #[test]
    fn exits_before_startup_are_flagged() {
        let temp = tempfile::tempdir().unwrap();
        let detection = detect(temp.path(), "", Some(0), false, false);
        assert_eq!(detection.kind(), CrashKind::StartupIncomplete);
    }

    #[test]
    fn ignores_random_text_files_named_txt() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("notes.txt"), "not a crash").unwrap();
        let detection = detect(temp.path(), "", Some(0), false, true);
        assert!(detection.crash_reports.is_empty());
    }
}
