//! Compose materializes containers and resources; Tako owns production updates.
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::process::{Child, Command};

const TAKO_APP_DATA_DIR_ENV: &str = "TAKO_DATA_DIR";
mod engine;
mod plan;
mod prepared;
pub use prepared::remove_release_preparations;
mod process_output;
mod rollout;
mod validation;
pub use engine::ServiceContainer;
pub use plan::{DependencyCondition, UpdateOrder};
pub use rollout::Rollout;

/// Remove containers from an interrupted first deployment, retaining its data.
pub async fn cleanup_pending_project(project: &str) -> Result<(), String> {
    let suffix = project
        .strip_prefix("tako-")
        .ok_or("Invalid Tako Compose project")?;
    if suffix.len() != 24 || !suffix.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("Invalid Tako Compose project".into());
    }
    for container in engine::project_containers(project, false).await? {
        engine::stop_container(&container.id, false).await?;
        let mut remove = docker_command(false);
        remove.args(["container", "rm", "--volumes", &container.id]);
        engine_output(remove, Duration::from_secs(60)).await?;
    }
    Ok(())
}
use tako_core::compose::ComposeConfig;

const OVERRIDE_FILE: &str = ".tako-compose-override.yaml";

fn docker_command(development: bool) -> Command {
    let mut command = Command::new("docker");
    command.env_clear();
    for key in [
        "PATH",
        "HOME",
        "DOCKER_CONFIG",
        "BUILDX_CONFIG",
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "NO_PROXY",
        "http_proxy",
        "https_proxy",
        "no_proxy",
        "SSL_CERT_FILE",
        "SSL_CERT_DIR",
    ] {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
    if development {
        for key in [
            "DOCKER_HOST",
            "DOCKER_CONTEXT",
            "DOCKER_TLS_VERIFY",
            "DOCKER_CERT_PATH",
        ] {
            if let Some(value) = std::env::var_os(key) {
                command.env(key, value);
            }
        }
    } else {
        command.env("DOCKER_HOST", "unix:///var/run/docker.sock");
    }
    command.kill_on_drop(true);
    command.process_group(0);
    command
}

#[derive(Clone)]
pub struct ComposeRuntime {
    pub project: String,
    pub directory: PathBuf,
    pub config: ComposeConfig,
    env: HashMap<String, String>,
    development: bool,
    override_file: PathBuf,
    // Bound to one immutable, service-private preparation during a rollout.
    source: Option<PathBuf>,
}

impl std::fmt::Debug for ComposeRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ComposeRuntime")
            .field("project", &self.project)
            .field("directory", &self.directory)
            .finish_non_exhaustive()
    }
}

fn digest(value: &str) -> String {
    hex::encode(Sha256::digest(value.as_bytes()))[..24].to_string()
}

impl ComposeRuntime {
    pub fn new(
        directory: PathBuf,
        config: ComposeConfig,
        env: HashMap<String, String>,
    ) -> Result<Self, String> {
        config.validate()?;
        let data = env
            .get(TAKO_APP_DATA_DIR_ENV)
            .ok_or("Compose requires an app data directory")?;
        let project = format!("tako-{}", digest(data));
        let prepared =
            prepared::root_for(&directory, &config.file, Path::new(data))?.join("current.json");
        if !prepared.is_file() {
            let file = directory.join(&config.file);
            let canonical = file
                .canonicalize()
                .map_err(|e| format!("Compose file: {e}"))?;
            if !canonical.starts_with(directory.canonicalize().map_err(|e| e.to_string())?) {
                return Err("Compose file must stay inside the app directory".into());
            }
        }
        for key in env.keys() {
            if key.starts_with("DOCKER_")
                || key.starts_with("COMPOSE_")
                || key.starts_with("LD_")
                || key.starts_with("DYLD_")
                || matches!(key.as_str(), "PATH" | "HOME")
            {
                return Err(format!("'{key}' is reserved for the Compose client"));
            }
        }
        Ok(Self {
            project,
            directory,
            config,
            env,
            development: false,
            override_file: PathBuf::from(OVERRIDE_FILE),
            source: None,
        })
    }

