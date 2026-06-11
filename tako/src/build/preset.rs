mod reference;
mod remote;

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::build::adapter::{BuildAdapter, PresetGroup};

pub use reference::{
    PresetReference, infer_adapter_from_preset_reference, parse_preset_reference,
    qualify_runtime_local_preset_ref,
};
pub use remote::{
    load_available_group_preset_definitions, load_available_group_presets, load_build_preset,
    load_dev_build_preset,
};

const FALLBACK_OFFICIAL_PRESET_REPO: &str = "tako-sh/presets";
const PACKAGE_REPOSITORY_URL: &str = env!("CARGO_PKG_REPOSITORY");
const OFFICIAL_JS_GROUP_PRESETS_PATH: &str = "presets/javascript.toml";
const OFFICIAL_GO_GROUP_PRESETS_PATH: &str = "presets/go.toml";
const OFFICIAL_RUST_GROUP_PRESETS_PATH: &str = "presets/rust.toml";
const EMBEDDED_JS_GROUP_PRESETS: &str = include_str!("../../../presets/javascript.toml");
const EMBEDDED_GO_GROUP_PRESETS: &str = include_str!("../../../presets/go.toml");
const EMBEDDED_RUST_GROUP_PRESETS: &str = include_str!("../../../presets/rust.toml");

/// Lightweight preset metadata: just name, main, and assets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PresetDefinition {
    pub name: String,
    pub main: Option<String>,
}

/// App preset providing entrypoint and asset defaults.
/// Loaded from `presets/<group>.toml` (fetched from GitHub, cached locally).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct AppPreset {
    pub name: String,
    #[serde(default)]
    pub main: Option<String>,
    #[serde(default)]
    pub assets: Vec<String>,
    /// Custom dev command (overrides runtime default in `tako dev`).
    #[serde(default)]
    pub dev: Vec<String>,
    /// Per-runtime overrides keyed by `BuildAdapter::id` (e.g. `"bun"`).
    /// When the active runtime has an entry, its fields take precedence over
    /// the preset defaults above. Missing fields in an override fall through
    /// to the preset default.
    #[serde(default)]
    pub runtime_overrides: HashMap<String, AppPresetRuntimeOverride>,
}

/// Per-runtime preset override. Parsed from `[<preset>.<runtime>]` sub-tables.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct AppPresetRuntimeOverride {
    #[serde(default)]
    pub dev: Vec<String>,
}

/// Backward-compatible alias.
pub type BuildPreset = AppPreset;

const KNOWN_PRESET_FIELDS: &[&str] = &["name", "main", "assets", "dev"];
const KNOWN_RUNTIME_OVERRIDE_FIELDS: &[&str] = &["dev"];

#[derive(Debug, Clone, Deserialize)]
struct AppPresetRaw {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    main: Option<String>,
    #[serde(default)]
    assets: Vec<String>,
    #[serde(default)]
    dev: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ResolvedPresetSource {
    pub preset_ref: String,
    pub repo: String,
    pub path: String,
    pub commit: String,
}

pub(super) fn official_alias_to_path(alias: &str) -> String {
    match alias.split_once('/') {
        Some((group, _)) => format!("presets/{group}.toml"),
        None => {
            if let Some(adapter) = BuildAdapter::from_id(alias) {
                let group = adapter.preset_group().id();
                format!("presets/{group}.toml")
            } else {
                format!("presets/{alias}.toml")
            }
        }
    }
}

pub(super) fn official_group_manifest_path(group: PresetGroup) -> Option<&'static str> {
    match group {
        PresetGroup::Js => Some(OFFICIAL_JS_GROUP_PRESETS_PATH),
        PresetGroup::Go => Some(OFFICIAL_GO_GROUP_PRESETS_PATH),
        PresetGroup::Rust => Some(OFFICIAL_RUST_GROUP_PRESETS_PATH),
        PresetGroup::Unknown => None,
    }
}

pub(super) fn embedded_group_manifest_content(path: &str) -> Option<&'static str> {
    match path {
        OFFICIAL_JS_GROUP_PRESETS_PATH => Some(EMBEDDED_JS_GROUP_PRESETS),
        OFFICIAL_GO_GROUP_PRESETS_PATH => Some(EMBEDDED_GO_GROUP_PRESETS),
        OFFICIAL_RUST_GROUP_PRESETS_PATH => Some(EMBEDDED_RUST_GROUP_PRESETS),
        _ => None,
    }
}

