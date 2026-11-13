//! Dedicated server runtime construction for every supported loader.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use java_manager::JavaRuntime;
use mats_core::config::Config;
use mats_core::types::{Loader, ResourceKind, ResourceRecord};
use process_monitor::{LaunchSpec, launch};
use resource_manager::{ResourceManager, ResourceRequest};
use version_resolver::mojang::MojangVersion;
use version_resolver::{LoaderResolver, ResolvedPlan};

use crate::client::java_environment;
use crate::common;
use crate::{EnvironmentSpec, LaunchCommand};

const INSTALLER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(900);

#[allow(clippy::too_many_arguments)]
pub(crate) fn prepare_server(
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
) -> Result<LaunchCommand> {
    if !spec.accept_eula {
        bail!(
            "dedicated server tests require accepting the Minecraft EULA. Re-run with --accept-eula to acknowledge https://aka.ms/MinecraftEULA"
        );
    }
    let server_root = dir.join("server");
    std::fs::create_dir_all(&server_root)
        .with_context(|| format!("creating {}", server_root.display()))?;
    std::fs::create_dir_all(server_root.join("logs"))?;
    write_server_properties(&server_root)?;
    std::fs::write(server_root.join("eula.txt"), "eula=true\n")
        .context("writing eula.txt for the dedicated server test")?;

    match spec.loader {
        Loader::Vanilla => {
            let download = version
                .downloads
                .server
                .clone()
                .context("Mojang metadata for this version does not include a server JAR")?;
            let request = ResourceRequest::new(
                "minecraft-server",
                ResourceKind::MinecraftServer,
                download.url.clone(),
            )
            .with_sha1(download.sha1.clone())
            .with_size(download.size);
            let fetched = manager.fetch(&request)?;
            records.push(fetched.record());
            let jar = server_root.join("server.jar");
            resource_manager::materialize(&fetched.path, &jar)?;
            notes.push("vanilla server JAR prepared".into());
            Ok(jar_launch(
                java,
                spec,
                &server_root,
                log_dir,
                "server.jar",
                "net.minecraft.server.Main",
                version,
            ))
        }
        Loader::Fabric => {
            let loader_version = plan
                .loader_version
                .as_deref()
                .context("the resolved plan has no Fabric Loader version")?;
            let installer = plan
                .installer
                .as_ref()
                .context("no Fabric installer could be resolved")?;
            let installer_jar = fetch_installer(
                manager,
                "fabric-installer",
                &installer.url,
                installer.sha1.as_deref(),
                records,
            )?;
            run_installer(
                java,
                &installer_jar,
                &[
                    "server".to_string(),
                    "-dir".to_string(),
                    server_root.display().to_string(),
                    "-mcversion".to_string(),
                    spec.mc_version.clone(),
                    "-loader".to_string(),
                    loader_version.to_string(),
                    "-downloadMinecraft".to_string(),
                ],
                dir,
                log_dir,
                "fabric-installer",
                notes,
            )?;
            let launcher_jar = server_root.join("fabric-server-launch.jar");
            if !launcher_jar.is_file() {
                bail!(
                    "the Fabric installer finished but {} is missing; see logs/fabric-installer.log",
                    launcher_jar.display()
                );
            }
            notes.push(format!("Fabric Loader {loader_version} server installed"));
            Ok(jar_launch(
                java,
                spec,
                &server_root,
                log_dir,
                "fabric-server-launch.jar",
                "net.fabricmc.loader.impl.launch.server.FabricServerLauncher",
                version,
            ))
        }
        Loader::Quilt => {
            let loader_version = plan
                .loader_version
                .as_deref()
                .context("the resolved plan has no Quilt Loader version")?;
            let installer = plan
                .installer
                .as_ref()
                .context("no Quilt installer could be resolved")?;
            let installer_jar = fetch_installer(
                manager,
                "quilt-installer",
                &installer.url,
                installer.sha1.as_deref(),
                records,
            )?;
            run_installer(
                java,
                &installer_jar,
                &[
                    "install".to_string(),
                    "server".to_string(),
                    spec.mc_version.clone(),
                    loader_version.to_string(),
                    format!("--install-dir={}", server_root.display()),
                    "--download-server".to_string(),
                ],
                dir,
                log_dir,
                "quilt-installer",
                notes,
            )?;
            let launcher_jar = server_root.join("quilt-server-launch.jar");
            if !launcher_jar.is_file() {
                bail!(
                    "the Quilt installer finished but {} is missing; see logs/quilt-installer.log",
                    launcher_jar.display()
                );
            }
            notes.push(format!("Quilt Loader {loader_version} server installed"));
            Ok(jar_launch(
                java,
                spec,
                &server_root,
                log_dir,
                "quilt-server-launch.jar",
                "org.quiltmc.loader.impl.launch.server.QuiltServerLauncher",
                version,
            ))
        }
        Loader::Forge | Loader::NeoForge => {
            let loader_version = plan
                .loader_version
                .as_deref()
                .context("the resolved plan has no loader version")?;
            let resolver = LoaderResolver::new(manager, &config.network);
            let url = resolver.loader_installer_url(spec.loader, &spec.mc_version, loader_version);
            let sha1 = manager
                .try_fetch_text(&format!("{url}.sha1"))?
                .map(|text| text.trim().to_string());
            let installer_jar = fetch_installer(
                manager,
                &format!("{}-installer", spec.loader.as_str()),
                &url,
                sha1.as_deref(),
                records,
            )?;
            run_installer(
                java,
                &installer_jar,
                &["--installServer".to_string()],
                &server_root,
                log_dir,
                &format!("{}-installer", spec.loader.as_str()),
                notes,
            )?;
            let args_name = if cfg!(windows) {
                "win_args.txt"
            } else {
                "unix_args.txt"
            };
            let args_file = common::find_file(&server_root, &|name| name == args_name)
                .with_context(|| {
                    format!(
                        "the {} installer completed but no {args_name} launcher argument file was produced; see logs",
                        spec.loader.display_name()
                    )
                })?;
            let relative = args_file
                .strip_prefix(&server_root)
                .unwrap_or(&args_file)
                .display()
                .to_string();
            notes.push(format!(
                "{} {loader_version} server installed via the official installer",
                spec.loader.display_name()
            ));
            let mut args = vec![
                format!("-Xmx{}M", spec.memory_mb),
                "-XX:+ExitOnOutOfMemoryError".to_string(),
                "-Dfile.encoding=UTF-8".to_string(),
                "-Dmats.side=server".to_string(),
            ];
            args.extend(crate::client::gc_args(log_dir, java.major));
            args.push(format!("@{relative}"));
            args.push("nogui".to_string());
            Ok(LaunchCommand {
                program: java.path.clone(),
                args,
                cwd: server_root.clone(),
                env: java_environment(dir),
                main_class: format!("{} server launcher (argfile)", spec.loader.display_name()),
                classpath_entries: 0,
                argfiles: vec![args_file],
            })
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn jar_launch(
    java: &JavaRuntime,
    spec: &EnvironmentSpec,
    server_root: &Path,
    log_dir: &Path,
    jar: &str,
    main_class: &str,
    _version: &MojangVersion,
) -> LaunchCommand {
    let mut args = vec![
        format!("-Xmx{}M", spec.memory_mb),
        "-XX:+ExitOnOutOfMemoryError".to_string(),
        "-Dfile.encoding=UTF-8".to_string(),
        "-Dmats.side=server".to_string(),
    ];
    args.extend(crate::client::gc_args(log_dir, java.major));
    args.push("-jar".to_string());
    args.push(server_root.join(jar).display().to_string());
    args.push("nogui".to_string());
    LaunchCommand {
        program: java.path.clone(),
        args,
        cwd: server_root.to_path_buf(),
        env: Vec::new(),
        main_class: main_class.to_string(),
        classpath_entries: 0,
        argfiles: Vec::new(),
    }
}

fn write_server_properties(server_root: &Path) -> Result<()> {
    let path = server_root.join("server.properties");
    if path.is_file() {
        return Ok(());
    }
    let properties = "\
# Written by MATS for isolated, offline test runs.
online-mode=false
level-type=minecraft:flat
level-name=world
spawn-protection=0
view-distance=4
simulation-distance=4
max-players=1
motd=MATS test server
enable-status=false
sync-chunk-writes=false
max-tick-time=120000
pause-when-empty-seconds=0
";
    std::fs::write(&path, properties).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

fn fetch_installer(
    manager: &ResourceManager,
    name: &str,
    url: &str,
    sha1: Option<&str>,
    records: &mut Vec<ResourceRecord>,
) -> Result<PathBuf> {
    let request = ResourceRequest::new(name, ResourceKind::Installer, url)
        .with_sha1(sha1.map(str::to_string));
    let fetched = manager
        .fetch(&request)
        .with_context(|| format!("downloading the {name} installer from {url}"))?;
    records.push(fetched.record());
    Ok(fetched.path)
}

#[allow(clippy::too_many_arguments)]
fn run_installer(
    java: &JavaRuntime,
    installer_jar: &Path,
    args: &[String],
    cwd: &Path,
    log_dir: &Path,
    label: &str,
    notes: &mut Vec<String>,
) -> Result<()> {
    let mut full_args = vec![
        format!("-Xmx{}M", 1024),
        "-jar".to_string(),
        installer_jar.display().to_string(),
    ];
    full_args.extend(args.iter().cloned());
    let spec = LaunchSpec {
        program: java.path.clone(),
        args: full_args,
        cwd: cwd.to_path_buf(),
        timeout: INSTALLER_TIMEOUT,
        log_dir: log_dir.to_path_buf(),
        label: format!("{label}-install"),
        ..LaunchSpec::default()
    };
    let outcome = launch(&spec).with_context(|| {
        format!(
            "running the {label} installer; see {}/{label}-install.log",
            log_dir.display()
        )
    })?;
    if !outcome.succeeded() {
        let tail = outcome
            .log_tail
            .lines()
            .rev()
            .take(12)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>()
            .join("\n");
        bail!(
            "the {label} installer failed with exit code {:?}; last output:\n{tail}",
            outcome.exit_code
        );
    }
    notes.push(format!("{label} installer completed"));
    Ok(())
}

/// Ensure EULA and server properties exist for an already prepared server.
pub fn refresh_server_files(environment_dir: &Path, accept_eula: bool) -> Result<()> {
    let server_root = environment_dir.join("server");
    if !server_root.is_dir() {
        return Ok(());
    }
    write_server_properties(&server_root)?;
    if accept_eula {
        std::fs::write(server_root.join("eula.txt"), "eula=true\n")?;
    }
    Ok(())
}

/// Read the prepared server directory for diagnostics.
pub fn server_root(environment_dir: &Path) -> PathBuf {
    environment_dir.join("server")
}

#[cfg(test)]
mod tests {
    use super::*;
    use mats_core::MatsHome;

    #[test]
    fn writes_offline_test_properties() {
        let temp = tempfile::tempdir().unwrap();
        write_server_properties(temp.path()).unwrap();
        let text = std::fs::read_to_string(temp.path().join("server.properties")).unwrap();
        assert!(text.contains("online-mode=false"));
        assert!(text.contains("level-type=minecraft:flat"));
        // Existing files are preserved.
        std::fs::write(temp.path().join("server.properties"), "custom=true").unwrap();
        write_server_properties(temp.path()).unwrap();
        assert_eq!(
            std::fs::read_to_string(temp.path().join("server.properties")).unwrap(),
            "custom=true"
        );
    }

    #[test]
    fn refresh_writes_eula_only_when_accepted() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join("server")).unwrap();
        refresh_server_files(temp.path(), false).unwrap();
        assert!(!temp.path().join("server/eula.txt").exists());
        refresh_server_files(temp.path(), true).unwrap();
        assert_eq!(
            std::fs::read_to_string(temp.path().join("server/eula.txt")).unwrap(),
            "eula=true\n"
        );
    }

    #[test]
    fn server_root_is_inside_the_environment() {
        assert_eq!(
            server_root(Path::new("/env/abc")),
            PathBuf::from("/env/abc/server")
        );
    }

    #[test]
    fn eula_is_required_before_preparing_a_server() {
        let temp = tempfile::tempdir().unwrap();
        let home = MatsHome::new(temp.path().join("home"));
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
            side: mats_core::Side::Server,
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
            side: mats_core::Side::Server,
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
            "id": "26.3", "mainClass": "net.minecraft.server.Main",
            "arguments": {"game": [], "jvm": []}
        }))
        .unwrap();
        let mut records = Vec::new();
        let mut notes = Vec::new();
        let error = prepare_server(
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
        assert!(error.to_string().contains("--accept-eula"));
    }
}