    pub fn development(
        directory: PathBuf,
        config: ComposeConfig,
        mut env: HashMap<String, String>,
        identity: &str,
    ) -> Result<Self, String> {
        // The identity includes the canonical config path and dev variant.
        env.entry(TAKO_APP_DATA_DIR_ENV.into())
            .or_insert_with(|| directory.display().to_string());
        let mut runtime = Self::new(directory, config, env)?;
        runtime.project = format!("tako-dev-{}", digest(identity));
        runtime.development = true;
        runtime.override_file = PathBuf::from(format!(".tako/compose/{}.yaml", runtime.project));
        Ok(runtime)
    }

    pub fn command(&self, with_override: bool) -> Command {
        let mut command = docker_command(self.development);
        command.envs(&self.env);
        command.current_dir(self.working_directory()).args([
            "compose",
            "--ansi",
            "never",
            "--project-name",
            &self.project,
            "--project-directory",
        ]);
        command.arg(self.working_directory()).arg("--file");
        command.arg(if let Some(source) = &self.source {
            source.parent().unwrap().join("compose.json")
        } else {
            self.directory.join(&self.config.file)
        });
        if !self.development {
            command.args(["--env-file", "/dev/null"]);
        }
        if with_override {
            command.arg("--file").arg(&self.override_file);
        }
        command.kill_on_drop(true);
        command
    }

    pub async fn output(
        &self,
        args: &[&str],
        with_override: bool,
        limit: Duration,
    ) -> Result<Vec<u8>, String> {
        self.capture_output(
            args,
            with_override,
            limit,
            process_output::StdoutMode::Complete,
        )
        .await
    }

    async fn run(&self, args: &[&str], with_override: bool, limit: Duration) -> Result<(), String> {
        self.capture_output(args, with_override, limit, process_output::StdoutMode::Tail)
            .await
            .map(|_| ())
    }

    async fn capture_output(
        &self,
        args: &[&str],
        with_override: bool,
        limit: Duration,
        stdout_mode: process_output::StdoutMode,
    ) -> Result<Vec<u8>, String> {
        let mut command = self.command(with_override);
        command
            .args(args)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let child = command
            .spawn()
            .map_err(|e| format!("Docker Compose v2 is required on this server: {e}"))?;
        let result = crate::process_output::capture(child, Some(limit), stdout_mode)
            .await
            .map_err(|e| e.to_string())?;
        if !result.status.is_some_and(|s| s.success()) {
            // Compose interpolation errors may contain private values. Keep
            // decrypted runtime environment out of stored error messages.
            let mut detail = String::from_utf8_lossy(&result.stderr).into_owned();
            for value in self.env.values().filter(|v| !v.is_empty()) {
                detail = detail.replace(value, "[redacted]");
            }
            return Err(format!(
                "Compose {} failed or timed out: {}",
                args[0],
                detail.trim()
            ));
        }
        Ok(result.stdout)
    }

    async fn model(&self) -> Result<Value, String> {
        let output = self
            .output(
                &["config", "--format", "json"],
                false,
                Duration::from_secs(30),
            )
            .await?;
        // `compose config` escapes every literal dollar in its exported model
        // so the output can be parsed by Compose again. Work with actual values
        // internally, then escape exactly once when rendering a generation.
        let output = String::from_utf8(output)
            .map_err(|_| "Compose config is not UTF-8")?
            .replace("$$", "$");
        let model: Value = serde_json::from_str(&output)
            .map_err(|e| format!("Invalid Compose config output: {e}"))?;
        validate_model_with_mode(
            &model,
            self.working_directory(),
            &self.config.service,
            self.development,
        )?;
        if !self.development {
            validation::validate_resources(&model, false, Some(&self.env))?;
        }
        Ok(model)
    }

    fn working_directory(&self) -> &Path {
        self.source.as_deref().unwrap_or(&self.directory)
    }

