//! Launching and supervising a JVM in a separate process.

use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use mats_core::Metrics;

use crate::sampler::{SAMPLE_INTERVAL, Sampler};

/// Everything needed to start one isolated JVM.
#[derive(Debug, Clone)]
pub struct LaunchSpec {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub env: Vec<(String, String)>,
    pub timeout: Duration,
    pub log_dir: PathBuf,
    pub label: String,
    /// Writing any of these means startup progressed past the marker.
    pub startup_markers: Vec<String>,
    /// Writing any of these means the run failed; MATS shuts it down early.
    pub failure_markers: Vec<String>,
    /// Wait this long after the startup marker before shutting the process down.
    /// `None` means keep running until the process exits on its own.
    pub stop_after_startup: Option<Duration>,
    /// Commands written to stdin when shutting down (e.g. `stop`).
    pub shutdown_commands: Vec<String>,
    /// How long to wait for a graceful exit before killing the process.
    pub shutdown_grace: Duration,
    /// Data written to stdin right after spawn.
    pub stdin_script: Option<String>,
}

impl Default for LaunchSpec {
    fn default() -> Self {
        LaunchSpec {
            program: PathBuf::from("java"),
            args: Vec::new(),
            cwd: PathBuf::from("."),
            env: Vec::new(),
            timeout: Duration::from_secs(300),
            log_dir: PathBuf::from("."),
            label: "run".into(),
            startup_markers: Vec::new(),
            failure_markers: Vec::new(),
            stop_after_startup: None,
            shutdown_commands: Vec::new(),
            shutdown_grace: Duration::from_secs(20),
            stdin_script: None,
        }
    }
}

/// Result of one supervised JVM run.
#[derive(Debug, Clone)]
pub struct ProcessOutcome {
    pub label: String,
    pub exit_code: Option<i32>,
    pub exit_signal: Option<String>,
    pub timed_out: bool,
    /// The process reached a startup marker.
    pub started: bool,
    /// The process was shut down by MATS after a successful startup.
    pub stopped_after_startup: bool,
    /// A failure marker was observed in the output.
    pub failure_marker_seen: bool,
    pub markers_seen: Vec<String>,
    pub duration_ms: u64,
    pub startup_ms: Option<u64>,
    pub stdout_path: PathBuf,
    pub stderr_path: PathBuf,
    pub log_path: PathBuf,
    pub metrics: Metrics,
    pub log_tail: String,
    pub stdout_tail: String,
    pub stderr_tail: String,
}

impl ProcessOutcome {
    /// A clean exit that was not caused by a MATS timeout.
    pub fn succeeded(&self) -> bool {
        self.exit_code == Some(0) && !self.timed_out
    }
}

#[derive(Debug, Default)]
struct MarkerState {
    seen: Vec<String>,
    started: bool,
    started_at: Option<Instant>,
    failure_seen: bool,
}

