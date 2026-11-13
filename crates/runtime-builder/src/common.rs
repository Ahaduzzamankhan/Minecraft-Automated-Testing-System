//! Shared helpers for building launcher-style runtimes: platform rules,
//! Maven paths, library installation, natives and placeholder substitution.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use mats_core::types::{ResourceKind, ResourceRecord};
use resource_manager::{ResourceManager, ResourceRequest, materialize, read_zip_entry};
use serde::{Deserialize, Serialize};
use version_resolver::mojang::{Library, Rule};

/// Mojang's operating system identifier.
pub fn os_name() -> &'static str {
    if cfg!(target_os = "windows") {
        "windows"
    } else if cfg!(target_os = "macos") {
        "osx"
    } else {
        "linux"
    }
}

/// Bit width token used in native classifier names.
pub fn arch_bits() -> &'static str {
    if cfg!(target_arch = "x86_64") || cfg!(target_arch = "aarch64") {
        "64"
    } else {
        "32"
    }
}

/// Architecture token used in Mojang rules.
pub fn arch_token() -> &'static str {
    if cfg!(target_arch = "aarch64") {
        "aarch64"
    } else if cfg!(target_arch = "x86") {
        "x86"
    } else {
        "x86_64"
    }
}

/// Convert a Maven coordinate (`group:artifact:version[:classifier][@ext]`)
/// into a repository-relative path.
pub fn maven_path(coordinate: &str) -> Option<String> {
    let (body, extension) = match coordinate.split_once('@') {
        Some((body, extension)) => (body, extension),
        None => (coordinate, "jar"),
    };
    let parts: Vec<&str> = body.split(':').collect();
    if parts.len() < 3 {
        return None;
    }
    let group = parts[0];
    let artifact = parts[1];
    let version = parts[2];
    let classifier = parts.get(3).copied();
    if group.is_empty()
        || artifact.is_empty()
        || version.is_empty()
        || !group.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '_')
        })
        || !artifact.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '_')
        })
        || !version.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '_' | '+')
        })
    {
        return None;
    }
    let file_name = match classifier {
        Some(classifier) => format!("{artifact}-{version}-{classifier}.{extension}"),
        None => format!("{artifact}-{version}.{extension}"),
    };
    Some(format!(
        "{}/{}/{version}/{file_name}",
        group.replace('.', "/"),
        artifact
    ))
}

/// Evaluate Mojang library rules for the current platform.
pub fn rule_allows(rules: &[Rule]) -> bool {
    if rules.is_empty() {
        return true;
    }
    let mut allowed = false;
    for rule in rules {
        if rule_matches(rule) {
            allowed = rule.action == "allow";
        }
    }
    allowed
}

fn rule_matches(rule: &Rule) -> bool {
    if let Some(os) = &rule.os {
        if let Some(name) = &os.name {
            if name != os_name() {
                return false;
            }
        }
        if let Some(arch) = &os.arch {
            // Mojang uses `x86` for 32-bit and `x86_64`/`arm64`/`aarch64` for
            // 64-bit. Treat `x86` as 32-bit only.
            let matches = match arch.as_str() {
                "x86" => arch_bits() == "32",
                "x86_64" | "amd64" => arch_token() == "x86_64",
                "arm64" | "aarch64" => arch_token() == "aarch64",
                _ => true,
            };
            if !matches {
                return false;
            }
        }
        // `os.version` patterns are rare (macOS 10.x era) and are ignored
        // rather than guessed at.
    }
    if let Some(features) = &rule.features {
        // MATS never enables launcher features (demo mode, custom resolution,
        // quick play), so any rule requiring a feature does not match.
        if features.values().any(|enabled| *enabled) {
            return false;
        }
    }
    true
}

/// Replace `${placeholder}` occurrences in a launcher argument.
pub fn substitute(template: &str, values: &BTreeMap<&str, String>) -> String {
    let mut output = template.to_string();
    for (key, value) in values {
        let needle = format!("${{{key}}}");
        if output.contains(&needle) {
            output = output.replace(&needle, value);
        }
    }
    output
}

