use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::process::ExitStatus;

use tako_core::StorageBinding;
use tako_core::bootstrap::{TAKO_BOOTSTRAP_DATA_ENV, envelope_string};
use tokio::process::Command as TokioCommand;

use crate::app_command::{ReleaseManifest, safe_subdir};

pub(crate) const DEFAULT_CONTAINER_PORT: u16 = 3000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ContainerEngine {
    Podman,
}

impl ContainerEngine {
    pub(crate) fn binary(self) -> &'static str {
        match self {
            Self::Podman => "podman",
        }
    }
}

pub(crate) fn detect_container_engine() -> Result<ContainerEngine, String> {
    let engine = ContainerEngine::Podman;
    if std::process::Command::new(engine.binary())
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
    {
        return Ok(engine);
    }
    Err("Container deploys require Podman on the server.".to_string())
}

pub(crate) async fn build_release_image(
    release_dir: &Path,
    manifest: &ReleaseManifest,
) -> Result<String, String> {
    let engine = detect_container_engine()?;
    let tag = image_tag_for_manifest(manifest)?;
    let context = container_build_context(release_dir, manifest)?;
    let container_file = container_file_path(release_dir, manifest)?;

    let output = TokioCommand::new(engine.binary())
        .arg("build")
        .arg("-f")
        .arg(&container_file)
        .arg("-t")
        .arg(&tag)
        .arg(&context)
        .output()
        .await
        .map_err(|e| format!("Failed to run {} build: {e}", engine.binary()))?;
    if !output.status.success() {
        return Err(format_process_failure(
            &format!("{} build", engine.binary()),
            output.status,
            &output.stdout,
            &output.stderr,
        ));
    }

    Ok(tag)
}

pub(crate) fn image_tag_for_manifest(manifest: &ReleaseManifest) -> Result<String, String> {
    if manifest.app_name.trim().is_empty() {
        return Err("container release manifest is missing app_name".to_string());
    }
    if manifest.version.trim().is_empty() {
        return Err("container release manifest is missing version".to_string());
    }
    let deployment_id = if manifest.environment.trim().is_empty() {
        manifest.app_name.clone()
    } else {
        tako_core::deployment_app_id(&manifest.app_name, &manifest.environment)
    };
    Ok(format!(
        "tako/{}:{}",
        sanitize_image_component(&deployment_id),
        sanitize_tag_component(&manifest.version)
    ))
}

pub(crate) fn container_build_context(
    release_dir: &Path,
    manifest: &ReleaseManifest,
) -> Result<PathBuf, String> {
    safe_subdir(release_dir, &manifest.app_dir)
        .map_err(|e| format!("Invalid app_dir in manifest: {e}"))
}

pub(crate) fn container_file_path(
    release_dir: &Path,
    manifest: &ReleaseManifest,
) -> Result<PathBuf, String> {
    let file = manifest
        .container_file
        .as_deref()
        .ok_or_else(|| "container release manifest is missing container_file".to_string())?;
    let context = container_build_context(release_dir, manifest)?;
    let path = safe_subdir(&context, file)
        .map_err(|e| format!("Invalid container_file in manifest: {e}"))?;
    if !path.is_file() {
        return Err(format!("container file {} does not exist", path.display()));
    }
    Ok(path)
}

pub(crate) fn build_container_run_args(
    name: &str,
    image: &str,
    host_port: u16,
    container_port: u16,
    env: &HashMap<String, String>,
    token: &str,
    secrets: &HashMap<String, String>,
    storages: &HashMap<String, StorageBinding>,
) -> Vec<String> {
    let mut args = vec![
        "run".to_string(),
        "--name".to_string(),
        name.to_string(),
        "--publish".to_string(),
        format!("127.0.0.1:{host_port}:{container_port}"),
    ];
    for key in build_container_run_env(env, token, secrets, storages, container_port).keys() {
        args.push("--env".to_string());
        args.push(key.clone());
    }
    args.push(image.to_string());
    args
}

