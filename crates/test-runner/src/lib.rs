//! End-to-end test orchestration.
//!
//! One `mats test` invocation performs static inspection, resolves a concrete
//! Minecraft/loader/Java combination, prepares an isolated environment, runs
//! the real game in a separate JVM per side, analyses the output and writes the
//! report. Tests are only marked `Passed` when the behaviour was observed.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::Utc;
use crash_detector::CrashDetection;
use diagnostics::{SeverityCounts, aggregate, count_severities};
use jar_inspector::JarInspection;
use java_manager::JavaRuntime;
use log_analyzer::LogAnalysis;
use mats_core::config::{Config, DisplayMode, NetworkConfig};
use mats_core::types::{
    DependencyKind, EnvironmentReport, Finding, FindingCategory, JarFingerprint, Loader, Metrics,
    ModMetadata, Report, ResourceKind, RunSummary, Severity, Side, TestOutcome, TestStatus,
    ToolInfo, Verdict,
};
use mats_core::{MatsHome, VersionPredicate, fsutil};
use metadata_parser::ParsedMod;
use performance_monitor::{GcStats, analyze_gc_log, apply_gc_stats};
use process_monitor::{LaunchSpec, ProcessOutcome, launch};
use report_generator::{ReportPaths, write_report};
use resource_manager::{ProgressFn, ResourceManager, ResourceRequest};
use runtime_builder::{EnvironmentSpec, PreparedEnvironment};
use storage::RunRecord;
use version_resolver::{PlanRequest, ResolvedPlan, resolve_plan};

/// Everything the CLI can influence about a test run.
#[derive(Debug, Clone)]
pub struct TestOptions {
    pub jar: PathBuf,
    pub extra_mods: Vec<PathBuf>,
    pub side: Side,
    pub mc_version: Option<String>,
    pub loader: Option<Loader>,
    pub loader_version: Option<String>,
    pub timeout: Option<Duration>,
    pub memory_mb: Option<u32>,
    pub accept_eula: bool,
    pub offline: bool,
    pub fresh: bool,
    pub download_assets: bool,
    pub display: Option<DisplayMode>,
    pub allow_snapshots: bool,
    pub keep_environments: bool,
}

impl TestOptions {
    pub fn new(jar: impl Into<PathBuf>) -> Self {
        TestOptions {
            jar: jar.into(),
            extra_mods: Vec::new(),
            side: Side::Both,
            mc_version: None,
            loader: None,
            loader_version: None,
            timeout: None,
            memory_mb: None,
            accept_eula: false,
            offline: false,
            fresh: false,
            download_assets: false,
            display: None,
            allow_snapshots: false,
            keep_environments: true,
        }
    }
}

#[derive(Debug)]
pub struct RunOutcome {
    pub report: Report,
    pub report_paths: Option<ReportPaths>,
    pub exit_code: i32,
}

/// Exit code convention: 0 success (or incomplete), 1 test failure, 2 setup
/// failure that prevented a meaningful report.
pub fn exit_code_for(report: &Report) -> i32 {
    match report.summary.verdict() {
        Verdict::Failed => 1,
        Verdict::Passed | Verdict::Incomplete => 0,
        Verdict::NotTested => 1,
    }
}

