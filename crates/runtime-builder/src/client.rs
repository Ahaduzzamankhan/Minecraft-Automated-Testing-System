//! Client runtime construction (Vanilla, Fabric and Quilt).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use java_manager::JavaRuntime;
use mats_core::config::Config;
use mats_core::types::{Loader, ResourceKind, ResourceRecord};
use resource_manager::{ResourceManager, ResourceRequest, encode_segment};
use version_resolver::ResolvedPlan;
use version_resolver::mojang::{ArgumentEntry, ArgumentValue, MojangVersion};

use crate::common::{self, LoaderProfile};
use crate::{EnvironmentSpec, LaunchCommand, base_placeholders};

const PROFILE_TTL: Duration = Duration::from_secs(60 * 60 * 24 * 7);

#[allow(clippy::too_many_arguments)]
pub(crate) fn prepare_client(
    config: &Config,
    manager: &ResourceManager,
    java: &JavaRuntime,
    spec: &EnvironmentSpec,
    plan: &ResolvedPlan,
    dir: &Path,
    log_dir: &Path,
    records: &mut Vec<ResourceRecord>,
    notes: &mut Vec<String>,
    version: &MojangVersion,
) -> Result<(LaunchCommand, bool)> {
    let client_download = version
        .downloads
        .client
        .clone()
        .context("Mojang metadata for this version does not include a client JAR")?;
    let client_request = ResourceRequest::new(
        "minecraft-client",
        ResourceKind::MinecraftClient,
        client_download.url.clone(),
    )
    .with_sha1(client_download.sha1.clone())
    .with_size(client_download.size);
    let cached_client = manager.fetch(&client_request)?;
    let from_cache = cached_client.from_cache;
    records.push(cached_client.record());
    let client_jar = common::place_version_jar(dir, &version.id, &cached_client.path)?;
    notes.push(format!(
        "client JAR {}",
        if from_cache {
            "reused from cache"
        } else {
            "downloaded from Mojang"
        }
    ));

    let mut classpath = common::install_libraries(
        &version.libraries,
        dir,
        manager,
        records,
        &dir.join("natives"),
    )?;

    let mut main_class = version.main_class.clone();
    let mut profile_game_args: Vec<String> = Vec::new();
    let mut profile_jvm_args: Vec<String> = Vec::new();
    let mut profile_classpath: Vec<PathBuf> = Vec::new();

    if matches!(spec.loader, Loader::Fabric | Loader::Quilt) {
        let loader_version = plan
            .loader_version
            .as_deref()
            .context("the resolved plan has no loader version for a client launch")?;
        let (profile_url, maven_base) = match spec.loader {
            Loader::Fabric => (
                format!(
                    "{}/v2/versions/loader/{}/{}/profile/json",
                    config.network.fabric_meta.trim_end_matches('/'),
                    encode_segment(&spec.mc_version),
                    encode_segment(loader_version)
                ),
                config.network.fabric_maven.clone(),
            ),
            Loader::Quilt => (
                format!(
                    "{}/v3/versions/loader/{}/{}/profile/json",
                    config.network.quilt_meta.trim_end_matches('/'),
                    encode_segment(&spec.mc_version),
                    encode_segment(loader_version)
                ),
                config.network.quilt_maven.clone(),
            ),
            _ => unreachable!("guarded by the match above"),
        };
        let value = manager
            .fetch_json(
                &profile_url,
                &format!(
                    "{}-client-profile-{}-{}",
                    spec.loader.as_str(),
                    spec.mc_version,
                    loader_version
                ),
                PROFILE_TTL,
            )
            .with_context(|| {
                format!(
                    "reading the {} launcher profile for Minecraft {}",
                    spec.loader.display_name(),
                    spec.mc_version
                )
            })?;
        let profile: LoaderProfile =
            serde_json::from_value(value).context("the loader profile has an unexpected shape")?;
        if let Some(profile_main) = &profile.main_class {
            main_class = profile_main.clone();
        }
        profile_game_args = profile.arguments.game;
        profile_jvm_args = profile.arguments.jvm;
        profile_classpath = common::install_profile_libraries(
            &profile.libraries,
            dir,
            manager,
            records,
            &maven_base,
        )?;
        if let Some(inherits) = &profile.inherits_from {
            if inherits != &version.id {
                notes.push(format!(
                    "loader profile inherits from `{inherits}` while the plan targets `{}`",
                    version.id
                ));
            }
        }
    }

    let mut assets_available = false;
    if let Some(index) = &version.asset_index {
        common::install_asset_index(
            dir,
            &index.url,
            index.sha1.as_deref(),
            index.size,
            &index.id,
            manager,
            records,
            spec.download_assets,
        )?;
        assets_available = true;
        if !spec.download_assets {
            notes.push(
                "asset index downloaded; asset objects were skipped (use --download-assets for a full asset set)"
                    .to_string(),
            );
        }
    }

    let mut classpath_entries = profile_classpath;
    classpath_entries.append(&mut classpath);
    classpath_entries.push(client_jar.clone());
    let classpath_entries = dedupe_paths(classpath_entries);
    let classpath_string = join_classpath(&classpath_entries);

    let mut values = base_placeholders(spec, dir, &java.path);
    values.insert("classpath", classpath_string);
    values.insert(
        "assets_index_name",
        version
            .asset_index
            .as_ref()
            .map(|index| index.id.clone())
            .or_else(|| version.assets.clone())
            .unwrap_or_else(|| "legacy".to_string()),
    );

    std::fs::create_dir_all(dir.join("game"))
        .with_context(|| format!("creating game directory in {}", dir.display()))?;

    let mut args = Vec::new();
    args.push(format!("-Xmx{}M", spec.memory_mb));
    args.push("-XX:+ExitOnOutOfMemoryError".to_string());
    args.push("-Dfile.encoding=UTF-8".to_string());
    args.push("-Dmats.side=client".to_string());
    args.push(format!("-Dmats.environment={}", dir.display()));
    args.extend(gc_args(log_dir, java.major));
    args.extend(build_jvm_args(version, &values));
    for arg in &profile_jvm_args {
        let substituted = common::substitute(arg, &values);
        if !substituted.trim().is_empty() {
            args.push(substituted);
        }
    }
    args.push("-cp".to_string());
    args.push(join_classpath(&classpath_entries));
    args.push(main_class.clone());
    args.extend(build_game_args(version, &values));
    for arg in &profile_game_args {
        let substituted = common::substitute(arg, &values);
        if !substituted.trim().is_empty() {
            args.push(substituted);
        }
    }
    args.extend(config.runtime.extra_jvm_args.iter().cloned());
    args.extend(config.runtime.extra_client_jvm_args.iter().cloned());

    Ok((
        LaunchCommand {
            program: java.path.clone(),
            args,
            cwd: dir.to_path_buf(),
            env: java_environment(dir),
            main_class,
            classpath_entries: classpath_entries.len(),
            argfiles: Vec::new(),
        },
        assets_available,
    ))
}

