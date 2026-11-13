//! Cross-platform process sampling.
//!
//! Metrics that a platform genuinely cannot provide are reported as `None`
//! instead of invented values. CPU percentages are per-process and can exceed
//! 100% on multi-threaded JVMs, exactly like `top` reports them.

#[cfg(any(target_os = "macos", windows))]
use std::process::Command;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Sample {
    pub cpu_percent: Option<f64>,
    pub rss_bytes: Option<u64>,
    pub threads: Option<u32>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Peaks {
    pub cpu_peak_percent: Option<f64>,
    pub cpu_avg_percent: Option<f64>,
    pub peak_rss_bytes: Option<u64>,
    pub thread_peak: Option<u32>,
    pub samples: u32,
}

/// Samples a running process at a fixed interval.
pub struct Sampler {
    pid: u32,
    last_wall: Option<Instant>,
    last_cpu_time: Option<u64>,
    cpu_total: f64,
    peaks: Peaks,
}

impl Sampler {
    pub fn new(pid: u32) -> Self {
        Sampler {
            pid,
            last_wall: None,
            last_cpu_time: None,
            cpu_total: 0.0,
            peaks: Peaks::default(),
        }
    }

    /// Take one sample and fold it into the running peaks.
    pub fn sample(&mut self) {
        let now = Instant::now();
        let mut sample = read_sample(self.pid);
        let cpu_time = read_cpu_time_ns(self.pid);
        if let (Some(cpu), Some(previous), Some(last_wall)) =
            (cpu_time, self.last_cpu_time, self.last_wall)
        {
            let wall_ns = now.duration_since(last_wall).as_nanos() as f64;
            if wall_ns > 0.0 && cpu >= previous {
                let percent = (cpu - previous) as f64 / wall_ns * 100.0;
                sample.cpu_percent = Some(percent);
                self.cpu_total += percent;
            }
        } else if sample.cpu_percent.is_none() {
            sample.cpu_percent = cpu_time.and(Some(0.0));
        }
        self.last_cpu_time = cpu_time.or(self.last_cpu_time);
        self.last_wall = Some(now);
        self.peaks.samples += 1;
        if let Some(cpu) = sample.cpu_percent {
            self.peaks.cpu_peak_percent = Some(
                self.peaks
                    .cpu_peak_percent
                    .map(|peak| peak.max(cpu))
                    .unwrap_or(cpu),
            );
        }
        if let Some(rss) = sample.rss_bytes {
            self.peaks.peak_rss_bytes = Some(
                self.peaks
                    .peak_rss_bytes
                    .map(|peak| peak.max(rss))
                    .unwrap_or(rss),
            );
        }
        if let Some(threads) = sample.threads {
            self.peaks.thread_peak = Some(
                self.peaks
                    .thread_peak
                    .map(|peak| peak.max(threads))
                    .unwrap_or(threads),
            );
        }
    }

    pub fn finish(mut self) -> Peaks {
        if self.peaks.samples > 1 {
            self.peaks.cpu_avg_percent = Some(self.cpu_total / (self.peaks.samples as f64 - 1.0));
        } else if let Some(peak) = self.peaks.cpu_peak_percent {
            self.peaks.cpu_avg_percent = Some(peak);
        }
        self.peaks
    }
}

#[cfg(target_os = "linux")]
fn read_sample(pid: u32) -> Sample {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap_or_default();
    let mut sample = Sample::default();
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            sample.rss_bytes = rest
                .trim()
                .split_whitespace()
                .next()
                .and_then(|value| value.parse::<u64>().ok())
                .map(|kilobytes| kilobytes * 1024);
        } else if let Some(rest) = line.strip_prefix("Threads:") {
            sample.threads = rest.trim().parse::<u32>().ok();
        }
    }
    sample
}