/// Execute a full test run and write the report.
pub fn run(
    manager: &ResourceManager,
    config: &Config,
    home: &MatsHome,
    options: &TestOptions,
    progress: Option<ProgressFn>,
) -> Result<RunOutcome> {
    let manager = match progress {
        Some(reporter) => manager.clone().with_reporter(reporter),
        None => manager.clone(),
    };
    home.ensure_layout()?;
    if !options.jar.is_file() {
        anyhow::bail!("mod file `{}` does not exist", options.jar.display());
    }

    let inspection = JarInspection::inspect(&options.jar)
        .with_context(|| format!("inspecting {}", options.jar.display()))?;
    let parsed = metadata_parser::parse_inspection(&inspection)
        .with_context(|| format!("parsing metadata from {}", options.jar.display()))?;

    let mut findings = Vec::new();
    let mut notes = Vec::new();

    if inspection.loaders_detected.len() > 1 {
        findings.push(
            Finding::new(
                "metadata.multiple-loaders",
                Severity::Warning,
                FindingCategory::Metadata,
                format!(
                    "the JAR contains metadata for multiple loaders: {}",
                    inspection
                        .loaders_detected
                        .iter()
                        .map(|loader| loader.display_name())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            )
            .with_detail(
                "MATS uses the most specific metadata available; declare the intended loader with --loader if this is wrong",
            ),
        );
    }
    for warning in &parsed.warnings {
        findings.push(Finding::new(
            "metadata.parser-warning",
            Severity::Warning,
            FindingCategory::Metadata,
            warning.clone(),
        ));
    }
    for warning in &inspection.warnings {
        findings.push(Finding::new(
            "inspection.warning",
            Severity::Warning,
            FindingCategory::Metadata,
            warning.clone(),
        ));
    }

    let loader = match determine_loader(&inspection, options.loader) {
        Ok(loader) => loader,
        Err(error) => {
            findings.push(
                Finding::new(
                    "metadata.no-loader",
                    Severity::Critical,
                    FindingCategory::Loader,
                    "MATS could not determine which mod loader this JAR targets",
                )
                .with_detail(error.to_string()),
            );
            let report = build_static_report(
                &inspection,
                &parsed,
                Loader::Vanilla,
                options,
                findings,
                notes,
            );
            return finish(report, config, home, options, None, None);
        }
    };
    notes.push(format!("loader detected: {}", loader.display_name()));

    // Static mixin analysis needs the parsed metadata and the archive.
    let mixin_static = mixin_analyzer::analyze_static(&parsed, &inspection);
    findings.extend(mixin_static.findings.clone());

    // Additional mods participate in conflict detection.
    let mut conflict_snapshots = Vec::new();
    conflict_snapshots.push(conflict_detector::ModSnapshot::build(
        &parsed.metadata,
        &parsed.mixin_configs,
        &inspection,
    ));
    for extra in &options.extra_mods {
        match extra_snapshot(extra) {
            Ok(snapshot) => conflict_snapshots.push(snapshot),
            Err(error) => findings.push(
                Finding::new(
                    "environment.extra-mod-unreadable",
                    Severity::Warning,
                    FindingCategory::Configuration,
                    format!("could not inspect extra mod `{}`", extra.display()),
                )
                .with_detail(error.to_string()),
            ),
        }
    }
    if conflict_snapshots.len() > 1 {
        let conflicts = conflict_detector::analyze(&conflict_snapshots);
        findings.extend(conflicts.findings.clone());
        notes.push(format!(
            "{} mods compared for conflicts",
            conflict_snapshots.len()
        ));
    }

    // Honour the environment declared by the mod. A client-only mod cannot be
    // loaded by a dedicated server (and vice versa), so running that side would
    // report a failure for something the mod never claimed to support.
    // Both sides are always tested when requested: a mod that only lists a
    // client entrypoint can still be loaded by a dedicated server (or fail to
    // be), and that observation is exactly what a tester should report.
    let run_client = options.side.includes_client();
    let run_server = options.side.includes_server();
    let effective_side = options.side;
    let declared_environment = parsed
        .metadata
        .environment
        .clone()
        .unwrap_or_else(|| "both".to_string());
    if declared_environment != "both" && declared_environment != "*" {
        notes.push(format!(
            "the mod declares environment `{declared_environment}`; MATS still tests every requested side and reports what actually happens"
        ));
    }

    // Resolve the concrete test target.
    let plan_request = PlanRequest {
        loader,
        side: effective_side,
        mc_version: options.mc_version.clone(),
        loader_version: options.loader_version.clone(),
        mc_constraints: parsed.metadata.mc_constraints.clone(),
        allow_snapshots: options.allow_snapshots,
    };
    let plan = match resolve_plan(&manager, &config.network, &plan_request) {
        Ok(plan) => plan,
        Err(error) => {
            findings.push(
                Finding::new(
                    "version.resolution-failed",
                    Severity::Critical,
                    FindingCategory::Loader,
                    "MATS could not resolve a testable Minecraft/loader version",
                )
                .with_detail(format!("{error:#}")),
            );
            let report =
                build_static_report(&inspection, &parsed, loader, options, findings, notes);
            return finish(report, config, home, options, None, None);
        }
    };
    notes.extend(plan.notes.iter().cloned());

    // Required mod dependencies (Fabric API and friends) are downloaded
    // automatically so pointing MATS at one jar is enough.
    let auto_mods = if config.runtime.auto_dependencies {
        resolve_dependencies(
            &manager,
            &config.network,
            &parsed.metadata,
            &plan,
            &options.extra_mods,
            &mut findings,
            &mut notes,
        )
    } else {
        Vec::new()
    };

    if plan.assumed_version {
        findings.push(
            Finding::new(
                "version.assumed",
                Severity::Warning,
                FindingCategory::Loader,
                format!(
                    "the mod declares no usable Minecraft version range; MATS tested Minecraft {}",
                    plan.mc_version
                ),
            )
            .with_detail(format!(
                "candidate versions considered: {}",
                plan.candidates.join(", ")
            )),
        );
    } else if plan.candidates.len() > 1 {
        findings.push(
            Finding::new(
                "version.candidates",
                Severity::Information,
                FindingCategory::Loader,
                format!(
                    "{} Minecraft versions satisfy the declared range",
                    plan.candidates.len()
                ),
            )
            .with_detail(format!(
                "tested {}; candidates: {}",
                plan.mc_version,
                plan.candidates.join(", ")
            )),
        );
    }

    if let Some(java_constraint) = &parsed.metadata.java_constraint {
        if let Some(required) = plan.java_major {
            let predicate = VersionPredicate::parse(java_constraint);
            if predicate.parse_error().is_none()
                && !predicate.matches(&mats_core::McVersion::new(required.to_string()))
            {
                findings.push(
                    Finding::new(
                        "environment.java-requirement-mismatch",
                        Severity::Critical,
                        FindingCategory::Environment,
                        "the mod's declared Java requirement is not satisfied by the runtime MATS selected",
                    )
                    .with_detail(format!(
                        "mod requires Java `{java_constraint}`, Minecraft {} needs Java {required}",
                        plan.mc_version
                    )),
                );
            }
        }
    }
    if let (Some(compiled), Some(required)) = (inspection.java_major_max(), plan.java_major) {
        if compiled > required {
            findings.push(
                Finding::new(
                    "mod.bytecode-newer-than-runtime",
                    Severity::Critical,
                    FindingCategory::Environment,
                    format!(
                        "the mod's newest class file targets Java {compiled}, but the runtime for Minecraft {} is Java {required}",
                        plan.mc_version
                    ),
                )
                .with_detail(
                    "the JVM will throw UnsupportedClassVersionError before the mod can initialise",
                ),
            );
        }
    }

    for warning in &parsed.metadata.warnings {
        findings.push(Finding::new(
            "metadata.warning",
            Severity::Warning,
            FindingCategory::Metadata,
            warning.clone(),
        ));
    }
    let installed_mods: Vec<PathBuf> = options
        .extra_mods
        .iter()
        .cloned()
        .chain(auto_mods.iter().cloned())
        .collect();
    findings.extend(missing_dependency_findings(
        &parsed.metadata,
        &inspection,
        &installed_mods,
    ));

    // Java runtime.
    let java_major = plan.java_major.unwrap_or(21);
    let (java, java_error) = match java_manager::ensure_runtime(home, config, &manager, java_major)
    {
        Ok(runtime) => (Some(runtime), None),
        Err(error) => (None, Some(format!("{error:#}"))),
    };
    if let Some(error) = &java_error {
        findings.push(
            Finding::new(
                "environment.java-unavailable",
                Severity::Critical,
                FindingCategory::Environment,
                format!("Java {java_major} could not be provisioned"),
            )
            .with_detail(error.clone()),
        );
        notes.push("the JVM could not be prepared, so no runtime tests were executed".to_string());
    }

    let mut tests = Vec::new();
    let mut environment = None;
    let mut resources = Vec::new();
    let mut logs = Vec::new();
    let mut metrics = Metrics::default();

    // Each side gets its own environment: the client needs the asset set and a
    // graphical launch, while the server needs its own jar and EULA. Sharing one
    // environment would run the client command for the server side.
    let mut environments: Vec<(Side, PreparedEnvironment)> = Vec::new();
    if let Some(java) = &java {
        let mut sides = Vec::new();
        if run_server {
            sides.push(Side::Server);
        }
        if run_client {
            sides.push(Side::Client);
        }
        for side in sides {
            let spec = EnvironmentSpec {
                mc_version: plan.mc_version.clone(),
                loader,
                loader_version: plan.loader_version.clone(),
                side,
                java_major: java_major,
                mods: std::iter::once(options.jar.clone())
                    .chain(options.extra_mods.iter().cloned())
                    .chain(auto_mods.iter().cloned())
                    .collect(),
                memory_mb: options.memory_mb.unwrap_or(config.runtime.max_heap_mb),
                display: options.display.unwrap_or(config.runtime.display),
                download_assets: options.download_assets || config.runtime.download_assets,
                accept_eula: options.accept_eula,
                fresh: options.fresh,
            };
            match runtime_builder::prepare(home, config, &manager, java, &plan, &spec) {
                Ok(prepared) => {
                    resources.extend(prepared.resources.clone());
                    notes.extend(prepared.notes.clone());
                    environments.push((side, prepared));
                }
                Err(error) => {
                    let message = format!("{error:#}");
                    findings.push(
                        Finding::new(
                            "environment.prepare-failed",
                            if message.contains("--accept-eula") {
                                Severity::Error
                            } else {
                                Severity::Critical
                            },
                            FindingCategory::Environment,
                            format!("the isolated {side} test environment could not be prepared"),
                        )
                        .with_detail(message.clone()),
                    );
                    tests.push(
                        TestOutcome::new("environment", "Environment preparation", side)
                            .status(TestStatus::NotTested)
                            .summary(message),
                    );
                }
            }
        }
    }

    {
        let mod_id = parsed.metadata.id.clone();
        let has_mixins = !parsed.mixin_configs.is_empty();
        for (side, prepared) in &environments {
            let outcome = run_side(
                &manager,
                config,
                options,
                prepared,
                &plan,
                *side,
                java.as_ref(),
                &mod_id,
                has_mixins,
                &declared_environment,
                parsed.metadata.declares_side(*side),
            );
            if metrics.total_duration_ms == 0 {
                metrics = outcome.1.metrics.clone();
            }
            tests.extend(outcome.0);
            findings.extend(outcome.2.clone());
            logs.push(outcome.3);
        }
        if effective_side == Side::Both && environments.len() > 1 {
            findings.push(Finding::new(
                "environment.side-coverage",
                Severity::NotTested,
                FindingCategory::Environment,
                "a dedicated-server pass does not prove client compatibility",
            )
            .with_detail(
                "both sides were exercised only where their tests actually reached a startup marker",
            ));
        }
    }
    environment = environments.first().map(|(_, prepared)| prepared.clone());

    if tests.is_empty() {
        tests.push(
            TestOutcome::new("runtime", "Runtime execution", effective_side)
                .status(TestStatus::NotTested)
                .summary("no runtime test could be executed"),
        );
    }

    // Fold static findings into the per-test findings and dedupe globally.
    let findings = aggregate(findings);
    let summary = RunSummary::from_parts(&tests, &findings);

    let report = Report {
        schema_version: Report::SCHEMA_VERSION,
        generated_at: Utc::now(),
        tool: tool_info(),
        jar: JarFingerprint {
            path: options.jar.display().to_string(),
            sha1: inspection.sha1.clone(),
            sha256: inspection.sha256.clone(),
            size_bytes: inspection.size_bytes,
            entries: inspection.entry_count,
        },
        metadata: Some(parsed.metadata.clone()),
        inspection: inspection.summary(),
        environment: EnvironmentReport {
            mc_version: plan.mc_version.clone(),
            loader,
            loader_version: plan.loader_version.clone(),
            side: effective_side,
            java_major: java
                .as_ref()
                .map(|runtime| runtime.major)
                .or(Some(java_major)),
            java_version: java.as_ref().map(|runtime| runtime.version.clone()),
            java_path: java
                .as_ref()
                .map(|runtime| runtime.path.display().to_string()),
            env_id: environment.as_ref().map(|prepared| prepared.id.clone()),
            env_dir: environment
                .as_ref()
                .map(|prepared| prepared.dir.display().to_string()),
            launch_command: environment
                .as_ref()
                .map(|prepared| prepared.launch.display()),
            jvm_args: environment
                .as_ref()
                .map(|prepared| prepared.launch.args.clone())
                .unwrap_or_default(),
            memory_mb: Some(options.memory_mb.unwrap_or(config.runtime.max_heap_mb)),
            notes: {
                let mut environment_notes = plan.notes.clone();
                if let Some(prepared) = &environment {
                    environment_notes.extend(prepared.notes.clone());
                }
                environment_notes
            },
        },
        resources,
        tests,
        findings,
        metrics,
        summary,
        command: Some(command_line(options)),
        logs_dir: environment
            .as_ref()
            .map(|prepared| prepared.log_dir.display().to_string()),
        notes,
    };

    finish(report, config, home, options, environment, Some(logs))
}

fn determine_loader(inspection: &JarInspection, override_loader: Option<Loader>) -> Result<Loader> {
    if let Some(loader) = override_loader {
        return Ok(loader);
    }
    inspection.primary_loader().with_context(|| {
        format!(
            "no supported loader metadata was found (detected: {})",
            if inspection.loaders_detected.is_empty() {
                "none".to_string()
            } else {
                inspection
                    .loaders_detected
                    .iter()
                    .map(|loader| loader.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            }
        )
    })
}

fn extra_snapshot(path: &Path) -> Result<conflict_detector::ModSnapshot> {
    let inspection = JarInspection::inspect(path)?;
    let parsed = metadata_parser::parse_inspection(&inspection)?;
    Ok(conflict_detector::ModSnapshot::build(
        &parsed.metadata,
        &parsed.mixin_configs,
        &inspection,
    ))
}

fn missing_dependency_findings(
    metadata: &ModMetadata,
    inspection: &JarInspection,
    extra_mods: &[PathBuf],
) -> Vec<Finding> {
    let mut provided: BTreeSet<String> = BTreeSet::from([metadata.id.to_ascii_lowercase()]);
    for nested in &inspection.nested_jars {
        if let Some(id) = &nested.mod_id {
            provided.insert(id.to_ascii_lowercase());
        }
    }
    for extra in extra_mods {
        if let Ok(snapshot) = extra_snapshot(extra) {
            provided.insert(snapshot.mod_id.to_ascii_lowercase());
        }
    }
    let mut findings = Vec::new();
    for dependency in &metadata.depends {
        if !dependency.mandatory || dependency.kind != DependencyKind::Mod {
            continue;
        }
        if dependency
            .side
            .as_deref()
            .is_some_and(|side| side.eq_ignore_ascii_case("CLIENT"))
        {
            continue;
        }
        if provided.contains(&dependency.mod_id.to_ascii_lowercase()) {
            continue;
        }
        findings.push(
            Finding::new(
                "dependency.not-installed",
                Severity::Warning,
                FindingCategory::Dependency,
                format!(
                    "the mod requires `{}`{} which is not installed in the test environment",
                    dependency.mod_id,
                    dependency
                        .constraint
                        .as_ref()
                        .map(|constraint| format!(" {constraint}"))
                        .unwrap_or_default()
                ),
            )
            .with_detail(
                "install it with --with <jar> to test the combination; the loader may refuse to start without it",
            ),
        );
    }
    findings
}

type SideRun = (
    Vec<TestOutcome>,
    ProcessOutcome,
    Vec<Finding>,
    (String, LogAnalysis, CrashDetection, GcStats),
);

#[allow(clippy::too_many_arguments)]
fn run_side(
    _manager: &ResourceManager,
    config: &Config,
    options: &TestOptions,
    prepared: &PreparedEnvironment,
    plan: &ResolvedPlan,
    side: Side,
    _java: Option<&JavaRuntime>,
    mod_id: &str,
    has_mixins: bool,
    declared_environment: &str,
    declared_for_side: bool,
) -> SideRun {
    let timeout = options
        .timeout
        .unwrap_or_else(|| Duration::from_secs(config.runtime.timeout_secs.max(10)));
    let launch_command = &prepared.launch;
    let display = options.display.unwrap_or(config.runtime.display);

    let (program, mut args, env, display_plan) = if side.includes_client() {
        match display_plan(display) {
            DisplayStrategy::Unavailable(reason) => {
                let finding = Finding::new(
                    "graphics.client-not-tested",
                    Severity::NotTested,
                    FindingCategory::Graphics,
                    "the client could not be started because no graphical context is available",
                )
                .with_detail(reason.clone())
                .with_evidence([
                    "dedicated-server success is not evidence of client compatibility".to_string(),
                ]);
                let test = TestOutcome::new("client-startup", "Client startup", side)
                    .status(TestStatus::NotTested)
                    .summary(reason);
                let dummy = ProcessOutcome {
                    label: "client-skipped".into(),
                    exit_code: None,
                    exit_signal: None,
                    timed_out: false,
                    started: false,
                    stopped_after_startup: false,
                    failure_marker_seen: false,
                    markers_seen: Vec::new(),
                    duration_ms: 0,
                    startup_ms: None,
                    stdout_path: PathBuf::new(),
                    stderr_path: PathBuf::new(),
                    log_path: PathBuf::new(),
                    metrics: Metrics::default(),
                    log_tail: String::new(),
                    stdout_tail: String::new(),
                    stderr_tail: String::new(),
                };
                return (
                    vec![test],
                    dummy,
                    vec![finding],
                    (
                        String::new(),
                        LogAnalysis::default(),
                        CrashDetection::default(),
                        GcStats::default(),
                    ),
                );
            }
            DisplayStrategy::Native => (
                launch_command.program.clone(),
                launch_command.args.clone(),
                launch_command.env.clone(),
                "native display",
            ),
            DisplayStrategy::Xvfb => {
                let mut wrapped = vec![
                    "-a".to_string(),
                    "-s".to_string(),
                    "-screen 0 1280x720x24".to_string(),
                    launch_command.program.display().to_string(),
                ];
                wrapped.extend(launch_command.args.clone());
                let mut env = launch_command.env.clone();
                env.push(("LIBGL_ALWAYS_SOFTWARE".into(), "1".into()));
                (
                    PathBuf::from("xvfb-run"),
                    wrapped,
                    env,
                    "Xvfb with software OpenGL",
                )
            }
        }
    } else {
        (
            launch_command.program.clone(),
            launch_command.args.clone(),
            launch_command.env.clone(),
            "not applicable",
        )
    };

    args.extend(config.runtime.extra_jvm_args.iter().cloned());

    let (startup_markers, failure_markers, stop_after, shutdown_commands, shutdown_grace) =
        if side.includes_client() {
            (
                vec![
                    "Backend library: LWJGL".to_string(),
                    "Sound engine started".to_string(),
                    "Created: ".to_string(),
                    "OpenAL initialized".to_string(),
                ],
                vec![
                    "A fatal error has been detected by the Java Runtime Environment".to_string(),
                    "Failed to create window".to_string(),
                    "GLFW error".to_string(),
                ],
                Some(Duration::from_secs(15)),
                Vec::new(),
                Duration::from_secs(10),
            )
        } else {
            (
                vec!["Done (".to_string(), "For help, type".to_string()],
                vec![
                    "A fatal error has been detected by the Java Runtime Environment".to_string(),
                    "Failed to start the minecraft server".to_string(),
                    "Missing or unsupported mandatory dependencies".to_string(),
                ],
                Some(Duration::from_secs(3)),
                vec!["stop".to_string()],
                Duration::from_secs(45),
            )
        };

    let label = format!("{}-{}", side.as_str(), prepared.id);
    let spec = LaunchSpec {
        program,
        args,
        cwd: launch_command.cwd.clone(),
        env,
        timeout,
        log_dir: prepared.log_dir.clone(),
        label: label.clone(),
        startup_markers,
        failure_markers,
        stop_after_startup: stop_after,
        shutdown_commands,
        shutdown_grace,
        stdin_script: None,
    };

    tracing::info!(
        side = side.as_str(),
        mc = %plan.mc_version,
        loader = plan.loader.as_str(),
        display = display_plan,
        "starting real Minecraft runtime"
    );

    let outcome = match launch(&spec) {
        Ok(outcome) => outcome,
        Err(error) => {
            let finding = Finding::new(
                "runtime.launch-failed",
                Severity::Critical,
                FindingCategory::Runtime,
                format!("the {} process could not be started", side),
            )
            .with_detail(format!("{error:#}"));
            return (
                vec![
                    TestOutcome::new(
                        format!("{}-startup", side.as_str()),
                        format!("{} startup", side),
                        side,
                    )
                    .status(TestStatus::NotTested)
                    .summary(format!("launch failed: {error}")),
                ],
                ProcessOutcome {
                    label: label.clone(),
                    exit_code: None,
                    exit_signal: None,
                    timed_out: false,
                    started: false,
                    stopped_after_startup: false,
                    failure_marker_seen: false,
                    markers_seen: Vec::new(),
                    duration_ms: 0,
                    startup_ms: None,
                    stdout_path: PathBuf::new(),
                    stderr_path: PathBuf::new(),
                    log_path: PathBuf::new(),
                    metrics: Metrics::default(),
                    log_tail: String::new(),
                    stdout_tail: String::new(),
                    stderr_tail: String::new(),
                },
                vec![finding],
                (
                    label,
                    LogAnalysis::default(),
                    CrashDetection::default(),
                    GcStats::default(),
                ),
            );
        }
    };

    let log_text = std::fs::read_to_string(&outcome.log_path).unwrap_or_default();
    let analysis = log_analyzer::analyze(&log_text);
    let mixin_findings = mixin_analyzer::analyze_output(&log_text);
    let gc = analyze_gc_log(&prepared.log_dir.join("gc.log")).unwrap_or_default();
    let server_root = prepared.dir.join("server");
    let crash = crash_detector::detect(
        if side.includes_server() && server_root.is_dir() {
            &server_root
        } else {
            &prepared.dir
        },
        &log_text,
        outcome.exit_code,
        outcome.timed_out,
        outcome.started,
    );

    let graphics_failure = side.includes_client() && is_graphics_failure(&log_text);
    let mut findings = Vec::new();
    findings.extend(analysis.findings.clone());
    findings.extend(mixin_findings);
    findings.extend(crash.findings.clone());

    let mut tests = Vec::new();
    let graphics_missing = graphics_failure && !outcome.started;

    let startup = TestOutcome::new(
        format!("{}-startup", side.as_str()),
        format!("{} startup", side),
        side,
    )
    .duration(outcome.startup_ms.unwrap_or(outcome.duration_ms));

    let startup = if graphics_missing {
        findings.push(
            Finding::new(
                "graphics.unavailable",
                Severity::NotTested,
                FindingCategory::Graphics,
                "the client could not create a graphical window in this environment",
            )
            .with_detail(
                "no display/GPU is available. Client behaviour is NOT tested; a server pass does not prove client compatibility.",
            )
            .with_evidence(tail_lines(&outcome.log_tail, 6)),
        );
        startup
            .status(TestStatus::NotTested)
            .summary("graphical initialisation is unavailable in this environment")
    } else if outcome.timed_out {
        startup
            .status(TestStatus::Failed)
            .summary(format!(
                "did not reach startup within {} seconds",
                timeout.as_secs()
            ))
            .finding(Finding::new(
                "runtime.timeout",
                Severity::Error,
                FindingCategory::Runtime,
                "the test exceeded its timeout",
            ))
    } else if outcome.started {
        startup.status(TestStatus::Passed).summary(format!(
            "reached startup in {}",
            outcome
                .startup_ms
                .map(fsutil::human_duration)
                .unwrap_or_else(|| "unknown time".to_string())
        ))
    } else {
        startup
            .status(TestStatus::Failed)
            .summary(match outcome.exit_code {
                Some(code) => format!("exited with code {code} before startup completed"),
                None => "terminated before startup completed".to_string(),
            })
            .finding(Finding::new(
                "runtime.startup-incomplete",
                Severity::Critical,
                FindingCategory::Runtime,
                "Minecraft exited before the startup marker was reached",
            ))
    };
    tests.push(startup);

    // Mod discovery.
    let discovery = &analysis.discovery;
    let discovered = discovery
        .mods
        .iter()
        .any(|(id, _)| id.eq_ignore_ascii_case(mod_id));
    let discovery_test = if graphics_missing {
        TestOutcome::new("mod-discovery", "Mod discovery", side)
            .status(TestStatus::NotTested)
            .summary("client never started")
    } else if !discovered && !declared_for_side {
        findings.push(
            Finding::new(
                "environment.side-not-declared",
                Severity::NotTested,
                FindingCategory::Environment,
                format!(
                    "the mod declares environment `{declared_environment}`, so the {side} side does not load it"
                ),
            )
            .with_detail(
                "this is expected for a single-side mod; the other side's result is the meaningful one",
            ),
        );
        TestOutcome::new("mod-discovery", "Mod discovery", side)
            .status(TestStatus::NotTested)
            .summary(format!(
                "the mod declares environment `{declared_environment}`; it is not loaded on the {side} side"
            ))
    } else if discovery.mods.is_empty() {
        TestOutcome::new("mod-discovery", "Mod discovery", side)
            .status(TestStatus::NotTested)
            .summary("the loader did not print a machine-readable mod list")
            .detail(format!("{} log line(s) analysed", analysis.line_count))
    } else if discovered {
        TestOutcome::new("mod-discovery", "Mod discovery", side)
            .status(TestStatus::Passed)
            .summary(format!(
                "the loader listed {} mod(s) including the tested mod",
                discovery.mods.len()
            ))
            .detail(format!(
                "mods: {}",
                discovery
                    .mods
                    .iter()
                    .map(|(id, version)| format!("{id} {version}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
    } else {
        TestOutcome::new("mod-discovery", "Mod discovery", side)
            .status(TestStatus::Failed)
            .summary("the tested mod is missing from the loader's mod list")
            .detail(format!(
                "listed: {}",
                discovery
                    .mods
                    .iter()
                    .map(|(id, _)| id.clone())
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
    };
    tests.push(discovery_test);

    // Mod initialisation.
    let runtime_problems = findings
        .iter()
        .filter(|finding| {
            matches!(
                finding.category,
                FindingCategory::Runtime
                    | FindingCategory::Dependency
                    | FindingCategory::Mixin
                    | FindingCategory::Classpath
            ) && matches!(finding.severity, Severity::Critical | Severity::Error)
        })
        .count();
    let init_test = if graphics_missing {
        TestOutcome::new("mod-init", "Mod initialisation", side)
            .status(TestStatus::NotTested)
            .summary("client never started")
    } else if !outcome.started {
        TestOutcome::new("mod-init", "Mod initialisation", side)
            .status(TestStatus::NotTested)
            .summary("startup did not complete, so initialisation could not be observed")
    } else if runtime_problems > 0 {
        TestOutcome::new("mod-init", "Mod initialisation", side)
            .status(TestStatus::Failed)
            .summary(format!(
                "{runtime_problems} runtime problem(s) were reported while the mod initialised"
            ))
    } else {
        TestOutcome::new("mod-init", "Mod initialisation", side)
            .status(TestStatus::Passed)
            .summary("no initialisation errors were reported")
    };
    tests.push(init_test);

    // Mixin application.
    let mixin_failures = findings
        .iter()
        .filter(|finding| finding.category == FindingCategory::Mixin)
        .count();
    let mixin_test = if graphics_missing {
        TestOutcome::new("mixin", "Mixin application", side)
            .status(TestStatus::NotTested)
            .summary("client never started")
    } else if !has_mixins {
        TestOutcome::new("mixin", "Mixin application", side)
            .status(TestStatus::NotTested)
            .summary("the mod declares no mixin configuration")
    } else if !outcome.started {
        TestOutcome::new("mixin", "Mixin application", side)
            .status(TestStatus::NotTested)
            .summary("startup did not complete, so mixin application could not be confirmed")
    } else if mixin_failures > 0 {
        TestOutcome::new("mixin", "Mixin application", side)
            .status(TestStatus::Failed)
            .summary(format!("{mixin_failures} mixin problem(s) were reported"))
    } else {
        TestOutcome::new("mixin", "Mixin application", side)
            .status(TestStatus::Passed)
            .summary("all declared mixins were applied without errors")
    };
    tests.push(mixin_test);

    // Uncaught runtime errors.
    let exceptions = analysis
        .exceptions
        .iter()
        .filter(|record| !record.class.contains("Mixin") || !record.class.contains("spongepowered"))
        .count();
    let runtime_test = if graphics_missing {
        TestOutcome::new("runtime", "Runtime errors", side)
            .status(TestStatus::NotTested)
            .summary("client never started")
    } else if outcome.stopped_after_startup {
        TestOutcome::new("runtime", "Runtime errors", side)
            .status(TestStatus::Passed)
            .summary("the process was shut down cleanly after the startup phase")
    } else if crash.crashed || outcome.exit_code.is_some_and(|code| code != 0) {
        TestOutcome::new("runtime", "Runtime errors", side)
            .status(TestStatus::Failed)
            .summary(
                crash
                    .summary
                    .clone()
                    .unwrap_or_else(|| "the process terminated abnormally".to_string()),
            )
            .detail(format!("detected: {}", crash.kind().as_str()))
    } else if exceptions > 0 {
        TestOutcome::new("runtime", "Runtime errors", side)
            .status(TestStatus::Failed)
            .summary(format!("{exceptions} exception(s) were reported"))
    } else {
        TestOutcome::new("runtime", "Runtime errors", side)
            .status(TestStatus::Passed)
            .summary("no uncaught exceptions or crashes were reported")
    };
    tests.push(runtime_test);

    // Keep the raw log data for the caller.
    let mut metrics = outcome.metrics.clone();
    apply_gc_stats(&mut metrics, &gc);
    let record = (label, analysis, crash, gc);
    (tests, outcome, findings, record)
}

fn is_graphics_failure(log: &str) -> bool {
    let lower = log.to_ascii_lowercase();
    [
        "glfw error",
        "failed to create window",
        "could not initialize glx",
        "no display",
        "display is not available",
        "x11: the display",
        "org.lwjgl.glfw.glfwinit",
        "headlessexception",
        "unable to open display",
    ]
    .iter()
    .any(|pattern| lower.contains(pattern))
}

fn tail_lines(text: &str, count: usize) -> Vec<String> {
    text.lines()
        .rev()
        .take(count)
        .map(|line| line.to_string())
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect()
}

enum DisplayStrategy {
    Native,
    Xvfb,
    Unavailable(String),
}

fn display_plan(mode: DisplayMode) -> DisplayStrategy {
    match mode {
        DisplayMode::Disabled => {
            return DisplayStrategy::Unavailable(
                "client testing is disabled (--display disabled)".into(),
            );
        }
        DisplayMode::Native => {
            if cfg!(target_os = "linux") && !has_native_display() {
                return DisplayStrategy::Unavailable(
                    "no DISPLAY/WAYLAND_DISPLAY is set and native display mode was requested"
                        .into(),
                );
            }
            return DisplayStrategy::Native;
        }
        DisplayMode::Virtual => {
            if !cfg!(target_os = "linux") {
                return DisplayStrategy::Unavailable(
                    "virtual displays are only supported on Linux (Xvfb)".into(),
                );
            }
            if which("xvfb-run").is_none() {
                return DisplayStrategy::Unavailable(
                    "Xvfb is not installed; install xvfb to run graphical client tests headlessly"
                        .into(),
                );
            }
            return DisplayStrategy::Xvfb;
        }
        DisplayMode::Auto => {}
    }
    if cfg!(target_os = "linux") {
        if has_native_display() {
            DisplayStrategy::Native
        } else if which("xvfb-run").is_some() {
            DisplayStrategy::Xvfb
        } else {
            DisplayStrategy::Unavailable(
                "no graphical display or Xvfb is available; client tests are reported as not tested"
                    .into(),
            )
        }
    } else if cfg!(target_os = "macos") {
        // A CI macOS runner has a window server session but no usable GPU in
        // many configurations; try natively and let the launcher report it.
        DisplayStrategy::Native
    } else {
        DisplayStrategy::Native
    }
}

fn has_native_display() -> bool {
    std::env::var_os("DISPLAY").is_some() || std::env::var_os("WAYLAND_DISPLAY").is_some()
}

fn which(program: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(program);
        if candidate.is_file() {
            return Some(candidate);
        }
        #[cfg(windows)]
        {
            let candidate = dir.join(format!("{program}.exe"));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

fn tool_info() -> ToolInfo {
    ToolInfo {
        name: "mats".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        os: std::env::consts::OS.into(),
        arch: std::env::consts::ARCH.into(),
        features: Vec::new(),
    }
}

fn command_line(options: &TestOptions) -> String {
    let mut parts = vec![
        "mats".to_string(),
        "test".to_string(),
        options.jar.display().to_string(),
    ];
    if let Some(mc) = &options.mc_version {
        parts.push("--mc".into());
        parts.push(mc.clone());
    }
    if let Some(loader) = options.loader {
        parts.push("--loader".into());
        parts.push(loader.as_str().into());
    }
    parts.push("--side".into());
    parts.push(options.side.as_str().into());
    if options.accept_eula {
        parts.push("--accept-eula".into());
    }
    for extra in &options.extra_mods {
        parts.push("--with".into());
        parts.push(extra.display().to_string());
    }
    parts.join(" ")
}

fn build_static_report(
    inspection: &JarInspection,
    parsed: &ParsedMod,
    loader: Loader,
    options: &TestOptions,
    findings: Vec<Finding>,
    notes: Vec<String>,
) -> Report {
    let findings = aggregate(findings);
    let tests = vec![
        TestOutcome::new("runtime", "Runtime execution", options.side)
            .status(TestStatus::NotTested)
            .summary("no runtime test could be executed; see diagnostics"),
    ];
    let summary = RunSummary::from_parts(&tests, &findings);
    Report {
        schema_version: Report::SCHEMA_VERSION,
        generated_at: Utc::now(),
        tool: tool_info(),
        jar: JarFingerprint {
            path: options.jar.display().to_string(),
            sha1: inspection.sha1.clone(),
            sha256: inspection.sha256.clone(),
            size_bytes: inspection.size_bytes,
            entries: inspection.entry_count,
        },
        metadata: Some(parsed.metadata.clone()),
        inspection: inspection.summary(),
        environment: EnvironmentReport {
            mc_version: options
                .mc_version
                .clone()
                .unwrap_or_else(|| "unresolved".into()),
            loader,
            loader_version: options.loader_version.clone(),
            side: options.side,
            java_major: None,
            java_version: None,
            java_path: None,
            env_id: None,
            env_dir: None,
            launch_command: None,
            jvm_args: Vec::new(),
            memory_mb: None,
            notes: Vec::new(),
        },
        resources: Vec::new(),
        tests,
        findings,
        metrics: Metrics::default(),
        summary,
        command: Some(command_line(options)),
        logs_dir: None,
        notes,
    }
}

fn finish(
    report: Report,
    config: &Config,
    home: &MatsHome,
    options: &TestOptions,
    environment: Option<PreparedEnvironment>,
    _logs: Option<Vec<(String, LogAnalysis, CrashDetection, GcStats)>>,
) -> Result<RunOutcome> {
    let output_root = config.reports_dir(home);
    std::fs::create_dir_all(&output_root)
        .with_context(|| format!("creating report directory {}", output_root.display()))?;
    let logs_source = environment
        .as_ref()
        .map(|prepared| prepared.log_dir.clone());
    let report_paths = write_report(&report, &output_root, logs_source.as_deref())?;

    let exit_code = exit_code_for(&report);
    let record = RunRecord {
        id: report_paths
            .dir
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "run".into()),
        timestamp: report.generated_at,
        jar_path: report.jar.path.clone(),
        jar_sha1: report.jar.sha1.clone(),
        mod_id: report.metadata.as_ref().map(|metadata| metadata.id.clone()),
        mod_version: report
            .metadata
            .as_ref()
            .map(|metadata| metadata.version.clone()),
        mc_version: report.environment.mc_version.clone(),
        loader: report.environment.loader,
        loader_version: report.environment.loader_version.clone(),
        side: report.environment.side,
        verdict: report.summary.verdict(),
        passed: report.summary.passed,
        failed: report.summary.failed,
        skipped: report.summary.skipped,
        not_tested: report.summary.not_tested,
        critical: report.summary.critical,
        errors: report.summary.errors,
        warnings: report.summary.warnings,
        duration_ms: report.metrics.total_duration_ms,
        report_dir: Some(report_paths.dir.display().to_string()),
    };
    if let Err(error) = storage::append_history(home, &record) {
        tracing::warn!("could not append to run history: {error:#}");
    }
    let _ = options;

    Ok(RunOutcome {
        report,
        report_paths: Some(report_paths),
        exit_code,
    })
}

/// Severity counts for CLI summaries.
pub fn summarize_findings(report: &Report) -> SeverityCounts {
    count_severities(&report.findings)
}

/// `true` when the run produced a report worth showing.
pub fn report_has_runtime_tests(report: &Report) -> bool {
    report
        .tests
        .iter()
        .any(|test| test.status != TestStatus::NotTested)
}

/// Download required mod dependencies that the user did not supply.
///
/// Dependencies are resolved through Modrinth's public API by project slug
/// (`fabric-api`, `architectury-api`, ...). A dependency that cannot be found
/// is reported as a warning and left to the user: MATS never guesses a
/// substitute mod.
fn resolve_dependencies(
    manager: &ResourceManager,
    network: &NetworkConfig,
    metadata: &ModMetadata,
    plan: &ResolvedPlan,
    extra_mods: &[PathBuf],
    findings: &mut Vec<Finding>,
    notes: &mut Vec<String>,
) -> Vec<PathBuf> {
    let loader_slug = match plan.loader {
        Loader::Fabric => "fabric",
        Loader::Quilt => "quilt",
        Loader::Forge => "forge",
        Loader::NeoForge => "neoforge",
        Loader::Vanilla => return Vec::new(),
    };

    let mut present: BTreeSet<String> = BTreeSet::new();
    for path in extra_mods {
        if let Ok(inspection) = JarInspection::inspect(path) {
            if let Ok(parsed) = metadata_parser::parse_inspection(&inspection) {
                present.insert(parsed.metadata.id.to_ascii_lowercase());
            }
        }
    }

    let mut downloaded = Vec::new();
    for dependency in &metadata.depends {
        if !dependency.mandatory || dependency.kind != DependencyKind::Mod {
            continue;
        }
        let id = dependency.mod_id.to_ascii_lowercase();
        if id == metadata.id.to_ascii_lowercase() || present.contains(&id) {
            continue;
        }
        let url = format!(
            "{}/v2/project/{id}/version?loaders=%5B%22{loader_slug}%22%5D&game_versions=%5B%22{}%22%5D",
            network.modrinth_api.trim_end_matches('/'),
            plan.mc_version
        );
        let cache_key = format!("modrinth-{id}-{loader_slug}-{}", plan.mc_version);
        let versions = match manager.fetch_json(&url, &cache_key, Duration::from_secs(3600)) {
            Ok(value) => value,
            Err(error) => {
                notes.push(format!(
                    "could not look up dependency `{id}` on Modrinth; add it with --with if the mod needs it"
                ));
                findings.push(
                    Finding::new(
                        "dependency.lookup-failed",
                        Severity::Warning,
                        FindingCategory::Configuration,
                        format!("required dependency `{id}` could not be resolved automatically"),
                    )
                    .with_detail(format!("{error:#}")),
                );
                continue;
            }
        };
        let file = versions.as_array().and_then(|versions| {
            versions.iter().find_map(|version| {
                let files = version.get("files")?.as_array()?;
                files
                    .iter()
                    .find(|file| {
                        file.get("primary")
                            .and_then(|primary| primary.as_bool())
                            .unwrap_or(false)
                    })
                    .or_else(|| files.first())
                    .cloned()
            })
        });
        let Some(file) = file else {
            findings.push(Finding::new(
                "dependency.no-build",
                Severity::Warning,
                FindingCategory::Configuration,
                format!(
                    "no `{id}` build exists for Minecraft {} on {}",
                    plan.mc_version,
                    plan.loader.display_name()
                ),
            ));
            continue;
        };
        let Some(file_url) = file.get("url").and_then(|value| value.as_str()) else {
            continue;
        };
        let file_name = file
            .get("filename")
            .and_then(|value| value.as_str())
            .unwrap_or("dependency.jar")
            .to_string();
        let sha1 = file
            .get("hashes")
            .and_then(|hashes| hashes.get("sha1"))
            .and_then(|value| value.as_str())
            .map(|value| value.to_string());
        let size = file.get("size").and_then(|value| value.as_u64());
        let request = ResourceRequest::new(
            format!("dependency-{id}"),
            ResourceKind::Mod,
            file_url.to_string(),
        )
        .with_sha1(sha1)
        .with_size(size);
        match manager.fetch(&request) {
            Ok(fetched) => {
                notes.push(format!(
                    "downloaded required dependency `{id}` ({file_name}) from Modrinth"
                ));
                downloaded.push(fetched.path.clone());
            }
            Err(error) => findings.push(
                Finding::new(
                    "dependency.download-failed",
                    Severity::Warning,
                    FindingCategory::Configuration,
                    format!("required dependency `{id}` could not be downloaded"),
                )
                .with_detail(format!("{error:#}")),
            ),
        }
    }
    downloaded
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_codes_reflect_the_verdict() {
        let mut report = Report {
            schema_version: 1,
            generated_at: Utc::now(),
            tool: tool_info(),
            jar: JarFingerprint {
                path: "m.jar".into(),
                sha1: "a".repeat(40),
                sha256: "b".repeat(64),
                size_bytes: 1,
                entries: 1,
            },
            metadata: None,
            inspection: Default::default(),
            environment: EnvironmentReport {
                mc_version: "26.3".into(),
                loader: Loader::Fabric,
                loader_version: None,
                side: Side::Server,
                java_major: Some(25),
                java_version: None,
                java_path: None,
                env_id: None,
                env_dir: None,
                launch_command: None,
                jvm_args: Vec::new(),
                memory_mb: None,
                notes: Vec::new(),
            },
            resources: Vec::new(),
            tests: vec![TestOutcome::new("a", "A", Side::Server).status(TestStatus::Passed)],
            findings: Vec::new(),
            metrics: Metrics::default(),
            summary: RunSummary::default(),
            command: None,
            logs_dir: None,
            notes: Vec::new(),
        };
        report.summary = RunSummary::from_parts(&report.tests, &report.findings);
        assert_eq!(exit_code_for(&report), 0);
        report.tests[0] = TestOutcome::new("a", "A", Side::Server).status(TestStatus::Failed);
        report.summary = RunSummary::from_parts(&report.tests, &report.findings);
        assert_eq!(exit_code_for(&report), 1);
        report.tests.clear();
        report.summary = RunSummary::from_parts(&report.tests, &report.findings);
        assert_eq!(exit_code_for(&report), 1);
    }

    #[test]
    fn detects_graphics_failures() {
        assert!(is_graphics_failure(
            "GLFW error 65542: WGL: The driver does not appear to support OpenGL"
        ));
        assert!(is_graphics_failure("java.awt.HeadlessException"));
        assert!(!is_graphics_failure("Done (2.1s)! For help, type \"help\""));
    }

    #[test]
    fn display_disabled_is_reported_as_unavailable() {
        assert!(matches!(
            display_plan(DisplayMode::Disabled),
            DisplayStrategy::Unavailable(_)
        ));
    }

    #[test]
    fn missing_dependencies_are_flagged_without_claiming_a_failure() {
        let metadata = ModMetadata {
            id: "example".into(),
            depends: vec![mats_core::types::DependencySpec {
                mod_id: "fabric-api".into(),
                kind: DependencyKind::Mod,
                constraint: Some(">=0.100.0".into()),
                mandatory: true,
                side: None,
                ordering: None,
                reason: None,
            }],
            ..ModMetadata::default()
        };
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("mod.jar");
        jar_inspector::write_jar(
            &path,
            &[(
                "fabric.mod.json",
                br#"{"schemaVersion":1,"id":"example","version":"1"}"#,
            )],
        )
        .unwrap();
        let inspection = JarInspection::inspect(&path).unwrap();
        let findings = missing_dependency_findings(&metadata, &inspection, &[]);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].severity, Severity::Warning);
        assert!(findings[0].message.contains("fabric-api"));
    }

    #[test]
    fn command_line_is_reproducible() {
        let mut options = TestOptions::new("mod.jar");
        options.mc_version = Some("26.3".into());
        options.loader = Some(Loader::Fabric);
        options.accept_eula = true;
        let command = command_line(&options);
        assert!(command.contains("mats test mod.jar"));
        assert!(command.contains("--mc 26.3"));
        assert!(command.contains("--accept-eula"));
    }
}
