//! Implementations of every MATS command.

use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::Parser;
use mats_core::config::{Config, DisplayMode, UpdateChannel};
use mats_core::types::{Finding, FindingCategory, Loader, Severity, Side, TestStatus, ToolInfo};
use mats_core::{MatsHome, fsutil};
use resource_manager::{ResourceManager, stderr_reporter};
use test_runner::TestOptions;

use crate::cli::{CacheAction, Cli, Command, EnvAction, TestArgs, UpdateAction, UpdateArgs};

pub fn dispatch(cli: Cli) -> Result<i32> {
    let Cli {
        home,
        config,
        verbose: _,
        quiet,
        json,
        no_update_check,
        command,
    } = cli;

    let home = match home {
        Some(path) => MatsHome::new(path),
        None => MatsHome::discover()?,
    };
    home.ensure_layout()?;
    let mut config = match &config {
        Some(path) => mats_core::config::load_from_path(path)?,
        None => Config::load(&home)?,
    };

    let code = match command {
        Command::Test(args) => run_test(&home, &config, &args, json, quiet)?,
        Command::Inspect { jar } => run_inspect(&jar, json)?,
        Command::Analyze { jar, extra_mods } => run_analyze(&jar, &extra_mods, json)?,
        Command::Matrix {
            jar,
            mc,
            loader,
            limit,
            allow_snapshots,
        } => run_matrix(
            &home,
            &config,
            &jar,
            mc.as_deref(),
            loader.as_deref(),
            limit,
            allow_snapshots,
            json,
        )?,
        Command::Versions { loader, limit } => {
            run_versions(&home, &config, loader.as_deref(), limit, json)?
        }
        Command::Doctor => run_doctor(&home, &config, json)?,
        Command::Cache { action } => run_cache(&home, &config, action, quiet)?,
        Command::Env { action } => run_env(&home, action, quiet)?,
        Command::History { limit } => run_history(&home, limit, json)?,
        Command::Report { path } => run_report(&home, path, json)?,
        Command::Version => {
            if json {
                print_json(&serde_json::json!({
                    "name": "mats",
                    "version": env!("CARGO_PKG_VERSION"),
                    "os": std::env::consts::OS,
                    "arch": std::env::consts::ARCH,
                    "home": home.root().display().to_string(),
                }))?;
            } else {
                println!(
                    "MATS {} ({} {})",
                    env!("CARGO_PKG_VERSION"),
                    std::env::consts::OS,
                    std::env::consts::ARCH
                );
            }
            0
        }
        Command::Update(args) => {
            if let Some(channel) = &args.channel {
                config.updates.channel = channel
                    .parse::<UpdateChannel>()
                    .map_err(|error| anyhow::anyhow!(error))?;
            }
            run_update(&home, &config, &args, json)?
        }
    };

    if !no_update_check {
        maybe_notify_update(&home, &config, quiet);
    }
    Ok(code)
}

fn manager_for(home: &MatsHome, config: &Config, quiet: bool) -> Result<ResourceManager> {
    let manager = ResourceManager::new(home, config)?;
    Ok(if quiet {
        manager
    } else {
        manager.with_reporter(stderr_reporter())
    })
}

// ---------------------------------------------------------------------------
// test
// ---------------------------------------------------------------------------