/// Launch and supervise a process.
pub fn launch(spec: &LaunchSpec) -> Result<ProcessOutcome> {
    // Bare program names are resolved through PATH by the operating system.
    let explicit_path = spec.program.components().count() > 1;
    if explicit_path && !spec.program.is_file() {
        bail!(
            "cannot launch `{}`: the executable does not exist",
            spec.program.display()
        );
    }
    fs::create_dir_all(&spec.log_dir)
        .with_context(|| format!("creating log directory {}", spec.log_dir.display()))?;
    let stdout_path = spec.log_dir.join(format!("{}.stdout.log", spec.label));
    let stderr_path = spec.log_dir.join(format!("{}.stderr.log", spec.label));
    let log_path = spec.log_dir.join(format!("{}.log", spec.label));
    let stdout_file = File::create(&stdout_path)
        .with_context(|| format!("creating {}", stdout_path.display()))?;
    let stderr_file = File::create(&stderr_path)
        .with_context(|| format!("creating {}", stderr_path.display()))?;
    let combined = Arc::new(Mutex::new(
        File::create(&log_path).with_context(|| format!("creating {}", log_path.display()))?,
    ));

    let mut command = Command::new(&spec.program);
    command
        .args(&spec.args)
        .current_dir(&spec.cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in &spec.env {
        command.env(key, value);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // Do not flash a console window for every JVM MATS starts.
        command.creation_flags(0x0800_0000);
    }

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Give every JVM its own process group so a timeout can stop wrappers
        // (Xvfb, scripts) together with the game and its helper processes.
        command.process_group(0);
    }

    let started_at = Instant::now();
    let mut child = command
        .spawn()
        .with_context(|| format!("starting `{}`", spec.program.display()))?;
    let pid = child.id();
    tracing::debug!(pid, label = %spec.label, "started JVM");

    let state = Arc::new(Mutex::new(MarkerState::default()));
    let stdout_reader = child.stdout.take().map(|stream| {
        spawn_reader(
            stream,
            stdout_file,
            "out",
            combined.clone(),
            state.clone(),
            spec,
        )
    });
    let stderr_reader = child.stderr.take().map(|stream| {
        spawn_reader(
            stream,
            stderr_file,
            "err",
            combined.clone(),
            state.clone(),
            spec,
        )
    });

    if let Some(script) = &spec.stdin_script {
        if let Some(stdin) = child.stdin.as_mut() {
            let _ = stdin.write_all(script.as_bytes());
            let _ = stdin.flush();
        }
    }

    let stop_sampler = Arc::new(AtomicBool::new(false));
    let peaks = Arc::new(Mutex::new(None));
    let sampler_handle = {
        let stop = stop_sampler.clone();
        let peaks = peaks.clone();
        std::thread::spawn(move || {
            let mut sampler = Sampler::new(pid);
            while !stop.load(Ordering::Relaxed) {
                sampler.sample();
                std::thread::sleep(SAMPLE_INTERVAL);
            }
            sampler.sample();
            *peaks.lock().expect("sampler peaks poisoned") = Some(sampler.finish());
        })
    };

    let mut timed_out = false;
    let mut stopped_after_startup = false;
    let mut exit_status = None;
    let mut shutdown_sent_at: Option<Instant> = None;

    loop {
        match child.try_wait().context("waiting for the JVM to exit")? {
            Some(status) => {
                exit_status = Some(status);
                break;
            }
            None => {}
        }
        let elapsed = started_at.elapsed();
        let (started, started_at_marker, failure) = {
            let guard = state.lock().expect("marker state poisoned");
            (guard.started, guard.started_at, guard.failure_seen)
        };
        if failure && shutdown_sent_at.is_none() {
            tracing::debug!(label = %spec.label, "failure marker observed; shutting down");
            send_shutdown(&mut child, spec);
            shutdown_sent_at = Some(Instant::now());
        }
        if started && shutdown_sent_at.is_none() && !failure {
            if let (Some(grace), Some(marker_time)) = (spec.stop_after_startup, started_at_marker) {
                if marker_time.elapsed() >= grace {
                    tracing::debug!(label = %spec.label, "startup complete; requesting shutdown");
                    send_shutdown(&mut child, spec);
                    shutdown_sent_at = Some(Instant::now());
                    stopped_after_startup = true;
                }
            }
        }
        if let Some(sent) = shutdown_sent_at {
            if sent.elapsed() > spec.shutdown_grace {
                tracing::warn!(label = %spec.label, "process did not exit gracefully; killing it");
                kill_tree(&mut child);
                exit_status = child.try_wait().ok().flatten().or(exit_status);
                break;
            }
        }
        if elapsed > spec.timeout {
            tracing::warn!(label = %spec.label, timeout_secs = spec.timeout.as_secs(), "JVM timed out");
            timed_out = true;
            kill_tree(&mut child);
            exit_status = child.try_wait().ok().flatten().or(exit_status);
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    stop_sampler.store(true, Ordering::Relaxed);
    let _ = sampler_handle.join();
    if exit_status.is_none() {
        exit_status = child.wait().ok();
    }
    if let Some(handle) = stdout_reader {
        let _ = handle.join();
    }
    if let Some(handle) = stderr_reader {
        let _ = handle.join();
    }
    // Ensure buffered writes are visible before tails are read.
    if let Ok(mut file) = combined.lock() {
        let _ = file.flush();
    }

    let duration_ms = started_at.elapsed().as_millis() as u64;
    let (markers_seen, started, startup_at) = {
        let guard = state.lock().expect("marker state poisoned");
        (guard.seen.clone(), guard.started, guard.started_at)
    };
    let peaks = peaks
        .lock()
        .expect("sampler peaks poisoned")
        .clone()
        .unwrap_or_default();
    let mut metrics = Metrics {
        total_duration_ms: duration_ms,
        startup_duration_ms: startup_at.map(|at| at.duration_since(started_at).as_millis() as u64),
        cpu_avg_percent: peaks.cpu_avg_percent.map(round2),
        cpu_peak_percent: peaks.cpu_peak_percent.map(round2),
        peak_rss_bytes: peaks.peak_rss_bytes,
        thread_peak: peaks.thread_peak,
        samples: Some(peaks.samples),
        timed_out,
        ..Metrics::default()
    };
    if let Some(status) = &exit_status {
        metrics.exit_code = status.code();
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt;
            metrics.exit_signal = status.signal().map(|signal| match signal {
                9 => "SIGKILL".to_string(),
                15 => "SIGTERM".to_string(),
                other => format!("signal {other}"),
            });
        }
    }

    let log_tail = tail_of(&log_path, 250);
    let stdout_tail = tail_of(&stdout_path, 80);
    let stderr_tail = tail_of(&stderr_path, 80);

    Ok(ProcessOutcome {
        label: spec.label.clone(),
        exit_code: metrics.exit_code,
        exit_signal: metrics.exit_signal.clone(),
        timed_out,
        started,
        stopped_after_startup,
        failure_marker_seen: state
            .lock()
            .map(|guard| guard.failure_seen)
            .unwrap_or(false),
        markers_seen,
        duration_ms,
        startup_ms: metrics.startup_duration_ms,
        stdout_path,
        stderr_path,
        log_path: log_path.clone(),
        metrics,
        log_tail,
        stdout_tail,
        stderr_tail,
    })
}

fn send_shutdown(child: &mut Child, spec: &LaunchSpec) {
    if spec.shutdown_commands.is_empty() {
        // No graceful command exists: closing stdin is the last hint, the
        // shutdown grace period then terminates the process group.
        drop(child.stdin.take());
        return;
    }
    if let Some(stdin) = child.stdin.as_mut() {
        for command in &spec.shutdown_commands {
            let _ = stdin.write_all(command.as_bytes());
            let _ = stdin.write_all(b"\n");
            let _ = stdin.flush();
        }
    }
}

/// Terminate the launched process and, on Unix, its whole process group.
fn kill_tree(child: &mut Child) {
    #[cfg(unix)]
    {
        let pid = child.id() as i32;
        // SAFETY: `killpg` only receives ids owned by this process tree.
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
        }
    }
    let _ = child.kill();
}

