use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Join `base/subpath` and verify the result stays within `base`.
/// Rejects `..` components and absolute subpaths.
pub(crate) fn safe_subdir(base: &Path, subpath: &str) -> Result<PathBuf, String> {
    if subpath.is_empty() {
        return Ok(base.to_path_buf());
    }
    let joined = base.join(subpath);
    // Lexical normalization: resolve away `.` and `..` without touching the filesystem.
    let mut normalized = PathBuf::new();
    for component in joined.components() {
        match component {
            std::path::Component::ParentDir => {
                if !normalized.pop() {
                    return Err(format!(
                        "manifest subpath '{}' escapes release directory",
                        subpath
                    ));
                }
            }
            c => normalized.push(c.as_os_str()),
        }
    }
    if !normalized.starts_with(base) {
        return Err(format!(
            "manifest subpath '{}' escapes release directory",
            subpath
        ));
    }
    Ok(normalized)
}

#[derive(Debug, Clone, serde::Deserialize)]
pub(crate) struct ReleaseManifest {
    #[serde(default)]
    pub release_kind: ReleaseKind,
    #[serde(default)]
    pub app_name: String,
    #[serde(default)]
    pub environment: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub runtime: String,
    #[serde(default)]
    pub main: String,
    #[serde(default)]
    pub workflow_worker_main: Option<String>,
    pub idle_timeout: u32,
    #[serde(default)]
    pub env_vars: HashMap<String, String>,
    #[serde(default)]
    pub images: tako_images::ImagesConfig,
    #[serde(default)]
    pub runtime_version: Option<String>,
    #[serde(default)]
    pub package_manager: Option<String>,
    #[serde(default)]
    pub package_manager_version: Option<String>,
    /// Path from the archive root to the app directory. Empty = archive root.
    #[serde(default)]
    pub app_dir: String,
    /// Path from the archive root to where deps should be installed (lockfile dir). Empty = archive root.
    #[serde(default)]
    pub install_dir: String,
    #[serde(default)]
    pub container_file: Option<String>,
    #[serde(default)]
    pub container_port: Option<u16>,
}

#[derive(Debug, Clone, Copy, serde::Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ReleaseKind {
    #[default]
    Native,
    Container,
}

pub(crate) fn load_release_manifest(release_dir: &Path) -> Result<ReleaseManifest, String> {
    let manifest_path = release_dir.join("app.json");
    let content = std::fs::read_to_string(&manifest_path).map_err(|e| {
        format!(
            "failed to read deploy manifest {}: {}",
            manifest_path.display(),
            e
        )
    })?;
    serde_json::from_str(&content).map_err(|e| {
        format!(
            "failed to parse deploy manifest {}: {}",
            manifest_path.display(),
            e
        )
    })
}

pub fn env_vars_from_release_dir(release_dir: &Path) -> Result<HashMap<String, String>, String> {
    Ok(load_release_manifest(release_dir)?.env_vars)
}

#[cfg(test)]
pub fn idle_timeout_secs_from_release_dir(release_dir: &Path) -> Result<u32, String> {
    Ok(load_release_manifest(release_dir)?.idle_timeout)
}