/// Download and place a version's libraries, extracting legacy natives.
pub fn install_libraries(
    libraries: &[Library],
    env_dir: &Path,
    manager: &ResourceManager,
    records: &mut Vec<ResourceRecord>,
    natives_dir: &Path,
) -> Result<Vec<PathBuf>> {
    let mut classpath = Vec::new();
    for library in libraries {
        if !rule_allows(&library.rules) {
            continue;
        }
        if !library.natives.is_empty() {
            install_natives(library, manager, records, natives_dir)?;
            continue;
        }
        let Some(artifact) = &library.downloads.artifact else {
            continue;
        };
        let relative = artifact
            .path
            .clone()
            .or_else(|| maven_path(&library.name))
            .with_context(|| format!("library `{}` has no downloadable path", library.name))?;
        if !artifact.url.starts_with("http") && !artifact.url.starts_with("file://") {
            continue;
        }
        let request = ResourceRequest::new(
            library.name.clone(),
            ResourceKind::Library,
            artifact.url.clone(),
        )
        .with_sha1(artifact.sha1.clone())
        .with_size(artifact.size);
        let fetched = manager.fetch(&request)?;
        records.push(fetched.record());
        let target = env_dir.join("libraries").join(&relative);
        materialize(&fetched.path, &target)?;
        classpath.push(target);
    }
    Ok(classpath)
}

fn install_natives(
    library: &Library,
    manager: &ResourceManager,
    records: &mut Vec<ResourceRecord>,
    natives_dir: &Path,
) -> Result<()> {
    let Some(expression) = library.natives.get(os_name()) else {
        return Ok(());
    };
    let classifier = expression.replace("${arch}", arch_bits());
    let Some(download) = library.downloads.classifiers.get(&classifier) else {
        bail!(
            "library `{}` declares native classifier `{classifier}` but does not provide a download for it",
            library.name
        );
    };
    let request = ResourceRequest::new(
        format!("{}:{classifier}", library.name),
        ResourceKind::Native,
        download.url.clone(),
    )
    .with_sha1(download.sha1.clone())
    .with_size(download.size);
    let fetched = manager.fetch(&request)?;
    records.push(fetched.record());
    std::fs::create_dir_all(natives_dir)
        .with_context(|| format!("creating {}", natives_dir.display()))?;
    let excludes = library
        .extract
        .as_ref()
        .map(|extract| extract.exclude.clone())
        .unwrap_or_default();
    extract_native_archive(&fetched.path, natives_dir, &excludes)?;
    Ok(())
}

/// Extract a native archive, skipping configured entries and META-INF.
pub fn extract_native_archive(archive: &Path, target: &Path, excludes: &[String]) -> Result<()> {
    let staging = target.with_extension("staging-natives");
    resource_manager::extract_zip_safely(archive, &staging)
        .with_context(|| format!("extracting natives from {}", archive.display()))?;
    copy_natives(&staging, target, "", excludes)?;
    let _ = std::fs::remove_dir_all(&staging);
    Ok(())
}