fn run_test(
    home: &MatsHome,
    config: &Config,
    args: &TestArgs,
    json: bool,
    quiet: bool,
) -> Result<i32> {
    let mut config = config.clone();
    if args.offline {
        config.runtime.offline = true;
    }
    if args.no_deps {
        config.runtime.auto_dependencies = false;
    }
    if let Some(dir) = &args.report_dir {
        config.reports.dir = Some(dir.display().to_string());
    }
    let manager = manager_for(home, &config, quiet)?;

    let side = match &args.side {
        Some(value) => value
            .parse::<Side>()
            .map_err(|error| anyhow::anyhow!(error))?,
        None => Side::Both,
    };
    let loader = match &args.loader {
        Some(value) => Some(
            value
                .parse::<Loader>()
                .map_err(|error| anyhow::anyhow!(error))?,
        ),
        None => None,
    };
    let display = match &args.display {
        Some(value) => Some(
            value
                .parse::<DisplayMode>()
                .map_err(|error| anyhow::anyhow!(error))?,
        ),
        None => None,
    };

    let options = TestOptions {
        jar: args.jar.clone(),
        extra_mods: args.extra_mods.clone(),
        side,
        mc_version: args.mc.clone(),
        loader,
        loader_version: args.loader_version.clone(),
        timeout: args.timeout.map(Duration::from_secs),
        memory_mb: args.memory,
        accept_eula: args.accept_eula,
        offline: args.offline,
        fresh: args.fresh,
        download_assets: args.download_assets,
        display,
        allow_snapshots: args.allow_snapshots,
        keep_environments: true,
    };

    if !quiet && !json {
        println!(
            "MATS {} — {}",
            env!("CARGO_PKG_VERSION"),
            args.jar.display()
        );
        println!("  using home: {}", home.root().display());
        println!("  side: {side}");
        if let Some(mc) = &args.mc {
            println!("  Minecraft override: {mc}");
        }
        println!();
    }

    let progress = if quiet || json {
        None
    } else {
        Some(stderr_reporter())
    };
    let outcome = test_runner::run(&manager, &config, home, &options, progress)?;

    if json {
        print_json(&outcome.report)?;
        return Ok(outcome.exit_code);
    }

    let report = &outcome.report;
    println!();
    print_separator("Run summary");
    println!(
        "  Mod:        {} {}",
        report
            .metadata
            .as_ref()
            .map(|metadata| metadata.name.as_str())
            .unwrap_or("(unknown)"),
        report
            .metadata
            .as_ref()
            .map(|metadata| metadata.version.as_str())
            .unwrap_or("")
    );
    println!(
        "  Target:     Minecraft {} · {} {} · {}",
        report.environment.mc_version,
        report.environment.loader.display_name(),
        report
            .environment
            .loader_version
            .as_deref()
            .unwrap_or("n/a"),
        report.environment.side
    );
    println!(
        "  Java:       {}",
        report
            .environment
            .java_version
            .clone()
            .map(|version| format!("Java {version}"))
            .unwrap_or_else(|| "not provisioned".to_string())
    );
    println!(
        "  Verdict:    {}",
        report.summary.verdict().as_str().to_uppercase()
    );
    println!();

    if !report.tests.is_empty() {
        print_separator("Tests");
        for test in &report.tests {
            println!(
                "  {:<10} {:<24} {}",
                status_label(test.status),
                test.name,
                test.summary.as_deref().unwrap_or("")
            );
        }
        println!();
    }

    if !report.findings.is_empty() {
        print_separator("Diagnostics");
        for severity in [Severity::Critical, Severity::Error, Severity::Warning] {
            let group: Vec<&Finding> = report
                .findings
                .iter()
                .filter(|finding| finding.severity == severity)
                .collect();
            for finding in group.iter().take(8) {
                println!(
                    "  {:<8} {:<32} {}",
                    severity.label(),
                    finding.code,
                    finding.message
                );
                if let Some(detail) = &finding.detail {
                    println!("           {detail}");
                }
            }
            if group.len() > 8 {
                println!(
                    "           … {} more {severity} finding(s)",
                    group.len() - 8
                );
            }
        }
        println!();
    }

    if let Some(paths) = &outcome.report_paths {
        print_separator("Reports");
        println!("  {}", paths.report_md.display());
        println!("  {}", paths.report_json.display());
        println!("  {}", paths.dir.display());
        println!();
    }
    println!(
        "Verdict: {} — {} test(s) passed, {} failed, {} not tested.",
        report.summary.verdict().as_str().to_uppercase(),
        report.summary.passed,
        report.summary.failed,
        report.summary.not_tested
    );
    Ok(outcome.exit_code)
}

// ---------------------------------------------------------------------------
// inspect / analyze / matrix / versions
// ---------------------------------------------------------------------------

fn run_inspect(jar: &Path, json: bool) -> Result<i32> {
    let inspection = jar_inspector::JarInspection::inspect(jar)?;
    let parsed = metadata_parser::parse_inspection(&inspection);
    if json {
        let document = serde_json::json!({
            "inspection": inspection,
            "metadata": parsed.as_ref().ok().map(|parsed| &parsed.metadata),
            "metadata_error": parsed.as_ref().err().map(|error| format!("{error:#}")),
            "mixin_configs": parsed.as_ref().ok().map(|parsed| &parsed.mixin_configs),
        });
        print_json(&document)?;
        return Ok(0);
    }
    print_separator("JAR");
    println!("  File:        {}", inspection.path.display());
    println!(
        "  Size:        {}",
        fsutil::human_bytes(inspection.size_bytes)
    );
    println!("  SHA-1:       {}", inspection.sha1);
    println!("  SHA-256:     {}", inspection.sha256);
    println!("  Entries:     {}", inspection.entry_count);
    println!(
        "  Loader(s):   {}",
        if inspection.loaders_detected.is_empty() {
            "none detected".to_string()
        } else {
            inspection
                .loaders_detected
                .iter()
                .map(|loader| loader.display_name())
                .collect::<Vec<_>>()
                .join(", ")
        }
    );
    for evidence in &inspection.loader_evidence {
        println!("               · {evidence}");
    }
    println!(
        "  Classes:     {} (Java {})",
        inspection.bytecode.class_count,
        inspection
            .java_major_max()
            .map(|major| major.to_string())
            .unwrap_or_else(|| "?".into())
    );
    println!(
        "  Mixins:      {} config(s), {} access widener(s)",
        inspection.mixin_configs.len(),
        inspection.access_wideners.len()
    );
    println!("  Nested JARs: {}", inspection.nested_jars.len());
    if !inspection.warnings.is_empty() {
        println!();
        print_separator("Warnings");
        for warning in &inspection.warnings {
            println!("  - {warning}");
        }
    }
    println!();
    match parsed {
        Ok(parsed) => {
            print_separator("Metadata");
            print_metadata(&parsed.metadata);
            if !parsed.mixin_configs.is_empty() {
                println!();
                print_separator("Mixin configurations");
                for config in &parsed.mixin_configs {
                    println!(
                        "  {} — {} mixin(s){}",
                        config.path,
                        config.mixin_count(),
                        config
                            .compatibility_level
                            .as_ref()
                            .map(|level| format!(", compatibility {level}"))
                            .unwrap_or_default()
                    );
                }
            }
        }
        Err(error) => {
            println!();
            println!("  No loader metadata could be parsed: {error:#}");
        }
    }
    Ok(0)
}

