use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::process::ExitStatus;

use crate::release::TAKO_APP_DATA_DIR_ENV;
use tako_core::StorageBinding;
use tako_core::bootstrap::{TAKO_BOOTSTRAP_DATA_ENV, envelope_string};
use tako_core::container::ContainerOptions;
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

pub(crate) async fn remove_container(engine: ContainerEngine, name: &str) -> std::io::Result<()> {
    let mut command = TokioCommand::new(engine.binary());
    command
        .args(["rm", "--force", "--ignore", "--time", "10", name])
        .kill_on_drop(true);
    let output = tokio::time::timeout(std::time::Duration::from_secs(15), command.output())
        .await
        .map_err(|_| std::io::Error::other("timed out stopping container"))??;
    if !output.status.success() {
        return Err(std::io::Error::other(format_process_failure(
            "stop container",
            output.status,
            &output.stdout,
            &output.stderr,
        )));
    }
    Ok(())
}

pub(crate) fn detect_container_engine() -> Result<ContainerEngine, String> {
    let engine = ContainerEngine::Podman;
    if std::process::Command::new(engine.binary())
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
    {
        return Ok(engine);
    }
    Err("Container deploys require Podman on the server.".to_string())
}

pub(crate) async fn build_release_image(
    release_dir: &Path,
    manifest: &ReleaseManifest,
) -> Result<String, String> {
    crate::process_output::check_cancellation().map_err(|e| e.to_string())?;
    let engine = detect_container_engine()?;
    let tag = image_tag_for_manifest(manifest)?;
    manifest.container_options.validate()?;
    if let Some(image) = &manifest.container_image {
        if manifest.container_file.is_some()
            || image.is_empty()
            || image.starts_with('-')
            || image.chars().any(char::is_whitespace)
        {
            return Err("invalid container image reference or conflicting Dockerfile".into());
        }
        run_image_command(engine, &["pull", "--", image]).await?;
        // Each release keeps its own local tag, so rolling back never repulls a
        // mutable upstream tag that may now refer to a different image.
        run_image_command(engine, &["tag", "--", image, &tag]).await?;
        return Ok(tag);
    }
    let context = container_build_context(release_dir, manifest)?;
    let container_file = container_file_path(release_dir, manifest)?;

    crate::process_output::check_cancellation().map_err(|e| e.to_string())?;
    let child = TokioCommand::new(engine.binary())
        .process_group(0)
        .arg("build")
        .arg("-f")
        .arg(&container_file)
        .arg("-t")
        .arg(&tag)
        .arg(&context)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("Failed to run {} build: {e}", engine.binary()))?;
    let output =
        crate::process_output::capture(child, Some(crate::process_output::PREPARATION_TIMEOUT))
            .await
            .map_err(|e| format!("Failed to collect {} build output: {e}", engine.binary()))?;
    let status = output
        .status
        .ok_or("Container preparation timed out after 600 seconds")?;
    if !status.success() {
        return Err(format_process_failure(
            &format!("{} build", engine.binary()),
            status,
            &output.stdout,
            &output.stderr,
        ));
    }

    Ok(tag)
}

async fn run_image_command(engine: ContainerEngine, args: &[&str]) -> Result<(), String> {
    crate::process_output::check_cancellation().map_err(|e| e.to_string())?;
    let child = TokioCommand::new(engine.binary())
        .process_group(0)
        .args(args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| e.to_string())?;
    let output =
        crate::process_output::capture(child, Some(crate::process_output::PREPARATION_TIMEOUT))
            .await
            .map_err(|e| e.to_string())?;
    let status = output
        .status
        .ok_or("Container preparation timed out after 600 seconds")?;
    if !status.success() {
        return Err(format_process_failure(
            "prepare container image",
            status,
            &output.stdout,
            &output.stderr,
        ));
    }
    Ok(())
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

pub(crate) struct ContainerRunConfig<'a> {
    pub(crate) name: &'a str,
    pub(crate) image: &'a str,
    pub(crate) host_port: u16,
    pub(crate) container_port: u16,
    pub(crate) env: &'a HashMap<String, String>,
    pub(crate) token: &'a str,
    pub(crate) secrets: &'a HashMap<String, String>,
    pub(crate) storages: &'a HashMap<String, StorageBinding>,
    pub(crate) options: &'a ContainerOptions,
}