/// Build the launch command from a manifest using the plugin system.
///
/// The manifest is declarative (runtime, main, package_manager). The plugin
/// provides the actual launch args and entrypoint path.
pub(crate) fn command_from_manifest(
    manifest: &ReleaseManifest,
    release_dir: &Path,
    runtime_bin: Option<&str>,
) -> Result<Vec<String>, String> {
    let manifest_path = release_dir.join("app.json");
    if manifest.release_kind == ReleaseKind::Container {
        return Err(format!(
            "deploy manifest {} is a container release, not a native release",
            manifest_path.display()
        ));
    }
    if manifest.main.trim().is_empty() {
        return Err(format!(
            "deploy manifest {} has empty main field",
            manifest_path.display()
        ));
    }

    let app_dir = safe_subdir(release_dir, &manifest.app_dir)?;
    let install_dir = safe_subdir(release_dir, &manifest.install_dir)?;
    let ctx = manifest
        .package_manager
        .as_ref()
        .map(|pm| tako_runtime::PluginContext {
            project_dir: &app_dir,
            package_manager: Some(pm.as_str()),
        });
    let def = tako_runtime::runtime_def_for(&manifest.runtime, ctx.as_ref()).ok_or_else(|| {
        format!(
            "unsupported runtime '{}' in deploy manifest {}",
            manifest.runtime,
            manifest_path.display()
        )
    })?;

    let bin = runtime_bin
        .map(str::to_string)
        .unwrap_or_else(|| manifest.runtime.clone());
    let resolved_main = resolve_main_path(&app_dir, &manifest.main);

    let cmd: Vec<String> = def
        .server
        .launch_args
        .iter()
        .map(|arg| match arg.as_str() {
            "{bin}" => bin.clone(),
            "{main}" => resolved_main.clone(),
            // Resolve node_modules-relative paths to absolute using the install dir
            // (workspace root where deps are hoisted). Falls back to the literal arg
            // if the file doesn't exist yet (e.g. in tests).
            other => resolve_node_modules_path(&install_dir, other),
        })
        .collect();

    Ok(cmd)
}

/// Determine the command to launch an app from its release directory.
///
/// Release launch behavior is derived from deploy manifest (`app.json`) only.
#[cfg(test)]
pub fn command_for_release_dir(release_dir: &Path) -> Result<Vec<String>, String> {
    let manifest = load_release_manifest(release_dir)?;
    command_from_manifest(&manifest, release_dir, None)
}

/// Resolve the main entrypoint for the launch command.
/// - If the file exists on disk, return the absolute path.
/// - Otherwise pass through as-is (bare module specifier).
fn resolve_main_path(base_dir: &Path, main: &str) -> String {
    let candidate = base_dir.join(main);
    if candidate.is_file() {
        return candidate.to_string_lossy().to_string();
    }
    main.to_string()
}

