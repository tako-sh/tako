use std::fs;
use std::path::Path;

use tako_runtime::{PluginContext, RuntimeDef};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PresetGroup {
    Js,
    Go,
    Unknown,
}

impl PresetGroup {
    pub fn id(self) -> &'static str {
        match self {
            PresetGroup::Js => "javascript",
            PresetGroup::Go => "go",
            PresetGroup::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuildAdapter {
    Bun,
    Node,
    Go,
    Unknown,
}

impl BuildAdapter {
    pub fn from_id(value: &str) -> Option<Self> {
        match value {
            "bun" => Some(BuildAdapter::Bun),
            "node" => Some(BuildAdapter::Node),
            "go" => Some(BuildAdapter::Go),
            _ => None,
        }
    }

    pub fn id(self) -> &'static str {
        match self {
            BuildAdapter::Bun => "bun",
            BuildAdapter::Node => "node",
            BuildAdapter::Go => "go",
            BuildAdapter::Unknown => "unknown",
        }
    }

    pub fn default_preset(self) -> &'static str {
        self.id()
    }

    pub fn preset_group(self) -> PresetGroup {
        match self {
            BuildAdapter::Bun | BuildAdapter::Node => PresetGroup::Js,
            BuildAdapter::Go => PresetGroup::Go,
            BuildAdapter::Unknown => PresetGroup::Unknown,
        }
    }

    pub fn runtime_def(self) -> Option<RuntimeDef> {
        tako_runtime::runtime_def_for(self.id(), None)
    }

    pub fn version_probe_tool(self) -> &'static str {
        self.id()
    }

    /// Produce a RuntimeDef using the plugin system with project context.
    pub fn runtime_def_with_context(self, ctx: &PluginContext) -> Option<RuntimeDef> {
        tako_runtime::runtime_def_for(self.id(), Some(ctx))
    }

    pub fn infer_main_entrypoint(self, project_dir: &Path) -> Option<String> {
        let def = self.runtime_def()?;
        infer_main_entrypoint_from_def(&def, project_dir)
    }
}

/// Infer the main entrypoint for a project using the runtime definition.
pub fn infer_main_entrypoint_from_def(def: &RuntimeDef, project_dir: &Path) -> Option<String> {
    if let Some(ref manifest) = def.entrypoint.manifest
        && let Some(main) = infer_manifest_main(project_dir, &manifest.file, &manifest.field)
    {
        // Only use manifest main if it looks like a file path (not a module specifier)
        if project_dir.join(&main).is_file() || main.starts_with('.') {
            return Some(main);
        }
    }

    for candidate in &def.entrypoint.candidates {
        if project_dir.join(candidate).is_file() {
            return Some(candidate.clone());
        }
    }

    None
}

/// Generate the builtin base preset TOML content for a runtime alias.
/// Returns a minimal preset with just `main` from the runtime definition.
pub fn builtin_base_preset_content_for_alias(alias: &str) -> Option<String> {
    let def = tako_runtime::runtime_def_for(alias, None)?;
    let mut out = String::new();
    if let Some(ref main) = def.preset.main {
        out.push_str(&format!(
            "main = \"{}\"\n",
            main.replace('\\', "\\\\").replace('"', "\\\"")
        ));
    }
    Some(out)
}

pub fn detect_build_adapter(project_dir: &Path) -> BuildAdapter {
    if has_ancestor_file(project_dir, "bun.lockb") || has_ancestor_file(project_dir, "bun.lock") {
        return BuildAdapter::Bun;
    }

    if project_dir.join("go.mod").is_file() {
        return BuildAdapter::Go;
    }

    if project_dir.join("package.json").is_file() {
        if let Ok(contents) = std::fs::read_to_string(project_dir.join("package.json"))
            && let Ok(json) = serde_json::from_str::<serde_json::Value>(&contents)
            && let Some(scripts) = json.get("scripts").and_then(|s| s.as_object())
            && scripts.values().any(|v| {
                v.as_str().is_some_and(|s| {
                    s.split(&[';', '&', '|', '\n'][..])
                        .any(|segment| segment.trim().starts_with("bun "))
                })
            })
        {
            return BuildAdapter::Bun;
        }
        return BuildAdapter::Node;
    }
    BuildAdapter::Unknown
}

