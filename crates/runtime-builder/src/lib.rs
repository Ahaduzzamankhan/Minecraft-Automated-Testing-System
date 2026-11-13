//! Construction of isolated Minecraft test environments.
//!
//! MATS launches the real game through the official launcher mechanisms: the
//! Mojang version JSON, the loader's own profile JSON and the official loader
//! installers. Nothing in Minecraft is reimplemented or stubbed.

pub mod client;
pub mod common;
pub mod server;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use java_manager::JavaRuntime;
use mats_core::config::{Config, DisplayMode};
use mats_core::types::{Loader, ResourceRecord, Side};
use mats_core::{MatsHome, fsutil};
use resource_manager::ResourceManager;
use serde::{Deserialize, Serialize};
use version_resolver::ResolvedPlan;
use version_resolver::mojang::{MojangClient, MojangVersion};

/// Everything that determines what a prepared environment contains.
#[derive(Debug, Clone)]
pub struct EnvironmentSpec {
    pub mc_version: String,
    pub loader: Loader,
    pub loader_version: Option<String>,
    pub side: Side,
    pub java_major: u32,
    /// Mod JARs to install into `mods/`.
    pub mods: Vec<PathBuf>,
    pub memory_mb: u32,
    pub display: DisplayMode,
    pub download_assets: bool,
    /// Accept the Minecraft EULA for dedicated server tests.
    pub accept_eula: bool,
    /// Rebuild even when a matching environment exists.
    pub fresh: bool,
}

#[derive(Debug, Clone)]
pub struct LaunchCommand {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub env: Vec<(String, String)>,
    pub main_class: String,
    pub classpath_entries: usize,
    /// Argument files passed with `@` (Forge/NeoForge).
    pub argfiles: Vec<PathBuf>,
}

impl LaunchCommand {
    pub fn display(&self) -> String {
        let mut parts = vec![self.program.display().to_string()];
        parts.extend(self.args.iter().cloned());
        parts.join(" ")
    }
}