/// Resolve a `node_modules/...` launch arg to an absolute path using the install dir.
/// The install dir is the workspace root where deps are hoisted after `bun install`.
/// Falls back to the literal arg if the file doesn't exist (e.g. before install or in tests).
fn resolve_node_modules_path(install_dir: &Path, arg: &str) -> String {
    if arg.starts_with("node_modules/") {
        let candidate = install_dir.join(arg);
        if candidate.is_file() {
            return candidate.to_string_lossy().to_string();
        }
    }
    arg.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn env_vars_from_release_dir_reads_env_vars_field() {
        let dir = TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("app.json"),
            r#"{"runtime":"bun","main":"index.ts","idle_timeout":300,"env_vars":{"NODE_ENV":"production","TAKO_BUILD":"v1"}}"#,
        )
        .unwrap();
        let vars = env_vars_from_release_dir(dir.path()).unwrap();
        assert_eq!(vars.get("NODE_ENV"), Some(&"production".to_string()));
        assert_eq!(vars.get("TAKO_BUILD"), Some(&"v1".to_string()));
    }

    #[test]
    fn env_vars_from_release_dir_returns_empty_when_field_missing() {
        let dir = TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("app.json"),
            r#"{"runtime":"bun","main":"index.ts","idle_timeout":300}"#,
        )
        .unwrap();
        let vars = env_vars_from_release_dir(dir.path()).unwrap();
        assert!(vars.is_empty());
    }

    #[test]
    fn env_vars_from_release_dir_errors_when_manifest_is_missing() {
        let dir = TempDir::new().unwrap();
        let err = env_vars_from_release_dir(dir.path()).unwrap_err();
        assert!(err.contains("failed to read deploy manifest"));
    }

    #[test]
    fn env_vars_from_release_dir_errors_on_invalid_json() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("app.json"), r#"not json"#).unwrap();
        let err = env_vars_from_release_dir(dir.path()).unwrap_err();
        assert!(err.contains("parse"));
    }

    #[test]
    fn idle_timeout_secs_from_release_dir_reads_required_field() {
        let dir = TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("app.json"),
            r#"{"runtime":"bun","main":"index.ts","idle_timeout":42}"#,
        )
        .unwrap();
        assert_eq!(idle_timeout_secs_from_release_dir(dir.path()).unwrap(), 42);
    }

    #[test]
    fn bun_command_uses_entrypoint_path() {
        let dir = TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("app.json"),
            r#"{"runtime":"bun","main":"server/entry.js","idle_timeout":300}"#,
        )
        .unwrap();

        let cmd = command_for_release_dir(dir.path()).unwrap();
        assert_eq!(cmd[0], "bun");
        assert_eq!(cmd[1], "run");
        assert!(cmd[2].contains("tako.sh/dist/entrypoints/bun-server.mjs"));
        assert_eq!(cmd.last().unwrap(), "server/entry.js");
    }

    #[test]
    fn errors_when_manifest_is_missing() {
        let dir = TempDir::new().unwrap();
        let err = command_for_release_dir(dir.path()).unwrap_err();
        assert!(err.contains("failed to read deploy manifest"));
    }

    #[test]
    fn errors_when_manifest_runtime_is_unknown() {
        let dir = TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("app.json"),
            r#"{"runtime":"python","main":"server/index.js","idle_timeout":300}"#,
        )
        .unwrap();
        let err = command_for_release_dir(dir.path()).unwrap_err();
        assert!(err.contains("unsupported runtime"));
    }

    #[test]
    fn node_command_uses_entrypoint_path() {
        let dir = TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("app.json"),
            r#"{"runtime":"node","main":"server/index.mjs","idle_timeout":300}"#,
        )
        .unwrap();

        let cmd = command_for_release_dir(dir.path()).unwrap();
        assert_eq!(cmd[0], "node");
        assert!(
            cmd.iter()
                .any(|a| a.contains("entrypoints/node-server.mjs"))
        );
        assert_eq!(cmd.last().unwrap(), "server/index.mjs");
    }

    #[test]
    fn errors_when_manifest_main_is_empty() {
        let dir = TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("app.json"),
            r#"{"runtime":"bun","main":"  ","idle_timeout":300}"#,
        )
        .unwrap();

        let err = command_for_release_dir(dir.path()).unwrap_err();
        assert!(err.contains("empty main"));
    }

    #[test]
    fn main_resolved_to_absolute_when_file_exists() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/app.ts"), "export default {};\n").unwrap();
        std::fs::write(
            dir.path().join("app.json"),
            r#"{"runtime":"bun","main":"src/app.ts","idle_timeout":300}"#,
        )
        .unwrap();

        let cmd = command_for_release_dir(dir.path()).unwrap();
        assert_eq!(
            cmd.last().unwrap(),
            &dir.path().join("src/app.ts").to_string_lossy().to_string()
        );
    }

    #[test]
    fn bare_specifier_main_passed_through() {
        let dir = TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("app.json"),
            r#"{"runtime":"bun","main":"@tanstack/react-start/server-entry","idle_timeout":300}"#,
        )
        .unwrap();

        let cmd = command_for_release_dir(dir.path()).unwrap();
        assert_eq!(cmd.last().unwrap(), "@tanstack/react-start/server-entry");
    }

    #[test]
    fn runtime_version_deserialized_from_manifest() {
        let dir = TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("app.json"),
            r#"{"runtime":"bun","main":"index.ts","idle_timeout":300,"runtime_version":"1.2.0"}"#,
        )
        .unwrap();

        let manifest = load_release_manifest(dir.path()).unwrap();
        assert_eq!(manifest.runtime_version.as_deref(), Some("1.2.0"));
    }

    #[test]
    fn runtime_version_defaults_to_none() {
        let dir = TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("app.json"),
            r#"{"runtime":"bun","main":"index.ts","idle_timeout":300}"#,
        )
        .unwrap();

        let manifest = load_release_manifest(dir.path()).unwrap();
        assert!(manifest.runtime_version.is_none());
        assert_eq!(manifest.release_kind, ReleaseKind::Native);
    }

    #[test]
    fn container_release_manifest_deserializes_metadata() {
        let dir = TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("app.json"),
            r#"{"release_kind":"container","app_name":"my-app","environment":"production","version":"v1","runtime":"container","main":"","idle_timeout":300,"container_file":"Dockerfile","container_port":3000}"#,
        )
        .unwrap();

        let manifest = load_release_manifest(dir.path()).unwrap();
        assert_eq!(manifest.release_kind, ReleaseKind::Container);
        assert_eq!(manifest.app_name, "my-app");
        assert_eq!(manifest.environment, "production");
        assert_eq!(manifest.version, "v1");
        assert_eq!(manifest.container_file.as_deref(), Some("Dockerfile"));
        assert_eq!(manifest.container_port, Some(3000));
    }

    #[test]
    fn command_for_release_dir_rejects_container_manifest() {
        let dir = TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("app.json"),
            r#"{"release_kind":"container","runtime":"container","main":"","idle_timeout":300,"container_file":"Dockerfile"}"#,
        )
        .unwrap();

        let err = command_for_release_dir(dir.path()).unwrap_err();
        assert!(err.contains("container release"));
    }

    #[test]
    fn package_manager_version_deserialized_from_manifest() {
        let dir = TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("app.json"),
            r#"{"runtime":"node","main":"index.ts","idle_timeout":300,"package_manager":"bun","package_manager_version":"1.3.11"}"#,
        )
        .unwrap();

        let manifest = load_release_manifest(dir.path()).unwrap();
        assert_eq!(manifest.package_manager.as_deref(), Some("bun"));
        assert_eq!(manifest.package_manager_version.as_deref(), Some("1.3.11"));
    }

    #[test]
    fn go_command_runs_binary_directly() {
        let dir = TempDir::new().unwrap();
        // Create the binary file so main resolves to absolute path
        std::fs::write(dir.path().join("app"), "").unwrap();
        std::fs::write(
            dir.path().join("app.json"),
            r#"{"runtime":"go","main":"app","idle_timeout":300}"#,
        )
        .unwrap();

        let cmd = command_for_release_dir(dir.path()).unwrap();
        // Go launch_args is ["{main}"] — binary runs directly, no runtime prefix
        assert_eq!(cmd.len(), 1);
        assert!(
            cmd[0].ends_with("/app"),
            "expected absolute path to binary, got: {}",
            cmd[0]
        );
    }

    #[test]
    fn go_command_no_bin_placeholder() {
        let dir = TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("app.json"),
            r#"{"runtime":"go","main":"my-server","idle_timeout":300}"#,
        )
        .unwrap();

        let cmd = command_for_release_dir(dir.path()).unwrap();
        assert_eq!(cmd.len(), 1);
        // When binary doesn't exist on disk, main is passed through as-is
        assert_eq!(cmd[0], "my-server");
    }

    #[test]
    fn safe_subdir_allows_normal_subpath() {
        let base = Path::new("/opt/tako/apps/myapp/releases/v1");
        let result = safe_subdir(base, "packages/web").unwrap();
        assert_eq!(result, base.join("packages/web"));
    }

    #[test]
    fn safe_subdir_allows_empty_subpath() {
        let base = Path::new("/opt/tako/apps/myapp/releases/v1");
        let result = safe_subdir(base, "").unwrap();
        assert_eq!(result, base);
    }

    #[test]
    fn safe_subdir_rejects_parent_escape() {
        let base = Path::new("/opt/tako/apps/myapp/releases/v1");
        assert!(safe_subdir(base, "../../etc/passwd").is_err());
    }

    #[test]
    fn safe_subdir_rejects_absolute_path() {
        let base = Path::new("/opt/tako/apps/myapp/releases/v1");
        assert!(safe_subdir(base, "/etc/passwd").is_err());
    }

    #[test]
    fn safe_subdir_allows_internal_dotdot_that_stays_within() {
        let base = Path::new("/opt/tako/apps/myapp/releases/v1");
        let result = safe_subdir(base, "packages/web/../api").unwrap();
        assert_eq!(result, base.join("packages/api"));
    }
}