pub(super) fn parse_group_manifest_preset_definitions(
    path: &str,
    content: &str,
) -> Result<Vec<PresetDefinition>, String> {
    let parsed: toml::Value = toml::from_str(content)
        .map_err(|e| format!("Failed to parse preset group manifest '{}': {e}", path))?;
    let manifest = parsed.as_table().ok_or_else(|| {
        format!(
            "Preset group manifest '{}' must be a TOML table with [preset-name] sections.",
            path
        )
    })?;

    let mut definitions = Vec::new();
    for (name, value) in manifest {
        let Some(preset_table) = value.as_table() else {
            continue;
        };
        let trimmed = name.trim();
        if trimmed.is_empty() {
            continue;
        }
        let main = preset_table
            .get("main")
            .and_then(|value| value.as_str())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string);
        definitions.push(PresetDefinition {
            name: trimmed.to_string(),
            main,
        });
    }
    definitions.sort_by(|left, right| left.name.cmp(&right.name));
    definitions.dedup_by(|left, right| left.name == right.name);
    Ok(definitions)
}

pub(super) fn parse_group_manifest_preset_names(
    path: &str,
    content: &str,
) -> Result<Vec<String>, String> {
    Ok(parse_group_manifest_preset_definitions(path, content)?
        .into_iter()
        .map(|definition| definition.name)
        .collect())
}

pub(super) fn parse_resolved_preset_from_content(
    parsed_ref: &PresetReference,
    path: &str,
    content: &str,
) -> Result<BuildPreset, String> {
    match parsed_ref {
        PresetReference::OfficialAlias { name, .. } => {
            parse_official_alias_preset_content(name, path, content)
        }
    }
}

fn parse_official_alias_preset_content(
    alias: &str,
    path: &str,
    content: &str,
) -> Result<BuildPreset, String> {
    if let Some((_group, preset_name)) = alias.split_once('/') {
        return parse_group_preset_content(path, content, preset_name);
    }
    if BuildAdapter::from_id(alias).is_some() {
        // Base runtime presets (bun, node, go, rust) may not have a section in
        // the presets file — they use runtime defaults instead.
        return match parse_group_preset_content(path, content, alias) {
            Ok(preset) => Ok(preset),
            Err(e) if e.contains("was not found") => Ok(BuildPreset {
                name: alias.to_string(),
                ..Default::default()
            }),
            Err(e) => Err(e),
        };
    }
    parse_and_validate_preset(content, alias)
}

fn parse_group_preset_content(
    path: &str,
    content: &str,
    preset_name: &str,
) -> Result<BuildPreset, String> {
    let parsed: toml::Value = toml::from_str(content)
        .map_err(|e| format!("Failed to parse preset group manifest '{}': {e}", path))?;
    let manifest = parsed.as_table().ok_or_else(|| {
        format!(
            "Preset group manifest '{}' must be a TOML table with [preset-name] sections.",
            path
        )
    })?;
    let preset = manifest
        .get(preset_name)
        .ok_or_else(|| format!("Preset '{}' was not found in '{}'.", preset_name, path))?;
    let preset_table = preset.as_table().ok_or_else(|| {
        format!(
            "Preset '{}' in '{}' must be a table section ([{}]).",
            preset_name, path, preset_name
        )
    })?;
    let preset_content = toml::to_string(preset_table).map_err(|e| {
        format!(
            "Failed to parse preset '{}' in '{}': {}",
            preset_name, path, e
        )
    })?;
    parse_and_validate_preset(&preset_content, preset_name)
}