#[derive(Debug, Clone)]
pub struct PreparedEnvironment {
    pub id: String,
    pub dir: PathBuf,
    pub launch: LaunchCommand,
    pub resources: Vec<ResourceRecord>,
    pub notes: Vec<String>,
    pub log_dir: PathBuf,
    pub assets_available: bool,
    pub reused: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct EnvironmentMarker {
    schema: u32,
    id: String,
    mc_version: String,
    loader: Loader,
    loader_version: Option<String>,
    side: Side,
    java_major: u32,
    created_at: String,
    mods: Vec<String>,
}

/// Prepare (or reuse) an isolated environment and return its launch command.
pub fn prepare(
    home: &MatsHome,
    config: &Config,
    manager: &ResourceManager,
    java: &JavaRuntime,
    plan: &ResolvedPlan,
    spec: &EnvironmentSpec,
) -> Result<PreparedEnvironment> {
    home.ensure_layout()?;
    let id = environment_id(spec)?;
    let dir = home.envs_dir().join(&id);
    let log_dir = dir.join("logs");
    let marker_path = dir.join("mats-env.json");
    std::fs::create_dir_all(&log_dir).with_context(|| format!("creating {}", log_dir.display()))?;

    let mut notes = plan.notes.clone();
    if spec.mc_version != plan.mc_version {
        notes.push(format!(
            "environment was requested for Minecraft {} but the plan resolved {}",
            spec.mc_version, plan.mc_version
        ));
    }

    let java_ok = java.major >= spec.java_major;
    if !java_ok {
        bail!(
            "Java {} is required for Minecraft {} but {} was provided",
            spec.java_major,
            spec.mc_version,
            java.describe()
        );
    }

    let marker_matches = !spec.fresh
        && read_marker(&marker_path)
            .map(|marker| marker.matches(spec))
            .unwrap_or(false);
    let launch_exists = marker_matches && dir.join(".mats-ready").is_file();

    let mut resources = Vec::new();
    let launch = if launch_exists {
        let launch = reconstruct_launch(
            home,
            config,
            manager,
            java,
            plan,
            spec,
            &dir,
            &log_dir,
            &mut resources,
            &mut notes,
        )?;
        return Ok(PreparedEnvironment {
            id,
            dir: dir.clone(),
            launch,
            resources,
            notes,
            log_dir,
            assets_available: spec.side.includes_client() && common::assets_dir(&dir).is_dir(),
            reused: true,
        });
    } else {
        fsutil::remove_dir_if_exists(&dir)?;
        std::fs::create_dir_all(&log_dir)?;
        let launch = build_launch(
            home,
            config,
            manager,
            java,
            plan,
            spec,
            &dir,
            &log_dir,
            &mut resources,
            &mut notes,
        )?;
        let marker = EnvironmentMarker {
            schema: 1,
            id: id.clone(),
            mc_version: spec.mc_version.clone(),
            loader: spec.loader,
            loader_version: spec.loader_version.clone(),
            side: spec.side,
            java_major: spec.java_major,
            created_at: chrono::Utc::now().to_rfc3339(),
            mods: spec
                .mods
                .iter()
                .filter_map(|path| {
                    path.file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                })
                .collect(),
        };
        fsutil::atomic_write(&marker_path, &serde_json::to_vec_pretty(&marker)?)?;
        fsutil::atomic_write(&dir.join(".mats-ready"), b"ready")?;
        launch
    };

    // Mods are refreshed on every prepare so stale JARs never leak between
    // test runs that share an environment.
    install_mods(&dir, &spec.mods)?;

    Ok(PreparedEnvironment {
        id,
        dir: dir.clone(),
        launch,
        resources,
        notes,
        log_dir,
        assets_available: spec.side.includes_client() && common::assets_dir(&dir).is_dir(),
        reused: false,
    })
}

#[allow(clippy::too_many_arguments)]
fn build_launch(
    _home: &MatsHome,
    config: &Config,
    manager: &ResourceManager,
    java: &JavaRuntime,
    plan: &ResolvedPlan,
    spec: &EnvironmentSpec,
    dir: &Path,
    log_dir: &Path,
    resources: &mut Vec<ResourceRecord>,
    notes: &mut Vec<String>,
) -> Result<LaunchCommand> {
    let mojang = MojangClient::new(manager, &config.network);
    let version = mojang
        .version_json_by_id(&spec.mc_version)
        .with_context(|| format!("reading Mojang metadata for Minecraft {}", spec.mc_version))?;
    let (launch, assets_available) = build_side_launch(
        _home, config, manager, java, plan, spec, dir, log_dir, resources, notes, &version,
    )?;
    if spec.side.includes_client() {
        notes.push(if assets_available {
            "client assets index is available".into()
        } else {
            "client started without a full asset object set; missing textures/sounds do not fail the test"
                .into()
        });
    }
    Ok(launch)
}

#[allow(clippy::too_many_arguments)]
fn reconstruct_launch(
    _home: &MatsHome,
    config: &Config,
    manager: &ResourceManager,
    java: &JavaRuntime,
    plan: &ResolvedPlan,
    spec: &EnvironmentSpec,
    dir: &Path,
    log_dir: &Path,
    resources: &mut Vec<ResourceRecord>,
    notes: &mut Vec<String>,
) -> Result<LaunchCommand> {
    // The environment contents are already on disk; rebuilding the command
    // only re-reads metadata and re-checks the files.
    let mojang = MojangClient::new(manager, &config.network);
    let version = mojang.version_json_by_id(&spec.mc_version)?;
    build_side_launch(
        _home, config, manager, java, plan, spec, dir, log_dir, resources, notes, &version,
    )
    .map(|(launch, _)| launch)
}

#[allow(clippy::too_many_arguments)]
fn build_side_launch(
    home: &MatsHome,
    config: &Config,
    manager: &ResourceManager,
    java: &JavaRuntime,
    plan: &ResolvedPlan,
    spec: &EnvironmentSpec,
    dir: &Path,
    log_dir: &Path,
    resources: &mut Vec<ResourceRecord>,
    notes: &mut Vec<String>,
    version: &MojangVersion,
) -> Result<(LaunchCommand, bool)> {
    if spec.side.includes_client() {
        match spec.loader {
            Loader::Fabric | Loader::Quilt | Loader::Vanilla => {
                return client::prepare_client(
                    config, manager, java, spec, plan, dir, log_dir, resources, notes, version,
                );
            }
            other => {
                bail!(
                    "{} client launches are not supported by MATS; run the test with --side server \
                     (Fabric, Quilt and Vanilla clients are supported)",
                    other.display_name()
                );
            }
        }
    }
    let _ = home;
    server::prepare_server(
        config, manager, java, spec, plan, dir, log_dir, resources, notes, version,
    )
    .map(|launch| (launch, false))
}

fn install_mods(dir: &Path, mods: &[PathBuf]) -> Result<()> {
    let mods_dir = dir.join("mods");
    std::fs::create_dir_all(&mods_dir)
        .with_context(|| format!("creating {}", mods_dir.display()))?;
    // Remove previously installed JARs; other files (config) are preserved.
    if let Ok(entries) = std::fs::read_dir(&mods_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|extension| extension.to_str()) == Some("jar") {
                let _ = std::fs::remove_file(path);
            }
        }
    }
    for mod_path in mods {
        if !mod_path.is_file() {
            bail!("mod file `{}` does not exist", mod_path.display());
        }
        let name = mod_path
            .file_name()
            .with_context(|| format!("`{}` has no file name", mod_path.display()))?;
        let target = mods_dir.join(name);
        std::fs::copy(mod_path, &target).with_context(|| {
            format!(
                "copying mod {} into {}",
                mod_path.display(),
                target.display()
            )
        })?;
    }
    Ok(())
}

fn environment_id(spec: &EnvironmentSpec) -> Result<String> {
    let mut hasher_input = format!(
        "mc={};loader={};loader_version={};side={};java={};assets={}",
        spec.mc_version,
        spec.loader.as_str(),
        spec.loader_version.as_deref().unwrap_or("auto"),
        spec.side.as_str(),
        spec.java_major,
        spec.download_assets
    );
    let mut mods: Vec<(String, String)> = Vec::new();
    for path in &spec.mods {
        mods.push((
            path.file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default(),
            fsutil::sha1_file(path).unwrap_or_default(),
        ));
    }
    mods.sort();
    for (name, digest) in mods {
        hasher_input.push_str(&format!(";{name}={digest}"));
    }
    Ok(fsutil::sha256_bytes(hasher_input.as_bytes())[..16].to_string())
}