fn build_jvm_args(version: &MojangVersion, values: &BTreeMap<&'static str, String>) -> Vec<String> {
    let mut args = Vec::new();
    let mut skip_next = false;
    for entry in &version.arguments.jvm {
        let raw = match entry {
            ArgumentEntry::Plain(value) => vec![value.clone()],
            ArgumentEntry::Gated { rules, value } => {
                if !common::rule_allows(rules) {
                    continue;
                }
                match value {
                    ArgumentValue::Single(value) => vec![value.clone()],
                    ArgumentValue::Many(items) => items.clone(),
                }
            }
        };
        for argument in raw {
            if skip_next {
                skip_next = false;
                continue;
            }
            if argument == "-cp" || argument == "-classpath" {
                skip_next = true;
                continue;
            }
            if argument == "${classpath}" {
                continue;
            }
            let substituted = common::substitute(&argument, values);
            if !substituted.trim().is_empty() {
                args.push(substituted);
            }
        }
    }
    if !args
        .iter()
        .any(|argument| argument.starts_with("-Djava.library.path"))
    {
        if let Some(natives) = values.get("natives_directory") {
            args.push(format!("-Djava.library.path={natives}"));
        }
    }
    args
}

fn build_game_args(
    version: &MojangVersion,
    values: &BTreeMap<&'static str, String>,
) -> Vec<String> {
    let mut args = Vec::new();
    if version.arguments.game.is_empty() {
        if let Some(legacy) = &version.minecraft_arguments {
            for token in legacy.split_whitespace() {
                let substituted = common::substitute(token, values);
                if !substituted.is_empty() {
                    args.push(substituted);
                }
            }
        }
        return args;
    }
    for entry in &version.arguments.game {
        let raw = match entry {
            ArgumentEntry::Plain(value) => vec![value.clone()],
            ArgumentEntry::Gated { rules, value } => {
                if !common::rule_allows(rules) {
                    continue;
                }
                match value {
                    ArgumentValue::Single(value) => vec![value.clone()],
                    ArgumentValue::Many(items) => items.clone(),
                }
            }
        };
        for argument in raw {
            let substituted = common::substitute(&argument, values);
            if !substituted.is_empty() {
                args.push(substituted);
            }
        }
    }
    args
}

