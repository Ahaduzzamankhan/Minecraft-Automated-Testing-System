//! Persistent run history and the prepared-environment registry.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use mats_core::types::{Loader, Side, Verdict};
use mats_core::{MatsHome, fsutil};
use serde::{Deserialize, Serialize};

/// One completed `mats test` run, appended to `history.jsonl`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunRecord {
    pub id: String,
    pub timestamp: DateTime<Utc>,
    pub jar_path: String,
    pub jar_sha1: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mod_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mod_version: Option<String>,
    pub mc_version: String,
    pub loader: Loader,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub loader_version: Option<String>,
    pub side: Side,
    pub verdict: Verdict,
    pub passed: usize,
    pub failed: usize,
    pub skipped: usize,
    pub not_tested: usize,
    pub critical: usize,
    pub errors: usize,
    pub warnings: usize,
    pub duration_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub report_dir: Option<String>,
}

/// Append a run to the history file.
pub fn append_history(home: &MatsHome, record: &RunRecord) -> Result<()> {
    home.ensure_layout()?;
    let path = home.history_file();
    let mut line = serde_json::to_string(record).context("serialising the history record")?;
    line.push('\n');
    use std::io::Write;
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("opening {}", path.display()))?;
    file.write_all(line.as_bytes())
        .with_context(|| format!("appending to {}", path.display()))?;
    Ok(())
}

/// Load the run history, skipping unreadable lines.
pub fn load_history(home: &MatsHome) -> Result<Vec<RunRecord>> {
    let path = home.history_file();
    if !path.is_file() {
        return Ok(Vec::new());
    }
    let text = fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let mut records = Vec::new();
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<RunRecord>(line) {
            Ok(record) => records.push(record),
            Err(error) => tracing::warn!("skipping unreadable history entry: {error}"),
        }
    }
    Ok(records)
}

/// The most recent run, if any.
pub fn latest_run(home: &MatsHome) -> Result<Option<RunRecord>> {
    Ok(load_history(home)?.pop())
}

/// A prepared environment on disk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvironmentRecord {
    pub id: String,
    pub dir: String,
    pub mc_version: String,
    pub loader: Loader,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub loader_version: Option<String>,
    pub side: Side,
    pub created_at: DateTime<Utc>,
    pub size_bytes: u64,
    pub ready: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct EnvironmentMarker {
    id: String,
    mc_version: String,
    loader: Loader,
    #[serde(default)]
    loader_version: Option<String>,
    side: Side,
    created_at: String,
}

/// Enumerate prepared environments from their markers.
pub fn list_environments(home: &MatsHome) -> Result<Vec<EnvironmentRecord>> {
    let envs_dir = home.envs_dir();
    if !envs_dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut records = Vec::new();
    for entry in fs::read_dir(&envs_dir)
        .with_context(|| format!("listing {}", envs_dir.display()))?
        .flatten()
    {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let marker_path = path.join("mats-env.json");
        let Ok(text) = fs::read_to_string(&marker_path) else {
            continue;
        };
        let Ok(marker) = serde_json::from_str::<EnvironmentMarker>(&text) else {
            continue;
        };
        let created_at = DateTime::parse_from_rfc3339(&marker.created_at)
            .map(|value| value.with_timezone(&Utc))
            .unwrap_or_else(|_| Utc::now());
        records.push(EnvironmentRecord {
            id: marker.id,
            dir: path.display().to_string(),
            mc_version: marker.mc_version,
            loader: marker.loader,
            loader_version: marker.loader_version,
            side: marker.side,
            created_at,
            size_bytes: fsutil::dir_size(&path),
            ready: path.join(".mats-ready").is_file(),
        });
    }
    records.sort_by(|left, right| right.created_at.cmp(&left.created_at));
    Ok(records)
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CleanSummary {
    pub removed: usize,
    pub bytes_freed: u64,
    pub skipped: usize,
}

/// Remove prepared environments.
///
/// When `older_than` is set only environments created before that age are
/// removed; when `all` is set every environment is removed. At least one of the
/// two must be provided.
pub fn clean_environments(
    home: &MatsHome,
    all: bool,
    older_than: Option<Duration>,
) -> Result<CleanSummary> {
    if !all && older_than.is_none() {
        anyhow::bail!("refusing to remove environments without --all or --older-than");
    }
    let now = Utc::now();
    let mut summary = CleanSummary::default();
    for environment in list_environments(home)? {
        let expired = match older_than {
            Some(age) => {
                let age = chrono::Duration::from_std(age)
                    .unwrap_or_else(|_| chrono::Duration::days(365 * 100));
                now.signed_duration_since(environment.created_at) > age
            }
            None => true,
        };
        if !all && !expired {
            summary.skipped += 1;
            continue;
        }
        let path = PathBuf::from(&environment.dir);
        if fsutil::remove_dir_if_exists(&path).is_ok() {
            summary.removed += 1;
            summary.bytes_freed += environment.size_bytes;
        }
    }
    Ok(summary)
}

/// Remove report directories older than a cutoff.
pub fn clean_reports(home: &MatsHome, older_than: Option<Duration>) -> Result<CleanSummary> {
    let reports_dir = home.reports_dir();
    if !reports_dir.is_dir() {
        return Ok(CleanSummary::default());
    }
    let mut summary = CleanSummary::default();
    let now = Utc::now();
    for entry in fs::read_dir(&reports_dir)?.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let modified = entry
            .metadata()
            .and_then(|metadata| metadata.modified())
            .ok()
            .map(DateTime::<Utc>::from);
        let expired = match (older_than, modified) {
            (Some(age), Some(modified)) => {
                let age = chrono::Duration::from_std(age)
                    .unwrap_or_else(|_| chrono::Duration::days(365 * 100));
                now.signed_duration_since(modified) > age
            }
            _ => true,
        };
        if !expired {
            summary.skipped += 1;
            continue;
        }
        let size = fsutil::dir_size(&path);
        if fsutil::remove_dir_if_exists(&path).is_ok() {
            summary.removed += 1;
            summary.bytes_freed += size;
        }
    }
    Ok(summary)
}