pub(crate) fn build_container_run_args(
    config: ContainerRunConfig<'_>,
) -> Result<Vec<String>, String> {
    let ContainerRunConfig {
        name,
        image,
        host_port,
        container_port,
        env,
        token,
        secrets,
        storages,
        options,
    } = config;
    for key in &options.secret_env {
        if !secrets.contains_key(key) {
            return Err(format!("container secret '{key}' is not configured"));
        }
    }

    let mut args = vec![
        "run".to_string(),
        "--name".to_string(),
        name.to_string(),
        "--publish".to_string(),
        format!("127.0.0.1:{host_port}:{container_port}"),
    ];
    append_data_mount(&mut args, env, options)?;
    args.extend(["--label".into(), "sh.tako.kind=http".into()]);
    if let Some(source) = env.get(TAKO_APP_DATA_DIR_ENV) {
        args.extend(["--label".into(), format!("sh.tako.data={source}")]);
    }
    for key in
        build_container_run_env(env, token, secrets, storages, container_port, options).keys()
    {
        args.push("--env".to_string());
        args.push(key.clone());
    }
    if let Some(entrypoint) = &options.entrypoint {
        args.extend(["--entrypoint".into(), entrypoint.clone()]);
    }
    args.push(image.to_string());
    args.extend(options.command.iter().cloned());
    Ok(args)
}

pub(crate) fn build_container_workflow_run_args(
    image: &str,
    env: &HashMap<String, String>,
    token: &str,
    secrets: &HashMap<String, String>,
    storages: &HashMap<String, StorageBinding>,
    run: &[String],
    internal_socket: &Path,
    options: &ContainerOptions,
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
    append_data_mount(&mut args, env, options)?;
    for key in build_container_run_env(
        env,
        token,
        secrets,
        storages,
        DEFAULT_CONTAINER_PORT,
        options,
    )
    .keys()
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
    options: &ContainerOptions,
) -> BTreeMap<String, String> {
    let mut merged = BTreeMap::new();
    for (key, value) in env {
        merged.insert(key.clone(), value.clone());
    }
    if env.contains_key(TAKO_APP_DATA_DIR_ENV) {
        merged.insert(TAKO_APP_DATA_DIR_ENV.to_string(), options.data_dir.clone());
    }
    merged.insert("HOST".to_string(), "0.0.0.0".to_string());
    merged.insert("PORT".to_string(), container_port.to_string());
    if options.sdk {
        merged.insert(
            TAKO_BOOTSTRAP_DATA_ENV.to_string(),
            envelope_string(token, secrets, storages),
        );
    } else {
        merged.remove(TAKO_BOOTSTRAP_DATA_ENV);
        merged.remove("TAKO_INTERNAL_SOCKET");
    }
    for key in &options.secret_env {
        if let Some(value) = secrets.get(key) {
            merged.insert(key.clone(), value.clone());
        }
    }
    merged
}

fn append_data_mount(
    args: &mut Vec<String>,
    env: &HashMap<String, String>,
    options: &ContainerOptions,
) -> Result<(), String> {
    options.validate()?;
    if let Some(source) = env.get(TAKO_APP_DATA_DIR_ENV) {
        if !Path::new(source).is_absolute() || source.chars().any(|c| c == ',' || c.is_control()) {
            return Err("container data source must be an absolute path without commas or control characters".into());
        }
        args.extend([
            "--mount".into(),
            format!("type=bind,src={source},dst={}", options.data_dir),
        ]);
    }
    if let Some(user) = &options.user {
        args.extend([
            "--userns".into(),
            format!("keep-id:uid={},gid={}", user.uid, user.gid),
            "--user".into(),
            format!("{}:{}", user.uid, user.gid),
        ]);
    }
    Ok(())
}