pub fn parse_and_validate_preset(content: &str, inferred_name: &str) -> Result<AppPreset, String> {
    let value: toml::Value =
        toml::from_str(content).map_err(|e| format!("Failed to parse preset TOML: {e}"))?;
    let table = value.as_table().ok_or_else(|| {
        "Preset TOML must be a table of key/value pairs at the top level.".to_string()
    })?;

    let mut runtime_overrides: HashMap<String, AppPresetRuntimeOverride> = HashMap::new();
    for (key, child) in table {
        if KNOWN_PRESET_FIELDS.contains(&key.as_str()) {
            continue;
        }
        if BuildAdapter::from_id(key).is_some() {
            let child_table = child.as_table().ok_or_else(|| {
                format!(
                    "Preset runtime override '[{key}]' must be a TOML table (got {})",
                    child.type_str()
                )
            })?;
            for field in child_table.keys() {
                if !KNOWN_RUNTIME_OVERRIDE_FIELDS.contains(&field.as_str()) {
                    tracing::warn!(
                        "Preset runtime override '[{key}]' has unknown field '{field}' — only dev is supported",
                    );
                }
            }
            let override_value = toml::Value::Table(child_table.clone())
                .try_into()
                .map_err(|e| format!("Failed to parse preset runtime override '[{key}]': {e}"))?;
            runtime_overrides.insert(key.to_string(), override_value);
            continue;
        }
        tracing::warn!(
            "Preset has unknown field '{key}' — only name, main, assets, dev, and [<runtime>] sub-tables are supported",
        );
    }

    let raw: AppPresetRaw =
        toml::from_str(content).map_err(|e| format!("Failed to parse preset TOML: {e}"))?;

    let name = raw
        .name
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| inferred_name.to_string());
    if name.is_empty() {
        return Err(
            "Preset name is empty. Set top-level `name` or use a .toml file name with a non-empty stem."
                .to_string(),
        );
    }

    Ok(AppPreset {
        name,
        main: raw.main,
        assets: raw.assets,
        dev: raw.dev,
        runtime_overrides,
    })
}