/// JVM GC logging flags. Java 8 needs the legacy flags.
pub(crate) fn gc_args(log_dir: &Path, java_major: u32) -> Vec<String> {
    let _ = std::fs::create_dir_all(log_dir);
    if java_major >= 9 {
        vec![format!(
            "-Xlog:gc*:file={}:time,uptime,level,tags",
            log_dir.join("gc.log").display()
        )]
    } else {
        vec![
            format!("-Xloggc:{}", log_dir.join("gc.log").display()),
            "-XX:+PrintGCDetails".to_string(),
            "-XX:+PrintGCDateStamps".to_string(),
        ]
    }
}

pub(crate) fn join_classpath(entries: &[PathBuf]) -> String {
    let separator = if cfg!(windows) { ";" } else { ":" };
    entries
        .iter()
        .map(|entry| entry.display().to_string())
        .collect::<Vec<_>>()
        .join(separator)
}

fn dedupe_paths(entries: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut seen = std::collections::HashSet::new();
    let mut output = Vec::new();
    for entry in entries {
        let key = entry.display().to_string();
        if seen.insert(key) {
            output.push(entry);
        }
    }
    output
}

pub(crate) fn java_environment(dir: &Path) -> Vec<(String, String)> {
    let mut environment = Vec::new();
    environment.push(("MATS_ENVIRONMENT".to_string(), dir.display().to_string()));
    if cfg!(target_os = "linux") {
        // Minecraft/LWJGL on many CI machines only has software GL available.
        environment.push((
            "LIBGL_ALWAYS_SOFTWARE".to_string(),
            std::env::var("LIBGL_ALWAYS_SOFTWARE").unwrap_or_else(|_| "1".to_string()),
        ));
    }
    environment
}

#[cfg(test)]
mod tests {
    use super::*;
    use version_resolver::mojang::MojangVersion;

    fn version() -> MojangVersion {
        serde_json::from_value(serde_json::json!({
            "id": "26.3",
            "mainClass": "net.minecraft.client.main.Main",
            "arguments": {
                "jvm": [
                    {"rules": [{"action": "allow", "os": {"name": "osx"}}], "value": ["-XstartOnFirstThread"]},
                    "-Djava.library.path=${natives_directory}",
                    "-cp",
                    "${classpath}"
                ],
                "game": [
                    "--username", "${auth_player_name}",
                    {"rules": [{"action": "allow", "features": {"is_demo_user": true}}], "value": "--demo"},
                    "--gameDir", "${game_directory}"
                ]
            },
            "downloads": {"client": {"url": "https://example.invalid/client.jar", "sha1": "abc", "size": 1}}
        }))
        .unwrap()
    }

    #[test]
    fn builds_jvm_arguments_without_duplicate_classpath() {
        let values = BTreeMap::from([
            ("natives_directory", "/tmp/natives".to_string()),
            ("classpath", "/tmp/a.jar".to_string()),
        ]);
        let args = build_jvm_args(&version(), &values);
        assert!(args.contains(&"-Djava.library.path=/tmp/natives".to_string()));
        assert!(!args.contains(&"/tmp/a.jar".to_string()));
        assert!(!args.iter().any(|arg| arg == "-cp"));
        assert!(!args.contains(&"-XstartOnFirstThread".to_string()) || cfg!(target_os = "macos"));
    }