/// A server crash can leave the engine's containers running after the client
/// exits. Reconcile only this server's app data path before restoring instances.
pub(crate) async fn cleanup_app_containers(data_path: &str) -> Result<(), String> {
    let engine = detect_container_engine()?;
    let mut command = TokioCommand::new(engine.binary());
    command
        .args([
            "ps",
            "--all",
            "--quiet",
            "--filter",
            "label=sh.tako.kind=http",
            "--filter",
        ])
        .arg(format!("label=sh.tako.data={data_path}"))
        .kill_on_drop(true);
    let output = tokio::time::timeout(std::time::Duration::from_secs(15), command.output())
        .await
        .map_err(|_| "timed out listing app containers".to_string())?
        .map_err(|e| e.to_string())?;
    if !output.status.success() {
        return Err(format_process_failure(
            "list app containers",
            output.status,
            &output.stdout,
            &output.stderr,
        ));
    }
    for id in String::from_utf8_lossy(&output.stdout).split_whitespace() {
        if !id.bytes().all(|c| c.is_ascii_hexdigit()) {
            return Err("container engine returned an invalid container ID".into());
        }
        remove_container(engine, id)
            .await
            .map_err(|e| e.to_string())?;
    }
    Ok(())
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
    let preview: String = detail
        .chars()
        .rev()
        .take(400)
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    if detail.chars().count() > 400 {
        format!("{context} ({status_text}): ...{preview}")
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
            protocol_version: tako_core::PROTOCOL_VERSION,
            release_kind: crate::app_command::ReleaseKind::Container,
            app_name: "my-app".to_string(),
            environment: "production".to_string(),
            version: "v1.2.3".to_string(),
            runtime: "container".to_string(),
            main: String::new(),
            start: None,
            workflow_worker_main: None,
            workflow_run: None,
            workflow_retention_ms: Some(tako_workflows::DEFAULT_RETENTION_MS),
            idle_timeout: 300,
            env_vars: HashMap::new(),
            images: tako_images::ImagesConfig::default(),
            runtime_version: None,
            package_manager: None,
            package_manager_version: None,
            app_dir: "apps/web".to_string(),
            install_dir: String::new(),
            container_file: Some("Dockerfile".to_string()),
            container_image: None,
            compose: None,
            container_options: Default::default(),
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
        let env = HashMap::from([("ENV".to_string(), "production".to_string())]);
        let secrets = HashMap::from([("API_KEY".to_string(), "secret".to_string())]);
        let storages = HashMap::new();
        let args = build_container_run_args(ContainerRunConfig {
            name: "tako-my-app-abc",
            image: "tako/my-app:v1",
            host_port: 49152,
            container_port: 3000,
            env: &env,
            token: "tok",
            secrets: &secrets,
            storages: &storages,
            options: &ContainerOptions::default(),
        })
        .unwrap();

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
            &ContainerOptions::default(),
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
            &ContainerOptions::default(),
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
    #[test]
    fn data_mount_preserves_host_identity_and_translates_container_path() {
        let env = HashMap::from([(
            TAKO_APP_DATA_DIR_ENV.into(),
            "/srv/tako/apps/demo/production/data/app".into(),
        )]);
        let options = ContainerOptions {
            data_dir: "/data".into(),
            user: Some(tako_core::container::ContainerUser {
                uid: 1000,
                gid: 1001,
            }),
            ..Default::default()
        };
        let args = build_container_run_args(ContainerRunConfig {
            name: "test",
            image: "test:v1",
            host_port: 40000,
            container_port: 3000,
            env: &env,
            token: "token",
            secrets: &HashMap::new(),
            storages: &HashMap::new(),
            options: &options,
        })
        .unwrap();
        assert!(
            args.contains(
                &"type=bind,src=/srv/tako/apps/demo/production/data/app,dst=/data".into()
            )
        );
        assert!(args.contains(&"keep-id:uid=1000,gid=1001".into()));
        assert!(!args.contains(&"--rm".into()));
        let runtime = build_container_run_env(
            &env,
            "token",
            &HashMap::new(),
            &HashMap::new(),
            3000,
            &options,
        );
        assert_eq!(runtime[TAKO_APP_DATA_DIR_ENV], "/data");
    }

    #[test]
    fn workflow_containers_receive_the_same_data_mount_and_translated_path() {
        let env = HashMap::from([(
            TAKO_APP_DATA_DIR_ENV.into(),
            "/srv/tako/apps/demo/production/data/app".into(),
        )]);
        let options = ContainerOptions::default();
        let args = build_container_workflow_run_args(
            "tako/demo:v1",
            &env,
            "token",
            &HashMap::new(),
            &HashMap::new(),
            &["./worker".to_string()],
            Path::new("/opt/tako/internal.sock"),
            &options,
        )
        .unwrap();
        assert!(args.windows(2).any(|w| {
            w[0] == "--mount"
                && w[1] == "type=bind,src=/srv/tako/apps/demo/production/data/app,dst=/tako/data"
        }));
        assert!(args.contains(&TAKO_APP_DATA_DIR_ENV.to_string()));
        let runtime = build_container_run_env(
            &env,
            "token",
            &HashMap::new(),
            &HashMap::new(),
            DEFAULT_CONTAINER_PORT,
            &options,
        );
        assert_eq!(runtime[TAKO_APP_DATA_DIR_ENV], "/tako/data");
    }

    #[test]
    fn plain_images_receive_only_selected_secrets_without_sdk_token() {
        let env = HashMap::new();
        let secrets = HashMap::from([
            ("DATABASE_URL".into(), "private-db".into()),
            ("OTHER_KEY".into(), "private-other".into()),
        ]);
        let options = ContainerOptions {
            sdk: false,
            port: 8080,
            health_path: "/health".into(),
            secret_env: vec!["DATABASE_URL".into()],
            ..Default::default()
        };
        let runtime = build_container_run_env(
            &env,
            "internal-token",
            &secrets,
            &HashMap::new(),
            8080,
            &options,
        );
        assert_eq!(runtime["DATABASE_URL"], "private-db");
        assert!(!runtime.contains_key("OTHER_KEY"));
        assert!(!runtime.contains_key(TAKO_BOOTSTRAP_DATA_ENV));
        let args = build_container_run_args(ContainerRunConfig {
            name: "plain",
            image: "plain:v1",
            host_port: 40000,
            container_port: 8080,
            env: &env,
            token: "internal-token",
            secrets: &secrets,
            storages: &HashMap::new(),
            options: &options,
        })
        .unwrap();
        assert!(args.contains(&"DATABASE_URL".into()));
        assert!(
            !args
                .iter()
                .any(|arg| arg.contains("private-db") || arg.contains("internal-token"))
        );
    }
}
