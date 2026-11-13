//! Performance data that MATS can actually measure.
//!
//! MATS reports JVM startup time, wall-clock test duration, sampled CPU and
//! resident memory, thread counts and heap figures parsed from a real GC log.
//! It never reports rendering performance (FPS) because it does not run a
//! rendering benchmark.

use std::fs;
use std::path::Path;
use std::sync::LazyLock;

use anyhow::{Context, Result};
use mats_core::Metrics;
use regex::Regex;
use serde::{Deserialize, Serialize};

static PAUSE_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?P<before>\d+)(?P<before_unit>[KMG])->(?P<after>\d+)(?P<after_unit>[KMG])\((?P<total>\d+)(?P<total_unit>[KMG])\)\s+(?P<pause>[0-9.]+)ms",
    )
    .expect("valid GC pause pattern")
});

/// Heap statistics derived from a JVM GC log.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct GcStats {
    pub collections: usize,
    pub pause_count: usize,
    pub pause_total_ms: f64,
    pub peak_heap_bytes: Option<u64>,
    pub max_heap_bytes: Option<u64>,
    pub final_heap_bytes: Option<u64>,
}

impl GcStats {
    pub fn is_empty(&self) -> bool {
        self.collections == 0
    }

    /// Average pause duration in milliseconds.
    pub fn average_pause_ms(&self) -> Option<f64> {
        if self.pause_count == 0 {
            None
        } else {
            Some(self.pause_total_ms / self.pause_count as f64)
        }
    }
}

/// Parse a unified-logging GC log produced with
/// `-Xlog:gc*:file=<path>:time,uptime,level,tags`.
pub fn parse_gc_log(text: &str) -> GcStats {
    let mut stats = GcStats::default();
    for line in text.lines() {
        if !line.contains("->") || !line.contains("ms") {
            continue;
        }
        let Some(captures) = PAUSE_PATTERN.captures(line) else {
            continue;
        };
        let before = scaled(&captures["before"], &captures["before_unit"]);
        let after = scaled(&captures["after"], &captures["after_unit"]);
        let total = scaled(&captures["total"], &captures["total_unit"]);
        if let Some(pause) = captures
            .name("pause")
            .and_then(|value| value.as_str().parse::<f64>().ok())
        {
            stats.pause_count += 1;
            stats.pause_total_ms += pause;
        }
        stats.collections += 1;
        if let (Some(before), Some(after)) = (before, after) {
            stats.peak_heap_bytes = Some(
                stats
                    .peak_heap_bytes
                    .map(|peak: u64| peak.max(before).max(after))
                    .unwrap_or(before.max(after)),
            );
            stats.final_heap_bytes = Some(after);
        }
        if let Some(total) = total {
            stats.max_heap_bytes = Some(total);
        }
    }
    stats
}

fn scaled(value: &str, unit: &str) -> Option<u64> {
    let number = value.parse::<u64>().ok()?;
    let multiplier = match unit {
        "K" => 1024,
        "M" => 1024 * 1024,
        "G" => 1024 * 1024 * 1024,
        _ => 1,
    };
    Some(number * multiplier)
}

/// Read and parse a GC log file, tolerating a missing or empty file.
pub fn analyze_gc_log(path: &Path) -> Result<GcStats> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(parse_gc_log(&text)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(GcStats::default()),
        Err(error) => Err(error).with_context(|| format!("reading GC log {}", path.display())),
    }
}

/// Fold GC statistics into the run metrics.
pub fn apply_gc_stats(metrics: &mut Metrics, stats: &GcStats) {
    if metrics.peak_heap_bytes.is_none() {
        metrics.peak_heap_bytes = stats.peak_heap_bytes;
    }
    if metrics.heap_max_bytes.is_none() {
        metrics.heap_max_bytes = stats.max_heap_bytes;
    }
}

/// A presentable performance row, with `None` meaning "not measured".
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PerformanceRow {
    pub label: String,
    pub value: Option<String>,
    pub note: Option<String>,
}

