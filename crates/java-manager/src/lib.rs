//! Java runtime discovery and provisioning.
//!
//! MATS prefers a JVM already installed on the machine and falls back to
//! downloading an official Eclipse Temurin build from the Adoptium API. Every
//! downloaded archive is verified against the SHA-256 published by Adoptium
//! before extraction.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use mats_core::MatsHome;
use mats_core::config::Config;
use mats_core::fsutil;
use resource_manager::{ResourceManager, ResourceRequest, extract};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum JavaSource {
    /// Found on `PATH`, `JAVA_HOME` or a platform JVM directory.
    System,
    /// Found in (or fetched into) the MATS managed Java cache.
    Managed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JavaRuntime {
    pub path: PathBuf,
    pub major: u32,
    pub version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vendor: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub arch: Option<String>,
    pub source: JavaSource,
}

impl JavaRuntime {
    pub fn describe(&self) -> String {
        format!(
            "Java {} ({}{}) at {}",
            self.major,
            self.version,
            self.vendor
                .as_ref()
                .map(|vendor| format!(", {vendor}"))
                .unwrap_or_default(),
            self.path.display()
        )
    }
}

/// Inspect a `java` executable and return its runtime details.
pub fn inspect_java(path: &Path) -> Option<JavaRuntime> {
    if !path.is_file() {
        return None;
    }
    let output = Command::new(path)
        .args(["-XshowSettings:properties", "-version"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout)
    );
    parse_java_properties(&text).map(|mut runtime| {
        runtime.path = path.to_path_buf();
        runtime
    })
}

/// Parse `java -XshowSettings:properties -version` output.
pub fn parse_java_properties(text: &str) -> Option<JavaRuntime> {
    let mut version = None;
    let mut specification = None;
    let mut vendor = None;
    let mut arch = None;
    for line in text.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim();
        match key {
            "java.version" => version = Some(value.to_string()),
            "java.specification.version" => specification = Some(value.to_string()),
            "java.vendor" => vendor = Some(value.to_string()),
            "os.arch" => arch = Some(value.to_string()),
            _ => {}
        }
    }
    let version = version?;
    let major = specification
        .as_deref()
        .and_then(|spec| spec.split('.').next())
        .and_then(|major| major.parse::<u32>().ok())
        .or_else(|| {
            version
                .split('.')
                .next()
                .and_then(|v| v.parse::<u32>().ok())
        })?;
    // Java 1.8 reports specification 1.8; normalise to 8.
    let major = if major == 1 {
        version
            .split('.')
            .nth(1)
            .and_then(|minor| minor.parse::<u32>().ok())
            .unwrap_or(8)
    } else {
        major
    };
    Some(JavaRuntime {
        path: PathBuf::from("java"),
        major,
        version,
        vendor,
        arch,
        source: JavaSource::System,
    })
}

fn java_executable_name() -> &'static str {
    if cfg!(windows) { "java.exe" } else { "java" }
}

/// Every Java install MATS can find, most preferred first.
pub fn detect_runtimes(home: &MatsHome) -> Vec<JavaRuntime> {
    let mut candidates: Vec<PathBuf> = Vec::new();

    // Explicit configuration wins.
    for variable in ["MATS_JAVA_HOME", "JAVA_HOME"] {
        if let Some(root) = std::env::var_os(variable) {
            candidates.push(PathBuf::from(root).join("bin").join(java_executable_name()));
        }
    }
    // PATH entries.
    if let Some(path_variable) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path_variable) {
            candidates.push(dir.join(java_executable_name()));
        }
    }
    // Managed runtimes.
    if let Ok(entries) = fs::read_dir(home.java_dir()) {
        for entry in entries.flatten() {
            candidates.push(entry.path().join("bin").join(java_executable_name()));
        }
    }
    // Common platform locations.
    for root in platform_java_roots() {
        if let Ok(entries) = fs::read_dir(&root) {
            for entry in entries.flatten() {
                let path = entry.path();
                candidates.push(path.join("bin").join(java_executable_name()));
                candidates.push(path.join("Contents/Home/bin").join(java_executable_name()));
            }
        }
    }

    let mut seen = HashSet::new();
    let mut runtimes = Vec::new();
    for candidate in candidates {
        let canonical = fs::canonicalize(&candidate).unwrap_or_else(|_| candidate.clone());
        if !seen.insert(canonical.clone()) {
            continue;
        }
        let Some(mut runtime) = inspect_java(&canonical) else {
            continue;
        };
        let managed = canonical.starts_with(home.java_dir());
        runtime.source = if managed {
            JavaSource::Managed
        } else {
            JavaSource::System
        };
        runtimes.push(runtime);
    }
    runtimes.sort_by(|left, right| right.major.cmp(&left.major));
    runtimes
}

