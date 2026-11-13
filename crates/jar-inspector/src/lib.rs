//! Static inspection of Minecraft mod JAR files.
//!
//! Inspection never executes the mod. It opens the archive, extracts mod
//! metadata, mixin configurations and access wideners, identifies the loader
//! from several independent signals and reads class file headers and constant
//! pools to determine the Java version the mod was compiled for and which
//! Minecraft classes it links against.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use mats_core::types::InspectionSummary;
use mats_core::{Loader, fsutil};
use serde::{Deserialize, Serialize};
use zip::ZipArchive;

/// Maximum size of a single metadata file read from the archive.
const MAX_METADATA_BYTES: u64 = 4 * 1024 * 1024;
/// Maximum size of a nested JAR read from the archive.
const MAX_NESTED_JAR_BYTES: u64 = 16 * 1024 * 1024;
/// Maximum number of class files analysed for bytecode facts.
const MAX_CLASSES_ANALYSED: usize = 20_000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JarEntryInfo {
    pub name: String,
    pub uncompressed_size: u64,
    pub is_dir: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MixinConfigText {
    pub path: String,
    pub contents: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccessWidenerInfo {
    pub path: String,
    /// `accessWidener v1 named` style header, when present.
    pub header: Option<String>,
    pub namespace: Option<String>,
    pub entry_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NestedJarInfo {
    pub path: String,
    pub mod_id: Option<String>,
    pub version: Option<String>,
    pub loader: Option<Loader>,
    pub size_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct BytecodeFacts {
    pub class_count: usize,
    /// Java feature release derived from the class file major version.
    pub java_majors: BTreeMap<u32, usize>,
    /// Top-level packages declared inside the mod.
    pub packages: BTreeMap<String, usize>,
    /// `net/minecraft/...` classes referenced by the mod.
    pub minecraft_references: BTreeSet<String>,
    /// Other referenced classes, limited to keep reports readable.
    pub external_references: BTreeSet<String>,
    pub uses_mixin_annotations: bool,
    pub references_loader_api: BTreeSet<String>,
    /// `true` when class files could not all be parsed.
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JarInspection {
    pub path: PathBuf,
    pub sha1: String,
    pub sha256: String,
    pub size_bytes: u64,
    pub entry_count: usize,
    pub entries: Vec<JarEntryInfo>,
    pub loaders_detected: Vec<Loader>,
    /// Why each loader was suspected, for transparency in reports.
    pub loader_evidence: Vec<String>,
    pub metadata_files: BTreeMap<String, String>,
    pub mixin_configs: Vec<MixinConfigText>,
    pub access_wideners: Vec<AccessWidenerInfo>,
    pub nested_jars: Vec<NestedJarInfo>,
    pub refmaps: Vec<String>,
    pub services: Vec<String>,
    pub manifest: BTreeMap<String, String>,
    pub bytecode: BytecodeFacts,
    pub warnings: Vec<String>,
}

impl JarInspection {
    /// Inspect a JAR file without executing any of its code.
    pub fn inspect(path: &Path) -> Result<Self> {
        let metadata = std::fs::metadata(path)
            .with_context(|| format!("reading metadata of {}", path.display()))?;
        if !metadata.is_file() {
            anyhow::bail!("{} is not a regular file", path.display());
        }
        let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
        let mut archive = ZipArchive::new(file)
            .with_context(|| format!("{} is not a readable JAR/ZIP archive", path.display()))?;

        let mut entries = Vec::new();
        let mut metadata_files = BTreeMap::new();
        let mut mixin_configs = Vec::new();
        let mut access_wideners = Vec::new();
        let mut nested_jars = Vec::new();
        let mut refmaps = Vec::new();
        let mut services = Vec::new();
        let mut bytecode = BytecodeFacts::default();
        let mut warnings = Vec::new();
        let mut manifest = BTreeMap::new();

        for index in 0..archive.len() {
            let mut entry = archive
                .by_index(index)
                .with_context(|| format!("reading archive entry #{index} of {}", path.display()))?;
            let name = entry.name().to_string();
            let is_dir = entry.is_dir();
            let size = entry.size();
            entries.push(JarEntryInfo {
                name: name.clone(),
                uncompressed_size: size,
                is_dir,
            });
            if is_dir {
                continue;
            }

            let lower = name.to_ascii_lowercase();
            let is_metadata = matches!(
                name.as_str(),
                "fabric.mod.json"
                    | "quilt.mod.json"
                    | "META-INF/mods.toml"
                    | "META-INF/neoforge.mods.toml"
                    | "META-INF/MANIFEST.MF"
            );
            let is_mixin_config = lower.ends_with(".mixins.json")
                || lower.ends_with(".mixin.json")
                || (lower.contains("mixin") && lower.ends_with(".json"));
            let is_access_widener = lower.ends_with(".accesswidener");
            let is_nested = lower.starts_with("meta-inf/jars/") && lower.ends_with(".jar");
            let is_refmap = lower.ends_with(".refmap.json");
            let is_service = lower.starts_with("meta-inf/services/") && !entries.is_empty();
            let is_class = lower.ends_with(".class");

            if is_metadata || is_mixin_config || (is_refmap && size < MAX_METADATA_BYTES) {
                if size > MAX_METADATA_BYTES {
                    warnings.push(format!(
                        "`{name}` is {size} bytes; truncated at {MAX_METADATA_BYTES} bytes"
                    ));
                }
                let mut buffer = Vec::new();
                entry
                    .by_ref()
                    .take(MAX_METADATA_BYTES)
                    .read_to_end(&mut buffer)
                    .with_context(|| format!("reading `{name}` from the archive"))?;
                let text = String::from_utf8_lossy(&buffer).into_owned();
                if name == "META-INF/MANIFEST.MF" {
                    manifest = parse_manifest(&text);
                } else if is_mixin_config {
                    mixin_configs.push(MixinConfigText {
                        path: name.clone(),
                        contents: text.clone(),
                    });
                } else if is_refmap {
                    refmaps.push(name.clone());
                }
                if is_metadata {
                    metadata_files.insert(name.clone(), text);
                }
                continue;
            }

            if is_access_widener {
                let mut buffer = Vec::new();
                entry
                    .by_ref()
                    .take(MAX_METADATA_BYTES)
                    .read_to_end(&mut buffer)
                    .with_context(|| format!("reading access widener `{name}`"))?;
                let text = String::from_utf8_lossy(&buffer);
                let mut header = None;
                let mut namespace = None;
                let mut entry_count = 0usize;
                for (line_index, line) in text.lines().enumerate() {
                    let trimmed = line.trim();
                    if trimmed.is_empty() || trimmed.starts_with('#') {
                        continue;
                    }
                    if line_index == 0 || trimmed.starts_with("accessWidener") {
                        header = Some(trimmed.to_string());
                        namespace = trimmed
                            .split_whitespace()
                            .nth(2)
                            .map(|value| value.to_string());
                        continue;
                    }
                    entry_count += 1;
                }
                access_wideners.push(AccessWidenerInfo {
                    path: name.clone(),
                    header,
                    namespace,
                    entry_count,
                });
                continue;
            }

            if is_nested {
                let mut buffer = Vec::new();
                entry
                    .by_ref()
                    .take(MAX_NESTED_JAR_BYTES)
                    .read_to_end(&mut buffer)
                    .with_context(|| format!("reading nested JAR `{name}`"))?;
                let nested = inspect_nested(&name, &buffer);
                nested_jars.push(nested);
                continue;
            }

            if is_service {
                services.push(name.clone());
                continue;
            }

            if is_class {
                if bytecode.class_count >= MAX_CLASSES_ANALYSED {
                    bytecode.truncated = true;
                    continue;
                }
                let mut buffer = Vec::new();
                entry
                    .by_ref()
                    .take(2 * 1024 * 1024)
                    .read_to_end(&mut buffer)
                    .with_context(|| format!("reading class `{name}`"))?;
                if let Some(facts) = analyse_class(&buffer) {
                    bytecode.class_count += 1;
                    *bytecode.java_majors.entry(facts.java_major).or_insert(0) += 1;
                    for package in facts.packages {
                        *bytecode.packages.entry(package).or_insert(0) += 1;
                    }
                    bytecode
                        .minecraft_references
                        .extend(facts.minecraft_references);
                    bytecode
                        .external_references
                        .extend(facts.external_references);
                    bytecode
                        .references_loader_api
                        .extend(facts.loader_references);
                    bytecode.uses_mixin_annotations |= facts.uses_mixin;
                } else {
                    warnings.push(format!("could not parse class file `{name}`"));
                }
            }
        }

        let (loaders_detected, loader_evidence) = detect_loaders(
            &metadata_files,
            &manifest,
            &mixin_configs,
            &nested_jars,
            &bytecode,
        );

        if !bytecode.truncated && bytecode.class_count == 0 {
            warnings.push("the archive contains no Java class files".to_string());
        }
        if entries.len() > 200_000 {
            warnings.push(format!(
                "archive contains an unusually large number of entries ({})",
                entries.len()
            ));
        }

        Ok(JarInspection {
            path: path.to_path_buf(),
            sha1: fsutil::sha1_file(path)?,
            sha256: fsutil::sha256_file(path)?,
            size_bytes: metadata.len(),
            entry_count: entries.len(),
            entries,
            loaders_detected,
            loader_evidence,
            metadata_files,
            mixin_configs,
            access_wideners,
            nested_jars,
            refmaps,
            services,
            manifest,
            bytecode,
            warnings,
        })
    }

    pub fn metadata_text(&self, name: &str) -> Option<&str> {
        self.metadata_files.get(name).map(|text| text.as_str())
    }

    /// Read an arbitrary text entry from the archive on demand.
    ///
    /// Returns `Ok(None)` when the entry does not exist. Entries larger than
    /// [`MAX_METADATA_BYTES`] are rejected instead of being loaded into memory.
    pub fn read_entry(&self, name: &str) -> Result<Option<String>> {
        let file =
            File::open(&self.path).with_context(|| format!("reopening {}", self.path.display()))?;
        let mut archive = ZipArchive::new(file)
            .with_context(|| format!("reopening {} as a JAR", self.path.display()))?;
        match archive.by_name(name) {
            Ok(mut entry) => {
                if entry.size() > MAX_METADATA_BYTES {
                    anyhow::bail!(
                        "entry `{name}` is {} bytes, above the {MAX_METADATA_BYTES} byte inspection limit",
                        entry.size()
                    );
                }
                let mut buffer = String::new();
                entry
                    .by_ref()
                    .take(MAX_METADATA_BYTES)
                    .read_to_string(&mut buffer)
                    .with_context(|| format!("reading `{name}`"))?;
                Ok(Some(buffer))
            }
            Err(zip::result::ZipError::FileNotFound) => Ok(None),
            Err(error) => Err(error).with_context(|| format!("reading `{name}`")),
        }
    }

    pub fn has_metadata(&self, name: &str) -> bool {
        self.metadata_files.contains_key(name)
    }

    /// Loader with the strongest evidence. Multiple detections are preserved
    /// in `loaders_detected`.
    pub fn primary_loader(&self) -> Option<Loader> {
        let priority = [
            Loader::NeoForge,
            Loader::Forge,
            Loader::Quilt,
            Loader::Fabric,
        ];
        if self.loaders_detected.len() == 1 {
            return self.loaders_detected.first().copied();
        }
        for loader in priority {
            if self.loaders_detected.contains(&loader) {
                // Quilt mods frequently also ship fabric.mod.json; prefer the
                // dedicated Quilt metadata in that case.
                if loader == Loader::Quilt && !self.has_metadata("quilt.mod.json") {
                    continue;
                }
                return Some(loader);
            }
        }
        None
    }

    pub fn java_major_min(&self) -> Option<u32> {
        self.bytecode.java_majors.keys().next().copied()
    }

    pub fn java_major_max(&self) -> Option<u32> {
        self.bytecode.java_majors.keys().next_back().copied()
    }

    pub fn has_refmap(&self) -> bool {
        !self.refmaps.is_empty()
    }

    pub fn summary(&self) -> InspectionSummary {
        InspectionSummary {
            loaders_detected: self.loaders_detected.clone(),
            primary_loader: self.primary_loader(),
            class_count: self.bytecode.class_count,
            java_major_min: self.java_major_min(),
            java_major_max: self.java_major_max(),
            mixin_configs: self
                .mixin_configs
                .iter()
                .map(|config| config.path.clone())
                .collect(),
            access_wideners: self
                .access_wideners
                .iter()
                .map(|widener| widener.path.clone())
                .collect(),
            nested_jars: self
                .nested_jars
                .iter()
                .map(|nested| nested.path.clone())
                .collect(),
            has_refmap: self.has_refmap(),
            uses_minecraft_classes: !self.bytecode.minecraft_references.is_empty(),
            warnings: self.warnings.clone(),
        }
    }
}

fn inspect_nested(name: &str, bytes: &[u8]) -> NestedJarInfo {
    let mut info = NestedJarInfo {
        path: name.to_string(),
        mod_id: None,
        version: None,
        loader: None,
        size_bytes: bytes.len() as u64,
    };
    let cursor = Cursor::new(bytes);
    let Ok(mut archive) = ZipArchive::new(cursor) else {
        return info;
    };
    if let Ok(mut entry) = archive.by_name("fabric.mod.json") {
        let mut text = String::new();
        if entry.read_to_string(&mut text).is_ok() {
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) {
                info.mod_id = value
                    .get("id")
                    .and_then(|value| value.as_str())
                    .map(str::to_string);
                info.version = value
                    .get("version")
                    .and_then(|value| value.as_str())
                    .map(str::to_string);
                info.loader = Some(Loader::Fabric);
            }
        }
    }
    if info.loader.is_none() {
        if let Ok(mut entry) = archive.by_name("quilt.mod.json") {
            let mut text = String::new();
            if entry.read_to_string(&mut text).is_ok() {
                if let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) {
                    let loader = value.get("quilt_loader");
                    info.mod_id = loader
                        .and_then(|section| section.get("id"))
                        .and_then(|value| value.as_str())
                        .map(str::to_string);
                    info.version = loader
                        .and_then(|section| section.get("version"))
                        .and_then(|value| value.as_str())
                        .map(str::to_string);
                    info.loader = Some(Loader::Quilt);
                }
            }
        }
    }
    info
}

fn detect_loaders(
    metadata_files: &BTreeMap<String, String>,
    manifest: &BTreeMap<String, String>,
    mixin_configs: &[MixinConfigText],
    nested_jars: &[NestedJarInfo],
    bytecode: &BytecodeFacts,
) -> (Vec<Loader>, Vec<String>) {
    let mut detected = Vec::new();
    let mut evidence = Vec::new();

    if metadata_files.contains_key("fabric.mod.json") {
        detected.push(Loader::Fabric);
        evidence.push("found fabric.mod.json".to_string());
    }
    if metadata_files.contains_key("quilt.mod.json") {
        detected.push(Loader::Quilt);
        evidence.push("found quilt.mod.json".to_string());
    }
    if metadata_files.contains_key("META-INF/neoforge.mods.toml") {
        detected.push(Loader::NeoForge);
        evidence.push("found META-INF/neoforge.mods.toml".to_string());
    } else if metadata_files.contains_key("META-INF/mods.toml") {
        let text = metadata_files
            .get("META-INF/mods.toml")
            .map(String::as_str)
            .unwrap_or_default();
        let lower = text.to_ascii_lowercase();
        if lower.contains("neoforge") || lower.contains("javafml") && lower.contains("neoforge") {
            detected.push(Loader::NeoForge);
            evidence.push("mods.toml mentions neoforge".to_string());
        } else {
            detected.push(Loader::Forge);
            evidence.push("found META-INF/mods.toml".to_string());
        }
    }

    if let Some(kind) = manifest.get("fmlmodtype") {
        if !detected.contains(&Loader::Forge) && !detected.contains(&Loader::NeoForge) {
            detected.push(Loader::Forge);
        }
        evidence.push(format!("manifest declares FMLModType: {kind}"));
    }
    if let Some(configs) = manifest.get("mixinconfigs") {
        evidence.push(format!("manifest declares MixinConfigs: {configs}"));
    }
    if manifest.contains_key("fmlmodtype") {
        evidence.push("manifest contains FML mod metadata".to_string());
    }

    if bytecode
        .references_loader_api
        .iter()
        .any(|reference| reference.contains("net/fabricmc/api"))
    {
        if !detected.contains(&Loader::Fabric) {
            detected.push(Loader::Fabric);
        }
        evidence.push("bytecode references net.fabricmc.api".to_string());
    }
    if bytecode
        .references_loader_api
        .iter()
        .any(|reference| reference.contains("net/neoforged"))
        && !detected.contains(&Loader::NeoForge)
    {
        detected.push(Loader::NeoForge);
        evidence.push("bytecode references net.neoforged".to_string());
    }
    if bytecode
        .references_loader_api
        .iter()
        .any(|reference| reference.contains("net/minecraftforge"))
        && !detected.contains(&Loader::Forge)
        && !detected.contains(&Loader::NeoForge)
    {
        detected.push(Loader::Forge);
        evidence.push("bytecode references net.minecraftforge".to_string());
    }
    if bytecode
        .references_loader_api
        .iter()
        .any(|reference| reference.contains("org/quiltmc"))
        && !detected.contains(&Loader::Quilt)
    {
        detected.push(Loader::Quilt);
        evidence.push("bytecode references org.quiltmc".to_string());
    }

    if mixin_configs
        .iter()
        .any(|config| config.contents.contains("fabricmc"))
        && !detected.contains(&Loader::Fabric)
    {
        evidence.push("mixin configuration references fabric mixin extensions".to_string());
    }

    for nested in nested_jars {
        if let Some(loader) = nested.loader {
            evidence.push(format!(
                "nested JAR {} targets {}",
                nested.path,
                loader.display_name()
            ));
        }
    }

    detected.sort_unstable();
    detected.dedup();
    (detected, evidence)
}

/// Parse `META-INF/MANIFEST.MF` including folded continuation lines.
pub fn parse_manifest(text: &str) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    let mut current_key: Option<String> = None;
    let mut current_value = String::new();
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix(' ') {
            if current_key.is_some() {
                current_value.push_str(rest);
            }
            continue;
        }
        if let Some(key) = current_key.take() {
            map.insert(key.to_ascii_lowercase(), current_value.trim().to_string());
            current_value.clear();
        }
        if let Some((key, value)) = line.split_once(':') {
            current_key = Some(key.trim().to_string());
            current_value.push_str(value.trim());
        }
    }
    if let Some(key) = current_key {
        map.insert(key.to_ascii_lowercase(), current_value.trim().to_string());
    }
    map
}

struct ClassFacts {
    java_major: u32,
    packages: Vec<String>,
    minecraft_references: BTreeSet<String>,
    external_references: BTreeSet<String>,
    loader_references: BTreeSet<String>,
    uses_mixin: bool,
}

/// Parse the class file header and constant pool for useful facts.
///
/// The parser is intentionally read-only and bounded: it never allocates based
/// on untrusted lengths without checking the remaining buffer.
fn analyse_class(bytes: &[u8]) -> Option<ClassFacts> {
    if bytes.len() < 10 || bytes[0..4] != [0xCA, 0xFE, 0xBA, 0xBE] {
        return None;
    }
    let major = u16::from_be_bytes([bytes[6], bytes[7]]);
    if major < 45 {
        return None;
    }
    let java_major = major as u32 - 44;
    let pool_count = u16::from_be_bytes([bytes[8], bytes[9]]) as usize;
    let mut cursor = 10usize;
    let mut strings: Vec<String> = Vec::new();
    let mut index = 1usize;
    while index < pool_count {
        let tag = *bytes.get(cursor)?;
        cursor += 1;
        match tag {
            1 => {
                let length =
                    u16::from_be_bytes([*bytes.get(cursor)?, *bytes.get(cursor + 1)?]) as usize;
                cursor += 2;
                let end = cursor.checked_add(length)?;
                let slice = bytes.get(cursor..end)?;
                cursor = end;
                if length <= 4096 {
                    strings.push(String::from_utf8_lossy(slice).into_owned());
                }
            }
            7 | 8 | 16 | 19 | 20 => cursor += 2,
            15 => cursor += 3,
            9 | 10 | 11 | 12 | 17 | 18 => cursor += 4,
            3 | 4 => cursor += 4,
            5 | 6 => {
                cursor += 8;
                // 8-byte constants occupy two constant pool slots.
                index += 1;
            }
            _ => return None,
        }
        index += 1;
    }

    let mut packages = Vec::new();
    let mut minecraft_references = BTreeSet::new();
    let mut external_references = BTreeSet::new();
    let mut loader_references = BTreeSet::new();
    let mut uses_mixin = false;

    for text in &strings {
        if text.contains("org/spongepowered/asm/mixin/Mixin") {
            uses_mixin = true;
        }
        if let Some(internal) = internal_class_name(text) {
            if internal.starts_with("net/minecraft/") {
                if minecraft_references.len() < 500 {
                    minecraft_references.insert(internal.to_string());
                }
                continue;
            }
            if internal.starts_with("net/fabricmc/")
                || internal.starts_with("net/minecraftforge/")
                || internal.starts_with("net/neoforged/")
                || internal.starts_with("org/quiltmc/")
            {
                if loader_references.len() < 200 {
                    loader_references.insert(internal.to_string());
                }
                continue;
            }
            if internal.starts_with("java/") || internal.starts_with("javax/") {
                continue;
            }
            if external_references.len() < 500 {
                external_references.insert(internal.to_string());
            }
        } else if text.contains('/') && !text.starts_with('(') && text.len() < 400 {
            // Package/class names appear as plain UTF-8 entries in some pools.
            if let Some((package, _)) = text.rsplit_once('/') {
                if (package.contains('/') || !package.is_empty())
                    && text.chars().all(|c| {
                        c.is_ascii_alphanumeric() || matches!(c, '/' | '_' | '$' | '.' | '-')
                    })
                    && package.split('/').all(|segment| {
                        !segment.is_empty()
                            && segment
                                .chars()
                                .next()
                                .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
                    })
                    && packages.len() < 50
                {
                    let top = package.split('/').take(2).collect::<Vec<_>>().join("/");
                    if !packages.contains(&top) {
                        packages.push(top);
                    }
                }
            }
        }
    }

    Some(ClassFacts {
        java_major,
        packages,
        minecraft_references,
        external_references,
        loader_references,
        uses_mixin,
    })
}

/// Extract an internal class name from a constant-pool UTF-8 entry.
fn internal_class_name(text: &str) -> Option<&str> {
    let candidate = if text.starts_with('L') && text.ends_with(';') && text.len() > 4 {
        &text[1..text.len() - 1]
    } else {
        text
    };
    if candidate.len() > 300 || candidate.contains([' ', '(', ')', '[', '*', ';']) {
        return None;
    }
    if !candidate.contains('/') {
        return None;
    }
    if !candidate.chars().all(|character| {
        character.is_ascii_alphanumeric() || matches!(character, '/' | '_' | '$' | '-' | '.')
    }) {
        return None;
    }
    let mut segments = candidate.split('/');
    if segments.any(|segment| {
        segment.is_empty()
            || !(segment.starts_with(|character: char| character.is_ascii_alphabetic())
                || segment.starts_with('_'))
    }) {
        return None;
    }
    Some(candidate)
}

/// Build a deterministic fixture JAR for tests and offline workflows.
///
/// This is a normal ZIP writer, not a test stub: fixtures are real archives.
pub fn write_jar(path: &Path, entries: &[(&str, &[u8])]) -> Result<()> {
    use std::io::Write;
    use zip::write::SimpleFileOptions;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = File::create(path).with_context(|| format!("creating {}", path.display()))?;
    let mut writer = zip::ZipWriter::new(file);
    let options = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    for (name, contents) in entries {
        writer
            .start_file(*name, options)
            .with_context(|| format!("adding `{name}` to {}", path.display()))?;
        writer.write_all(contents)?;
    }
    writer.finish()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fabric_jar() -> (tempfile::TempDir, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("example.jar");
        let mod_json = br#"{
            "schemaVersion": 1,
            "id": "example-mod",
            "version": "1.2.3",
            "name": "Example Mod",
            "environment": "*",
            "entrypoints": {"main": ["com.example.ExampleMod"]},
            "mixins": ["example.mixins.json"],
            "depends": {"minecraft": ">=1.20.1 <1.21", "fabricloader": ">=0.15.0"}
        }"#;
        let mixin_json = br#"{
            "required": true,
            "package": "com.example.mixin",
            "compatibilityLevel": "JAVA_17",
            "mixins": ["ServerPlayerMixin"],
            "client": ["TitleScreenMixin"],
            "refmap": "example.refmap.json"
        }"#;
        write_jar(
            &path,
            &[
                ("fabric.mod.json", mod_json.as_slice()),
                ("example.mixins.json", mixin_json.as_slice()),
                ("example.refmap.json", b"{}"),
                ("example.accesswidener", b"accessWidener v2 named\n"),
                (
                    "META-INF/MANIFEST.MF",
                    b"Manifest-Version: 1.0\nMixinConfigs: example.mixins.json\n",
                ),
                ("com/example/ExampleMod.class", b"\xCA\xFE\xBA\xBE"),
            ],
        )
        .unwrap();
        (temp, path)
    }

    #[test]
    fn detects_fabric_mod_and_metadata() {
        let (_temp, path) = fabric_jar();
        let inspection = JarInspection::inspect(&path).unwrap();
        assert_eq!(inspection.primary_loader(), Some(Loader::Fabric));
        assert!(inspection.loaders_detected.contains(&Loader::Fabric));
        assert!(inspection.has_metadata("fabric.mod.json"));
        assert_eq!(inspection.mixin_configs.len(), 1);
        assert_eq!(inspection.access_wideners.len(), 1);
        assert_eq!(
            inspection.access_wideners[0].namespace.as_deref(),
            Some("named")
        );
        assert!(inspection.has_refmap());
        assert_eq!(
            inspection.manifest.get("mixinconfigs").unwrap(),
            "example.mixins.json"
        );
        assert_eq!(inspection.sha1.len(), 40);
        assert_eq!(inspection.sha256.len(), 64);
    }

    #[test]
    fn detects_quilt_from_dedicated_metadata() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("quilt.jar");
        write_jar(
            &path,
            &[
                (
                    "quilt.mod.json",
                    br#"{"schema_version":1,"quilt_loader":{"group":"com.example","id":"qm","version":"1.0.0"}}"#,
                ),
                ("fabric.mod.json", br#"{"schemaVersion":1,"id":"qm","version":"1.0.0"}"#),
            ],
        )
        .unwrap();
        let inspection = JarInspection::inspect(&path).unwrap();
        assert_eq!(inspection.primary_loader(), Some(Loader::Quilt));
        assert!(inspection.loaders_detected.contains(&Loader::Fabric));
    }

    #[test]
    fn detects_forge_and_neoforge_metadata() {
        let temp = tempfile::tempdir().unwrap();
        let forge = temp.path().join("forge.jar");
        write_jar(
            &forge,
            &[(
                "META-INF/mods.toml",
                b"modLoader=\"javafml\"\nloaderVersion=\"[47,)\"\nlicense=\"MIT\"\n[[mods]]\nmodId=\"fm\"\nversion=\"1.0\"\n",
            )],
        )
        .unwrap();
        assert_eq!(
            JarInspection::inspect(&forge).unwrap().primary_loader(),
            Some(Loader::Forge)
        );

        let neo = temp.path().join("neo.jar");
        write_jar(
            &neo,
            &[
                (
                    "META-INF/neoforge.mods.toml",
                    b"modLoader=\"javafml\"\nloaderVersion=\"[1,)\"\n[[mods]]\nmodId=\"nm\"\nversion=\"1.0\"\n",
                ),
                (
                    "META-INF/MANIFEST.MF",
                    b"Manifest-Version: 1.0\nFMLModType: MOD\n",
                ),
            ],
        )
        .unwrap();
        assert_eq!(
            JarInspection::inspect(&neo).unwrap().primary_loader(),
            Some(Loader::NeoForge)
        );
    }

    #[test]
    fn jar_without_metadata_reports_no_loader() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("plain.jar");
        write_jar(&path, &[("README.txt", b"not a mod")]).unwrap();
        let inspection = JarInspection::inspect(&path).unwrap();
        assert!(inspection.loaders_detected.is_empty());
        assert_eq!(inspection.primary_loader(), None);
        assert!(
            inspection
                .warnings
                .iter()
                .any(|w| w.contains("no Java class"))
        );
    }

    #[test]
    fn non_zip_input_is_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("broken.jar");
        std::fs::write(&path, b"definitely not a zip").unwrap();
        let error = JarInspection::inspect(&path).unwrap_err();
        assert!(error.to_string().contains("readable JAR"));
    }

    #[test]
    fn parses_folded_manifest_lines() {
        let manifest =
            parse_manifest("Manifest-Version: 1.0\r\nMixinConfigs: one.json,two.j\r\n son\r\n\r\n");
        assert_eq!(manifest.get("mixinconfigs").unwrap(), "one.json,two.json");
        assert_eq!(manifest.get("manifest-version").unwrap(), "1.0");
    }

    #[test]
    fn bytecode_analysis_reads_java_version() {
        // Minimal class file: magic, minor, major 61 (Java 17), one Utf8 entry.
        let name = b"net/minecraft/server/MinecraftServer";
        let mut bytes = vec![0xCA, 0xFE, 0xBA, 0xBE, 0x00, 0x00, 0x00, 0x3D, 0x00, 0x02];
        bytes.push(0x01);
        bytes.extend_from_slice(&(name.len() as u16).to_be_bytes());
        bytes.extend_from_slice(name);
        let facts = analyse_class(&bytes).expect("class parses");
        assert_eq!(facts.java_major, 17);
        assert!(
            facts
                .minecraft_references
                .iter()
                .any(|reference| reference.contains("MinecraftServer"))
        );
    }

    #[test]
    fn ignores_truncated_class_files() {
        let bytes = vec![0xCA, 0xFE, 0xBA, 0xBE, 0x00, 0x00, 0x00, 0x3D];
        assert!(analyse_class(&bytes).is_none());
    }

    #[test]
    fn reads_manifest_and_nested_jars() {
        let temp = tempfile::tempdir().unwrap();
        let nested = temp.path().join("nested.jar");
        write_jar(
            &nested,
            &[(
                "fabric.mod.json",
                br#"{"id":"lib","version":"2.0.0","schemaVersion":1}"#,
            )],
        )
        .unwrap();
        let nested_bytes = std::fs::read(&nested).unwrap();
        let outer = temp.path().join("outer.jar");
        write_jar(
            &outer,
            &[
                ("fabric.mod.json", br#"{"id":"outer","version":"1.0.0"}"#),
                ("META-INF/jars/lib.jar", nested_bytes.as_slice()),
            ],
        )
        .unwrap();
        let inspection = JarInspection::inspect(&outer).unwrap();
        assert_eq!(inspection.nested_jars.len(), 1);
        assert_eq!(inspection.nested_jars[0].mod_id.as_deref(), Some("lib"));
        assert_eq!(inspection.nested_jars[0].loader, Some(Loader::Fabric));
    }
}