/// Build the performance section of a report from real measurements only.
pub fn summarize(metrics: &Metrics, gc: Option<&GcStats>) -> Vec<PerformanceRow> {
    let mut rows = Vec::new();
    rows.push(PerformanceRow {
        label: "Total test duration".into(),
        value: Some(format!(
            "{:.2} s",
            metrics.total_duration_ms as f64 / 1000.0
        )),
        note: None,
    });
    rows.push(PerformanceRow {
        label: "JVM startup to first startup marker".into(),
        value: metrics.startup_duration_ms.map(|ms| match ms {
            0..=999 => format!("{ms} ms"),
            _ => format!("{:.2} s", ms as f64 / 1000.0),
        }),
        note: metrics
            .startup_duration_ms
            .is_none()
            .then(|| "no startup marker was observed in the captured output".to_string()),
    });
    rows.push(PerformanceRow {
        label: "CPU usage (average / peak)".into(),
        value: match (metrics.cpu_avg_percent, metrics.cpu_peak_percent) {
            (Some(average), Some(peak)) => Some(format!("{average:.1}% / {peak:.1}%")),
            (_, Some(peak)) => Some(format!("peak {peak:.1}%")),
            _ => None,
        },
        note: (metrics.cpu_avg_percent.is_none())
            .then(|| "CPU sampling is not available on this platform".to_string()),
    });
    rows.push(PerformanceRow {
        label: "Peak resident memory".into(),
        value: metrics.peak_rss_bytes.map(mats_core::fsutil::human_bytes),
        note: metrics
            .peak_rss_bytes
            .is_none()
            .then(|| "the process exited before a sample was taken".to_string()),
    });
    rows.push(PerformanceRow {
        label: "Peak JVM heap used".into(),
        value: metrics.peak_heap_bytes.map(mats_core::fsutil::human_bytes),
        note: metrics
            .peak_heap_bytes
            .is_none()
            .then(|| "no GC log was produced; heap usage is unknown rather than zero".to_string()),
    });
    rows.push(PerformanceRow {
        label: "Maximum JVM heap".into(),
        value: metrics.heap_max_bytes.map(mats_core::fsutil::human_bytes),
        note: None,
    });
    rows.push(PerformanceRow {
        label: "Peak thread count".into(),
        value: metrics.thread_peak.map(|threads| threads.to_string()),
        note: metrics
            .thread_peak
            .is_none()
            .then(|| "thread counts are not exposed on this platform".to_string()),
    });
    if let Some(gc) = gc.filter(|stats| !stats.is_empty()) {
        rows.push(PerformanceRow {
            label: "GC pauses".into(),
            value: Some(format!(
                "{} pause(s), {:.1} ms total, {:.1} ms average",
                gc.pause_count,
                gc.pause_total_ms,
                gc.average_pause_ms().unwrap_or(0.0)
            )),
            note: None,
        });
    }
    rows.push(PerformanceRow {
        label: "Exit code".into(),
        value: metrics.exit_code.map(|code| code.to_string()),
        note: metrics
            .exit_signal
            .as_ref()
            .map(|signal| format!("terminated by {signal}")),
    });
    rows.push(PerformanceRow {
        label: "FPS / rendering performance".into(),
        value: None,
        note: Some("not measured: MATS does not run a rendering benchmark".into()),
    });
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    const GC_LOG: &str = "\
[2026-11-13T10:00:00.100+0000][12.345s][info][gc] Using G1
[2026-11-13T10:00:01.200+0000][13.446s][info][gc] GC(0) Pause Young (Normal) (G1 Evacuation Pause) 87M->12M(2048M) 5.123ms
[2026-11-13T10:00:02.200+0000][14.446s][info][gc] GC(1) Pause Young (Normal) (G1 Evacuation Pause) 512M->190M(2048M) 12.500ms
[2026-11-13T10:00:03.200+0000][15.446s][info][gc] GC(2) Pause Full (System.gc()) 900M->300M(2048M) 120.000ms
";

    #[test]
    fn parses_gc_pauses_and_heap() {
        let stats = parse_gc_log(GC_LOG);
        assert_eq!(stats.collections, 3);
        assert_eq!(stats.pause_count, 3);
        assert!((stats.pause_total_ms - 137.623).abs() < 0.001);
        assert_eq!(stats.max_heap_bytes, Some(2048 * 1024 * 1024));
        assert_eq!(stats.final_heap_bytes, Some(300 * 1024 * 1024));
        assert_eq!(stats.peak_heap_bytes, Some(900 * 1024 * 1024));
        assert!((stats.average_pause_ms().unwrap() - 45.874).abs() < 0.01);
    }

    #[test]
    fn empty_or_unrelated_logs_produce_no_values() {
        let stats = parse_gc_log("just some server output\nDone (1.2s)!\n");
        assert!(stats.is_empty());
        assert!(stats.peak_heap_bytes.is_none());
        assert_eq!(stats.average_pause_ms(), None);
    }

    #[test]
    fn missing_file_is_not_an_error() {
        let temp = tempfile::tempdir().unwrap();
        let stats = analyze_gc_log(&temp.path().join("absent.log")).unwrap();
        assert!(stats.is_empty());
    }

    #[test]
    fn summarize_marks_unmeasured_values_as_unknown() {
        let metrics = Metrics {
            total_duration_ms: 1234,
            exit_code: Some(0),
            ..Metrics::default()
        };
        let rows = summarize(&metrics, None);
        let heap = rows
            .iter()
            .find(|row| row.label.contains("Peak JVM heap"))
            .unwrap();
        assert!(heap.value.is_none());
        assert!(heap.note.is_some());
        let fps = rows.iter().find(|row| row.label.contains("FPS")).unwrap();
        assert!(fps.value.is_none());
        assert!(fps.note.as_deref().unwrap().contains("not measured"));
    }

    #[test]
    fn applies_gc_stats_without_overwriting_known_values() {
        let stats = GcStats {
            peak_heap_bytes: Some(100),
            max_heap_bytes: Some(200),
            ..GcStats::default()
        };
        let mut metrics = Metrics {
            peak_heap_bytes: Some(50),
            ..Metrics::default()
        };
        apply_gc_stats(&mut metrics, &stats);
        assert_eq!(metrics.peak_heap_bytes, Some(50));
        assert_eq!(metrics.heap_max_bytes, Some(200));
    }
}