/// Find the workspace root: the nearest ancestor (or self) containing `.git`.
/// Falls back to `max_levels` directories up if no `.git` is found.
fn find_workspace_root(start: &Path) -> &Path {
    const MAX_LEVELS: usize = 5;
    let mut current = start;
    for _ in 0..MAX_LEVELS {
        if current.join(".git").exists() {
            return current;
        }
        match current.parent() {
            Some(parent) => current = parent,
            None => break,
        }
    }
    current
}

/// Walk up from `dir` to workspace root looking for `filename`.
fn has_ancestor_file(dir: &Path, filename: &str) -> bool {
    let root = find_workspace_root(dir);
    let mut current = dir;
    loop {
        if current.join(filename).is_file() {
            return true;
        }
        if current == root {
            break;
        }
        match current.parent() {
            Some(parent) => current = parent,
            None => break,
        }
    }
    false
}

/// Read a main entrypoint from an arbitrary manifest file and field.
/// Supports JSON manifests (e.g. package.json with field "main").
/// Accepts any non-empty value — file paths, module specifiers, scoped packages.
/// Validation happens at deploy/dev time when the entrypoint actually resolves.
fn infer_manifest_main(project_dir: &Path, file: &str, field: &str) -> Option<String> {
    let manifest_path = project_dir.join(file);
    if !manifest_path.is_file() {
        return None;
    }

    let raw = fs::read_to_string(manifest_path).ok()?;
    let parsed: serde_json::Value = serde_json::from_str(&raw).ok()?;

    let mut value = &parsed;
    for segment in field.split('.') {
        value = value.get(segment)?;
    }
    let main = value.as_str()?.trim();
    if main.is_empty() {
        return None;
    }

    Some(main.to_string())
}

#[cfg(test)]
mod tests {
    use super::{
        BuildAdapter, PresetGroup, builtin_base_preset_content_for_alias, detect_build_adapter,
    };
    use tempfile::TempDir;

    #[test]
    fn detect_build_adapter_uses_bun_lock_for_bun() {
        let temp = TempDir::new().unwrap();
        std::fs::write(temp.path().join("bun.lock"), "").unwrap();
        assert_eq!(detect_build_adapter(temp.path()), BuildAdapter::Bun);
    }