fn print_metadata(metadata: &mats_core::types::ModMetadata) {
    println!("  Mod id:      {}", metadata.id);
    println!("  Name:        {}", metadata.name);
    println!("  Version:     {}", metadata.version);
    println!(
        "  Loader:      {}",
        metadata
            .loader
            .map(|loader| loader.display_name().to_string())
            .unwrap_or_else(|| "unknown".into())
    );
    if !metadata.authors.is_empty() {
        println!("  Authors:     {}", metadata.authors.join(", "));
    }
    if let Some(license) = &metadata.license {
        println!("  License:     {license}");
    }
    if let Some(environment) = &metadata.environment {
        println!("  Environment: {environment}");
    }
    if !metadata.mc_constraints.is_empty() {
        println!(
            "  Minecraft:   {}",
            metadata.mc_constraint_joined().unwrap_or_default()
        );
    } else {
        println!("  Minecraft:   (no constraint declared)");
    }
    if let Some(java) = &metadata.java_constraint {
        println!("  Java:        {java}");
    }
    if !metadata.entrypoints.is_empty() {
        println!("  Entrypoints:");
        for entrypoint in metadata.entrypoints.iter().take(10) {
            println!("    - {} = {}", entrypoint.kind, entrypoint.value);
        }
    }
    let required: Vec<_> = metadata
        .depends
        .iter()
        .filter(|dependency| dependency.mandatory)
        .collect();
    if !required.is_empty() {
        println!("  Required dependencies:");
        for dependency in required.iter().take(12) {
            println!(
                "    - {} {}{}",
                dependency.mod_id,
                dependency.constraint.as_deref().unwrap_or(""),
                match dependency.kind {
                    mats_core::types::DependencyKind::Minecraft => " (minecraft)",
                    mats_core::types::DependencyKind::Loader => " (loader)",
                    mats_core::types::DependencyKind::Java => " (java)",
                    mats_core::types::DependencyKind::Mod => "",
                }
            );
        }
    }
    if !metadata.mixins.is_empty() {
        println!("  Mixin configs: {}", metadata.mixins.join(", "));
    }
    if !metadata.access_wideners.is_empty() {
        println!("  Access wideners: {}", metadata.access_wideners.join(", "));
    }
}