/// Human-readable summary of recent runs.
pub fn render_history(records: &[RunRecord], limit: usize) -> String {
    if records.is_empty() {
        return "No test runs recorded yet. Run `mats test <mod.jar>` first.\n".to_string();
    }
    let mut out = String::new();
    out.push_str(&format!(
        "{:<20} {:<22} {:<14} {:<10} {:<8} {}\n",
        "TIME (UTC)", "MOD", "MINECRAFT", "LOADER", "VERDICT", "REPORT"
    ));
    for record in records.iter().rev().take(limit) {
        out.push_str(&format!(
            "{:<20} {:<22} {:<14} {:<10} {:<8} {}\n",
            record.timestamp.format("%Y-%m-%d %H:%M:%S").to_string(),
            truncate(
                &format!(
                    "{} {}",
                    record.mod_id.as_deref().unwrap_or("-"),
                    record.mod_version.as_deref().unwrap_or("")
                ),
                22
            ),
            record.mc_version,
            record.loader.as_str(),
            record.verdict.as_str(),
            record.report_dir.as_deref().unwrap_or("-")
        ));
    }
    out
}

fn truncate(input: &str, width: usize) -> String {
    if input.chars().count() <= width {
        return input.to_string();
    }
    input
        .chars()
        .take(width.saturating_sub(1))
        .collect::<String>()
        + "…"
}

/// Read a report file for `mats report`.
pub fn read_report_file(path: &Path) -> Result<String> {
    fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home(temp: &tempfile::TempDir) -> MatsHome {
        let home = MatsHome::new(temp.path().join("home"));
        home.ensure_layout().unwrap();
        home
    }

    fn record(index: usize) -> RunRecord {
        RunRecord {
            id: format!("run-{index}"),
            timestamp: Utc::now(),
            jar_path: "/tmp/mod.jar".into(),
            jar_sha1: "a".repeat(40),
            mod_id: Some("example".into()),
            mod_version: Some("1.0.0".into()),
            mc_version: "26.3".into(),
            loader: Loader::Fabric,
            loader_version: Some("0.19.5".into()),
            side: Side::Server,
            verdict: Verdict::Passed,
            passed: 2,
            failed: 0,
            skipped: 0,
            not_tested: 1,
            critical: 0,
            errors: 0,
            warnings: 1,
            duration_ms: 1200,
            report_dir: Some("/tmp/reports/run".into()),
        }
    }

    #[test]
    fn history_round_trips_and_keeps_order() {
        let temp = tempfile::tempdir().unwrap();
        let home = home(&temp);
        append_history(&home, &record(1)).unwrap();
        append_history(&home, &record(2)).unwrap();
        let history = load_history(&home).unwrap();
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].id, "run-1");
        assert_eq!(latest_run(&home).unwrap().unwrap().id, "run-2");
        assert!(render_history(&history, 10).contains("run"));
    }

    #[test]
    fn empty_history_is_not_an_error() {
        let temp = tempfile::tempdir().unwrap();
        let home = home(&temp);
        assert!(load_history(&home).unwrap().is_empty());
        assert!(latest_run(&home).unwrap().is_none());
        assert!(render_history(&[], 5).contains("No test runs"));
    }

    #[test]
    fn lists_environments_from_markers() {
        let temp = tempfile::tempdir().unwrap();
        let home = home(&temp);
        let env_dir = home.envs_dir().join("abc123");
        std::fs::create_dir_all(&env_dir).unwrap();
        std::fs::write(
            env_dir.join("mats-env.json"),
            serde_json::json!({
                "schema": 1,
                "id": "abc123",
                "mc_version": "26.3",
                "loader": "fabric",
                "loader_version": "0.19.5",
                "side": "server",
                "java_major": 25,
                "created_at": "2026-11-13T10:00:00Z",
                "mods": []
            })
            .to_string(),
        )
        .unwrap();
        std::fs::write(env_dir.join(".mats-ready"), b"ready").unwrap();
        let environments = list_environments(&home).unwrap();
        assert_eq!(environments.len(), 1);
        assert_eq!(environments[0].mc_version, "26.3");
        assert!(environments[0].ready);
    }

    #[test]
    fn cleaning_requires_an_explicit_scope() {
        let temp = tempfile::tempdir().unwrap();
        let home = home(&temp);
        let error = clean_environments(&home, false, None).unwrap_err();
        assert!(error.to_string().contains("--all"));
    }

    #[test]
    fn cleans_all_environments_when_asked() {
        let temp = tempfile::tempdir().unwrap();
        let home = home(&temp);
        let env_dir = home.envs_dir().join("old");
        std::fs::create_dir_all(&env_dir).unwrap();
        std::fs::write(
            env_dir.join("mats-env.json"),
            serde_json::json!({
                "schema": 1, "id": "old", "mc_version": "1.21.1", "loader": "fabric",
                "loader_version": "0.16.0", "side": "server", "java_major": 21,
                "created_at": "2025-01-01T00:00:00Z", "mods": []
            })
            .to_string(),
        )
        .unwrap();
        let summary = clean_environments(&home, true, None).unwrap();
        assert_eq!(summary.removed, 1);
        assert!(!env_dir.exists());
    }

    #[test]
    fn truncates_long_values_for_display() {
        assert_eq!(truncate("abcdef", 4), "abc…");
        assert_eq!(truncate("abc", 4), "abc");
    }
}