pub fn apply_adapter_base_runtime_defaults(
    preset: &mut AppPreset,
    adapter: BuildAdapter,
    plugin_ctx: Option<&tako_runtime::PluginContext>,
) -> Result<(), String> {
    if adapter == BuildAdapter::Unknown {
        return Ok(());
    }

    let def = tako_runtime::runtime_def_for(adapter.id(), plugin_ctx).ok_or_else(|| {
        format!(
            "Missing built-in runtime definition for '{}'.",
            adapter.id()
        )
    })?;

    if preset.main.is_none() {
        preset.main = def.preset.main;
    }

    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;

    fn parse_preset(raw: &str) -> Result<AppPreset, String> {
        parse_and_validate_preset(raw, "bun")
    }

    #[test]
    fn official_alias_to_path_maps_group_layout() {
        assert_eq!(official_alias_to_path("bun"), "presets/javascript.toml");
        assert_eq!(
            official_alias_to_path("javascript/tanstack-start"),
            "presets/javascript.toml"
        );
        assert_eq!(official_alias_to_path("node"), "presets/javascript.toml");
        assert_eq!(official_alias_to_path("go"), "presets/go.toml");
    }

    #[test]
    fn official_group_manifest_path_supports_known_families() {
        assert_eq!(
            official_group_manifest_path(PresetGroup::Js),
            Some("presets/javascript.toml")
        );
        assert_eq!(
            official_group_manifest_path(PresetGroup::Go),
            Some("presets/go.toml")
        );
        assert_eq!(official_group_manifest_path(PresetGroup::Unknown), None);
    }

    #[test]
    fn embedded_group_manifest_content_supports_known_group_paths() {
        assert!(embedded_group_manifest_content("presets/javascript.toml").is_some());
        assert!(embedded_group_manifest_content("presets/go.toml").is_some());
        assert!(embedded_group_manifest_content("presets/unknown.toml").is_none());
    }

    #[test]
    fn embedded_javascript_group_manifest_includes_nextjs() {
        let preset = parse_official_alias_preset_content(
            "javascript/nextjs",
            "presets/javascript.toml",
            embedded_group_manifest_content("presets/javascript.toml")
                .expect("embedded javascript manifest should exist"),
        )
        .expect("embedded javascript manifest should parse nextjs preset");
        assert_eq!(preset.name, "nextjs");
        assert_eq!(preset.main.as_deref(), Some(".next/tako-entry.mjs"));
        assert_eq!(preset.dev, vec!["next", "dev"]);
    }

    #[test]
    fn fetched_group_manifest_missing_preset_still_errors() {
        let fetched_content = r#"
[vite]
dev = ["vite", "dev"]
"#;

        let error = parse_resolved_preset_from_content(
            &PresetReference::OfficialAlias {
                name: "javascript/nextjs".to_string(),
                commit: None,
            },
            "presets/javascript.toml",
            fetched_content,
        )
        .expect_err("fetched manifest should not contain nextjs");
        assert!(error.contains("Preset 'nextjs' was not found"));
    }

    #[test]
    fn parse_group_manifest_preset_names_collects_sorted_sections() {
        let names = parse_group_manifest_preset_names(
            "presets/javascript.toml",
            r#"
[zeta]
main = "z.ts"

foo = "bar"

[alpha]
main = "a.ts"
"#,
        )
        .unwrap();
        assert_eq!(names, vec!["alpha".to_string(), "zeta".to_string()]);
    }

    #[test]
    fn parse_group_manifest_preset_definitions_reads_optional_main() {
        let definitions = parse_group_manifest_preset_definitions(
            "presets/javascript.toml",
            r#"
[tanstack-start]
main = "dist/server/tako-entry.mjs"

[no-main]
foo = "bar"
"#,
        )
        .unwrap();
        assert_eq!(
            definitions,
            vec![
                PresetDefinition {
                    name: "no-main".to_string(),
                    main: None,
                },
                PresetDefinition {
                    name: "tanstack-start".to_string(),
                    main: Some("dist/server/tako-entry.mjs".to_string()),
                },
            ]
        );
    }

    #[test]
    fn tanstack_start_preset_parses_from_group_manifest() {
        let content = r#"
[tanstack-start]
main = "dist/server/tako-entry.mjs"
assets = ["dist/client"]
"#;
        let preset = parse_official_alias_preset_content(
            "javascript/tanstack-start",
            "presets/javascript.toml",
            content,
        )
        .unwrap();
        assert_eq!(preset.name, "tanstack-start");
        assert_eq!(preset.main.as_deref(), Some("dist/server/tako-entry.mjs"));
        assert_eq!(preset.assets, vec!["dist/client"]);
    }

    #[test]
    fn nextjs_preset_parses_from_group_manifest() {
        let content = r#"
[nextjs]
main = ".next/tako-entry.mjs"
dev = ["next", "dev"]
"#;
        let preset = parse_official_alias_preset_content(
            "javascript/nextjs",
            "presets/javascript.toml",
            content,
        )
        .unwrap();
        assert_eq!(preset.name, "nextjs");
        assert_eq!(preset.main.as_deref(), Some(".next/tako-entry.mjs"));
        assert_eq!(preset.dev, vec!["next", "dev"]);
    }

    #[test]
    fn runtime_alias_returns_empty_preset_when_missing_from_manifest() {
        let content = r#"
[tanstack-start]
main = "dist/server/tako-entry.mjs"
"#;
        let preset = parse_official_alias_preset_content("bun", "presets/javascript.toml", content)
            .expect("base runtime preset should fall back to empty defaults");
        assert_eq!(preset.name, "bun");
        assert!(preset.main.is_none());
    }

    #[test]
    fn parse_official_alias_preset_content_rejects_missing_non_runtime_group_alias() {
        let content = r#"
[tanstack-start]
main = "dist/server/tako-entry.mjs"
"#;
        let err = parse_official_alias_preset_content(
            "javascript/missing",
            "presets/javascript.toml",
            content,
        )
        .expect_err("non-runtime group alias should still require manifest section");
        assert!(err.contains("Preset 'missing' was not found"));
    }

    #[test]
    fn apply_adapter_base_runtime_defaults_fills_missing_main_from_runtime() {
        let raw = r#"
name = "tanstack-start"
assets = ["dist/client"]
"#;
        let mut preset = parse_preset(raw).unwrap();
        apply_adapter_base_runtime_defaults(&mut preset, BuildAdapter::Bun, None).unwrap();

        // main was not set, so it gets filled from the bun runtime default.
        assert!(preset.main.is_some());
        // assets are untouched by the runtime defaults.
        assert_eq!(preset.assets, vec!["dist/client".to_string()]);
    }

    #[test]
    fn apply_adapter_base_runtime_defaults_keeps_explicit_main() {
        let raw = r#"
name = "custom-bun"
main = "custom-main.ts"
"#;
        let mut preset = parse_preset(raw).unwrap();
        apply_adapter_base_runtime_defaults(&mut preset, BuildAdapter::Bun, None).unwrap();

        assert_eq!(preset.main.as_deref(), Some("custom-main.ts"));
    }

    #[test]
    fn apply_adapter_base_runtime_defaults_skips_unknown_adapter() {
        let raw = r#"
name = "custom"
"#;
        let mut preset = parse_preset(raw).unwrap();
        apply_adapter_base_runtime_defaults(&mut preset, BuildAdapter::Unknown, None).unwrap();

        // Unknown adapter does nothing — main stays None.
        assert!(preset.main.is_none());
    }

    #[test]
    fn parse_and_validate_preset_accepts_name_main_assets() {
        let raw = r#"
name = "bun"
main = "index.ts"
assets = ["dist/client", "public"]
"#;
        let preset = parse_preset(raw).unwrap();
        assert_eq!(preset.name, "bun");
        assert_eq!(preset.main.as_deref(), Some("index.ts"));
        assert_eq!(
            preset.assets,
            vec!["dist/client".to_string(), "public".to_string()]
        );
    }

    #[test]
    fn parse_and_validate_preset_uses_inferred_name_when_missing() {
        let raw = r#"
main = "index.ts"
"#;
        let preset = parse_preset(raw).unwrap();
        assert_eq!(preset.name, "bun");
    }

    #[test]
    fn parse_and_validate_preset_defaults_to_empty_assets() {
        let raw = r#"
name = "bun"
main = "index.ts"
"#;
        let preset = parse_preset(raw).unwrap();
        assert!(preset.assets.is_empty());
    }

    #[test]
    fn parse_and_validate_preset_accepts_unknown_fields_with_warning() {
        // Unknown fields are accepted (parsed successfully) but logged as warnings.
        let raw = r#"
name = "bun"
main = "index.ts"
extra_field = "ignored"
"#;
        let preset = parse_preset(raw).unwrap();
        assert_eq!(preset.name, "bun");
        assert_eq!(preset.main.as_deref(), Some("index.ts"));
    }

    #[test]
    fn parse_and_validate_preset_accepts_top_level_assets() {
        let raw = r#"
name = "bun"
assets = ["dist/client"]
"#;
        let preset = parse_preset(raw).unwrap();
        assert_eq!(preset.assets, vec!["dist/client".to_string()]);
    }

    #[test]
    fn parse_and_validate_preset_collects_runtime_override_dev() {
        let raw = r#"
name = "tanstack-start"
main = "dist/server/tako-entry.mjs"
dev = ["vite", "dev"]

[bun]
dev = ["bunx", "--bun", "vite", "dev"]
"#;
        let preset = parse_preset(raw).unwrap();
        assert_eq!(preset.dev, vec!["vite".to_string(), "dev".to_string()]);
        let bun_override = preset
            .runtime_overrides
            .get("bun")
            .expect("bun runtime override");
        assert_eq!(
            bun_override.dev,
            vec![
                "bunx".to_string(),
                "--bun".to_string(),
                "vite".to_string(),
                "dev".to_string(),
            ]
        );
    }

    #[test]
    fn parse_and_validate_preset_collects_multiple_runtime_overrides() {
        let raw = r#"
dev = ["vite", "dev"]

[bun]
dev = ["bunx", "--bun", "vite", "dev"]

[node]
dev = ["npm", "run", "dev"]
"#;
        let preset = parse_preset(raw).unwrap();
        assert_eq!(preset.runtime_overrides.len(), 2);
        assert_eq!(
            preset.runtime_overrides.get("bun").unwrap().dev,
            vec![
                "bunx".to_string(),
                "--bun".to_string(),
                "vite".to_string(),
                "dev".to_string(),
            ]
        );
        assert_eq!(
            preset.runtime_overrides.get("node").unwrap().dev,
            vec!["npm".to_string(), "run".to_string(), "dev".to_string()],
        );
    }

    #[test]
    fn parse_and_validate_preset_without_runtime_overrides_has_empty_map() {
        let raw = r#"
name = "vite"
dev = ["vite", "dev"]
"#;
        let preset = parse_preset(raw).unwrap();
        assert!(preset.runtime_overrides.is_empty());
    }

    #[test]
    fn tanstack_start_runtime_override_parses_from_group_manifest() {
        let content = r#"
[tanstack-start]
main = "dist/server/tako-entry.mjs"
assets = ["dist/client"]
dev = ["vite", "dev"]

[tanstack-start.bun]
dev = ["bunx", "--bun", "vite", "dev"]
"#;
        let preset = parse_official_alias_preset_content(
            "javascript/tanstack-start",
            "presets/javascript.toml",
            content,
        )
        .unwrap();
        assert_eq!(preset.dev, vec!["vite".to_string(), "dev".to_string()]);
        assert_eq!(
            preset
                .runtime_overrides
                .get("bun")
                .expect("bun runtime override from group manifest")
                .dev,
            vec![
                "bunx".to_string(),
                "--bun".to_string(),
                "vite".to_string(),
                "dev".to_string(),
            ],
        );
    }
}
