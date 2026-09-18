//! Parsing and capability validation for runtime Fabric mod payloads.
//!
//! The launcher must know what it is about to load before creating the JVM.
//! This module reads a mod jar entirely in memory and produces a plan:
//! metadata, entrypoints, Mixin configs, nested jars, and explicit capability
//! limits.
//!
//! AccessWidener and Accessor/Invoker Mixins are not silently ignored. They are
//! recorded with the owning mod and source path. Launch-time preparation uses
//! the plan structurally and lets Fabric itself apply those features; only the
//! post-launch JVMTI fallback rejects them because it must preserve an already
//! loaded class's schema.

use std::io::{Cursor, Read};

/// One Fabric entrypoint declaration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeEntrypoint {
    /// Fabric language adapter, normally `default`.
    pub adapter: String,
    /// Entrypoint class name.
    pub value: String,
}

/// One Mixin configuration and the classes it declares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeMixinConfig {
    /// Resource path inside the mod jar.
    pub path: String,
    /// Mixin package in slash form.
    pub package: String,
    /// Refmap resource declared by the config, when present.
    pub refmap: Option<String>,
    /// Mixin class binary names from the client scope.
    pub client: Vec<String>,
    /// Mixin class binary names from the server/common scopes.
    pub server: Vec<String>,
    /// Mixin class binary names from the common `mixins` scope.
    pub mixins: Vec<String>,
}

impl RuntimeMixinConfig {
    /// Every class declared by this config.
    pub fn classes(&self) -> impl Iterator<Item = &str> {
        self.client
            .iter()
            .chain(self.server.iter())
            .chain(self.mixins.iter())
            .map(String::as_str)
    }
}

/// Why a mod cannot be injected with the currently implemented feature set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnsupportedFeature {
    /// A stable machine-readable category.
    pub kind: UnsupportedFeatureKind,
    /// The owning mod id.
    pub mod_id: String,
    /// The source resource or class path.
    pub path: String,
    /// Human-readable detail suitable for a diagnostic.
    pub detail: String,
}

/// Categories of currently unsupported runtime features.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnsupportedFeatureKind {
    /// The mod declares an access widener.
    AccessWidener,
    /// A Mixin class uses `@Accessor`.
    AccessorMixin,
    /// A Mixin class uses `@Invoker`.
    InvokerMixin,
}

/// A validated, in-memory view of one Fabric mod.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeModPlan {
    /// `fabric.mod.json` id.
    pub id: String,
    /// Human-readable name.
    pub name: String,
    /// Mod version.
    pub version: String,
    /// Fabric environment string (`client`, `server`, `*`).
    pub environment: String,
    /// Client entrypoints in declaration order.
    pub client_entrypoints: Vec<RuntimeEntrypoint>,
    /// Mixin configs declared by the mod.
    pub mixin_configs: Vec<RuntimeMixinConfig>,
    /// Nested jar paths declared by `fabric.mod.json`, in declaration order.
    pub nested_jars: Vec<String>,
    /// Unsupported features discovered during validation.
    pub unsupported: Vec<UnsupportedFeature>,
}

impl RuntimeModPlan {
    /// True when no unsupported feature blocks a full injection.
    #[must_use]
    pub fn is_fully_supported(&self) -> bool {
        self.unsupported.is_empty()
    }
}