    #[test]
    fn builds_game_arguments_and_skips_feature_rules() {
        let values = BTreeMap::from([
            ("auth_player_name", "MATS".to_string()),
            ("game_directory", "/tmp/game".to_string()),
        ]);
        let args = build_game_args(&version(), &values);
        assert_eq!(args, vec!["--username", "MATS", "--gameDir", "/tmp/game"]);
    }

    #[test]
    fn supports_legacy_minecraft_arguments() {
        let legacy: MojangVersion = serde_json::from_value(serde_json::json!({
            "id": "1.6.4",
            "mainClass": "net.minecraft.client.main.Main",
            "minecraftArguments": "--username ${auth_player_name} --gameDir ${game_directory}",
            "arguments": {"game": [], "jvm": []}
        }))
        .unwrap();
        let values = BTreeMap::from([
            ("auth_player_name", "MATS".to_string()),
            ("game_directory", "/g".to_string()),
        ]);
        assert_eq!(
            build_game_args(&legacy, &values),
            vec!["--username", "MATS", "--gameDir", "/g"]
        );
    }

    #[test]
    fn joins_classpath_with_the_platform_separator() {
        let entries = vec![PathBuf::from("/a.jar"), PathBuf::from("/b.jar")];
        let joined = join_classpath(&entries);
        if cfg!(windows) {
            assert_eq!(joined, "/a.jar;/b.jar");
        } else {
            assert_eq!(joined, "/a.jar:/b.jar");
        }
    }

    #[test]
    fn gc_flags_follow_the_java_version() {
        let temp = tempfile::tempdir().unwrap();
        let modern = gc_args(temp.path(), 25);
        assert!(modern[0].starts_with("-Xlog:gc*"));
        let legacy = gc_args(temp.path(), 8);
        assert!(legacy[0].starts_with("-Xloggc:"));
        assert!(legacy.contains(&"-XX:+PrintGCDetails".to_string()));
    }

    #[test]
    fn rejects_client_launch_without_assets_metadata() {
        // A version with no client download cannot be launched.
        let temp = tempfile::tempdir().unwrap();
        let home = mats_core::MatsHome::new(temp.path().join("home"));
        let config = Config::default();
        let manager = ResourceManager::new(&home, &config).unwrap();
        let java = java_manager::JavaRuntime {
            path: PathBuf::from("java"),
            major: 25,
            version: "25".into(),
            vendor: None,
            arch: None,
            source: java_manager::JavaSource::System,
        };
        let spec = EnvironmentSpec {
            mc_version: "26.3".into(),
            loader: Loader::Vanilla,
            loader_version: None,
            side: mats_core::Side::Client,
            java_major: 25,
            mods: Vec::new(),
            memory_mb: 512,
            display: mats_core::config::DisplayMode::Auto,
            download_assets: false,
            accept_eula: false,
            fresh: false,
        };
        let plan = ResolvedPlan {
            loader: Loader::Vanilla,
            side: mats_core::Side::Client,
            mc_version: "26.3".into(),
            loader_version: None,
            installer: None,
            java_major: Some(25),
            java_major_source: "test".into(),
            candidates: vec![],
            assumed_version: false,
            notes: vec![],
        };
        let version: MojangVersion = serde_json::from_value(serde_json::json!({
            "id": "26.3",
            "mainClass": "net.minecraft.client.main.Main",
            "arguments": {"game": [], "jvm": []}
        }))
        .unwrap();
        let mut records = Vec::new();
        let mut notes = Vec::new();
        let error = prepare_client(
            &config,
            &manager,
            &java,
            &spec,
            &plan,
            temp.path(),
            temp.path(),
            &mut records,
            &mut notes,
            &version,
        )
        .unwrap_err();
        assert!(error.to_string().contains("client JAR"));
    }
}