pub(crate) fn build_container_workflow_run_args(
    image: &str,
    env: &HashMap<String, String>,
    token: &str,
    secrets: &HashMap<String, String>,
    storages: &HashMap<String, StorageBinding>,
    run: &[String],
    internal_socket: &Path,
) -> Result<Vec<String>, String> {
    let Some((entrypoint, args_after_entrypoint)) = run.split_first() else {
        return Err("workflow run command cannot be empty".to_string());
    };
    let mut args = vec![
        "run".to_string(),
        "--mount".to_string(),
        format!(
            "type=bind,src={},dst={}",
            internal_socket.display(),
            internal_socket.display()
        ),
        "--entrypoint".to_string(),
        entrypoint.clone(),
    ];
    for key in build_container_run_env(env, token, secrets, storages, DEFAULT_CONTAINER_PORT).keys()
    {
        args.push("--env".to_string());
        args.push(key.clone());
    }
    args.push(image.to_string());
    args.extend(args_after_entrypoint.iter().cloned());
    Ok(args)
}

pub(crate) fn build_container_run_env(
    env: &HashMap<String, String>,
    token: &str,
    secrets: &HashMap<String, String>,
    storages: &HashMap<String, StorageBinding>,
    container_port: u16,
) -> BTreeMap<String, String> {
    let mut merged = BTreeMap::new();
    for (key, value) in env {
        merged.insert(key.clone(), value.clone());
    }
    merged.insert("HOST".to_string(), "0.0.0.0".to_string());
    merged.insert("PORT".to_string(), container_port.to_string());
    merged.insert(
        TAKO_BOOTSTRAP_DATA_ENV.to_string(),
        envelope_string(token, secrets, storages),
    );
    merged
}

fn sanitize_image_component(value: &str) -> String {
    sanitize_with(value, '-')
        .trim_matches('-')
        .trim_matches('.')
        .to_string()
}

fn sanitize_tag_component(value: &str) -> String {
    let value = sanitize_with(value, '_')
        .trim_matches('_')
        .trim_matches('.')
        .to_string();
    if value.is_empty() {
        "latest".to_string()
    } else {
        value
    }
}

fn sanitize_with(value: &str, replacement: char) -> String {
    value
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c.to_ascii_lowercase()
            } else {
                replacement
            }
        })
        .collect()
}