fn copy_natives(source: &Path, target: &Path, prefix: &str, excludes: &[String]) -> Result<()> {
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let relative = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}/{name}")
        };
        if excludes
            .iter()
            .any(|excluded| relative.starts_with(excluded))
        {
            continue;
        }
        if entry.file_type()?.is_dir() {
            copy_natives(&entry.path(), target, &relative, excludes)?;
        } else if !relative.starts_with("META-INF") {
            let destination = target.join(&name);
            std::fs::create_dir_all(target)?;
            std::fs::copy(entry.path(), &destination).with_context(|| {
                format!(
                    "copying native {} to {}",
                    entry.path().display(),
                    destination.display()
                )
            })?;
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct LoaderProfile {
    #[serde(default, rename = "mainClass")]
    pub main_class: Option<String>,
    #[serde(default)]
    pub libraries: Vec<ProfileLibrary>,
    #[serde(default)]
    pub arguments: ProfileArguments,
    #[serde(default, rename = "inheritsFrom")]
    pub inherits_from: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ProfileArguments {
    #[serde(default)]
    pub game: Vec<String>,
    #[serde(default)]
    pub jvm: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ProfileLibrary {
    pub name: String,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub sha1: Option<String>,
    #[serde(default)]
    pub size: Option<u64>,
}

/// Install libraries declared by a loader profile JSON.
pub fn install_profile_libraries(
    libraries: &[ProfileLibrary],
    env_dir: &Path,
    manager: &ResourceManager,
    records: &mut Vec<ResourceRecord>,
    default_base: &str,
) -> Result<Vec<PathBuf>> {
    let mut classpath = Vec::new();
    for library in libraries {
        let Some(relative) = maven_path(&library.name) else {
            tracing::warn!(name = %library.name, "skipping unparseable loader library coordinate");
            continue;
        };
        let base = library
            .url
            .clone()
            .unwrap_or_else(|| default_base.to_string());
        let url = format!("{}/{}", base.trim_end_matches('/'), relative);
        let request = ResourceRequest::new(library.name.clone(), ResourceKind::Loader, url.clone())
            .with_sha1(library.sha1.clone())
            .with_size(library.size);
        let fetched = manager
            .fetch(&request)
            .with_context(|| format!("downloading loader library `{}`", library.name))?;
        records.push(fetched.record());
        let target = env_dir.join("libraries").join(&relative);
        materialize(&fetched.path, &target)?;
        classpath.push(target);
    }
    Ok(classpath)
}

/// Copy a version JAR into `versions/<id>/<id>.jar` and return its path.
pub fn place_version_jar(env_dir: &Path, version_id: &str, cached_jar: &Path) -> Result<PathBuf> {
    let target = env_dir
        .join("versions")
        .join(version_id)
        .join(format!("{version_id}.jar"));
    materialize(cached_jar, &target)?;
    Ok(target)
}

/// Assets root used by the client (`--assetsDir`).
pub fn assets_dir(env_dir: &Path) -> PathBuf {
    env_dir.join("assets")
}

/// Fetch the asset index JSON; object files are optional.
pub fn install_asset_index(
    env_dir: &Path,
    index_url: &str,
    index_sha1: Option<&str>,
    index_size: Option<u64>,
    index_id: &str,
    manager: &ResourceManager,
    records: &mut Vec<ResourceRecord>,
    download_objects: bool,
) -> Result<PathBuf> {
    let request = ResourceRequest::new(
        format!("asset-index-{index_id}"),
        ResourceKind::AssetIndex,
        index_url.to_string(),
    )
    .with_sha1(index_sha1.map(str::to_string))
    .with_size(index_size);
    let fetched = manager.fetch(&request)?;
    records.push(fetched.record());
    let target = assets_dir(env_dir)
        .join("indexes")
        .join(format!("{index_id}.json"));
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::copy(&fetched.path, &target).with_context(|| {
        format!(
            "copying asset index {} to {}",
            fetched.path.display(),
            target.display()
        )
    })?;
    if download_objects {
        let text = std::fs::read_to_string(&target)?;
        let index: serde_json::Value =
            serde_json::from_str(&text).context("the asset index is not valid JSON")?;
        let mut requests = Vec::new();
        if let Some(objects) = index.get("objects").and_then(|value| value.as_object()) {
            for object in objects.values() {
                let Some(hash) = object.get("hash").and_then(|value| value.as_str()) else {
                    continue;
                };
                let size = object.get("size").and_then(|value| value.as_u64());
                let url = format!(
                    "https://resources.download.minecraft.net/{}/{}",
                    &hash[..2],
                    hash
                );
                requests.push(
                    ResourceRequest::new(format!("asset-{}", &hash[..8]), ResourceKind::Asset, url)
                        .with_sha1(Some(hash.to_string()))
                        .with_size(size),
                );
            }
        }
        let fetched = manager.fetch_many(&requests)?;
        for resource in fetched {
            let target = assets_dir(env_dir)
                .join("objects")
                .join(&resource.sha1[..2])
                .join(&resource.sha1);
            materialize(&resource.path, &target)?;
        }
    }
    Ok(target)
}

/// Read a version JSON that a loader installer placed in an environment.
pub fn read_embedded_version_json(path: &Path) -> Result<serde_json::Value> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))
}