fn run_analyze(jar: &Path, extra_mods: &[PathBuf], json: bool) -> Result<i32> {
    let inspection = jar_inspector::JarInspection::inspect(jar)?;
    let parsed = metadata_parser::parse_inspection(&inspection);
    let mut findings: Vec<Finding> = Vec::new();
    let mut snapshots = Vec::new();
    let mut mixin_analysis = mixin_analyzer::MixinAnalysis::default();

    match &parsed {
        Ok(parsed) => {
            mixin_analysis = mixin_analyzer::analyze_static(parsed, &inspection);
            findings.extend(mixin_analysis.findings.clone());
            for warning in &parsed.warnings {
                findings.push(Finding::new(
                    "metadata.warning",
                    Severity::Warning,
                    FindingCategory::Metadata,
                    warning.clone(),
                ));
            }
            snapshots.push(conflict_detector::ModSnapshot::build(
                &parsed.metadata,
                &parsed.mixin_configs,
                &inspection,
            ));
        }
        Err(error) => findings.push(
            Finding::new(
                "metadata.unparseable",
                Severity::Critical,
                FindingCategory::Metadata,
                "no loader metadata could be parsed",
            )
            .with_detail(format!("{error:#}")),
        ),
    }
    for warning in &inspection.warnings {
        findings.push(Finding::new(
            "inspection.warning",
            Severity::Warning,
            FindingCategory::Metadata,
            warning.clone(),
        ));
    }
    if inspection.loaders_detected.len() > 1 {
        findings.push(Finding::new(
            "metadata.multiple-loaders",
            Severity::Warning,
            FindingCategory::Metadata,
            format!(
                "metadata for multiple loaders: {}",
                inspection
                    .loaders_detected
                    .iter()
                    .map(|loader| loader.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        ));
    }
    for extra in extra_mods {
        match jar_inspector::JarInspection::inspect(extra) {
            Ok(extra_inspection) => match metadata_parser::parse_inspection(&extra_inspection) {
                Ok(extra_parsed) => snapshots.push(conflict_detector::ModSnapshot::build(
                    &extra_parsed.metadata,
                    &extra_parsed.mixin_configs,
                    &extra_inspection,
                )),
                Err(error) => findings.push(
                    Finding::new(
                        "metadata.extra-mod-unparseable",
                        Severity::Warning,
                        FindingCategory::Metadata,
                        format!("extra mod `{}` has no parseable metadata", extra.display()),
                    )
                    .with_detail(format!("{error:#}")),
                ),
            },
            Err(error) => findings.push(
                Finding::new(
                    "metadata.extra-mod-unreadable",
                    Severity::Warning,
                    FindingCategory::Metadata,
                    format!("could not read extra mod `{}`", extra.display()),
                )
                .with_detail(format!("{error:#}")),
            ),
        }
    }
    let conflicts = conflict_detector::analyze(&snapshots);
    findings.extend(conflicts.findings.clone());
    let findings = diagnostics::aggregate(findings);
    let counts = diagnostics::count_severities(&findings);

    if json {
        let document = serde_json::json!({
            "jar": inspection,
            "metadata": parsed.as_ref().ok().map(|parsed| &parsed.metadata),
            "mixin_analysis": mixin_analysis,
            "conflicts": conflicts,
            "findings": findings,
            "counts": counts,
        });
        print_json(&document)?;
        return Ok(if counts.blocking() > 0 { 1 } else { 0 });
    }

    println!(
        "Static analysis of {} ({})",
        jar.display(),
        fsutil::human_bytes(inspection.size_bytes)
    );
    println!(
        "  Loader: {} · Classes: {} · Mixin configs: {} · Access wideners: {}",
        inspection
            .primary_loader()
            .map(|loader| loader.display_name().to_string())
            .unwrap_or_else(|| "unknown".into()),
        inspection.bytecode.class_count,
        inspection.mixin_configs.len(),
        inspection.access_wideners.len()
    );
    if let Ok(parsed) = &parsed {
        println!(
            "  Mod: {} {} · Minecraft {}",
            parsed.metadata.name,
            parsed.metadata.version,
            parsed
                .metadata
                .mc_constraint_joined()
                .unwrap_or_else(|| "(unspecified)".into())
        );
    }
    println!();
    if findings.is_empty() {
        println!("No static problems were detected.");
    } else {
        for finding in &findings {
            println!(
                "  {:<8} {:<34} {}",
                finding.severity.label(),
                finding.code,
                finding.message
            );
            if let Some(detail) = &finding.detail {
                println!("           {detail}");
            }
        }
    }
    println!();
    println!(
        "{} critical, {} error(s), {} warning(s), {} information — static analysis only; nothing was executed.",
        counts.critical, counts.error, counts.warning, counts.information
    );
    Ok(if counts.blocking() > 0 { 1 } else { 0 })
}

#[allow(clippy::too_many_arguments)]
fn run_matrix(
    home: &MatsHome,
    config: &Config,
    jar: &Path,
    mc_override: Option<&str>,
    loader_override: Option<&str>,
    limit: usize,
    allow_snapshots: bool,
    json: bool,
) -> Result<i32> {
    let inspection = jar_inspector::JarInspection::inspect(jar)?;
    let parsed = metadata_parser::parse_inspection(&inspection).ok();
    let loader = match loader_override {
        Some(value) => value
            .parse::<Loader>()
            .map_err(|error| anyhow::anyhow!(error))?,
        None => inspection
            .primary_loader()
            .context("no loader detected; pass --loader")?,
    };
    let constraints = parsed
        .as_ref()
        .map(|parsed| parsed.metadata.mc_constraints.clone())
        .unwrap_or_default();
    let manager = ResourceManager::new(home, config)?;
    let mojang = version_resolver::MojangClient::new(&manager, &config.network);
    let manifest = mojang.manifest()?;
    let releases: Vec<_> = manifest
        .versions
        .iter()
        .filter(|entry| entry.is_release())
        .cloned()
        .collect();

    let rows = if let Some(mc) = mc_override {
        let resolver = version_resolver::LoaderResolver::new(&manager, &config.network);
        let resolved = resolver.resolve(loader, mc, None);
        vec![version_resolver::MatrixRow {
            mc_version: mc.to_string(),
            loader_version: resolved
                .as_ref()
                .ok()
                .map(|artifact| artifact.loader_version.clone()),
            supported: resolved.is_ok(),
            java_major: Some(version_resolver::fallback_java_major(
                &mats_core::McVersion::new(mc),
            )),
            notes: resolved
                .err()
                .map(|error| vec![format!("{error:#}")])
                .unwrap_or_default(),
        }]
    } else {
        version_resolver::build_matrix(
            &manager,
            &config.network,
            loader,
            &releases,
            &manifest.versions,
            &constraints,
            limit,
            allow_snapshots,
        )?
    };

    if json {
        print_json(&serde_json::json!({
            "loader": loader,
            "constraints": constraints,
            "candidates": rows,
        }))?;
        return Ok(0);
    }
    println!(
        "{} support matrix for {}",
        loader.display_name(),
        parsed
            .as_ref()
            .map(|parsed| format!("{} {}", parsed.metadata.name, parsed.metadata.version))
            .unwrap_or_else(|| jar.display().to_string())
    );
    println!(
        "  Declared range: {}",
        if constraints.is_empty() {
            "(none — newest releases shown)".to_string()
        } else {
            constraints.join(", ")
        }
    );
    println!();
    println!(
        "  {:<14} {:<10} {:<18} {}",
        "MINECRAFT", "JAVA", "LOADER", "STATUS"
    );
    for row in &rows {
        println!(
            "  {:<14} {:<10} {:<18} {}",
            row.mc_version,
            row.java_major
                .map(|major| format!("{major}"))
                .unwrap_or_else(|| "?".into()),
            row.loader_version.clone().unwrap_or_else(|| "-".into()),
            if row.supported {
                "supported".to_string()
            } else {
                format!("not available: {}", row.notes.join("; "))
            }
        );
    }
    Ok(0)
}

fn run_versions(
    home: &MatsHome,
    config: &Config,
    loader: Option<&str>,
    limit: usize,
    json: bool,
) -> Result<i32> {
    let manager = ResourceManager::new(home, config)?;
    let mojang = version_resolver::MojangClient::new(&manager, &config.network);
    let manifest = mojang.manifest()?;
    let releases: Vec<_> = manifest
        .versions
        .iter()
        .filter(|entry| entry.is_release())
        .take(limit)
        .cloned()
        .collect();
    let mut rows = Vec::new();
    for entry in &releases {
        rows.push(serde_json::json!({
            "version": entry.id,
            "type": entry.kind,
            "released": entry.release_time,
            "java_major": version_resolver::fallback_java_major(&mats_core::McVersion::new(entry.id.clone())),
        }));
    }
    let mut loader_note = None;
    if let Some(loader_name) = loader {
        let loader = loader_name
            .parse::<Loader>()
            .map_err(|error| anyhow::anyhow!(error))?;
        if let Some(latest) = releases.first() {
            let resolver = version_resolver::LoaderResolver::new(&manager, &config.network);
            match resolver.resolve(loader, &latest.id, None) {
                Ok(artifact) => {
                    loader_note = Some(format!(
                        "{} {} is available for Minecraft {}",
                        loader.display_name(),
                        artifact.loader_version,
                        latest.id
                    ));
                }
                Err(error) => {
                    loader_note = Some(format!(
                        "{} has no build for Minecraft {}: {error:#}",
                        loader.display_name(),
                        latest.id
                    ));
                }
            }
        }
    }
    if json {
        print_json(&serde_json::json!({
            "latest": manifest.latest,
            "releases": rows,
            "loader": loader_note,
        }))?;
        return Ok(0);
    }
    println!("Latest release: {}", manifest.latest.release);
    println!("Latest snapshot: {}", manifest.latest.snapshot);
    println!();
    println!(
        "  {:<14} {:<10} {:<26} {}",
        "MINECRAFT", "JAVA", "RELEASED", "TYPE"
    );
    for entry in &releases {
        println!(
            "  {:<14} {:<10} {:<26} {}",
            entry.id,
            version_resolver::fallback_java_major(&mats_core::McVersion::new(entry.id.clone())),
            entry.release_time,
            entry.kind
        );
    }
    if let Some(note) = loader_note {
        println!();
        println!("{note}");
    }
    Ok(0)
}

// ---------------------------------------------------------------------------
// doctor
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CheckStatus {
    Pass,
    Warn,
    Fail,
}

impl CheckStatus {
    fn label(self) -> &'static str {
        match self {
            CheckStatus::Pass => "PASS",
            CheckStatus::Warn => "WARN",
            CheckStatus::Fail => "FAIL",
        }
    }
}

fn run_doctor(home: &MatsHome, config: &Config, json: bool) -> Result<i32> {
    let mut checks: Vec<(String, CheckStatus, String)> = Vec::new();

    // Home directory.
    let writable = home.tmp_dir().exists()
        && std::fs::write(home.tmp_dir().join("doctor-check.tmp"), b"ok").is_ok()
        && std::fs::remove_file(home.tmp_dir().join("doctor-check.tmp")).is_ok();
    checks.push((
        "MATS home directory".into(),
        if writable {
            CheckStatus::Pass
        } else {
            CheckStatus::Fail
        },
        format!(
            "{} ({})",
            home.root().display(),
            if writable {
                "writable".to_string()
            } else {
                "not writable".to_string()
            }
        ),
    ));

    // Configuration.
    let config_status = if home.config_file().is_file() {
        (CheckStatus::Pass, home.config_file().display().to_string())
    } else {
        (
            CheckStatus::Warn,
            format!(
                "{} does not exist; defaults are in use and will be written on demand",
                home.config_file().display()
            ),
        )
    };
    checks.push(("Configuration".into(), config_status.0, config_status.1));

    // Network access to the official metadata.
    let manager = ResourceManager::new(home, config)?;
    if config.runtime.offline {
        checks.push((
            "Network".into(),
            CheckStatus::Warn,
            "offline mode is enabled; only cached resources can be used".into(),
        ));
    } else {
        match manager.http().get_json(&config.network.mojang_manifest) {
            Ok(value) => {
                let count = value
                    .get("versions")
                    .and_then(|versions| versions.as_array())
                    .map(Vec::len)
                    .unwrap_or(0);
                checks.push((
                    "Mojang metadata".into(),
                    if count > 0 {
                        CheckStatus::Pass
                    } else {
                        CheckStatus::Warn
                    },
                    format!("reachable, {count} versions listed"),
                ));
            }
            Err(error) => checks.push((
                "Mojang metadata".into(),
                CheckStatus::Fail,
                format!("unreachable: {error}"),
            )),
        }
        match manager.http().get_text(&format!(
            "{}/v2/versions/installer",
            config.network.fabric_meta.trim_end_matches('/')
        )) {
            Ok(_) => checks.push(("Fabric meta".into(), CheckStatus::Pass, "reachable".into())),
            Err(error) => checks.push((
                "Fabric meta".into(),
                CheckStatus::Warn,
                format!("unreachable: {error}"),
            )),
        }
    }

    // Java.
    let runtimes = java_manager::detect_runtimes(home);
    if runtimes.is_empty() {
        checks.push((
            "Java".into(),
            CheckStatus::Warn,
            "no Java installation found; MATS will download Temurin on demand".into(),
        ));
    } else {
        let newest = runtimes
            .iter()
            .max_by_key(|runtime| runtime.major)
            .expect("non-empty");
        let java25 = runtimes.iter().any(|runtime| runtime.major >= 25);
        checks.push((
            "Java".into(),
            if java25 {
                CheckStatus::Pass
            } else {
                CheckStatus::Warn
            },
            format!(
                "{} found ({} runtime(s)); Java 25 is required by Minecraft 26.x{}",
                newest.describe(),
                runtimes.len(),
                if java25 {
                    ""
                } else {
                    " — MATS can download it automatically"
                }
            ),
        ));
    }

    // Graphics.
    let has_display =
        std::env::var_os("DISPLAY").is_some() || std::env::var_os("WAYLAND_DISPLAY").is_some();
    let xvfb = which("xvfb-run");
    checks.push((
        "Client graphics".into(),
        if has_display || xvfb.is_some() {
            CheckStatus::Pass
        } else {
            CheckStatus::Warn
        },
        if has_display {
            "a display is available; client tests can run natively".into()
        } else if let Some(path) = xvfb {
            format!(
                "no display; client tests will use Xvfb ({}) with software OpenGL",
                path.display()
            )
        } else {
            "no display and no Xvfb; client tests will be reported as not tested".into()
        },
    ));

    // Cache.
    let stats = manager.stats(home);
    checks.push((
        "Resource cache".into(),
        CheckStatus::Pass,
        format!(
            "{} ({} object(s), {} Java runtime(s), {} environment(s))",
            fsutil::human_bytes(stats.bytes),
            stats.objects,
            stats.java_runtimes,
            stats.environments
        ),
    ));

    // Updates.
    checks.push((
        "Updates".into(),
        CheckStatus::Pass,
        format!(
            "{} channel, auto-check {}, auto-install {} (repository {})",
            config.updates.channel.as_str(),
            if config.updates.auto_check {
                "on"
            } else {
                "off"
            },
            if config.updates.auto_install {
                "on"
            } else {
                "off"
            },
            config.updates.repository
        ),
    ));

    let failures = checks
        .iter()
        .filter(|(_, status, _)| *status == CheckStatus::Fail)
        .count();
    let warnings = checks
        .iter()
        .filter(|(_, status, _)| *status == CheckStatus::Warn)
        .count();

    if json {
        let entries: Vec<serde_json::Value> = checks
            .iter()
            .map(|(name, status, detail)| {
                serde_json::json!({
                    "check": name,
                    "status": status.label().to_lowercase(),
                    "detail": detail,
                })
            })
            .collect();
        print_json(&serde_json::json!({
            "checks": entries,
            "failures": failures,
            "warnings": warnings,
            "tool": tool_info(),
        }))?;
        return Ok(if failures > 0 { 1 } else { 0 });
    }
    print_separator("MATS doctor");
    for (name, status, detail) in &checks {
        println!("  [{}] {name}", status.label());
        println!("        {detail}");
    }
    println!();
    println!(
        "{} check(s) passed, {warnings} warning(s), {failures} failure(s).",
        checks.len() - warnings - failures
    );
    Ok(if failures > 0 { 1 } else { 0 })
}

// ---------------------------------------------------------------------------
// cache / env / history / report
// ---------------------------------------------------------------------------

fn run_cache(home: &MatsHome, config: &Config, action: CacheAction, quiet: bool) -> Result<i32> {
    let manager = ResourceManager::new(home, config)?;
    match action {
        CacheAction::Info => {
            let stats = manager.stats(home);
            println!("MATS cache");
            println!("  Root:         {}", home.cache_dir().display());
            println!(
                "  Objects:      {} ({})",
                stats.objects,
                fsutil::human_bytes(stats.bytes)
            );
            println!("  Metadata:     {} file(s)", stats.metas);
            println!("  Java:         {} runtime(s)", stats.java_runtimes);
            println!(
                "  Environments: {} (in {})",
                stats.environments,
                home.envs_dir().display()
            );
            println!("  Reports:      {}", home.reports_dir().display());
            let threshold = config.cache.warn_above_gb * 1024 * 1024 * 1024;
            if stats.bytes > threshold {
                println!(
                    "  Note: the cache is larger than {} GiB; `mats cache clean` frees space.",
                    config.cache.warn_above_gb
                );
            }
            Ok(0)
        }
        CacheAction::Clean { envs, yes } => {
            let stats = manager.stats(home);
            if !quiet {
                println!(
                    "This removes {} of cached Minecraft resources{}.",
                    fsutil::human_bytes(stats.bytes),
                    if envs {
                        " and all prepared environments"
                    } else {
                        ""
                    }
                );
            }
            if !yes && !confirm("Continue?")? {
                println!("Cancelled.");
                return Ok(0);
            }
            let removed = manager.clear(home, envs)?;
            println!("Removed {removed} cached object(s).");
            Ok(0)
        }
    }
}

fn run_env(home: &MatsHome, action: EnvAction, quiet: bool) -> Result<i32> {
    match action {
        EnvAction::List => {
            let environments = storage::list_environments(home)?;
            if environments.is_empty() {
                println!("No prepared environments. Run `mats test <mod.jar>` first.");
                return Ok(0);
            }
            println!(
                "  {:<18} {:<12} {:<10} {:<18} {:<8} {}",
                "ID", "MINECRAFT", "LOADER", "LOADER VERSION", "SIZE", "CREATED"
            );
            for environment in environments {
                println!(
                    "  {:<18} {:<12} {:<10} {:<18} {:<8} {}",
                    environment.id,
                    environment.mc_version,
                    environment.loader.as_str(),
                    environment.loader_version.as_deref().unwrap_or("-"),
                    fsutil::human_bytes(environment.size_bytes),
                    environment.created_at.format("%Y-%m-%d %H:%M")
                );
            }
            Ok(0)
        }
        EnvAction::Clean {
            all,
            older_than_hours,
            yes,
        } => {
            if !all && older_than_hours.is_none() {
                bail!("specify --all or --older-than-hours <HOURS>");
            }
            if !quiet {
                println!(
                    "This removes {}.",
                    if all {
                        "every prepared environment".to_string()
                    } else {
                        format!(
                            "environments older than {} hour(s)",
                            older_than_hours.unwrap_or(0)
                        )
                    }
                );
            }
            if !yes && !confirm("Continue?")? {
                println!("Cancelled.");
                return Ok(0);
            }
            let summary = storage::clean_environments(
                home,
                all,
                older_than_hours.map(|hours| Duration::from_secs(hours * 3600)),
            )?;
            println!(
                "Removed {} environment(s), freed {}{}.",
                summary.removed,
                fsutil::human_bytes(summary.bytes_freed),
                if summary.skipped > 0 {
                    format!(", kept {}", summary.skipped)
                } else {
                    String::new()
                }
            );
            Ok(0)
        }
    }
}

fn run_history(home: &MatsHome, limit: usize, json: bool) -> Result<i32> {
    let records = storage::load_history(home)?;
    if json {
        print_json(&serde_json::json!({ "runs": records }))?;
        return Ok(0);
    }
    print!("{}", storage::render_history(&records, limit));
    Ok(0)
}

fn run_report(home: &MatsHome, path_only: bool, json: bool) -> Result<i32> {
    let Some(record) = storage::latest_run(home)? else {
        bail!("no reports yet; run `mats test <mod.jar>` first");
    };
    let Some(dir) = record.report_dir else {
        bail!("the most recent run has no report directory");
    };
    let dir = PathBuf::from(dir);
    if path_only {
        println!("{}", dir.display());
        return Ok(0);
    }
    let file = if json {
        dir.join("report.json")
    } else {
        dir.join("report.md")
    };
    let contents = storage::read_report_file(&file)?;
    if json {
        let value: serde_json::Value = serde_json::from_str(&contents)?;
        print_json(&value)?;
    } else {
        print!("{contents}");
        if !contents.ends_with('\n') {
            println!();
        }
    }
    Ok(0)
}

// ---------------------------------------------------------------------------
// update
// ---------------------------------------------------------------------------

fn run_update(home: &MatsHome, config: &Config, args: &UpdateArgs, json: bool) -> Result<i32> {
    let manager = ResourceManager::new(home, config)?;
    if args.show_version {
        let check = updater::check(&manager, config)?;
        if json {
            print_json(&check)?;
        } else {
            println!("Installed: {}", check.current_version);
            println!(
                "Latest available: {}",
                check
                    .latest
                    .as_ref()
                    .map(|release| release.version.clone())
                    .unwrap_or_else(|| "unknown".into())
            );
            println!("Channel: {}", check.channel.as_str());
            println!("Status: {}", check.status.as_str());
        }
        return Ok(0);
    }
    match args.action {
        None | Some(UpdateAction::Check) => {
            let check = updater::check(&manager, config)?;
            updater::record_check(home, &check.current_version);
            if json {
                print_json(&check)?;
                return Ok(0);
            }
            print!("{}", updater::render_check(&check, true));
            Ok(0)
        }
        Some(UpdateAction::Install) => {
            let check = updater::check(&manager, config)?;
            if !check.is_update_available() {
                println!("{}", updater::render_check(&check, false));
                return Ok(0);
            }
            if !json {
                print!("{}", updater::render_check(&check, true));
            }
            if !args.yes && !confirm("Download and install this update?")? {
                println!("Cancelled.");
                return Ok(0);
            }
            let current_exe = std::env::current_exe()
                .context("could not determine the path of the running MATS executable")?;
            let outcome = updater::install(
                &manager,
                &check,
                home,
                &current_exe,
                args.allow_downgrade,
                false,
            )?;
            if json {
                print_json(&serde_json::json!({
                    "installed": outcome.installed,
                    "version": outcome.version,
                    "message": outcome.message,
                }))?;
            } else {
                println!("{}", outcome.message);
            }
            Ok(0)
        }
    }
}

// ---------------------------------------------------------------------------
// shared helpers
// ---------------------------------------------------------------------------

fn maybe_notify_update(home: &MatsHome, config: &Config, quiet: bool) {
    if quiet
        || !config.updates.enabled
        || !config.updates.auto_check
        || config.runtime.offline
        || !updater::should_check(home)
    {
        return;
    }
    let timeout = Duration::from_millis(config.updates.timeout_ms.clamp(500, 15_000));
    let (sender, receiver) = std::sync::mpsc::channel();
    let home_for_thread = home.clone();
    let config_for_thread = config.clone();
    std::thread::spawn(move || {
        let result = ResourceManager::new(&home_for_thread, &config_for_thread)
            .and_then(|manager| updater::check(&manager, &config_for_thread));
        let _ = sender.send(result);
    });
    let Ok(Ok(check)) = receiver.recv_timeout(timeout) else {
        return;
    };
    updater::record_check(home, &check.current_version);
    if check.is_update_available() {
        let version = check
            .latest
            .as_ref()
            .map(|release| release.version.clone())
            .unwrap_or_default();
        eprintln!(
            "\nA newer MATS release is available ({} → {version}). Run `mats update install`.",
            check.current_version
        );
        if config.updates.auto_install {
            let manager = match ResourceManager::new(home, config) {
                Ok(manager) => manager,
                Err(error) => {
                    tracing::warn!("automatic update failed: {error:#}");
                    return;
                }
            };
            let Ok(executable) = std::env::current_exe() else {
                return;
            };
            match updater::install(&manager, &check, home, &executable, false, false) {
                Ok(outcome) => eprintln!("{}", outcome.message),
                Err(error) => eprintln!("Automatic update failed: {error:#}"),
            }
        }
    }
}

fn confirm(question: &str) -> Result<bool> {
    if !std::io::stdin().is_terminal() {
        bail!("refusing to continue without confirmation; re-run with --yes");
    }
    print!("{question} [y/N] ");
    std::io::stdout().flush().ok();
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    let answer = line.trim().to_ascii_lowercase();
    Ok(answer == "y" || answer == "yes")
}

fn print_json(value: &impl serde::Serialize) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

fn print_separator(title: &str) {
    println!("{title}");
    println!("{}", "-".repeat(title.chars().count().max(8)));
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

fn tool_info() -> ToolInfo {
    ToolInfo {
        name: "mats".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        os: std::env::consts::OS.into(),
        arch: std::env::consts::ARCH.into(),
        features: Vec::new(),
    }
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

/// Clap parse helper used by tests and the binary.
#[allow(dead_code)]
pub fn parse_cli() -> Cli {
    Cli::parse()
}