#[cfg(target_os = "linux")]
fn read_cpu_time_ns(pid: u32) -> Option<u64> {
    // `schedstat` reports cumulative on-CPU nanoseconds.
    if let Ok(text) = std::fs::read_to_string(format!("/proc/{pid}/schedstat")) {
        if let Some(value) = text
            .split_whitespace()
            .next()
            .and_then(|value| value.parse::<u64>().ok())
        {
            return Some(value);
        }
    }
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let close = stat.rfind(')')?;
    let fields: Vec<&str> = stat[close + 1..].split_whitespace().collect();
    // Fields after `comm`: index 0 is field 3 (state); utime is field 14.
    let utime = fields.get(11)?.parse::<u64>().ok()?;
    let stime = fields.get(12)?.parse::<u64>().ok()?;
    Some((utime + stime) * 10_000_000)
}

#[cfg(target_os = "macos")]
fn read_sample(pid: u32) -> Sample {
    let output = Command::new("ps")
        .args([
            "-o",
            "rss=",
            "-o",
            "%cpu=",
            "-o",
            "thcount=",
            "-p",
            &pid.to_string(),
        ])
        .output()
        .ok();
    let Some(output) = output else {
        return Sample::default();
    };
    let text = String::from_utf8_lossy(&output.stdout);
    let mut parts = text.split_whitespace();
    let rss = parts.next().and_then(|value| value.parse::<u64>().ok());
    let cpu = parts.next().and_then(|value| value.parse::<f64>().ok());
    let threads = parts.next().and_then(|value| value.parse::<u32>().ok());
    Sample {
        cpu_percent: cpu,
        rss_bytes: rss.map(|kilobytes| kilobytes * 1024),
        threads,
    }
}

#[cfg(target_os = "macos")]
fn read_cpu_time_ns(_pid: u32) -> Option<u64> {
    // macOS `ps` provides percentages directly; cumulative CPU time requires
    // task_info(). Report CPU from the percentage samples only.
    None
}

#[cfg(windows)]
fn read_sample(pid: u32) -> Sample {
    let output = Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
        .output()
        .ok();
    let Some(output) = output else {
        return Sample::default();
    };
    let text = String::from_utf8_lossy(&output.stdout);
    // "java.exe","1234","Console","1","123,456 K"
    let fields: Vec<&str> = text
        .lines()
        .next()
        .unwrap_or_default()
        .split("\",\"")
        .collect();
    let memory = fields.get(4).map(|value| value.trim_matches('"').trim());
    let rss_bytes = memory.and_then(|value| {
        let cleaned = value.replace(',', "").replace(" K", "").replace('K', "");
        cleaned
            .trim()
            .parse::<u64>()
            .ok()
            .map(|kilobytes| kilobytes * 1024)
    });
    Sample {
        cpu_percent: None,
        rss_bytes,
        threads: None,
    }
}

#[cfg(windows)]
fn read_cpu_time_ns(_pid: u32) -> Option<u64> {
    None
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn read_sample(_pid: u32) -> Sample {
    Sample::default()
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn read_cpu_time_ns(_pid: u32) -> Option<u64> {
    None
}

/// Interval between samples.
pub const SAMPLE_INTERVAL: Duration = Duration::from_millis(250);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn samples_the_current_process() {
        let mut sampler = Sampler::new(std::process::id());
        std::thread::sleep(Duration::from_millis(50));
        sampler.sample();
        std::thread::sleep(Duration::from_millis(200));
        sampler.sample();
        let peaks = sampler.finish();
        assert!(peaks.samples >= 2);
        #[cfg(target_os = "linux")]
        {
            assert!(peaks.peak_rss_bytes.unwrap_or(0) > 0);
            assert!(peaks.thread_peak.unwrap_or(0) >= 1);
        }
    }

    #[test]
    fn missing_process_yields_no_samples() {
        let mut sampler = Sampler::new(4_000_000_000);
        sampler.sample();
        let peaks = sampler.finish();
        assert!(peaks.peak_rss_bytes.is_none());
    }
}
