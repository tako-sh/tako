use std::collections::{BTreeMap, HashMap};
use std::path::{Component, Path};

use crate::build::{BuildAdapter, BuildError, BuildExecutor};
use crate::config::{EncryptedSecretValue, SecretsStore, TakoToml};

#[derive(Debug, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub(super) struct DeployArchiveManifest {
    #[serde(default, skip_serializing_if = "DeployReleaseKind::is_native")]
    pub(super) release_kind: DeployReleaseKind,
    pub(super) app_name: String,
    pub(super) environment: String,
    pub(super) version: String,
    pub(super) runtime: String,
    pub(super) main: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) start: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) workflow_worker_main: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) workflow_run: Option<Vec<String>>,
    pub(super) idle_timeout: u32,
    pub(super) env_vars: BTreeMap<String, String>,
    pub(super) secret_names: Vec<String>,
    #[serde(default)]
    pub(super) images: tako_images::ImagesConfig,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) package_manager: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) package_manager_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) commit_message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) git_dirty: Option<bool>,
    /// Path from the archive root to the app directory (where tako.toml lives).
    /// Empty string means the app is at the archive root (single-app projects).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub(super) app_dir: String,
    /// Path from the archive root to the directory where deps should be installed.
    /// This is the runtime project root (where the lockfile lives).
    /// Empty string means install at the archive root.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub(super) install_dir: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) container_file: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) container_port: Option<u16>,
}

#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub(super) enum DeployReleaseKind {
    #[default]
    Native,
    Container,
}

impl DeployReleaseKind {
    fn is_native(&self) -> bool {
        matches!(self, Self::Native)
    }
}

pub(super) fn resolve_deploy_version_and_source_hash(
    executor: &BuildExecutor,
    source_root: &Path,
) -> Result<(String, String), BuildError> {
    let source_hash = executor.compute_source_hash(source_root)?;
    let version = executor.generate_version(Some(&source_hash))?;
    Ok((version, source_hash))
}

pub(super) fn resolve_git_commit_message(source_root: &Path) -> Option<String> {
    let output = std::process::Command::new("git")
        .args(["log", "-1", "--pretty=%s"])
        .current_dir(source_root)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let message = String::from_utf8(output.stdout).ok()?.trim().to_string();
    if message.is_empty() {
        None
    } else {
        Some(message)
    }
}

pub(super) fn normalize_main_path(value: &str, source: &str) -> Result<String, String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(format!("{source} main is empty"));
    }

    let raw_path = Path::new(trimmed);
    if raw_path.is_absolute() {
        return Err(format!(
            "{source} main '{trimmed}' must be relative to project root"
        ));
    }

    let mut normalized = trimmed.replace('\\', "/");
    while let Some(stripped) = normalized.strip_prefix("./") {
        normalized = stripped.to_string();
    }
    if normalized.starts_with('/') {
        return Err(format!(
            "{source} main '{trimmed}' must be relative to project root"
        ));
    }
    if Path::new(&normalized)
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(format!("{source} main '{trimmed}' must not contain '..'"));
    }
    if normalized.is_empty() {
        return Err(format!("{source} main is empty"));
    }
    Ok(normalized)
}

fn js_entrypoint_extension_for_index_paths(main: &str) -> Option<&str> {
    let extension = if let Some(value) = main.strip_prefix("index.") {
        value
    } else {
        main.strip_prefix("src/index.")?
    };

    if matches!(extension, "ts" | "tsx" | "js" | "jsx") {
        Some(extension)
    } else {
        None
    }
}

fn resolve_js_preset_main_for_project(
    project_dir: &Path,
    runtime_adapter: BuildAdapter,
    preset_main: &str,
) -> Option<String> {
    if !matches!(runtime_adapter, BuildAdapter::Bun | BuildAdapter::Node) {
        return None;
    }

    let extension = js_entrypoint_extension_for_index_paths(preset_main)?;
    let candidates = [
        format!("index.{extension}"),
        format!("src/index.{extension}"),
    ];
    candidates
        .into_iter()
        .find(|candidate| project_dir.join(candidate).is_file())
}