    async fn check_version(&self) -> Result<(), String> {
        let provider = self
            .output(&["version", "--short"], false, Duration::from_secs(15))
            .await?;
        let provider = String::from_utf8_lossy(&provider);
        let numbers: Vec<u32> = provider
            .trim()
            .trim_start_matches('v')
            .split('.')
            .take(3)
            .map(|part| {
                part.split('-')
                    .next()
                    .unwrap_or_default()
                    .parse()
                    .unwrap_or_default()
            })
            .collect();
        if numbers.as_slice() < [2, 24, 4].as_slice() {
            return Err("Compose deployments require Docker Compose 2.24.4 or newer".into());
        }
        Ok(())
    }

    pub async fn prepare(&self, _version: &str) -> Result<(), String> {
        if self.development {
            return Err("Production preparation requested for a development project".into());
        }
        let capture = self.clone();
        let (runtime, snapshot) = tokio::task::spawn_blocking(move || capture.snapshot())
            .await
            .map_err(|e| format!("Compose source capture failed: {e}"))??;
        runtime.check_version().await?;
        let mut model = runtime.model().await?;
        // Compose otherwise builds into a mutable project/service tag (or the
        // user's image tag). Concurrent preparations must never share that tag.
        // Build exactly the validated model, with inputs in the private source
        // copy. Resolved values travel on stdin, never through a release file.
        let attempt = digest(&snapshot.path().to_string_lossy());
        let mut temporary_images = Vec::new();
        for (service, definition) in model["services"]
            .as_object_mut()
            .ok_or("Compose has no services")?
        {
            if definition.get("build").is_some() {
                let image = format!(
                    "tako-compose-build/{}:{attempt}-{}",
                    self.project,
                    digest(service)
                );
                definition["image"] = serde_json::json!(image);
                temporary_images.push(image);
            }
        }
        for args in [&["build"][..], &["pull", "--ignore-buildable"][..]] {
            let child = runtime.spawn_model(&model, args).await?;
            let output = process_output::capture(
                child,
                Some(Duration::from_secs(1800)),
                process_output::StdoutMode::Tail,
            )
            .await
            .map_err(|e| e.to_string())?;
            if !output.status.is_some_and(|status| status.success()) {
                let mut detail = String::from_utf8_lossy(&output.stderr).into_owned();
                for value in self.env.values().filter(|value| !value.is_empty()) {
                    detail = detail.replace(value, "[redacted]");
                }
                return Err(format!(
                    "Compose {} failed or timed out: {}",
                    args[0],
                    detail.trim()
                ));
            }
        }
        let services = model["services"]
            .as_object()
            .ok_or("Compose has no services")?;
        let mut images = BTreeMap::new();
        for (service, definition) in services {
            let original = definition["image"]
                .as_str()
                .map(str::to_string)
                .unwrap_or_else(|| format!("{}-{service}", self.project));
            let mut inspect = docker_command(self.development);
            inspect.args(["image", "inspect", "--format", "{{json .}}"]);
            if let Some(platform) = definition["platform"].as_str() {
                inspect.args(["--platform", platform]);
            }
            inspect.arg(&original);
            let metadata = engine_output(inspect, Duration::from_secs(30)).await?;
            let metadata: Value = serde_json::from_slice(&metadata)
                .map_err(|_| "Docker returned invalid image metadata")?;
            let id = metadata["Id"]
                .as_str()
                .ok_or("Docker returned no image ID")?;
            id.strip_prefix("sha256:")
                .filter(|hash| hash.len() == 64 && hash.bytes().all(|c| c.is_ascii_hexdigit()))
                .ok_or("Docker returned an invalid image ID")?;
            let hash = image_fingerprint(&metadata)?;
            // Content-addressed tags keep unchanged services stable across
            // releases while protecting prepared images from dangling-image GC.
            // Containerd-backed engines can identify an attestation-bearing
            // index whose digest changes on a cached rebuild. Identity follows
            // the runnable configuration, filesystem, and platform instead.
            let image = format!("tako-compose/{}:{}-{hash}", self.project, digest(service));
            let result = docker_command(self.development)
                .args([
                    "--host",
                    "unix:///var/run/docker.sock",
                    "image",
                    "tag",
                    id,
                    &image,
                ])
                .output()
                .await
                .map_err(|e| e.to_string())?;
            if !result.status.success() {
                return Err(format!(
                    "Could not preserve the image for Compose service '{service}'"
                ));
            }
            images.insert(service.clone(), image);
        }
        for image in temporary_images {
            // The content-addressed tag now protects this image. Removing only
            // this attempt's build tag cannot affect another preparation.
            let mut remove = docker_command(self.development);
            remove.args(["image", "rm", &image]);
            let _ = engine_output(remove, Duration::from_secs(30)).await;
        }
        self.commit_preparation(snapshot, images)
    }