/// Read one entry from a JAR without executing it.
pub fn read_jar_entry(jar: &Path, name: &str) -> Result<Option<Vec<u8>>> {
    read_zip_entry(jar, name)
}

/// Recursively find files whose name matches a predicate.
pub fn find_file(root: &Path, predicate: &dyn Fn(&str) -> bool) -> Option<PathBuf> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return None;
    };
    let mut dirs = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            dirs.push(path);
        } else if let Some(name) = path.file_name().and_then(|name| name.to_str()) {
            if predicate(name) {
                return Some(path);
            }
        }
    }
    for dir in dirs {
        if let Some(found) = find_file(&dir, predicate) {
            return Some(found);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use version_resolver::mojang::OsRule;

    #[test]
    fn maven_coordinates_map_to_paths() {
        assert_eq!(
            maven_path("net.fabricmc:fabric-loader:0.19.5").unwrap(),
            "net/fabricmc/fabric-loader/0.19.5/fabric-loader-0.19.5.jar"
        );
        assert_eq!(
            maven_path("org.lwjgl:lwjgl:3.3.3:natives-linux").unwrap(),
            "org/lwjgl/lwjgl/3.3.3/lwjgl-3.3.3-natives-linux.jar"
        );
        assert_eq!(
            maven_path("com.example:lib:1.0@zip").unwrap(),
            "com/example/lib/1.0/lib-1.0.zip"
        );
        assert!(maven_path("not-a-coordinate").is_none());
        assert!(maven_path("group:artifact:../escape").is_none());
    }

    #[test]
    fn rules_filter_by_os() {
        let windows_only = vec![Rule {
            action: "allow".into(),
            os: Some(OsRule {
                name: Some("windows".into()),
                ..OsRule::default()
            }),
            features: None,
        }];
        assert_eq!(rule_allows(&windows_only), cfg!(windows));
        assert!(rule_allows(&[]));
        let disallowed = vec![Rule {
            action: "disallow".into(),
            os: None,
            features: None,
        }];
        assert!(!rule_allows(&disallowed));
    }

    #[test]
    fn feature_gated_rules_do_not_match() {
        let rule = vec![Rule {
            action: "allow".into(),
            os: None,
            features: Some(BTreeMap::from([("is_demo_user".to_string(), true)])),
        }];
        assert!(!rule_allows(&rule));
    }

    #[test]
    fn substitution_replaces_placeholders() {
        let values = BTreeMap::from([
            ("classpath", "a.jar:b.jar".to_string()),
            ("natives_directory", "/tmp/n".to_string()),
        ]);
        assert_eq!(
            substitute(
                "-cp ${classpath} -Djava.library.path=${natives_directory}",
                &values
            ),
            "-cp a.jar:b.jar -Djava.library.path=/tmp/n"
        );
        assert_eq!(substitute("no placeholders", &values), "no placeholders");
    }

    #[test]
    fn finds_files_recursively() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join("a/b")).unwrap();
        std::fs::write(temp.path().join("a/b/unix_args.txt"), "args").unwrap();
        let found = find_file(temp.path(), &|name| name == "unix_args.txt").unwrap();
        assert!(found.ends_with("a/b/unix_args.txt"));
        assert!(find_file(temp.path(), &|name| name == "absent.txt").is_none());
    }
}