fn format_process_failure(
    context: &str,
    status: ExitStatus,
    stdout: &[u8],
    stderr: &[u8],
) -> String {
    let status_text = match status.code() {
        Some(code) => format!("exit code {code}"),
        None => "terminated by signal".to_string(),
    };
    let stderr_text = String::from_utf8_lossy(stderr).trim().to_string();
    let stdout_text = String::from_utf8_lossy(stdout).trim().to_string();
    let detail = if stderr_text.is_empty() {
        stdout_text
    } else {
        stderr_text
    };
    if detail.is_empty() {
        return format!("{context} ({status_text})");
    }
    let preview: String = detail.chars().take(400).collect();
    if detail.chars().count() > 400 {
        format!("{context} ({status_text}): {preview}...")
    } else {
        format!("{context} ({status_text}): {preview}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn container_manifest() -> ReleaseManifest {
        ReleaseManifest {
            release_kind: crate::app_command::ReleaseKind::Container,
            app_name: "my-app".to_string(),
            environment: "production".to_string(),
            version: "v1.2.3".to_string(),
            runtime: "container".to_string(),
            main: String::new(),
            start: None,
            workflow_worker_main: None,
            workflow_run: None,
            idle_timeout: 300,
            env_vars: HashMap::new(),
            images: tako_images::ImagesConfig::default(),
            runtime_version: None,
            package_manager: None,
            package_manager_version: None,
            app_dir: "apps/web".to_string(),
            install_dir: String::new(),
            container_file: Some("Dockerfile".to_string()),
            container_port: Some(DEFAULT_CONTAINER_PORT),
        }
    }

    #[test]
    fn image_tag_for_manifest_uses_app_and_version() {
        let manifest = container_manifest();
        assert_eq!(
            image_tag_for_manifest(&manifest).unwrap(),
            "tako/my-app-production:v1.2.3"
        );
    }

    #[test]
    fn container_file_path_resolves_under_app_dir() {
        let temp = TempDir::new().unwrap();
        std::fs::create_dir_all(temp.path().join("apps/web")).unwrap();
        std::fs::write(temp.path().join("apps/web/Dockerfile"), "FROM scratch\n").unwrap();

        let manifest = container_manifest();
        assert_eq!(
            container_file_path(temp.path(), &manifest).unwrap(),
            temp.path().join("apps/web/Dockerfile")
        );
    }

    #[test]
    fn build_container_run_args_names_env_without_values() {
        let args = build_container_run_args(
            "tako-my-app-abc",
            "tako/my-app:v1",
            49152,
            3000,
            &HashMap::from([("ENV".to_string(), "production".to_string())]),
            "tok",
            &HashMap::from([("API_KEY".to_string(), "secret".to_string())]),
            &HashMap::new(),
        );

        assert!(args.contains(&"127.0.0.1:49152:3000".to_string()));
        assert!(args.contains(&"ENV".to_string()));
        assert!(args.contains(&TAKO_BOOTSTRAP_DATA_ENV.to_string()));
        assert!(!args.contains(&"API_KEY".to_string()));
        assert!(args.contains(&"HOST".to_string()));
        assert!(args.contains(&"PORT".to_string()));
        assert!(!args.contains(&"ENV=production".to_string()));
        assert!(!args.contains(&"API_KEY=secret".to_string()));
        assert!(!args.iter().any(|arg| arg.contains("secret")));
        assert!(!args.contains(&"HOST=0.0.0.0".to_string()));
        assert!(!args.contains(&"PORT=3000".to_string()));
    }

    #[test]
    fn build_container_workflow_run_args_replaces_entrypoint() {
        let args = build_container_workflow_run_args(
            "tako/my-app:v1",
            &HashMap::from([("ENV".to_string(), "production".to_string())]),
            "tok",
            &HashMap::new(),
            &HashMap::new(),
            &["./worker".to_string(), "video".to_string()],
            Path::new("/opt/tako/internal.sock"),
        )
        .unwrap();

        assert_eq!(args[0], "run");
        assert!(args.windows(2).any(|w| {
            w[0] == "--mount"
                && w[1] == "type=bind,src=/opt/tako/internal.sock,dst=/opt/tako/internal.sock"
        }));
        assert!(args.windows(2).any(|w| w == ["--entrypoint", "./worker"]));
        assert_eq!(args.last().map(String::as_str), Some("video"));
        assert!(args.contains(&"tako/my-app:v1".to_string()));
        assert!(args.contains(&TAKO_BOOTSTRAP_DATA_ENV.to_string()));
    }

    #[test]
    fn build_container_run_env_passes_sdk_bootstrap_data() {
        let env = HashMap::from([("ENV".to_string(), "production".to_string())]);
        let secrets = HashMap::from([("API_KEY".to_string(), "secret".to_string())]);

        let merged = build_container_run_env(
            &env,
            "tok",
            &secrets,
            &HashMap::new(),
            DEFAULT_CONTAINER_PORT,
        );

        assert_eq!(merged.get("ENV").map(String::as_str), Some("production"));
        assert_eq!(merged.get("API_KEY"), None);
        assert_eq!(merged.get("HOST").map(String::as_str), Some("0.0.0.0"));
        assert_eq!(merged.get("PORT").map(String::as_str), Some("3000"));

        let bootstrap: serde_json::Value =
            serde_json::from_str(merged.get(TAKO_BOOTSTRAP_DATA_ENV).unwrap()).unwrap();
        assert_eq!(bootstrap["token"], "tok");
        assert_eq!(bootstrap["secrets"]["API_KEY"], "secret");
    }
}