    pub async fn start_development(&self, port: u16) -> Result<Child, String> {
        if !self.development {
            return Err("development startup requires a development project".into());
        }
        self.check_version().await?;
        let model = self.model().await?;
        let images = model["services"]
            .as_object()
            .ok_or("Compose has no services")?
            .iter()
            .map(|(service, definition)| {
                (
                    service.clone(),
                    definition["image"]
                        .as_str()
                        .map(str::to_string)
                        .unwrap_or_else(|| format!("{}-{service}", self.project)),
                )
            })
            .collect();
        let override_path = self.directory.join(&self.override_file);
        std::fs::create_dir_all(override_path.parent().unwrap()).map_err(|e| e.to_string())?;
        write_private(
            &override_path,
            render_override(&images, &self.config, port).as_bytes(),
        )?;
        self.run(
            &[
                "up",
                "--detach",
                "--build",
                "--wait",
                "--wait-timeout",
                "120",
                "--remove-orphans",
            ],
            true,
            Duration::from_secs(1800),
        )
        .await?;
        self.command(true)
            .args(["logs", "--follow", "--no-color", "--tail", "100"])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| e.to_string())
    }

    pub async fn stop(&self) -> Result<(), String> {
        if !self.development {
            for container in self.containers().await?.into_iter().rev() {
                self.stop_container(&container.id).await?;
            }
            return Ok(());
        }
        // Cleanup must work even when interpolation of a new configuration or
        // rotated secret fails. Select only this stable project's containers.
        let mut list = docker_command(self.development);
        list.args(["container", "ls", "--all", "--quiet", "--filter"])
            .arg(format!("label=com.docker.compose.project={}", self.project));
        let ids = engine_output(list, Duration::from_secs(15)).await?;
        let ids = String::from_utf8_lossy(&ids);
        let ids: Vec<_> = ids.split_whitespace().collect();
        if ids
            .iter()
            .any(|id| !id.bytes().all(|c| c.is_ascii_hexdigit()))
        {
            return Err("Docker returned an invalid container ID".into());
        }
        if !ids.is_empty() {
            let mut stop = docker_command(self.development);
            stop.args(["container", "stop", "--time", "10"]).args(ids);
            engine_output(stop, Duration::from_secs(60)).await?;
        }
        Ok(())
    }

    pub async fn status(&self) -> Result<Vec<tako_core::ComposeServiceStatus>, String> {
        if !self.development {
            return Ok(self
                .containers()
                .await?
                .into_iter()
                .map(|c| tako_core::ComposeServiceStatus {
                    service: c.service,
                    state: c.state,
                    health: c.health,
                    exit_code: c.exit_code,
                })
                .collect());
        }
        let output = self
            .output(
                &["ps", "--all", "--format", "json"],
                false,
                Duration::from_secs(15),
            )
            .await?;
        // Compose versions emit either a JSON array or one object per line.
        let items = if let Ok(Value::Array(items)) = serde_json::from_slice(&output) {
            items
        } else {
            String::from_utf8_lossy(&output)
                .lines()
                .filter(|line| !line.trim().is_empty())
                .map(serde_json::from_str)
                .collect::<Result<Vec<Value>, _>>()
                .map_err(|e| e.to_string())?
        };
        Ok(items
            .into_iter()
            .map(|item| tako_core::ComposeServiceStatus {
                service: item["Service"].as_str().unwrap_or_default().into(),
                state: item["State"].as_str().unwrap_or_default().into(),
                health: item["Health"].as_str().unwrap_or_default().into(),
                exit_code: item["ExitCode"].as_i64().unwrap_or_default(),
            })
            .collect())
    }
}