pub(crate) fn resolve_deploy_main(
    project_dir: &Path,
    runtime_adapter: BuildAdapter,
    tako_config: &TakoToml,
    preset_main: Option<&str>,
) -> Result<String, String> {
    if let Some(main) = &tako_config.main {
        return normalize_main_path(main, "tako.toml");
    }

    // Try runtime entrypoint inference (candidate files like index.ts)
    if let Some(inferred) = runtime_adapter.infer_main_entrypoint(project_dir) {
        return Ok(inferred);
    }

    if let Some(main) = preset_main {
        let normalized = normalize_main_path(main, "build preset")?;
        if let Some(resolved) =
            resolve_js_preset_main_for_project(project_dir, runtime_adapter, &normalized)
        {
            return Ok(resolved);
        }
        return Ok(normalized);
    }

    Err("No deploy entrypoint configured. Set `main` in tako.toml or preset `main`.".to_string())
}

#[allow(clippy::too_many_arguments)]
pub(super) fn build_deploy_archive_manifest(
    app_name: &str,
    environment: &str,
    version: &str,
    runtime_name: &str,
    main: &str,
    start: Option<Vec<String>>,
    workflow_worker_main: Option<String>,
    idle_timeout: u32,
    package_manager: Option<String>,
    commit_message: Option<String>,
    git_dirty: Option<bool>,
    app_env_vars: HashMap<String, String>,
    runtime_env_vars: HashMap<String, String>,
    env_secrets: Option<&HashMap<String, EncryptedSecretValue>>,
    images: tako_images::ImagesConfig,
    app_dir: String,
    install_dir: String,
) -> DeployArchiveManifest {
    let mut secret_names = env_secrets
        .map(|map| map.keys().cloned().collect::<Vec<_>>())
        .unwrap_or_default();
    secret_names.sort();

    let mut env_vars =
        build_manifest_env_vars(app_env_vars, runtime_env_vars, environment, runtime_name);
    // TAKO_BUILD is a non-secret env var derived from the version.
    // It's stored in app.json so the server can read it without the CLI.
    env_vars.insert("TAKO_BUILD".to_string(), version.to_string());

    DeployArchiveManifest {
        release_kind: DeployReleaseKind::Native,
        app_name: app_name.to_string(),
        environment: environment.to_string(),
        version: version.to_string(),
        runtime: runtime_name.to_string(),
        main: main.to_string(),
        start,
        workflow_worker_main,
        workflow_run: None,
        idle_timeout,
        env_vars,
        secret_names,
        images,
        package_manager,
        package_manager_version: None,
        commit_message,
        git_dirty,
        app_dir,
        install_dir,
        container_file: None,
        container_port: None,
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn build_container_deploy_archive_manifest(
    app_name: &str,
    environment: &str,
    version: &str,
    container_file: &str,
    idle_timeout: u32,
    commit_message: Option<String>,
    git_dirty: Option<bool>,
    app_env_vars: HashMap<String, String>,
    env_secrets: Option<&HashMap<String, EncryptedSecretValue>>,
    images: tako_images::ImagesConfig,
    app_dir: String,
    workflow_run: Option<Vec<String>>,
) -> DeployArchiveManifest {
    let mut secret_names = env_secrets
        .map(|map| map.keys().cloned().collect::<Vec<_>>())
        .unwrap_or_default();
    secret_names.sort();

    let mut env_vars =
        build_manifest_env_vars(app_env_vars, HashMap::new(), environment, "container");
    env_vars.insert("TAKO_BUILD".to_string(), version.to_string());

    DeployArchiveManifest {
        release_kind: DeployReleaseKind::Container,
        app_name: app_name.to_string(),
        environment: environment.to_string(),
        version: version.to_string(),
        runtime: "container".to_string(),
        main: String::new(),
        start: None,
        workflow_worker_main: None,
        workflow_run,
        idle_timeout,
        env_vars,
        secret_names,
        images,
        package_manager: None,
        package_manager_version: None,
        commit_message,
        git_dirty,
        app_dir,
        install_dir: String::new(),
        container_file: Some(container_file.to_string()),
        container_port: Some(3000),
    }
}

pub(super) fn decrypt_deploy_secrets(
    env: &str,
    secrets: &SecretsStore,
    usage_path: Option<&Path>,
) -> Result<HashMap<String, String>, Box<dyn std::error::Error>> {
    let encrypted = match secrets.get_env(env) {
        Some(s) if !s.is_empty() => s,
        _ => return Ok(HashMap::new()),
    };

    let key = super::super::secret::load_secret_key(env, secrets, usage_path)?;
    let mut decrypted = HashMap::new();
    for (name, encrypted_value) in encrypted {
        let value = crate::crypto::decrypt(&encrypted_value.value, &key)
            .map_err(|e| format!("Failed to decrypt secret '{}': {}", name, e))?;
        decrypted.insert(name.clone(), value);
    }
    Ok(decrypted)
}

pub(super) fn build_manifest_env_vars(
    app_env_vars: HashMap<String, String>,
    runtime_env_vars: HashMap<String, String>,
    environment: &str,
    runtime_name: &str,
) -> BTreeMap<String, String> {
    let mut merged = BTreeMap::new();

    // 1. Runtime defaults for this environment (lowest priority)
    if let Some(def) = tako_runtime::runtime_def_for(runtime_name, None)
        && let Some(env_defaults) = def.envs.environments.get(environment)
    {
        for (key, value) in env_defaults {
            merged.insert(key.clone(), value.clone());
        }
    }

    // 2. App-level env vars from tako.toml [vars] + [vars.<env>]
    for (key, value) in app_env_vars {
        merged.insert(key, value);
    }

    // 3. Runtime env vars (from runtime detection)
    for (key, value) in runtime_env_vars {
        merged.insert(key, value);
    }

    // 4. Derived env markers always set (highest priority)
    merged.insert("ENV".to_string(), environment.to_string());
    merged
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::build::BuildExecutor;
    use crate::config::{SecretsStore, TakoToml};
    use std::collections::HashMap;
    use tempfile::TempDir;

    #[test]
    fn decrypt_deploy_secrets_returns_empty_for_no_secrets() {
        let secrets = SecretsStore::default();
        let result = decrypt_deploy_secrets("production", &secrets, None).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn build_deploy_archive_manifest_includes_tako_build_in_env_vars() {
        let manifest = build_deploy_archive_manifest(
            "my-app",
            "production",
            "v123",
            "bun",
            "server/index.ts",
            None,
            None,
            300,
            Some("bun".to_string()),
            Some("feat: ship it".to_string()),
            Some(false),
            HashMap::new(),
            HashMap::new(),
            None,
            tako_images::ImagesConfig::default(),
            String::new(),
            String::new(),
        );
        assert_eq!(manifest.idle_timeout, 300);
        assert_eq!(
            manifest.env_vars.get("TAKO_BUILD"),
            Some(&"v123".to_string())
        );
        assert_eq!(manifest.package_manager, Some("bun".to_string()));
        assert_eq!(
            manifest.env_vars.get("ENV"),
            Some(&"production".to_string())
        );
        assert_eq!(manifest.commit_message.as_deref(), Some("feat: ship it"));
        assert_eq!(manifest.git_dirty, Some(false));
        assert_eq!(manifest.release_kind, DeployReleaseKind::Native);
        assert_eq!(manifest.container_file, None);
    }

    #[test]
    fn build_container_deploy_archive_manifest_marks_container_release() {
        let secrets = HashMap::from([(
            "API_KEY".to_string(),
            EncryptedSecretValue::new("x".to_string(), None),
        )]);

        let manifest = build_container_deploy_archive_manifest(
            "my-app",
            "production",
            "v123",
            "Dockerfile",
            300,
            Some("feat: ship it".to_string()),
            Some(false),
            HashMap::from([("APP_ENV".to_string(), "prod".to_string())]),
            Some(&secrets),
            tako_images::ImagesConfig::default(),
            "apps/web".to_string(),
            Some(vec!["./worker".to_string(), "video".to_string()]),
        );

        assert_eq!(manifest.release_kind, DeployReleaseKind::Container);
        assert_eq!(manifest.runtime, "container");
        assert_eq!(manifest.main, "");
        assert_eq!(manifest.container_file.as_deref(), Some("Dockerfile"));
        assert_eq!(manifest.container_port, Some(3000));
        assert_eq!(manifest.app_dir, "apps/web");
        assert_eq!(
            manifest.workflow_run,
            Some(vec!["./worker".to_string(), "video".to_string()])
        );
        assert_eq!(manifest.secret_names, vec!["API_KEY".to_string()]);
        assert_eq!(
            manifest.env_vars.get("TAKO_BUILD"),
            Some(&"v123".to_string())
        );
        assert_eq!(
            manifest.env_vars.get("ENV"),
            Some(&"production".to_string())
        );
        assert_eq!(manifest.env_vars.get("APP_ENV"), Some(&"prod".to_string()));
    }

    #[test]
    fn resolve_deploy_version_uses_source_hash_when_git_commit_missing() {
        let temp = TempDir::new().unwrap();
        let source_root = temp.path().join("source");
        std::fs::create_dir_all(&source_root).unwrap();
        std::fs::write(source_root.join("index.ts"), "export default 1;\n").unwrap();

        let executor = BuildExecutor::new(temp.path());
        let source_hash = executor.compute_source_hash(&source_root).unwrap();
        let (version, _source_hash) =
            resolve_deploy_version_and_source_hash(&executor, &source_root).unwrap();

        assert_eq!(version, format!("nogit_{}", &source_hash[..8]));
    }

    #[test]
    fn build_deploy_archive_manifest_includes_sorted_env_and_secret_names() {
        let app_env_vars = HashMap::from([
            ("Z_KEY".to_string(), "z".to_string()),
            ("A_KEY".to_string(), "a".to_string()),
        ]);
        let runtime_env_vars = HashMap::from([
            ("NODE_ENV".to_string(), "production".to_string()),
            ("BUN_ENV".to_string(), "production".to_string()),
        ]);
        let secrets = HashMap::from([
            (
                "API_KEY".to_string(),
                EncryptedSecretValue::new("x".to_string(), None),
            ),
            (
                "DB_URL".to_string(),
                EncryptedSecretValue::new("y".to_string(), None),
            ),
        ]);

        let manifest = build_deploy_archive_manifest(
            "my-app",
            "staging",
            "v1",
            "bun",
            "server/index.mjs",
            None,
            None,
            600,
            None,
            None,
            Some(true),
            app_env_vars,
            runtime_env_vars,
            Some(&secrets),
            tako_images::ImagesConfig::default(),
            String::new(),
            String::new(),
        );

        assert_eq!(manifest.app_name, "my-app");
        assert_eq!(manifest.environment, "staging");
        assert_eq!(manifest.version, "v1");
        assert_eq!(manifest.runtime, "bun");
        assert_eq!(manifest.main, "server/index.mjs");
        assert_eq!(manifest.start, None);
        assert_eq!(manifest.workflow_worker_main, None);
        assert_eq!(manifest.idle_timeout, 600);
        assert_eq!(manifest.git_dirty, Some(true));
        assert_eq!(
            manifest.env_vars.keys().cloned().collect::<Vec<_>>(),
            vec![
                "A_KEY".to_string(),
                "BUN_ENV".to_string(),
                "ENV".to_string(),
                "NODE_ENV".to_string(),
                "TAKO_BUILD".to_string(),
                "Z_KEY".to_string()
            ]
        );
        assert_eq!(manifest.env_vars.get("ENV"), Some(&"staging".to_string()));
        assert_eq!(
            manifest.env_vars.get("NODE_ENV"),
            Some(&"production".to_string())
        );
        assert_eq!(
            manifest.env_vars.get("BUN_ENV"),
            Some(&"production".to_string())
        );
        assert_eq!(
            manifest.secret_names,
            vec!["API_KEY".to_string(), "DB_URL".to_string()]
        );
    }

    #[test]
    fn build_deploy_archive_manifest_includes_explicit_start_command() {
        let manifest = build_deploy_archive_manifest(
            "my-app",
            "production",
            "v1",
            "bun",
            "app",
            Some(vec!["./app".to_string(), "--serve".to_string()]),
            None,
            300,
            None,
            None,
            None,
            HashMap::new(),
            HashMap::new(),
            None,
            tako_images::ImagesConfig::default(),
            String::new(),
            String::new(),
        );

        assert_eq!(
            manifest.start,
            Some(vec!["./app".to_string(), "--serve".to_string()])
        );
    }

    #[test]
    fn build_manifest_env_vars_overrides_configured_env_with_derived_environment() {
        let env_vars = build_manifest_env_vars(
            HashMap::from([("ENV".to_string(), "custom".to_string())]),
            HashMap::new(),
            "production",
            "bun",
        );

        assert_eq!(env_vars.get("ENV"), Some(&"production".to_string()));
    }

    #[test]
    fn resolve_deploy_main_prefers_tako_toml_main() {
        let temp = TempDir::new().unwrap();
        let config = TakoToml {
            main: Some("server/custom.mjs".to_string()),
            ..Default::default()
        };
        let resolved = resolve_deploy_main(
            temp.path(),
            BuildAdapter::Node,
            &config,
            Some("preset-default.ts"),
        )
        .unwrap();
        assert_eq!(resolved, "server/custom.mjs");
    }

    #[test]
    fn resolve_deploy_main_uses_preset_default_main_when_tako_main_is_missing() {
        let temp = TempDir::new().unwrap();
        let resolved = resolve_deploy_main(
            temp.path(),
            BuildAdapter::Node,
            &TakoToml::default(),
            Some("./dist/server/entry.mjs"),
        )
        .unwrap();
        assert_eq!(resolved, "dist/server/entry.mjs");
    }

    #[test]
    fn resolve_deploy_main_errors_when_tako_and_preset_main_are_missing() {
        let temp = TempDir::new().unwrap();
        let err = resolve_deploy_main(temp.path(), BuildAdapter::Node, &TakoToml::default(), None)
            .unwrap_err();
        assert!(
            err.contains("Set `main` in tako.toml or preset `main`"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn resolve_deploy_main_rejects_parent_directory_segments_from_tako_toml() {
        let temp = TempDir::new().unwrap();
        let config = TakoToml {
            main: Some("../outside.js".to_string()),
            ..Default::default()
        };
        let err = resolve_deploy_main(temp.path(), BuildAdapter::Node, &config, None).unwrap_err();
        assert!(
            err.contains("must not contain '..'"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn resolve_deploy_main_rejects_empty_tako_toml_main() {
        let temp = TempDir::new().unwrap();
        let config = TakoToml {
            main: Some("  ".to_string()),
            ..Default::default()
        };
        let err = resolve_deploy_main(temp.path(), BuildAdapter::Node, &config, None).unwrap_err();
        assert!(err.contains("main is empty"), "unexpected error: {err}");
    }

    #[test]
    fn resolve_deploy_main_rejects_invalid_preset_main() {
        let temp = TempDir::new().unwrap();
        let err = resolve_deploy_main(
            temp.path(),
            BuildAdapter::Node,
            &TakoToml::default(),
            Some("../outside.js"),
        )
        .unwrap_err();
        assert!(
            err.contains("must not contain '..'"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn resolve_deploy_main_prefers_root_index_for_js_presets() {
        let temp = TempDir::new().unwrap();
        std::fs::write(temp.path().join("index.tsx"), "export {};\n").unwrap();

        let resolved = resolve_deploy_main(
            temp.path(),
            BuildAdapter::Bun,
            &TakoToml::default(),
            Some("src/index.tsx"),
        )
        .unwrap();

        assert_eq!(resolved, "index.tsx");
    }

    #[test]
    fn resolve_deploy_main_falls_back_to_src_index_when_root_index_is_missing() {
        let temp = TempDir::new().unwrap();
        std::fs::create_dir_all(temp.path().join("src")).unwrap();
        std::fs::write(temp.path().join("src/index.js"), "export {};\n").unwrap();

        let resolved = resolve_deploy_main(
            temp.path(),
            BuildAdapter::Node,
            &TakoToml::default(),
            Some("index.js"),
        )
        .unwrap();

        assert_eq!(resolved, "src/index.js");
    }

    #[test]
    fn resolve_deploy_main_applies_index_fallback_for_node() {
        let temp = TempDir::new().unwrap();
        std::fs::write(temp.path().join("index.ts"), "export {};\n").unwrap();

        let resolved = resolve_deploy_main(
            temp.path(),
            BuildAdapter::Node,
            &TakoToml::default(),
            Some("src/index.ts"),
        )
        .unwrap();

        assert_eq!(resolved, "index.ts");
    }
}