/// Parse and validate a Fabric mod jar image.
///
/// # Errors
///
/// Returns [`RuntimeModError`] if the zip, `fabric.mod.json`, or a declared
/// Mixin config is malformed. Unsupported features are *not* errors; they are
/// returned in [`RuntimeModPlan::unsupported`] so the caller can choose strict
/// or partial behavior.
pub fn parse_runtime_mod(bytes: &[u8]) -> Result<RuntimeModPlan, RuntimeModError> {
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes)).map_err(RuntimeModError::Zip)?;
    let metadata_text = read_entry_string(&mut archive, "fabric.mod.json")?;
    let metadata: serde_json::Value =
        serde_json::from_str(&metadata_text).map_err(RuntimeModError::MetadataJson)?;

    let id = string_field(&metadata, "id").ok_or(RuntimeModError::MissingField("id"))?;
    let name = string_field(&metadata, "name").unwrap_or_else(|| id.clone());
    let version =
        value_string(metadata.get("version")).ok_or(RuntimeModError::MissingField("version"))?;
    let environment = string_field(&metadata, "environment").unwrap_or_else(|| "*".to_string());

    let client_entrypoints = parse_entrypoints(&metadata, "client");
    let mixin_configs = parse_mixin_configs(&mut archive, &metadata, &id)?;
    let nested_jars = parse_nested_jars(&metadata);
    let mut unsupported = Vec::new();

    if metadata
        .get("accessWidener")
        .is_some_and(|value| !value.is_null())
    {
        unsupported.push(UnsupportedFeature {
            kind: UnsupportedFeatureKind::AccessWidener,
            mod_id: id.clone(),
            path: "fabric.mod.json".to_string(),
            detail: "runtime AccessWidener application is not implemented".to_string(),
        });
    }

    for config in &mixin_configs {
        for class in config.classes() {
            let entry = format!("{}.class", class.replace('.', "/"));
            let class_bytes = read_entry_bytes(&mut archive, &entry)?;
            if contains_utf8(&class_bytes, "Lorg/spongepowered/asm/mixin/gen/Accessor;") {
                unsupported.push(UnsupportedFeature {
                    kind: UnsupportedFeatureKind::AccessorMixin,
                    mod_id: id.clone(),
                    path: entry,
                    detail: format!("{class} declares @Accessor"),
                });
            } else if contains_utf8(&class_bytes, "Lorg/spongepowered/asm/mixin/gen/Invoker;") {
                unsupported.push(UnsupportedFeature {
                    kind: UnsupportedFeatureKind::InvokerMixin,
                    mod_id: id.clone(),
                    path: entry,
                    detail: format!("{class} declares @Invoker"),
                });
            }
        }
    }

    Ok(RuntimeModPlan {
        id,
        name,
        version,
        environment,
        client_entrypoints,
        mixin_configs,
        nested_jars,
        unsupported,
    })
}

/// Read one entry from a mod jar image without touching the filesystem.
///
/// # Errors
///
/// Returns [`RuntimeModError`] if the entry is absent, unreadable, or not a
/// valid zip when the caller expects to recurse into a nested jar.
pub fn read_mod_entry(bytes: &[u8], path: &str) -> Result<Vec<u8>, RuntimeModError> {
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes)).map_err(RuntimeModError::Zip)?;
    read_entry_bytes(&mut archive, path)
}
fn parse_nested_jars(metadata: &serde_json::Value) -> Vec<String> {
    metadata
        .get("jars")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            entry
                .get("file")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        })
        .collect()
}
fn parse_entrypoints(metadata: &serde_json::Value, key: &str) -> Vec<RuntimeEntrypoint> {
    let Some(entries) = metadata
        .get("entrypoints")
        .and_then(|value| value.get(key))
        .and_then(serde_json::Value::as_array)
    else {
        return Vec::new();
    };

    entries
        .iter()
        .filter_map(|entry| match entry {
            serde_json::Value::String(value) => Some(RuntimeEntrypoint {
                adapter: "default".to_string(),
                value: value.clone(),
            }),
            serde_json::Value::Object(object) => {
                let value = value_string(object.get("value"))?;
                let adapter = string_field(entry, "adapter").unwrap_or_else(|| "default".into());
                Some(RuntimeEntrypoint { adapter, value })
            }
            _ => None,
        })
        .collect()
}

fn parse_mixin_configs<R: Read + std::io::Seek>(
    archive: &mut zip::ZipArchive<R>,
    metadata: &serde_json::Value,
    mod_id: &str,
) -> Result<Vec<RuntimeMixinConfig>, RuntimeModError> {
    let Some(configs) = metadata.get("mixins").and_then(serde_json::Value::as_array) else {
        return Ok(Vec::new());
    };

    let mut out = Vec::new();
    for config in configs {
        let path = match config {
            serde_json::Value::String(path) => Some(path.as_str()),
            serde_json::Value::Object(object) => {
                object.get("config").and_then(serde_json::Value::as_str)
            }
            _ => None,
        };
        let Some(path) = path else {
            return Err(RuntimeModError::UnsupportedMixinDeclaration {
                mod_id: mod_id.to_string(),
            });
        };
        let text = read_entry_string(archive, path)?;
        let json: serde_json::Value =
            serde_json::from_str(&text).map_err(RuntimeModError::MixinJson)?;
        let package = string_field(&json, "package").unwrap_or_default();
        let refmap = string_field(&json, "refmap");
        let classes = |keys: &[&str]| -> Vec<String> {
            let mut classes = Vec::new();
            for key in keys {
                let Some(values) = json.get(*key).and_then(serde_json::Value::as_array) else {
                    continue;
                };
                for value in values {
                    if let Some(simple) = value.as_str() {
                        classes.push(if package.is_empty() {
                            simple.to_string()
                        } else {
                            format!("{package}.{simple}")
                        });
                    }
                }
            }
            classes
        };
        out.push(RuntimeMixinConfig {
            path: path.to_string(),
            package: package.clone(),
            refmap,
            client: classes(&["client"]),
            server: classes(&["server"]),
            mixins: classes(&["mixins"]),
        });
    }
    Ok(out)
}