fn spawn_reader(
    mut stream: impl Read + Send + 'static,
    mut file: File,
    prefix: &'static str,
    combined: Arc<Mutex<File>>,
    state: Arc<Mutex<MarkerState>>,
    spec: &LaunchSpec,
) -> std::thread::JoinHandle<()> {
    let startup_markers = spec.startup_markers.clone();
    let failure_markers = spec.failure_markers.clone();
    std::thread::spawn(move || {
        let mut buffer = vec![0u8; 32 * 1024];
        let mut search = String::new();
        loop {
            match stream.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => {
                    let chunk = &buffer[..read];
                    let _ = file.write_all(chunk);
                    let text = String::from_utf8_lossy(chunk).into_owned();
                    if let Ok(mut combined) = combined.lock() {
                        for line in text.split_inclusive('\n') {
                            let _ = combined.write_all(prefix.as_bytes());
                            let _ = combined.write_all(b" ");
                            let _ = combined.write_all(line.as_bytes());
                        }
                        let _ = combined.flush();
                    }
                    search.push_str(&text);
                    {
                        let mut guard = state.lock().expect("marker state poisoned");
                        for marker in &startup_markers {
                            if search.contains(marker) && !guard.seen.iter().any(|m| m == marker) {
                                guard.seen.push(marker.clone());
                                if !guard.started {
                                    guard.started = true;
                                    guard.started_at = Some(Instant::now());
                                }
                            }
                        }
                        for marker in &failure_markers {
                            if search.contains(marker) && !guard.seen.iter().any(|m| m == marker) {
                                guard.seen.push(marker.clone());
                                guard.failure_seen = true;
                            }
                        }
                    }
                    // Bound the search window; keep enough overlap for markers
                    // split across reads.
                    if search.len() > 256 * 1024 {
                        let drain_to = search.len() - 64 * 1024;
                        let boundary = search
                            .char_indices()
                            .map(|(index, _)| index)
                            .find(|index| *index >= drain_to)
                            .unwrap_or(drain_to);
                        search.drain(..boundary);
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
        let _ = file.flush();
    })
}

fn tail_of(path: &PathBuf, lines: usize) -> String {
    let Ok(text) = fs::read_to_string(path) else {
        return String::new();
    };
    let collected: Vec<&str> = text.lines().collect();
    let start = collected.len().saturating_sub(lines);
    collected[start..].join("\n")
}

fn round2(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shell_command(label: &str, script: &str) -> LaunchSpec {
        #[cfg(unix)]
        let (program, args) = (
            PathBuf::from("sh"),
            vec!["-c".to_string(), script.to_string()],
        );
        #[cfg(windows)]
        let (program, args) = (
            PathBuf::from("powershell"),
            vec![
                "-NoProfile".to_string(),
                "-NonInteractive".to_string(),
                "-Command".to_string(),
                script.to_string(),
            ],
        );
        LaunchSpec {
            program,
            args,
            label: label.into(),
            ..LaunchSpec::default()
        }
    }

    #[test]
    fn captures_output_and_exit_code() {
        let temp = tempfile::tempdir().unwrap();
        #[cfg(unix)]
        let script = "echo hello-stdout; echo hello-stderr 1>&2; exit 0";
        #[cfg(windows)]
        let script = r#"[Console]::Out.WriteLine('hello-stdout'); [Console]::Error.WriteLine('hello-stderr'); exit 0"#;
        let spec = LaunchSpec {
            log_dir: temp.path().to_path_buf(),
            timeout: Duration::from_secs(20),
            ..shell_command("echo", script)
        };
        let outcome = launch(&spec).unwrap();
        assert_eq!(outcome.exit_code, Some(0));
        assert!(outcome.succeeded());
        assert!(outcome.stdout_tail.contains("hello-stdout"));
        assert!(outcome.stderr_tail.contains("hello-stderr"));
        assert!(outcome.log_tail.contains("out hello-stdout"));
        assert!(outcome.stdout_path.is_file());
    }

    #[test]
    fn reports_non_zero_exit() {
        let temp = tempfile::tempdir().unwrap();
        let spec = LaunchSpec {
            log_dir: temp.path().to_path_buf(),
            timeout: Duration::from_secs(20),
            ..shell_command("fail", "exit 3")
        };
        let outcome = launch(&spec).unwrap();
        assert_eq!(outcome.exit_code, Some(3));
        assert!(!outcome.succeeded());
    }

    #[test]
    fn detects_startup_marker_and_stops_the_process() {
        let temp = tempfile::tempdir().unwrap();
        let spec = LaunchSpec {
            log_dir: temp.path().to_path_buf(),
            timeout: Duration::from_secs(30),
            startup_markers: vec!["READY".into()],
            stop_after_startup: Some(Duration::from_millis(50)),
            shutdown_commands: Vec::new(),
            shutdown_grace: Duration::from_secs(2),
            ..shell_command("marker", "echo READY; sleep 30")
        };
        let start = Instant::now();
        let outcome = launch(&spec).unwrap();
        assert!(outcome.started);
        assert!(outcome.stopped_after_startup);
        assert!(start.elapsed() < Duration::from_secs(10));
        assert!(outcome.startup_ms.is_some());
        assert!(outcome.markers_seen.contains(&"READY".to_string()));
    }

    #[test]
    fn enforces_timeout() {
        let temp = tempfile::tempdir().unwrap();
        let spec = LaunchSpec {
            log_dir: temp.path().to_path_buf(),
            timeout: Duration::from_millis(400),
            ..shell_command("hang", "sleep 30")
        };
        let outcome = launch(&spec).unwrap();
        assert!(outcome.timed_out);
        assert!(!outcome.succeeded());
        assert_eq!(outcome.metrics.timed_out, true);
    }

    #[test]
    fn failure_marker_stops_the_run() {
        let temp = tempfile::tempdir().unwrap();
        let spec = LaunchSpec {
            log_dir: temp.path().to_path_buf(),
            timeout: Duration::from_secs(30),
            failure_markers: vec!["CRASH".into()],
            shutdown_grace: Duration::from_secs(2),
            ..shell_command("crash", "echo CRASH; sleep 30")
        };
        let outcome = launch(&spec).unwrap();
        assert!(outcome.failure_marker_seen);
    }

    #[test]
    fn missing_program_is_reported() {
        let temp = tempfile::tempdir().unwrap();
        let spec = LaunchSpec {
            program: PathBuf::from("/definitely/missing/java"),
            log_dir: temp.path().to_path_buf(),
            ..LaunchSpec::default()
        };
        let error = launch(&spec).unwrap_err();
        assert!(error.to_string().contains("does not exist"));
    }
}