fn image_fingerprint(metadata: &Value) -> Result<String, String> {
    if !metadata["Config"].is_object()
        || metadata["RootFS"]["Type"] != "layers"
        || !(metadata["RootFS"]["Layers"].is_array() || metadata["RootFS"]["Layers"].is_null())
        || !metadata["Architecture"].is_string()
        || !metadata["Os"].is_string()
    {
        return Err("Docker image metadata is missing its runtime configuration".into());
    }
    let identity = json!({
        "config":metadata["Config"], "rootfs":metadata["RootFS"],
        "architecture":metadata["Architecture"], "os":metadata["Os"],
        "variant":metadata["Variant"], "os_version":metadata["OsVersion"],
        "os_features":metadata["OsFeatures"]
    });
    Ok(hex::encode(Sha256::digest(
        serde_json::to_vec(&identity).map_err(|e| e.to_string())?,
    )))
}

async fn engine_output(mut command: Command, deadline: Duration) -> Result<Vec<u8>, String> {
    let child = command
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;
    let output =
        crate::process_output::capture(child, Some(deadline), process_output::StdoutMode::Complete)
            .await
            .map_err(|e| e.to_string())?;
    if !output.status.is_some_and(|status| status.success()) {
        return Err(format!(
            "Docker project cleanup failed or timed out: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(output.stdout)
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<(), String> {
    use std::io::Write;
    let parent = path
        .parent()
        .ok_or("Compose record has no parent directory")?;
    let mut file = tempfile::NamedTempFile::new_in(parent).map_err(|e| e.to_string())?;
    file.write_all(bytes).map_err(|e| e.to_string())?;
    file.as_file().sync_all().map_err(|e| e.to_string())?;
    file.persist(path).map_err(|e| e.to_string())?;
    std::fs::File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|e| e.to_string())
}

pub fn validate_model(model: &Value, directory: &Path, ingress: &str) -> Result<(), String> {
    validate_model_with_mode(model, directory, ingress, false)
}

fn validate_model_with_mode(
    model: &Value,
    directory: &Path,
    ingress: &str,
    development: bool,
) -> Result<(), String> {
    let services = model["services"]
        .as_object()
        .ok_or("Compose has no services")?;
    if !services.contains_key(ingress) {
        return Err(format!("Compose service '{ingress}' does not exist"));
    }
    let root = directory.canonicalize().map_err(|e| e.to_string())?;
    if !development {
        validation::validate_resources(model, false, None)?;
        validation::validate_resource_files(model, &root)?;
    }
    for (name, service) in services {
        if !development {
            validation::validate_service(name, service)?;
            if let Some(build) = service.get("build") {
                validation::validate_build(build, &root, false)?;
            }
        }
        for key in ["container_name", "network_mode", "profiles"] {
            if service.get(key).is_some() {
                return Err(format!(
                    "Compose service '{name}': {key} is not supported by Tako"
                ));
            }
        }
        if let Some(context) = service.pointer("/build/context").and_then(Value::as_str) {
            let context = Path::new(context)
                .canonicalize()
                .map_err(|e| format!("Compose build context: {e}"))?;
            if !context.starts_with(&root) {
                return Err(format!(
                    "Compose service '{name}' build context must stay inside the app directory"
                ));
            }
        }
        if let Some(volumes) = service["volumes"].as_array() {
            for volume in volumes {
                if volume["type"] == "bind" {
                    let source = volume["source"]
                        .as_str()
                        .ok_or("Compose bind mount is missing a source")?;
                    let source = Path::new(source)
                        .canonicalize()
                        .map_err(|e| format!("Compose bind mount: {e}"))?;
                    if (!development && volume["read_only"] != true) || !source.starts_with(&root) {
                        return Err(format!(
                            "Compose service '{name}': use named volumes for persistent data; bind mounts must be read-only files inside the app directory"
                        ));
                    }
                }
            }
        }
    }
    Ok(())
}

pub fn render_override(
    images: &BTreeMap<String, String>,
    config: &ComposeConfig,
    host_port: u16,
) -> String {
    let mut text = String::from("services:\n");
    for (service, image) in images {
        text.push_str(&format!(
            "  {}:\n    image: {}\n    ports: !override ",
            json!(service),
            json!(image)
        ));
        if service == &config.service {
            text.push_str(&format!(
                "[{}]\n",
                json!(format!("127.0.0.1:{host_port}:{}", config.port))
            ));
        } else {
            text.push_str("[]\n");
        }
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_identity_ignores_attestations_but_tracks_runtime_contents() {
        let original = json!({
            "Id":"sha256:first-index", "Created":"first-build",
            "Architecture":"amd64", "Os":"linux",
            "Config":{"Cmd":["serve"], "Env":["MODE=production"]},
            "RootFS":{"Type":"layers", "Layers":["sha256:filesystem"]}
        });
        let mut rebuilt = original.clone();
        rebuilt["Id"] = json!("sha256:second-index");
        rebuilt["Created"] = json!("second-build");
        rebuilt["RepoTags"] = json!(["different-build-attempt"]);
        assert_eq!(
            image_fingerprint(&original).unwrap(),
            image_fingerprint(&rebuilt).unwrap()
        );
        for (pointer, replacement) in [
            ("/Config/Cmd/0", "different-command"),
            ("/Config/Env/0", "MODE=staging"),
            ("/RootFS/Layers/0", "sha256:different-filesystem"),
            ("/Architecture", "arm64"),
        ] {
            let mut changed = original.clone();
            *changed.pointer_mut(pointer).unwrap() = json!(replacement);
            assert_ne!(
                image_fingerprint(&original).unwrap(),
                image_fingerprint(&changed).unwrap()
            );
        }
    }

    fn config() -> ComposeConfig {
        ComposeConfig {
            file: "compose.yaml".into(),
            service: "web".into(),
            port: 3000,
            health_path: "/health".into(),
            health_host: None,
        }
    }

    #[test]
    fn development_projects_separate_checkouts_variants_and_production() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("compose.yaml"), "services: {}\n").unwrap();
        let make = |identity| {
            ComposeRuntime::development(directory.path().into(), config(), HashMap::new(), identity)
                .unwrap()
        };
        let a = make("/work/a/tako.toml:");
        assert_eq!(a.project, make("/work/a/tako.toml:").project);
        assert_ne!(a.project, make("/work/b/tako.toml:").project);
        assert_ne!(a.project, make("/work/a/tako.toml:preview").project);
        let production = ComposeRuntime::new(
            directory.path().into(),
            config(),
            HashMap::from([(
                TAKO_APP_DATA_DIR_ENV.into(),
                directory.path().display().to_string(),
            )]),
        )
        .unwrap();
        assert_ne!(a.project, production.project);
    }

    #[test]
    fn development_allows_project_source_mounts_without_relaxing_production() {
        let directory = tempfile::tempdir().unwrap();
        let model = json!({"services":{"web":{"volumes":[{"type":"bind","source":directory.path(),"read_only":false}]}}});
        assert!(validate_model_with_mode(&model, directory.path(), "web", true).is_ok());
        assert!(validate_model(&model, directory.path(), "web").is_err());
        let outside = tempfile::tempdir().unwrap();
        assert!(validate_model_with_mode(&model, outside.path(), "web", true).is_err());
    }
}