    #[test]
    fn detect_build_adapter_uses_package_json_for_node() {
        let temp = TempDir::new().unwrap();
        std::fs::write(temp.path().join("package.json"), r#"{"name":"demo"}"#).unwrap();
        assert_eq!(detect_build_adapter(temp.path()), BuildAdapter::Node);
    }

    #[test]
    fn detect_build_adapter_finds_bun_lock_in_ancestor() {
        let temp = TempDir::new().unwrap();
        std::fs::write(temp.path().join("bun.lock"), "").unwrap();
        let nested = temp.path().join("packages").join("my-app");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("package.json"), r#"{"name":"my-app"}"#).unwrap();
        assert_eq!(detect_build_adapter(&nested), BuildAdapter::Bun);
    }

    #[test]
    fn detect_build_adapter_finds_bun_from_package_json_scripts() {
        let temp = TempDir::new().unwrap();
        std::fs::write(
            temp.path().join("package.json"),
            r#"{"name":"demo","scripts":{"dev":"bun run index.ts"}}"#,
        )
        .unwrap();
        assert_eq!(detect_build_adapter(temp.path()), BuildAdapter::Bun);
    }

    #[test]
    fn node_main_inference_prioritizes_package_json_main() {
        let temp = TempDir::new().unwrap();
        std::fs::create_dir_all(temp.path().join("src")).unwrap();
        std::fs::write(temp.path().join("src/index.ts"), "export {};").unwrap();
        std::fs::write(temp.path().join("custom-entry.js"), "export default {};").unwrap();
        std::fs::write(
            temp.path().join("package.json"),
            r#"{"name":"demo","main":"custom-entry.js"}"#,
        )
        .unwrap();

        let inferred = BuildAdapter::Node.infer_main_entrypoint(temp.path());
        assert_eq!(inferred.as_deref(), Some("custom-entry.js"));
    }

    #[test]
    fn bun_main_inference_uses_requested_candidate_priority() {
        let temp = TempDir::new().unwrap();
        std::fs::create_dir_all(temp.path().join("src")).unwrap();
        std::fs::write(temp.path().join("index.jsx"), "export default {};").unwrap();
        std::fs::write(temp.path().join("src/index.ts"), "export {};").unwrap();

        let inferred = BuildAdapter::Bun.infer_main_entrypoint(temp.path());
        assert_eq!(inferred.as_deref(), Some("index.jsx"));
    }

    #[test]
    fn infer_skips_module_specifiers_from_package_json() {
        let temp = TempDir::new().unwrap();
        std::fs::write(
            temp.path().join("package.json"),
            r#"{"name":"demo","main":"@example/server"}"#,
        )
        .unwrap();

        // Module specifiers are not file paths — inference returns None.
        // Preset main handles them instead.
        let inferred = BuildAdapter::Node.infer_main_entrypoint(temp.path());
        assert_eq!(inferred, None);
    }

    #[test]
    fn infer_skips_bare_package_names_from_package_json() {
        let temp = TempDir::new().unwrap();
        std::fs::write(
            temp.path().join("package.json"),
            r#"{"name":"demo","main":"my-lib"}"#,
        )
        .unwrap();

        let inferred = BuildAdapter::Node.infer_main_entrypoint(temp.path());
        assert_eq!(inferred, None);
    }

    #[test]
    fn infer_skips_nonexistent_file_paths_from_package_json() {
        let temp = TempDir::new().unwrap();
        std::fs::write(
            temp.path().join("package.json"),
            r#"{"name":"demo","main":"dist/server.js"}"#,
        )
        .unwrap();

        let inferred = BuildAdapter::Node.infer_main_entrypoint(temp.path());
        assert_eq!(inferred, None);
    }

    #[test]
    fn builtin_base_preset_content_is_available_by_alias() {
        let bun = builtin_base_preset_content_for_alias("bun").expect("bun preset");
        let node = builtin_base_preset_content_for_alias("node").expect("node preset");
        let go = builtin_base_preset_content_for_alias("go").expect("go preset");

        assert!(bun.contains("main = \"src/index.ts\""));
        assert!(node.contains("main = \"index.js\""));
        assert!(go.contains("main = \"app\""));
        assert!(builtin_base_preset_content_for_alias("rust").is_none());
    }

    #[test]
    fn build_adapter_from_id_parses_known_values() {
        assert_eq!(BuildAdapter::from_id("bun"), Some(BuildAdapter::Bun));
        assert_eq!(BuildAdapter::from_id("node"), Some(BuildAdapter::Node));
        assert_eq!(BuildAdapter::from_id("go"), Some(BuildAdapter::Go));
        assert_eq!(BuildAdapter::from_id("rust"), None);
        assert_eq!(BuildAdapter::from_id("python"), None);
    }

    #[test]
    fn build_adapter_maps_to_preset_group() {
        assert_eq!(BuildAdapter::Bun.preset_group(), PresetGroup::Js);
        assert_eq!(BuildAdapter::Node.preset_group(), PresetGroup::Js);
        assert_eq!(BuildAdapter::Go.preset_group(), PresetGroup::Go);
        assert_eq!(BuildAdapter::Unknown.preset_group(), PresetGroup::Unknown);
        assert_eq!(PresetGroup::Js.id(), "javascript");
        assert_eq!(PresetGroup::Go.id(), "go");
    }

    #[test]
    fn version_probe_tool_matches_runtime_id() {
        assert_eq!(BuildAdapter::Bun.version_probe_tool(), "bun");
        assert_eq!(BuildAdapter::Go.version_probe_tool(), "go");
    }

    #[test]
    fn detect_build_adapter_uses_go_mod_for_go() {
        let temp = TempDir::new().unwrap();
        std::fs::write(temp.path().join("go.mod"), "module example.com/app").unwrap();
        assert_eq!(detect_build_adapter(temp.path()), BuildAdapter::Go);
    }

    #[test]
    fn detect_build_adapter_ignores_cargo_toml() {
        let temp = TempDir::new().unwrap();
        std::fs::write(
            temp.path().join("Cargo.toml"),
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"",
        )
        .unwrap();
        assert_eq!(detect_build_adapter(temp.path()), BuildAdapter::Unknown);
    }

    #[test]
    fn builtin_base_preset_content_parses_as_valid_preset() {
        for alias in &["bun", "node", "go"] {
            let content = builtin_base_preset_content_for_alias(alias).unwrap();
            let parsed: toml::Value = toml::from_str(&content).unwrap_or_else(|e| {
                panic!("failed to parse generated preset for {alias}: {e}\n---\n{content}")
            });
            assert!(parsed.get("main").is_some(), "{alias} missing main");
        }
    }
}