impl EnvironmentMarker {
    fn matches(&self, spec: &EnvironmentSpec) -> bool {
        self.schema == 1
            && self.mc_version == spec.mc_version
            && self.loader == spec.loader
            && self.side == spec.side
            && self.java_major <= spec.java_major
            && self.loader_version.as_deref() == spec.loader_version.as_deref()
    }
}

fn read_marker(path: &Path) -> Option<EnvironmentMarker> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

/// Values shared by every launcher argument template.
pub(crate) fn base_placeholders(
    spec: &EnvironmentSpec,
    dir: &Path,
    java_path: &Path,
) -> BTreeMap<&'static str, String> {
    let mut values = BTreeMap::new();
    values.insert("version_type", "release".to_string());
    values.insert("launcher_name", "mats".to_string());
    values.insert("launcher_version", env!("CARGO_PKG_VERSION").to_string());
    values.insert("game_directory", dir.join("game").display().to_string());
    values.insert("user_type", "legacy".to_string());
    values.insert("auth_player_name", "MATS".to_string());
    values.insert("auth_uuid", "00000000000000000000000000000000".to_string());
    values.insert("auth_access_token", "0".to_string());
    values.insert("clientid", String::new());
    values.insert("auth_xuid", String::new());
    values.insert("version_name", spec.mc_version.clone());
    values.insert("assets_root", common::assets_dir(dir).display().to_string());
    values.insert(
        "natives_directory",
        dir.join("natives").display().to_string(),
    );
    values.insert(
        "library_directory",
        dir.join("libraries").display().to_string(),
    );
    values.insert(
        "classpath_separator",
        if cfg!(windows) { ";" } else { ":" }.to_string(),
    );
    values.insert("resolution_width", "854".to_string());
    values.insert("resolution_height", "480".to_string());
    values.insert("quickPlayPath", String::new());
    values.insert("quickPlaySingleplayer", String::new());
    values.insert("quickPlayMultiplayer", String::new());
    values.insert("quickPlayRealms", String::new());
    values.insert("java_executable", java_path.display().to_string());
    let _ = spec;
    values
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> EnvironmentSpec {
        EnvironmentSpec {
            mc_version: "26.3".into(),
            loader: Loader::Fabric,
            loader_version: Some("0.19.5".into()),
            side: Side::Server,
            java_major: 25,
            mods: Vec::new(),
            memory_mb: 1024,
            display: DisplayMode::Auto,
            download_assets: false,
            accept_eula: false,
            fresh: false,
        }
    }

    #[test]
    fn environment_ids_are_stable_and_content_sensitive() {
        let first = environment_id(&spec()).unwrap();
        let second = environment_id(&spec()).unwrap();
        assert_eq!(first, second);
        assert_eq!(first.len(), 16);

        let mut other = spec();
        other.mc_version = "26.2".into();
        assert_ne!(environment_id(&other).unwrap(), first);

        let temp = tempfile::tempdir().unwrap();
        let jar = temp.path().join("mod.jar");
        std::fs::write(&jar, b"mod").unwrap();
        other.mods = vec![jar.clone()];
        let with_mod = environment_id(&other).unwrap();
        assert_ne!(with_mod, environment_id(&spec()).unwrap());
        std::fs::write(&jar, b"other mod").unwrap();
        assert_ne!(environment_id(&other).unwrap(), with_mod);
    }

    #[test]
    fn marker_matching_requires_same_target() {
        let spec = spec();
        let marker = EnvironmentMarker {
            schema: 1,
            id: "x".into(),
            mc_version: spec.mc_version.clone(),
            loader: spec.loader,
            loader_version: spec.loader_version.clone(),
            side: spec.side,
            java_major: 25,
            created_at: String::new(),
            mods: Vec::new(),
        };
        assert!(marker.matches(&spec));
        let mut different = spec.clone();
        different.mc_version = "26.2".into();
        assert!(!marker.matches(&different));
    }

    #[test]
    fn mods_are_copied_and_stale_jars_removed() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("env");
        std::fs::create_dir_all(dir.join("mods")).unwrap();
        std::fs::write(dir.join("mods/old.jar"), b"old").unwrap();
        std::fs::write(dir.join("mods/keep.txt"), b"keep").unwrap();
        let source = temp.path().join("new.jar");
        std::fs::write(&source, b"new").unwrap();
        install_mods(&dir, &[source]).unwrap();
        assert!(!dir.join("mods/old.jar").exists());
        assert!(dir.join("mods/keep.txt").exists());
        assert_eq!(std::fs::read(dir.join("mods/new.jar")).unwrap(), b"new");
    }

    #[test]
    fn missing_mod_is_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let error = install_mods(temp.path(), &[temp.path().join("nope.jar")]).unwrap_err();
        assert!(error.to_string().contains("does not exist"));
    }
}
