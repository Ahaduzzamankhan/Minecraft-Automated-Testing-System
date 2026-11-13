//! Loader metadata parsing.
//!
//! The parsers are deliberately tolerant: real-world mods contain small
//! deviations from the specifications, and MATS records anything suspicious as
//! a warning instead of refusing to test the mod. Required fields are still
//! required, because testing a mod whose identity is unknown is not useful.

use std::collections::BTreeMap;

use anyhow::{Context, Result, bail};
use jar_inspector::JarInspection;
use mats_core::Loader;
use mats_core::types::{DependencyKind, DependencySpec, Entrypoint, ModMetadata};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A parsed `*.mixins.json` configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct MixinConfig {
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub package: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mixins: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub client: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub server: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refmap: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compatibility_level: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plugin: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_version: Option<String>,
    #[serde(default)]
    pub required: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_require: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub priority: Option<i64>,
    /// Raw `injectors` block when present.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub injectors: BTreeMap<String, i64>,
    /// Problems noticed while reading the configuration.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

impl MixinConfig {
    pub fn mixin_count(&self) -> usize {
        self.mixins.len() + self.client.len() + self.server.len()
    }

    /// Fully qualified mixin class names, for overlap analysis.
    pub fn qualified_classes(&self) -> Vec<String> {
        let prefix = self.package.as_deref().unwrap_or("");
        self.mixins
            .iter()
            .chain(self.client.iter())
            .chain(self.server.iter())
            .map(|name| {
                if prefix.is_empty() {
                    name.clone()
                } else {
                    format!("{prefix}.{name}")
                }
            })
            .collect()
    }
}

/// Everything MATS extracted from a mod JAR.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParsedMod {
    pub metadata: ModMetadata,
    pub loader: Option<Loader>,
    pub mixin_configs: Vec<MixinConfig>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub access_wideners: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub manifest: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

/// Parse the metadata of an already-inspected JAR.
pub fn parse_inspection(inspection: &JarInspection) -> Result<ParsedMod> {
    let loader = inspection.primary_loader();
    let mut warnings = Vec::new();

    let metadata = match loader {
        Some(Loader::Fabric) => {
            let text = inspection
                .metadata_text("fabric.mod.json")
                .context("fabric.mod.json disappeared between inspection and parsing")?;
            parse_fabric(text)?
        }
        Some(Loader::Quilt) => {
            if inspection.has_metadata("quilt.mod.json") {
                parse_quilt(inspection.metadata_text("quilt.mod.json").unwrap_or("{}"))?
            } else {
                let text = inspection
                    .metadata_text("fabric.mod.json")
                    .context("Quilt mod without quilt.mod.json or fabric.mod.json")?;
                let mut metadata = parse_fabric(text)?;
                metadata.loader = Some(Loader::Quilt);
                metadata
                    .warnings
                    .push("quilt.mod.json is absent; parsed fabric.mod.json instead".to_string());
                metadata
            }
        }
        Some(Loader::Forge) | Some(Loader::NeoForge) => {
            let (file, is_neoforge) = if inspection.has_metadata("META-INF/neoforge.mods.toml") {
                ("META-INF/neoforge.mods.toml", true)
            } else {
                ("META-INF/mods.toml", loader == Some(Loader::NeoForge))
            };
            let text = inspection
                .metadata_text(file)
                .with_context(|| format!("{file} disappeared between inspection and parsing"))?;
            parse_toml_mods(text, file, is_neoforge)?
        }
        Some(Loader::Vanilla) | None => {
            bail!(
                "no supported mod metadata found (looked for fabric.mod.json, quilt.mod.json, META-INF/mods.toml, META-INF/neoforge.mods.toml)"
            );
        }
    };

    let mut metadata = metadata;
    if metadata.loader.is_none() {
        metadata.loader = loader;
    }
    if metadata.name.trim().is_empty() {
        metadata.name = metadata.id.clone();
    }
    warnings.extend(metadata.warnings.iter().cloned());

    // Mixin configurations declared inline in metadata plus any config file
    // discovered by name in the archive.
    let mut configs: Vec<MixinConfig> = Vec::new();
    let mut declared: Vec<String> = metadata.mixins.clone();
    if let Some(manifest_mixins) = inspection.manifest.get("mixinconfigs") {
        declared.extend(
            manifest_mixins
                .split([',', ' '])
                .filter(|entry| !entry.trim().is_empty())
                .map(|entry| entry.trim().to_string()),
        );
    }
    declared.sort();
    declared.dedup();
    for path in &declared {
        match inspection.read_entry(path) {
            Ok(Some(text)) => match parse_mixin_config(path, &text) {
                Ok(config) => configs.push(config),
                Err(error) => {
                    warnings.push(format!("could not parse mixin config `{path}`: {error}"))
                }
            },
            Ok(None) => warnings.push(format!(
                "mixin config `{path}` is declared but missing from the archive"
            )),
            Err(error) => warnings.push(format!("could not read mixin config `{path}`: {error}")),
        }
    }
    for text_config in &inspection.mixin_configs {
        if configs.iter().any(|config| config.path == text_config.path) {
            continue;
        }
        match parse_mixin_config(&text_config.path, &text_config.contents) {
            Ok(config) => configs.push(config),
            Err(error) => warnings.push(format!(
                "could not parse mixin config `{}`: {error}",
                text_config.path
            )),
        }
    }

    if let Some(environment) = metadata.environment.as_deref() {
        if !matches!(environment, "client" | "server" | "*" | "both") {
            warnings.push(format!(
                "unrecognised environment value `{environment}` in metadata"
            ));
        }
    }

    let mut access_wideners: Vec<String> = metadata.access_wideners.clone();
    access_wideners.extend(
        inspection
            .access_wideners
            .iter()
            .map(|widener| widener.path.clone()),
    );
    access_wideners.sort();
    access_wideners.dedup();
    metadata.access_wideners = access_wideners.clone();

    if metadata.entrypoints.is_empty() && loader.is_some_and(|l| l.is_fabric_like()) {
        warnings.push("no entrypoints are declared; the mod may only ship resources".into());
    }

    Ok(ParsedMod {
        metadata,
        loader,
        mixin_configs: configs,
        access_wideners,
        manifest: inspection.manifest.clone(),
        warnings,
    })
}

/// Parse `fabric.mod.json`.
pub fn parse_fabric(text: &str) -> Result<ModMetadata> {
    let value: Value = serde_json::from_str(text).context("fabric.mod.json is not valid JSON")?;
    let object = value
        .as_object()
        .context("fabric.mod.json must contain a JSON object")?;
    let id = required_str(object, "id", "fabric.mod.json")?;
    let version = object
        .get("version")
        .and_then(Value::as_str)
        .unwrap_or("0.0.0")
        .to_string();

    let mut metadata = ModMetadata {
        id: id.to_string(),
        name: object
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or(id)
            .to_string(),
        version,
        description: object
            .get("description")
            .and_then(Value::as_str)
            .map(str::to_string),
        loader: Some(Loader::Fabric),
        metadata_file: Some("fabric.mod.json".into()),
        environment: object
            .get("environment")
            .and_then(Value::as_str)
            .map(str::to_string),
        license: object
            .get("license")
            .and_then(Value::as_str)
            .map(str::to_string),
        ..ModMetadata::default()
    };

    if let Some(schema) = object.get("schemaVersion").and_then(Value::as_u64) {
        if schema > 1 {
            metadata.warnings.push(format!(
                "declares unknown fabric.mod.json schemaVersion {schema}"
            ));
        }
    }

    if let Some(authors) = object.get("authors") {
        metadata.authors = names_from_value(authors);
    }
    if let Some(contact) = object.get("contact").and_then(Value::as_object) {
        for (key, value) in contact {
            if let Some(text) = value.as_str() {
                metadata.contacts.insert(key.clone(), text.to_string());
            }
        }
    }
    if let Some(contributors) = object.get("contributors").and_then(Value::as_object) {
        for (name, _) in contributors {
            if !metadata.authors.contains(name) {
                metadata.authors.push(name.clone());
            }
        }
    }

    if let Some(entrypoints) = object.get("entrypoints").and_then(Value::as_object) {
        for (kind, values) in entrypoints {
            for entry in values.as_array().into_iter().flatten() {
                match entry {
                    Value::String(text) => metadata.entrypoints.push(Entrypoint {
                        kind: kind.clone(),
                        value: text.clone(),
                        side: None,
                    }),
                    Value::Object(map) => {
                        if let Some(text) = map.get("value").and_then(Value::as_str) {
                            metadata.entrypoints.push(Entrypoint {
                                kind: kind.clone(),
                                value: text.to_string(),
                                side: map
                                    .get("environment")
                                    .and_then(Value::as_str)
                                    .map(str::to_string),
                            });
                        }
                    }
                    _ => metadata
                        .warnings
                        .push(format!("entrypoint `{kind}` has an unsupported value type")),
                }
            }
        }
    }

    if let Some(mixins) = object.get("mixins").and_then(Value::as_array) {
        for entry in mixins {
            match entry {
                Value::String(text) => metadata.mixins.push(text.clone()),
                Value::Object(map) => {
                    if let Some(config) = map.get("config").and_then(Value::as_str) {
                        metadata.mixins.push(config.to_string());
                    }
                    if let Some(environment) = map.get("environment").and_then(Value::as_str) {
                        metadata.warnings.push(format!(
                            "mixin config for `{environment}` environment declared"
                        ));
                    }
                }
                _ => metadata
                    .warnings
                    .push("mixin entry has an unsupported value type".into()),
            }
        }
    }
    if let Some(widener) = object.get("accessWidener").and_then(Value::as_str) {
        metadata.access_wideners.push(widener.to_string());
    }

    if let Some(depends) = object.get("depends").and_then(Value::as_object) {
        for (name, requirement) in depends {
            for constraint in constraints_from_value(requirement) {
                let kind = classify_dependency(name);
                if kind == DependencyKind::Minecraft {
                    metadata.mc_constraints.push(constraint.clone());
                }
                if kind == DependencyKind::Java {
                    metadata.java_constraint = Some(constraint.clone());
                }
                let mut spec = DependencySpec::new(name, kind, true);
                spec.constraint = Some(constraint);
                metadata.depends.push(spec);
            }
        }
    }
    if let Some(breaks) = object.get("breaks").and_then(Value::as_object) {
        for (name, requirement) in breaks {
            for constraint in constraints_from_value(requirement) {
                let mut spec = DependencySpec::new(name, classify_dependency(name), false);
                spec.constraint = Some(constraint);
                metadata.breaks.push(spec);
            }
        }
    }
    if let Some(recommends) = object
        .get("recommends")
        .or_else(|| object.get("suggests"))
        .and_then(Value::as_object)
    {
        for (name, requirement) in recommends {
            for constraint in constraints_from_value(requirement) {
                let mut spec = DependencySpec::new(name, classify_dependency(name), false);
                spec.constraint = Some(constraint);
                metadata.depends.push(spec);
            }
        }
    }
    if let Some(jars) = object.get("jars").and_then(Value::as_array) {
        for entry in jars {
            if let Some(file) = entry.get("file").and_then(Value::as_str) {
                metadata.nested_jars.push(file.to_string());
            }
        }
    }

    if metadata.mc_constraints.is_empty() {
        metadata
            .warnings
            .push("no Minecraft dependency is declared".to_string());
    }
    Ok(metadata)
}

/// Parse `quilt.mod.json`.
pub fn parse_quilt(text: &str) -> Result<ModMetadata> {
    let value: Value = serde_json::from_str(text).context("quilt.mod.json is not valid JSON")?;
    let loader = value
        .get("quilt_loader")
        .and_then(Value::as_object)
        .context("quilt.mod.json must contain a `quilt_loader` object")?;
    let id = required_str(loader, "id", "quilt.mod.json")?;
    let version = loader
        .get("version")
        .and_then(Value::as_str)
        .unwrap_or("0.0.0")
        .to_string();
    let metadata_section = loader.get("metadata").and_then(Value::as_object);

    let mut metadata = ModMetadata {
        id: id.to_string(),
        name: metadata_section
            .and_then(|section| section.get("name"))
            .and_then(Value::as_str)
            .unwrap_or(id)
            .to_string(),
        version,
        description: metadata_section
            .and_then(|section| section.get("description"))
            .and_then(Value::as_str)
            .map(str::to_string),
        loader: Some(Loader::Quilt),
        metadata_file: Some("quilt.mod.json".into()),
        license: loader
            .get("license")
            .and_then(Value::as_str)
            .map(str::to_string),
        ..ModMetadata::default()
    };

    if let Some(contributors) = metadata_section
        .and_then(|section| section.get("contributors"))
        .and_then(Value::as_object)
    {
        metadata.authors = contributors.keys().cloned().collect();
    }
    if let Some(contact) = metadata_section
        .and_then(|section| section.get("contact"))
        .and_then(Value::as_object)
    {
        for (key, value) in contact {
            if let Some(text) = value.as_str() {
                metadata.contacts.insert(key.clone(), text.to_string());
            }
        }
    }

    if let Some(entrypoints) = loader.get("entrypoints").and_then(Value::as_object) {
        for (kind, values) in entrypoints {
            for entry in values.as_array().into_iter().flatten() {
                match entry {
                    Value::String(text) => metadata.entrypoints.push(Entrypoint {
                        kind: kind.clone(),
                        value: text.clone(),
                        side: None,
                    }),
                    Value::Object(map) => {
                        if let Some(text) = map.get("value").and_then(Value::as_str) {
                            metadata.entrypoints.push(Entrypoint {
                                kind: kind.clone(),
                                value: text.to_string(),
                                side: None,
                            });
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    if let Some(mixins) = loader.get("mixins").and_then(Value::as_array) {
        for entry in mixins {
            match entry {
                Value::String(text) => metadata.mixins.push(text.clone()),
                Value::Object(map) => {
                    if let Some(config) = map.get("config").and_then(Value::as_str) {
                        metadata.mixins.push(config.to_string());
                    }
                }
                _ => {}
            }
        }
    }
    if let Some(widener) = loader.get("access_widener").and_then(Value::as_str) {
        metadata.access_wideners.push(widener.to_string());
    }

    if let Some(depends) = loader.get("depends").and_then(Value::as_array) {
        for entry in depends {
            if let Some(object) = entry.as_object() {
                let Some(name) = object.get("id").and_then(Value::as_str) else {
                    continue;
                };
                let constraint = object
                    .get("versions")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                let kind = classify_dependency(name);
                if kind == DependencyKind::Minecraft {
                    if let Some(constraint) = &constraint {
                        metadata.mc_constraints.push(constraint.clone());
                    }
                }
                if kind == DependencyKind::Java {
                    metadata.java_constraint = constraint.clone();
                }
                let mut spec = DependencySpec::new(name, kind, true);
                spec.constraint = constraint;
                metadata.depends.push(spec);
            }
        }
    }
    if let Some(breaks) = loader.get("breaks").and_then(Value::as_array) {
        for entry in breaks {
            if let Some(object) = entry.as_object() {
                if let Some(name) = object.get("id").and_then(Value::as_str) {
                    let mut spec = DependencySpec::new(name, classify_dependency(name), false);
                    spec.constraint = object
                        .get("versions")
                        .and_then(Value::as_str)
                        .map(str::to_string);
                    metadata.breaks.push(spec);
                }
            }
        }
    }

    if metadata.mc_constraints.is_empty() {
        metadata
            .warnings
            .push("no Minecraft dependency is declared".to_string());
    }
    Ok(metadata)
}

/// Parse `META-INF/mods.toml` (Forge) or `META-INF/neoforge.mods.toml`.
pub fn parse_toml_mods(text: &str, file: &str, is_neoforge: bool) -> Result<ModMetadata> {
    let value: toml::Value =
        toml::from_str(text).with_context(|| format!("{file} is not valid TOML"))?;
    let table = value
        .as_table()
        .with_context(|| format!("{file} must contain a TOML table"))?;

    let mods = table
        .get("mods")
        .and_then(toml::Value::as_array)
        .cloned()
        .unwrap_or_default();
    let first = mods
        .first()
        .and_then(toml::Value::as_table)
        .with_context(|| format!("{file} does not declare any [[mods]] entry"))?;

    let id = first
        .get("modId")
        .and_then(toml::Value::as_str)
        .with_context(|| format!("{file}: [[mods]] is missing modId"))?
        .to_string();
    let mut metadata = ModMetadata {
        id: id.clone(),
        name: first
            .get("displayName")
            .and_then(toml::Value::as_str)
            .unwrap_or(&id)
            .to_string(),
        version: first
            .get("version")
            .and_then(toml::Value::as_str)
            .unwrap_or("0.0.0")
            .to_string(),
        description: first
            .get("description")
            .and_then(toml::Value::as_str)
            .map(str::to_string),
        loader: Some(if is_neoforge {
            Loader::NeoForge
        } else {
            Loader::Forge
        }),
        metadata_file: Some(file.to_string()),
        license: table
            .get("license")
            .and_then(toml::Value::as_str)
            .map(str::to_string),
        ..ModMetadata::default()
    };

    if let Some(authors) = first.get("authors").and_then(toml::Value::as_str) {
        metadata.authors = authors
            .split(',')
            .map(|author| author.trim().to_string())
            .filter(|author| !author.is_empty())
            .collect();
    }
    if let Some(display_test) = first.get("displayTest").and_then(toml::Value::as_str) {
        metadata
            .warnings
            .push(format!("displayTest is set to `{display_test}`"));
    }
    if let Some(display_url) = table
        .get("displayURL")
        .and_then(toml::Value::as_str)
        .or_else(|| first.get("displayURL").and_then(toml::Value::as_str))
    {
        metadata
            .contacts
            .insert("homepage".into(), display_url.to_string());
    }
    if let Some(loader_version) = table.get("loaderVersion").and_then(toml::Value::as_str) {
        metadata.depends.push({
            let name = if is_neoforge { "neoforge" } else { "forge" };
            let mut spec = DependencySpec::new(name, DependencyKind::Loader, true);
            spec.constraint = Some(loader_version.to_string());
            spec
        });
    }
    if let Some(mod_loader) = table.get("modLoader").and_then(toml::Value::as_str) {
        if mod_loader != "javafml" {
            metadata.warnings.push(format!(
                "unusual modLoader `{mod_loader}` (expected javafml for Java mods)"
            ));
        }
    }
    if mods.len() > 1 {
        metadata.warnings.push(format!(
            "{file} declares {} mods; MATS reports the first one",
            mods.len()
        ));
    }

    if let Some(dependencies) = table.get("dependencies").and_then(toml::Value::as_table) {
        for (owner, entries) in dependencies {
            let entries = entries.as_array().cloned().unwrap_or_default();
            for entry in entries {
                let Some(object) = entry.as_table() else {
                    continue;
                };
                let Some(mod_id) = object.get("modId").and_then(toml::Value::as_str) else {
                    continue;
                };
                let constraint = object
                    .get("versionRange")
                    .and_then(toml::Value::as_str)
                    .map(str::to_string);
                let mandatory = object
                    .get("mandatory")
                    .and_then(toml::Value::as_bool)
                    .unwrap_or(true);
                let kind = classify_dependency(mod_id);
                if kind == DependencyKind::Minecraft {
                    if let Some(constraint) = &constraint {
                        metadata.mc_constraints.push(constraint.clone());
                    }
                }
                let mut spec = DependencySpec::new(mod_id, kind, mandatory);
                spec.constraint = constraint;
                spec.side = object
                    .get("side")
                    .and_then(toml::Value::as_str)
                    .map(str::to_string);
                spec.ordering = object
                    .get("ordering")
                    .and_then(toml::Value::as_str)
                    .map(str::to_string);
                if owner != &id {
                    spec.reason = Some(format!("declared by `{owner}`"));
                }
                metadata.depends.push(spec);
            }
        }
    }
    if let Some(mixins) = table.get("mixins").and_then(toml::Value::as_array) {
        for entry in mixins {
            if let Some(config) = entry
                .as_table()
                .and_then(|table| table.get("config"))
                .and_then(toml::Value::as_str)
            {
                metadata.mixins.push(config.to_string());
            }
        }
    }

    if metadata.mc_constraints.is_empty() {
        metadata
            .warnings
            .push("no `minecraft` dependency range is declared".to_string());
    }
    Ok(metadata)
}

/// Parse a mixin configuration file.
pub fn parse_mixin_config(path: &str, text: &str) -> Result<MixinConfig> {
    let value: Value =
        serde_json::from_str(text).with_context(|| format!("{path} is not valid JSON"))?;
    let object = value
        .as_object()
        .with_context(|| format!("{path} must contain a JSON object"))?;
    let mut config = MixinConfig {
        path: path.to_string(),
        package: object
            .get("package")
            .and_then(Value::as_str)
            .map(str::to_string),
        refmap: object
            .get("refmap")
            .and_then(Value::as_str)
            .map(str::to_string),
        compatibility_level: object
            .get("compatibilityLevel")
            .and_then(Value::as_str)
            .map(str::to_string),
        plugin: object
            .get("plugin")
            .and_then(Value::as_str)
            .map(str::to_string),
        min_version: object
            .get("minVersion")
            .and_then(Value::as_str)
            .map(str::to_string),
        required: object
            .get("required")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        default_require: object
            .get("injectors")
            .and_then(|injectors| injectors.get("defaultRequire").and_then(Value::as_i64)),
        priority: object.get("priority").and_then(Value::as_i64),
        ..MixinConfig::default()
    };
    for key in ["mixins", "client", "server"] {
        let Some(entries) = object.get(key).and_then(Value::as_array) else {
            continue;
        };
        for entry in entries {
            match entry {
                Value::String(name) => match key {
                    "mixins" => config.mixins.push(name.clone()),
                    "client" => config.client.push(name.clone()),
                    _ => config.server.push(name.clone()),
                },
                Value::Object(map) => {
                    if let Some(name) = map.get("config").and_then(Value::as_str) {
                        // Nested configuration reference; recorded as a warning
                        // so the analyzer can follow it up.
                        config
                            .warnings
                            .push(format!("nested mixin config reference `{name}`"));
                    }
                }
                _ => config
                    .warnings
                    .push(format!("`{key}` contains an unsupported entry")),
            }
        }
    }
    if config.package.is_none() && config.mixin_count() > 0 {
        config
            .warnings
            .push("mixin classes are declared without a `package`".into());
    }
    if let Some(injectors) = object.get("injectors").and_then(Value::as_object) {
        for (key, value) in injectors {
            if let Some(number) = value.as_i64() {
                config.injectors.insert(key.clone(), number);
            }
        }
    }
    Ok(config)
}

fn required_str<'a>(
    object: &'a serde_json::Map<String, Value>,
    key: &str,
    file: &str,
) -> Result<&'a str> {
    object
        .get(key)
        .and_then(Value::as_str)
        .with_context(|| format!("{file} is missing required field `{key}`"))
}

fn constraints_from_value(value: &Value) -> Vec<String> {
    match value {
        Value::String(text) => vec![text.clone()],
        Value::Array(entries) => entries
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect(),
        _ => Vec::new(),
    }
}

fn names_from_value(value: &Value) -> Vec<String> {
    match value {
        Value::String(text) => vec![text.clone()],
        Value::Array(entries) => entries
            .iter()
            .filter_map(|entry| match entry {
                Value::String(text) => Some(text.clone()),
                Value::Object(map) => map.get("name").and_then(Value::as_str).map(str::to_string),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// Classify a dependency id into the loader-agnostic categories MATS uses.
pub fn classify_dependency(name: &str) -> DependencyKind {
    match name.to_ascii_lowercase().as_str() {
        "minecraft" => DependencyKind::Minecraft,
        "java" => DependencyKind::Java,
        "fabricloader"
        | "fabric-loader"
        | "forge"
        | "neoforge"
        | "quilt_loader"
        | "quilt-loader"
        | "javafml"
        | "lowcodefml"
        | "quilt_loader_plugin" => DependencyKind::Loader,
        _ => DependencyKind::Mod,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FABRIC_JSON: &str = r#"{
        "schemaVersion": 1,
        "id": "example-mod",
        "version": "1.2.3",
        "name": "Example Mod",
        "description": "Does example things",
        "authors": ["Alice", {"name": "Bob"}],
        "license": "MIT",
        "environment": "client",
        "entrypoints": {
            "main": ["com.example.ExampleMod", {"value": "com.example.Other::init", "environment": "client"}],
            "client": ["com.example.ClientInit"]
        },
        "mixins": ["example.mixins.json", {"config": "example.client.mixins.json", "environment": "client"}],
        "accessWidener": "example.accesswidener",
        "depends": {"minecraft": ">=26.1 <26.3", "fabricloader": ">=0.19.0", "java": ">=25"},
        "breaks": {"old-lib": "<2.0"},
        "contact": {"homepage": "https://example.invalid"}
    }"#;

    #[test]
    fn parses_fabric_metadata() {
        let meta = parse_fabric(FABRIC_JSON).unwrap();
        assert_eq!(meta.id, "example-mod");
        assert_eq!(meta.name, "Example Mod");
        assert_eq!(meta.version, "1.2.3");
        assert_eq!(meta.environment.as_deref(), Some("client"));
        assert_eq!(meta.authors, vec!["Alice", "Bob"]);
        assert_eq!(meta.mc_constraints, vec![">=26.1 <26.3"]);
        assert_eq!(meta.java_constraint.as_deref(), Some(">=25"));
        assert_eq!(meta.mixins.len(), 2);
        assert_eq!(meta.access_wideners, vec!["example.accesswidener"]);
        assert_eq!(meta.entrypoints.len(), 3);
        assert_eq!(
            meta.mixins,
            vec![
                "example.mixins.json".to_string(),
                "example.client.mixins.json".to_string()
            ]
        );
        assert!(meta.declares_side(mats_core::Side::Client));
        assert!(!meta.declares_side(mats_core::Side::Server));
    }

    #[test]
    fn requires_id_in_fabric_metadata() {
        let error = parse_fabric(r#"{"version":"1.0"}"#).unwrap_err();
        assert!(error.to_string().contains("missing required field `id`"));
        assert!(parse_fabric("not json").is_err());
    }

    #[test]
    fn parses_quilt_metadata() {
        let text = r#"{
            "schema_version": 1,
            "quilt_loader": {
                "group": "com.example",
                "id": "quilt-mod",
                "version": "2.0.0",
                "license": "Apache-2.0",
                "metadata": {
                    "name": "Quilt Mod",
                    "description": "Quilted",
                    "contributors": {"Carol": "Author"},
                    "contact": {"source": "https://example.invalid/src"}
                },
                "entrypoints": {"init": [{"adapter": "default", "value": "com.example.Init"}]},
                "mixins": ["quiltmod.mixins.json"],
                "access_widener": "quiltmod.accesswidener",
                "depends": [
                    {"id": "minecraft", "versions": ">=26.1"},
                    {"id": "quilt_loader", "versions": ">=0.29.0"}
                ]
            }
        }"#;
        let meta = parse_quilt(text).unwrap();
        assert_eq!(meta.id, "quilt-mod");
        assert_eq!(meta.loader, Some(Loader::Quilt));
        assert_eq!(meta.mc_constraints, vec![">=26.1"]);
        assert_eq!(meta.authors, vec!["Carol"]);
        assert_eq!(meta.entrypoints[0].value, "com.example.Init");
        assert_eq!(meta.mixins, vec!["quiltmod.mixins.json"]);
        assert_eq!(meta.access_wideners, vec!["quiltmod.accesswidener"]);
    }

    #[test]
    fn parses_forge_toml() {
        let text = r#"
modLoader="javafml"
loaderVersion="[47,)"
license="MIT"
issueTrackerURL="https://example.invalid/issues"

[[mods]]
modId="forge-mod"
version="1.5.0"
displayName="Forge Mod"
authors="Dave, Erin"
description='''
Multi
line
'''
logoFile="logo.png"

[[dependencies.forge-mod]]
modId="forge"
mandatory=true
versionRange="[47,)"
ordering="NONE"
side="BOTH"

[[dependencies.forge-mod]]
modId="minecraft"
mandatory=true
versionRange="[1.20.1,1.21)"
ordering="NONE"
side="BOTH"

[[mixins]]
config="forgemod.mixins.json"
"#;
        let meta = parse_toml_mods(text, "META-INF/mods.toml", false).unwrap();
        assert_eq!(meta.id, "forge-mod");
        assert_eq!(meta.loader, Some(Loader::Forge));
        assert_eq!(meta.authors, vec!["Dave", "Erin"]);
        assert_eq!(meta.mc_constraints, vec!["[1.20.1,1.21)"]);
        assert_eq!(meta.mixins, vec!["forgemod.mixins.json"]);
        assert!(meta.description.unwrap().contains("Multi"));
        let loader_dep = meta
            .depends
            .iter()
            .find(|dep| dep.mod_id == "forge")
            .unwrap();
        assert_eq!(loader_dep.kind, DependencyKind::Loader);
    }

    #[test]
    fn parses_neoforge_toml_with_optional_dependency() {
        let text = r#"
modLoader="javafml"
loaderVersion="[1,)"
license="LGPL-2.1"
[[mods]]
modId="neo-mod"
version="3.0.0"
displayName="Neo Mod"
[[dependencies.neo-mod]]
modId="neoforge"
type="required"
versionRange="[26.3.0,)"
ordering="NONE"
side="BOTH"
[[dependencies.neo-mod]]
modId="minecraft"
type="required"
versionRange="[26.3,26.4)"
ordering="NONE"
side="BOTH"
[[dependencies.neo-mod]]
modId="jei"
type="optional"
versionRange="[1,)"
ordering="AFTER"
side="CLIENT"
mandatory=false
"#;
        let meta = parse_toml_mods(text, "META-INF/neoforge.mods.toml", true).unwrap();
        assert_eq!(meta.loader, Some(Loader::NeoForge));
        assert_eq!(meta.mc_constraints, vec!["[26.3,26.4)"]);
        let optional = meta.depends.iter().find(|dep| dep.mod_id == "jei").unwrap();
        assert!(!optional.mandatory);
        assert_eq!(optional.side.as_deref(), Some("CLIENT"));
        assert_eq!(optional.ordering.as_deref(), Some("AFTER"));
    }

    #[test]
    fn rejects_toml_without_mods_table() {
        let error =
            parse_toml_mods("modLoader=\"javafml\"", "META-INF/mods.toml", false).unwrap_err();
        assert!(error.to_string().contains("[[mods]]"));
    }

    #[test]
    fn parses_mixin_configuration() {
        let text = r#"{
            "required": true,
            "package": "com.example.mixin",
            "compatibilityLevel": "JAVA_21",
            "refmap": "example.refmap.json",
            "minVersion": "0.8",
            "plugin": "com.example.ExampleMixinPlugin",
            "mixins": ["CommonMixin"],
            "client": ["ClientMixin"],
            "server": ["ServerMixin"],
            "injectors": {"defaultRequire": 1},
            "priority": 1000
        }"#;
        let config = parse_mixin_config("example.mixins.json", text).unwrap();
        assert_eq!(config.mixin_count(), 3);
        assert_eq!(config.compatibility_level.as_deref(), Some("JAVA_21"));
        assert_eq!(config.default_require, Some(1));
        assert_eq!(config.priority, Some(1000));
        assert_eq!(
            config.qualified_classes(),
            vec![
                "com.example.mixin.CommonMixin",
                "com.example.mixin.ClientMixin",
                "com.example.mixin.ServerMixin"
            ]
        );
    }

    #[test]
    fn classifies_dependencies() {
        assert_eq!(classify_dependency("minecraft"), DependencyKind::Minecraft);
        assert_eq!(classify_dependency("java"), DependencyKind::Java);
        assert_eq!(classify_dependency("FabricLoader"), DependencyKind::Loader);
        assert_eq!(classify_dependency("neoforge"), DependencyKind::Loader);
        assert_eq!(classify_dependency("sodium"), DependencyKind::Mod);
    }

    #[test]
    fn parses_full_jar_metadata() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("mod.jar");
        jar_inspector::write_jar(
            &path,
            &[
                ("fabric.mod.json", FABRIC_JSON.as_bytes()),
                (
                    "example.mixins.json",
                    br#"{"package":"com.example.mixin","mixins":["A"]}"#,
                ),
            ],
        )
        .unwrap();
        let inspection = JarInspection::inspect(&path).unwrap();
        let parsed = parse_inspection(&inspection).unwrap();
        assert_eq!(parsed.loader, Some(Loader::Fabric));
        assert_eq!(parsed.mixin_configs.len(), 1);
        assert!(
            parsed
                .warnings
                .iter()
                .any(|warning| warning.contains("missing from the archive"))
        );
    }
}