fn read_entry_bytes<R: Read + std::io::Seek>(
    archive: &mut zip::ZipArchive<R>,
    path: &str,
) -> Result<Vec<u8>, RuntimeModError> {
    let mut entry = archive
        .by_name(path)
        .map_err(|_| RuntimeModError::MissingEntry(path.to_string()))?;
    let mut bytes = Vec::new();
    entry
        .read_to_end(&mut bytes)
        .map_err(|source| RuntimeModError::Read {
            path: path.to_string(),
            source,
        })?;
    Ok(bytes)
}

fn read_entry_string<R: Read + std::io::Seek>(
    archive: &mut zip::ZipArchive<R>,
    path: &str,
) -> Result<String, RuntimeModError> {
    String::from_utf8(read_entry_bytes(archive, path)?).map_err(|source| RuntimeModError::Utf8 {
        path: path.to_string(),
        source,
    })
}

fn string_field(value: &serde_json::Value, key: &str) -> Option<String> {
    value_string(value.get(key))
}

fn value_string(value: Option<&serde_json::Value>) -> Option<String> {
    match value? {
        serde_json::Value::String(text) => Some(text.clone()),
        serde_json::Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

fn contains_utf8(haystack: &[u8], needle: &str) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle.as_bytes())
}

/// Build a single-entry jar containing a `required:false` Mixin config.
///
/// Runtime injection targets classes which may already be loaded. Mixin's
/// normal policy rejects that for required configs; the reference runtime
/// injection path marks the injected config non-required and relies on JVMTI
/// redefinition instead. The refmap resource remains in the original mod jar,
/// so this override only changes the config entry.
///
/// # Errors
///
/// Returns [`RuntimeModError`] if the jar or JSON cannot be written.
pub fn relaxed_mixin_config_jar(
    config: &RuntimeMixinConfig,
    entry_name: &str,
    client_classes: &[&str],
) -> Result<Vec<u8>, RuntimeModError> {
    let package = config.package.clone();
    let client: Vec<String> = client_classes
        .iter()
        .map(|class| {
            class
                .strip_prefix(&format!("{package}."))
                .unwrap_or(class)
                .to_string()
        })
        .collect();
    let json = serde_json::json!({
        "required": false,
        "minVersion": "0.8",
        "package": package,
        "compatibilityLevel": "JAVA_16",
        "client": client,
        "injectors": { "defaultRequire": 1 },
        "refmap": config.refmap,
    });
    let json = serde_json::to_vec(&json).map_err(RuntimeModError::MetadataJson)?;

    let mut out = Cursor::new(Vec::new());
    {
        let mut writer = zip::ZipWriter::new(&mut out);
        writer
            .start_file(entry_name, zip::write::FileOptions::<()>::default())
            .map_err(RuntimeModError::Zip)?;
        std::io::Write::write_all(&mut writer, &json).map_err(|source| RuntimeModError::Read {
            path: entry_name.to_string(),
            source,
        })?;
        writer.finish().map_err(RuntimeModError::Zip)?;
    }
    Ok(out.into_inner())
}
/// Errors from parsing a runtime mod plan.
#[derive(Debug, thiserror::Error)]
pub enum RuntimeModError {
    /// The input was not a readable zip.
    #[error("mod jar is not a valid zip: {0}")]
    Zip(#[source] zip::result::ZipError),

    /// A required entry is absent.
    #[error("mod jar is missing {0}")]
    MissingEntry(String),

    /// `fabric.mod.json` is not valid JSON.
    #[error("fabric.mod.json is invalid: {0}")]
    MetadataJson(#[source] serde_json::Error),

    /// A Mixin config is not valid JSON.
    #[error("mixin config is invalid: {0}")]
    MixinJson(#[source] serde_json::Error),

    /// Metadata lacks a required field.
    #[error("fabric.mod.json is missing {0}")]
    MissingField(&'static str),

    /// A Mixin declaration is neither a string nor an object with `config`.
    #[error("mod {mod_id} has a malformed Mixin declaration")]
    UnsupportedMixinDeclaration {
        /// The owning mod id.
        mod_id: String,
    },

    /// A UTF-8 resource could not be decoded.
    #[error("{path} is not UTF-8: {source}")]
    Utf8 {
        /// The resource path.
        path: String,
        /// The decode error.
        #[source]
        source: std::string::FromUtf8Error,
    },

    /// An entry could not be read.
    #[error("cannot read {path}: {source}")]
    Read {
        /// The resource path.
        path: String,
        /// The I/O error.
        #[source]
        source: std::io::Error,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn jar(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut out = Cursor::new(Vec::new());
        {
            let mut writer = zip::ZipWriter::new(&mut out);
            for (name, data) in entries {
                writer
                    .start_file(*name, zip::write::FileOptions::<()>::default())
                    .expect("start");
                writer.write_all(data).expect("write");
            }
            writer.finish().expect("finish");
        }
        out.into_inner()
    }

    #[test]
    fn a_client_entrypoint_and_mixin_config_are_parsed() {
        let metadata = br#"{
            "schemaVersion": 1,
            "id": "fixture",
            "name": "Fixture",
            "version": "1.0.0",
            "environment": "client",
            "entrypoints": {"client": ["example.Mod"]},
            "mixins": ["fixture.mixins.json"]
        }"#;
        let mixins = br#"{
            "package": "example.mixin",
            "client": ["TitleMixin"]
        }"#;
        let class = b"\xca\xfe\xba\xbeLorg/spongepowered/asm/mixin/Mixin;";
        let bytes = jar(&[
            ("fabric.mod.json", metadata),
            ("fixture.mixins.json", mixins),
            ("example/mixin/TitleMixin.class", class),
        ]);

        let plan = parse_runtime_mod(&bytes).expect("parse");
        assert_eq!(plan.id, "fixture");
        assert_eq!(plan.client_entrypoints[0].value, "example.Mod");
        assert_eq!(plan.mixin_configs[0].client, ["example.mixin.TitleMixin"]);
        assert!(plan.is_fully_supported());
    }

    #[test]
    fn object_form_mixin_declarations_are_parsed() {
        let metadata = br#"{
            "schemaVersion": 1,
            "id": "fixture",
            "version": "1.0.0",
            "mixins": [{"config": "fixture.mixins.json", "environment": "client"}]
        }"#;
        let mixins = br#"{
            "package": "example.mixin",
            "client": ["TitleMixin"]
        }"#;
        let class = b"\xca\xfe\xba\xbeLorg/spongepowered/asm/mixin/Mixin;";
        let mut out = Cursor::new(Vec::new());
        {
            let mut writer = zip::ZipWriter::new(&mut out);
            let options: zip::write::FileOptions<'_, ()> = zip::write::FileOptions::default();
            writer
                .start_file("fabric.mod.json", options)
                .expect("write metadata");
            std::io::Write::write_all(&mut writer, metadata).expect("metadata");
            writer
                .start_file("fixture.mixins.json", options)
                .expect("write mixin config");
            std::io::Write::write_all(&mut writer, mixins).expect("mixin config");
            writer
                .start_file("example/mixin/TitleMixin.class", options)
                .expect("write class");
            std::io::Write::write_all(&mut writer, class).expect("class");
            writer.finish().expect("finish");
        }

        let plan = parse_runtime_mod(&out.into_inner()).expect("parse");
        assert_eq!(plan.mixin_configs.len(), 1);
        assert_eq!(plan.mixin_configs[0].path, "fixture.mixins.json");
        assert_eq!(plan.mixin_configs[0].client, ["example.mixin.TitleMixin"]);
    }

    #[test]
    fn accessor_mixins_are_reported_with_their_class_path() {
        let metadata = br#"{
            "schemaVersion": 1,
            "id": "fixture",
            "version": "1.0.0",
            "mixins": ["fixture.mixins.json"]
        }"#;
        let mixins = br#"{
            "package": "example.mixin",
            "mixins": ["Accessor"]
        }"#;
        let class = b"\xca\xfe\xba\xbeLorg/spongepowered/asm/mixin/gen/Accessor;";
        let bytes = jar(&[
            ("fabric.mod.json", metadata),
            ("fixture.mixins.json", mixins),
            ("example/mixin/Accessor.class", class),
        ]);

        let plan = parse_runtime_mod(&bytes).expect("parse");
        assert_eq!(plan.unsupported.len(), 1);
        assert_eq!(
            plan.unsupported[0].kind,
            UnsupportedFeatureKind::AccessorMixin
        );
        assert_eq!(plan.unsupported[0].path, "example/mixin/Accessor.class");
    }

    #[test]
    fn access_widener_is_reported() {
        let metadata = br#"{
            "schemaVersion": 1,
            "id": "fixture",
            "version": "1.0.0",
            "accessWidener": "fixture.accesswidener"
        }"#;
        let bytes = jar(&[("fabric.mod.json", metadata)]);

        let plan = parse_runtime_mod(&bytes).expect("parse");
        assert_eq!(
            plan.unsupported[0].kind,
            UnsupportedFeatureKind::AccessWidener
        );
    }
}
pub mod helper;
pub mod jni;