fn platform_java_roots() -> Vec<PathBuf> {
    #[cfg(target_os = "linux")]
    {
        vec![
            PathBuf::from("/usr/lib/jvm"),
            PathBuf::from("/usr/java"),
            PathBuf::from("/opt/java"),
        ]
    }
    #[cfg(target_os = "macos")]
    {
        vec![
            PathBuf::from("/Library/Java/JavaVirtualMachines"),
            PathBuf::from("/System/Library/Java/JavaVirtualMachines"),
        ]
    }
    #[cfg(windows)]
    {
        let mut roots = Vec::new();
        for program_files in ["ProgramFiles", "ProgramFiles(x86)"] {
            if let Some(root) = std::env::var_os(program_files) {
                let root = PathBuf::from(root);
                roots.push(root.join("Java"));
                roots.push(root.join("Eclipse Adoptium"));
                roots.push(root.join("Microsoft"));
                roots.push(root.join("Zulu"));
                roots.push(root.join("BellSoft"));
            }
        }
        roots
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    {
        Vec::new()
    }
}

/// Find a runtime with at least `major` (exact feature release preferred).
pub fn find_runtime(home: &MatsHome, major: u32) -> Option<JavaRuntime> {
    let runtimes = detect_runtimes(home);
    runtimes
        .iter()
        .find(|runtime| runtime.major == major)
        .or_else(|| runtimes.iter().find(|runtime| runtime.major >= major))
        .cloned()
}

/// Ensure a Java runtime with the requested feature release is available,
/// downloading Temurin when necessary.
pub fn ensure_runtime(
    home: &MatsHome,
    config: &Config,
    manager: &ResourceManager,
    major: u32,
) -> Result<JavaRuntime> {
    if let Some(configured) = &config.runtime.java {
        let path = PathBuf::from(configured);
        let runtime = inspect_java(&path).with_context(|| {
            format!(
                "the configured Java executable `{}` is not usable",
                path.display()
            )
        })?;
        if runtime.major < major {
            bail!(
                "the configured Java `{}` is Java {}, but this test needs Java {major} or newer",
                path.display(),
                runtime.major
            );
        }
        return Ok(runtime);
    }
    if let Some(runtime) = find_runtime(home, major) {
        return Ok(runtime);
    }
    if manager.offline() {
        bail!(
            "Java {major} is required but no compatible JVM was found and offline mode prevents downloading one"
        );
    }
    download_temurin(home, config, manager, major)
}

/// Adoptium release asset selected for this platform.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct AdoptiumAsset {
    pub link: String,
    pub checksum: String,
    pub name: String,
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct AdoptiumBinary {
    package: AdoptiumPackage,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct AdoptiumPackage {
    link: String,
    checksum: String,
    name: String,
    size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct AdoptiumRelease {
    binary: AdoptiumBinary,
}

/// Platform identifiers used by the Adoptium API.
pub fn adoptium_platform() -> (&'static str, &'static str) {
    let os = if cfg!(target_os = "linux") {
        "linux"
    } else if cfg!(target_os = "macos") {
        "mac"
    } else if cfg!(windows) {
        "windows"
    } else {
        "linux"
    };
    let arch = if cfg!(target_arch = "aarch64") {
        "aarch64"
    } else if cfg!(target_arch = "x86_64") {
        "x64"
    } else if cfg!(target_arch = "x86") {
        "x86"
    } else {
        "x64"
    };
    (os, arch)
}

fn download_temurin(
    home: &MatsHome,
    config: &Config,
    manager: &ResourceManager,
    major: u32,
) -> Result<JavaRuntime> {
    let (os, arch) = adoptium_platform();
    let api = config.network.adoptium_api.trim_end_matches('/');
    let metadata_url = format!(
        "{api}/v3/assets/latest/{major}/hotspot?os={os}&architecture={arch}&image_type=jdk&vendor=eclipse"
    );
    let value = manager
        .fetch_json(
            &metadata_url,
            &format!("adoptium-{major}-{os}-{arch}"),
            std::time::Duration::from_secs(3600),
        )
        .with_context(|| format!("querying Adoptium for a Java {major} build for {os}/{arch}"))?;
    let releases: Vec<AdoptiumRelease> =
        serde_json::from_value(value).context("Adoptium returned an unexpected response shape")?;
    let asset = releases
        .into_iter()
        .next()
        .map(|release| AdoptiumAsset {
            link: release.binary.package.link,
            checksum: release.binary.package.checksum,
            name: release.binary.package.name,
            size: release.binary.package.size,
        })
        .with_context(|| format!("Adoptium publishes no Java {major} JDK for {os}/{arch}"))?;

    println!(
        "Downloading Java {major} ({}) from Adoptium…",
        fsutil::human_bytes(asset.size)
    );
    let request = ResourceRequest::new(
        "temurin-jdk",
        mats_core::types::ResourceKind::Java,
        asset.link.clone(),
    )
    .with_size(Some(asset.size));
    let fetched = manager
        .fetch(&request)
        .with_context(|| format!("downloading {major} JDK from {}", asset.link))?;
    let actual_sha256 = fsutil::sha256_file(&fetched.path)?;
    if !actual_sha256.eq_ignore_ascii_case(&asset.checksum) {
        let _ = fs::remove_file(&fetched.path);
        bail!(
            "SHA-256 verification failed for the downloaded Java {major} archive: Adoptium published {} but the download hashes to {}",
            asset.checksum,
            actual_sha256
        );
    }

    let destination = home.java_dir().join(format!("temurin-{major}-{os}-{arch}"));
    if destination
        .join("bin")
        .join(java_executable_name())
        .is_file()
    {
        fsutil::remove_dir_if_exists(&destination)?;
    }
    if asset.name.ends_with(".zip") {
        extract::extract_zip_flattened(&fetched.path, &destination)?;
    } else if asset.name.ends_with(".tar.gz") {
        extract::extract_tar_gz_flattened(&fetched.path, &destination)?;
    } else {
        bail!("unsupported Java archive format `{}`", asset.name);
    }

    let java_path = destination.join("bin").join(java_executable_name());
    fsutil::set_executable(&java_path)?;
    let mut runtime = inspect_java(&java_path).with_context(|| {
        format!(
            "the downloaded Java runtime at {} could not be executed",
            java_path.display()
        )
    })?;
    if runtime.major != major {
        bail!(
            "downloaded Java reports major version {} but {major} was required",
            runtime.major
        );
    }
    runtime.source = JavaSource::Managed;
    Ok(runtime)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PROPERTIES_OUTPUT: &str = r#"
Property settings:
    java.home = /usr/lib/jvm/temurin-25-jdk
    java.specification.version = 25
    java.vendor = Eclipse Adoptium
    java.version = 25.0.2
    os.arch = amd64
"#;

    #[test]
    fn parses_modern_java_properties() {
        let runtime = parse_java_properties(PROPERTIES_OUTPUT).unwrap();
        assert_eq!(runtime.major, 25);
        assert_eq!(runtime.version, "25.0.2");
        assert_eq!(runtime.vendor.as_deref(), Some("Eclipse Adoptium"));
        assert_eq!(runtime.arch.as_deref(), Some("amd64"));
    }

    #[test]
    fn normalises_legacy_java_8() {
        let output = "    java.specification.version = 1.8\n    java.version = 1.8.0_402\n";
        let runtime = parse_java_properties(output).unwrap();
        assert_eq!(runtime.major, 8);
    }

    #[test]
    fn rejects_output_without_a_version() {
        assert!(parse_java_properties("java.home = /x\n").is_none());
    }

    #[test]
    fn detects_installed_java_when_present() {
        let temp = tempfile::tempdir().unwrap();
        let home = MatsHome::new(temp.path().join("home"));
        let runtimes = detect_runtimes(&home);
        // The test machine may not have Java; only assert structural sanity.
        for runtime in &runtimes {
            assert!(runtime.major > 0);
            assert!(!runtime.version.is_empty());
        }
    }

    #[test]
    fn maps_platform_names() {
        let (os, arch) = adoptium_platform();
        assert!(["linux", "mac", "windows"].contains(&os));
        assert!(["x64", "aarch64", "x86"].contains(&arch));
    }

    #[test]
    fn parses_adoptium_response() {
        let json = serde_json::json!([{
            "binary": {
                "package": {
                    "link": "https://github.com/adoptium/temurin25-binaries/jdk.tar.gz",
                    "checksum": "abc123",
                    "name": "OpenJDK25U-jdk_x64_linux_hotspot.tar.gz",
                    "size": 200000000u64
                }
            }
        }]);
        let releases: Vec<AdoptiumRelease> = serde_json::from_value(json).unwrap();
        assert_eq!(releases.len(), 1);
        assert!(releases[0].binary.package.name.contains("OpenJDK25"));
    }

    #[test]
    fn explicit_java_configuration_is_respected() {
        let temp = tempfile::tempdir().unwrap();
        let home = MatsHome::new(temp.path().join("home"));
        let mut config = Config::default();
        config.runtime.java = Some("/definitely/not/java".into());
        let manager = ResourceManager::new(&home, &config).unwrap();
        let error = ensure_runtime(&home, &config, &manager, 25).unwrap_err();
        assert!(error.to_string().contains("configured Java"));
    }
}
