//! Compose materializes containers and resources; Tako owns production updates.
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::process::{Child, Command};

const TAKO_APP_DATA_DIR_ENV: &str = "TAKO_DATA_DIR";
const TAKO_SHARED_DATA_DIR_ENV: &str = "TAKO_SHARED_DATA_DIR";
const SHARED_DATA_MOUNT: &str = "/tako/shared-data";
/// The group's internal socket directory inside `sdk: true` services.
pub const SDK_SOCKET_MOUNT: &str = "/run/tako";
/// The group's app data directory inside `sdk: true` services.
pub const SDK_DATA_MOUNT: &str = "/tako/data";
mod config;
mod engine;
pub use config::{SDK_DEFAULT_PORT, http_port, ingress_port, load_config};
mod native_network;
pub use native_network::NativeNetwork;
mod plan;
mod prepared;
pub use prepared::remove_release_preparations;
mod process_output;
pub use process_output::with_cancellation as with_command_cancellation;
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

/// Remove every owned container before deleting its prepared inputs. Named
/// volumes remain available if the same app/environment is deployed again.
pub async fn remove_app_containers(app_data: &Path) -> Result<(), String> {
    cleanup_pending_project(&production_project(app_data)).await
}

/// The production Compose project of an app data directory. It survives
/// releases, so the group's socket directory can be named after it.
pub fn production_project(app_data: &Path) -> String {
    format!("tako-{}", digest(&app_data.to_string_lossy()))
}
use tako_core::compose::ComposeConfig;

const OVERRIDE_FILE: &str = ".tako-compose-override.yaml";
const CLIENT_ENV: &[&str] = &[
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
];

fn docker_command(development: bool) -> Command {
    let mut command = Command::new("docker");
    command.env_clear();
    for &key in CLIENT_ENV {
        if let Some(value) = std::env::var_os(key) {
            // Resolve service-owned client state before switching the child to
            // the captured app directory. Relative config paths must never
            // select files supplied by that app.
            if matches!(key, "HOME" | "DOCKER_CONFIG" | "BUILDX_CONFIG") {
                let path = PathBuf::from(&value);
                if path.is_relative() {
                    // A missing service cwd cannot safely resolve client state.
                    command.env(
                        key,
                        std::env::current_dir()
                            .map(|cwd| cwd.join(path))
                            .unwrap_or_else(|_| PathBuf::from("/dev/null")),
                    );
                } else {
                    command.env(key, value);
                }
            } else {
                command.env(key, value);
            }
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
    development_hosts: Option<(String, Option<String>)>,
    override_file: PathBuf,
    // Bound to one immutable, service-private preparation during a rollout.
    source: Option<PathBuf>,
    replicas: BTreeMap<String, u32>,
    stopped: bool,
    stopped_services: BTreeSet<String>,
    native_network: Option<NativeNetwork>,
    ingress_ports: BTreeMap<String, u16>,
    sdk: Option<SdkRuntime>,
}

/// The Tako runtime that `sdk: true` services receive. One group is one Tako
/// app: every SDK service shares this identity, token, socket and data.
// Do not derive Debug: the envelope carries secret values.
#[derive(Clone)]
pub struct SdkRuntime {
    /// `TAKO_APP_NAME`: the group's deployment ID, or the dev app name.
    pub app_name: String,
    /// The group's internal token. The bootstrap envelope carries it.
    pub token: String,
    /// The `TAKO_BOOTSTRAP_DATA` envelope, with the token, group secrets and
    /// S3 storages.
    pub envelope: String,
    /// `TAKO_BUILD`.
    pub build: String,
    /// `ENV`.
    pub environment: String,
    /// Host directory of the group's pinned internal socket, mounted
    /// read-only at [`SDK_SOCKET_MOUNT`].
    pub socket_dir: PathBuf,
    /// Host app data directory, mounted read-write at [`SDK_DATA_MOUNT`].
    pub data_dir: PathBuf,
    /// The `Host` of SDK readiness requests: `<app>.tako`.
    pub health_host: String,
}

/// Name of the internal socket in [`SdkRuntime::socket_dir`].
pub const SDK_SOCKET_FILE: &str = "internal.sock";
/// Variable that marks every `x-tako` service, so an SDK can explain a
/// missing `sdk: true`.
pub const TAKO_COMPOSE_SERVICE_ENV: &str = "TAKO_COMPOSE_SERVICE";
/// Client variable that carries the envelope to a development override.
/// Production never puts the envelope in the client environment.
const DEV_BOOTSTRAP_ENV: &str = "TAKO_DEV_BOOTSTRAP_DATA";

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
                || key.starts_with("BUILDX_")
                || key.starts_with("BUILDKIT_")
                || key.starts_with("LD_")
                || key.starts_with("DYLD_")
                || CLIENT_ENV.contains(&key.as_str())
                || matches!(key.as_str(), "EXPERIMENTAL_BUILDKIT_SOURCE_POLICY")
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
            development_hosts: None,
            override_file: PathBuf::from(OVERRIDE_FILE),
            source: None,
            replicas: BTreeMap::new(),
            stopped: false,
            stopped_services: BTreeSet::new(),
            native_network: None,
            ingress_ports: BTreeMap::new(),
            sdk: None,
        })
    }

    /// Give `sdk: true` services the Tako runtime.
    pub fn with_sdk(mut self, sdk: SdkRuntime) -> Self {
        if self.development {
            // The development override references this client variable, so
            // the project directory never holds the envelope.
            self.env
                .insert(DEV_BOOTSTRAP_ENV.into(), sdk.envelope.clone());
        }
        self.sdk = Some(sdk);
        self
    }

    /// Drop the Tako runtime, for a reloaded group with no `sdk: true`
    /// services left.
    pub fn without_sdk(mut self) -> Self {
        self.env.remove(DEV_BOOTSTRAP_ENV);
        self.sdk = None;
        self
    }

    pub fn sdk(&self) -> Option<&SdkRuntime> {
        self.sdk.as_ref()
    }

    /// The internal socket path SDK services see.
    pub fn sdk_socket_path() -> String {
        format!("{SDK_SOCKET_MOUNT}/{SDK_SOCKET_FILE}")
    }

    /// What a development override adds to each `x-tako` service. Secret
    /// values are `${...}` references to the development client environment.
    fn development_services(
        &self,
        ports: &BTreeMap<String, (u16, u16)>,
    ) -> Result<BTreeMap<String, DevelopmentService>, String> {
        let literal = |value: &str| value.replace('$', "$$");
        let mut services = BTreeMap::new();
        for (name, settings) in &self.config.services {
            let mut service = DevelopmentService::default();
            service
                .environment
                .push((TAKO_COMPOSE_SERVICE_ENV.into(), literal(name)));
            if settings.sdk {
                let sdk = self.sdk.as_ref().ok_or_else(|| {
                    format!("Compose service '{name}' is marked sdk: true, but Tako has no SDK runtime for this app")
                })?;
                let port = ports.get(name).map(|(_, port)| *port);
                for (key, value) in self.sdk_environment(name, sdk, port) {
                    service.environment.push((key.into(), literal(&value)));
                }
                service.environment.push((
                    tako_core::bootstrap::TAKO_BOOTSTRAP_DATA_ENV.into(),
                    format!("${{{DEV_BOOTSTRAP_ENV}}}"),
                ));
                service.volumes.extend(sdk_mounts(sdk).map(|mut mount| {
                    let source = literal(mount["source"].as_str().unwrap_or_default());
                    mount["source"] = json!(source);
                    mount
                }));
                #[cfg(unix)]
                if let Ok(metadata) = std::fs::metadata(&sdk.data_dir) {
                    use std::os::unix::fs::MetadataExt;
                    service.group_add.push(metadata.gid().to_string());
                }
            }
            services.insert(name.clone(), service);
        }
        Ok(services)
    }

    pub fn with_native_network(mut self, network: NativeNetwork) -> Self {
        self.native_network = Some(network);
        self
    }

    /// Desired replica overrides, scoped to service names in this project.
    pub fn with_replicas(mut self, replicas: BTreeMap<String, u32>) -> Self {
        self.replicas = replicas;
        self
    }

    /// Runtime intent survives releases without changing configured replica counts.
    pub fn with_stopped(mut self, stopped: bool, services: BTreeSet<String>) -> Self {
        self.stopped = stopped;
        self.stopped_services = services;
        self
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

    pub fn with_development_hosts(
        mut self,
        default_host: String,
        base_host: Option<String>,
    ) -> Self {
        self.config = self.config.for_local_environment(
            self.local_environment(),
            &default_host,
            base_host.as_deref(),
        );
        self.development_hosts = Some((default_host, base_host));
        self
    }

    pub fn reload_development(&self) -> Result<Self, String> {
        if !self.development {
            return Err("Only development projects can reload Compose metadata".into());
        }
        let mut runtime = self.clone();
        runtime.config = load_config(&self.directory, &self.config.file)?;
        if let Some((host, base)) = &self.development_hosts {
            runtime.config = runtime.config.for_local_environment(
                self.local_environment(),
                host,
                base.as_deref(),
            );
        }
        runtime.ingress_ports.clear();
        Ok(runtime)
    }

    fn shared_mount(&self) -> Option<Value> {
        self.env.get(TAKO_SHARED_DATA_DIR_ENV).map(|source| {
            json!({
                "type": "bind", "source": source, "target": SHARED_DATA_MOUNT,
                "bind": {"create_host_path": false}
            })
        })
    }

    pub fn local_environment(&self) -> &str {
        self.env
            .get("ENV")
            .map(String::as_str)
            .unwrap_or("development")
    }

    fn inject_shared_data(&self, service: &mut Value) {
        let Some(mount) = self.shared_mount() else {
            return;
        };
        let mut volumes = service["volumes"].as_array().cloned().unwrap_or_default();
        volumes.push(mount);
        service["volumes"] = json!(volumes);
        if !service["environment"].is_object() {
            service["environment"] = json!({});
        }
        service["environment"][TAKO_SHARED_DATA_DIR_ENV] = json!(SHARED_DATA_MOUNT);
        #[cfg(unix)]
        if let Some(source) = self.env.get(TAKO_SHARED_DATA_DIR_ENV) {
            use std::os::unix::fs::MetadataExt;
            if let Ok(metadata) = std::fs::metadata(source) {
                let mut groups = service["group_add"].as_array().cloned().unwrap_or_default();
                groups.push(json!(metadata.gid().to_string()));
                service["group_add"] = json!(groups);
            }
        }
    }

    /// Add the Tako runtime to an `x-tako` service definition, after its
    /// fingerprint is computed. Values travel to Docker on stdin, never through
    /// the client environment or a file.
    fn inject_tako(&self, name: &str, service: &mut Value) -> Result<(), String> {
        let Some(settings) = self.config.services.get(name) else {
            return Ok(());
        };
        if !service["environment"].is_object() {
            service["environment"] = json!({});
        }
        service["environment"][TAKO_COMPOSE_SERVICE_ENV] = json!(name);
        if !settings.sdk {
            return Ok(());
        }
        let sdk = self.sdk.as_ref().ok_or_else(|| {
            format!("Compose service '{name}' is marked sdk: true, but Tako has no SDK runtime for this group")
        })?;
        let port = self.ingress_ports.get(name).copied();
        for (key, value) in self.sdk_environment(name, sdk, port) {
            service["environment"][key] = json!(value);
        }
        service["environment"][tako_core::bootstrap::TAKO_BOOTSTRAP_DATA_ENV] = json!(sdk.envelope);
        let mut volumes = service["volumes"].as_array().cloned().unwrap_or_default();
        volumes.extend(sdk_mounts(sdk));
        service["volumes"] = json!(volumes);
        #[cfg(unix)]
        if let Ok(metadata) = std::fs::metadata(&sdk.data_dir) {
            use std::os::unix::fs::MetadataExt;
            let mut groups = service["group_add"].as_array().cloned().unwrap_or_default();
            let group = json!(metadata.gid().to_string());
            // The shared data directory may already have added this group;
            // Compose rejects duplicates.
            if !groups.contains(&group) {
                groups.push(group);
            }
            service["group_add"] = json!(groups);
        }
        Ok(())
    }

    /// Non-secret runtime variables of one SDK service.
    fn sdk_environment(
        &self,
        name: &str,
        sdk: &SdkRuntime,
        port: Option<u16>,
    ) -> Vec<(&'static str, String)> {
        let mut environment = vec![
            (
                tako_core::instance_env::TAKO_APP_NAME_ENV,
                sdk.app_name.clone(),
            ),
            (
                tako_core::instance_env::TAKO_INTERNAL_SOCKET_ENV,
                Self::sdk_socket_path(),
            ),
            (TAKO_APP_DATA_DIR_ENV, SDK_DATA_MOUNT.into()),
            ("TAKO_BUILD", sdk.build.clone()),
            ("ENV", sdk.environment.clone()),
        ];
        if self.is_ingress(name) {
            environment.push((tako_core::instance_env::HOST_ENV, "0.0.0.0".into()));
            if let Some(port) = port {
                environment.push((tako_core::instance_env::PORT_ENV, port.to_string()));
            }
        }
        environment
    }

    /// The SDK runtime's part of a service's fingerprint. Every deploy and
    /// every secret change replaces SDK services, and only them.
    fn sdk_identity(&self, name: &str) -> Value {
        match (&self.sdk, self.config.is_sdk(name)) {
            (Some(sdk), true) => json!({
                "app": sdk.app_name,
                "envelope": hex::encode(Sha256::digest(sdk.envelope.as_bytes())),
                "build": sdk.build,
                "environment": sdk.environment,
                "mounts": sdk_mounts(sdk),
            }),
            _ => Value::Null,
        }
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
        process_output::check_cancellation().map_err(|e| e.to_string())?;
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
        // Docker removes service extensions from normalized output. Validate
        // metadata against the captured source before giving that source to it.
        let services = if let Some(source) = &self.source {
            let bytes = std::fs::read(source.parent().unwrap().join("compose.json"))
                .map_err(|e| e.to_string())?;
            let raw =
                serde_json::from_slice(&bytes).map_err(|_| "Invalid captured Compose model")?;
            config::service_configs(&raw)?
        } else {
            load_config(&self.directory, &self.config.file)?.services
        };
        if !self.development && services != self.config.services {
            return Err(
                "Compose x-tako settings differ from the captured release; deploy again".into(),
            );
        }
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
        validate_model_with_mode(&model, self.working_directory(), "", self.development)?;
        // Profiles and other normalization can drop a service declared in the
        // source; its routes would then point at an app with no containers.
        if let Some(missing) = services
            .keys()
            .find(|name| model["services"].get(name.as_str()).is_none())
        {
            return Err(format!(
                "Compose service '{missing}' has x-tako settings but is not part of the Compose model"
            ));
        }
        if !self.development {
            validation::validate_resources(&model, false, Some(&self.env))?;
        }
        validation::validate_tako_services(&model, &self.config)?;
        if self.env.contains_key(TAKO_SHARED_DATA_DIR_ENV) {
            for service in model["services"]
                .as_object()
                .into_iter()
                .flat_map(|services| services.values())
            {
                for volume in service["volumes"].as_array().into_iter().flatten() {
                    if let Some(target) = volume["target"].as_str() {
                        if Path::new(target).starts_with(SHARED_DATA_MOUNT)
                            || Path::new(SHARED_DATA_MOUNT).starts_with(target)
                        {
                            return Err("Compose volumes must not overlap the managed /tako/shared-data mount".into());
                        }
                    }
                }
            }
        }
        Ok(model)
    }

    /// HTTP apps own a backend pool. SDK workers, plain services and jobs do not.
    pub fn is_ingress(&self, service: &str) -> bool {
        self.config.is_http(service)
    }

    pub fn ingress_count(&self, replicas: &BTreeMap<String, u32>) -> u32 {
        replicas
            .iter()
            .filter(|(service, _)| self.is_ingress(service))
            .map(|(_, count)| *count)
            .sum()
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
        process_output::check_cancellation().map_err(|e| e.to_string())?;
        if self.development {
            return Err("Production preparation requested for a development project".into());
        }
        if self
            .preparation_root()?
            .join("current.json")
            .try_exists()
            .map_err(|e| e.to_string())?
        {
            self.prepared()?;
            return Ok(());
        }
        let capture = self.clone();
        let (runtime, snapshot) = tokio::task::spawn_blocking(move || capture.snapshot())
            .await
            .map_err(|e| format!("Compose source capture failed: {e}"))??;
        process_output::check_cancellation().map_err(|e| e.to_string())?;
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
            let mut tag = docker_command(self.development);
            tag.args([
                "--host",
                "unix:///var/run/docker.sock",
                "image",
                "tag",
                id,
                &image,
            ]);
            engine_output(tag, Duration::from_secs(30)).await?;
            images.insert(service.clone(), image);
        }
        for image in temporary_images {
            // The content-addressed tag now protects this image. Removing only
            // this attempt's build tag cannot affect another preparation.
            let mut remove = docker_command(self.development);
            remove.args(["image", "rm", &image]);
            let _ = engine_output(remove, Duration::from_secs(30)).await;
        }
        process_output::check_cancellation().map_err(|e| e.to_string())?;
        self.commit_preparation(snapshot, images)
    }

    pub async fn start_development(&self) -> Result<Child, String> {
        if !self.development {
            return Err("development startup requires a development project".into());
        }
        self.check_version().await?;
        let model = self.model().await?;
        // Build/pull before inspecting image EXPOSE metadata; no containers start here.
        self.run(&["build"], false, Duration::from_secs(1800))
            .await?;
        self.run(
            &["pull", "--ignore-buildable"],
            false,
            Duration::from_secs(1800),
        )
        .await?;
        let mut images = BTreeMap::new();
        let mut ports = BTreeMap::new();
        for (service, definition) in model["services"]
            .as_object()
            .ok_or("Compose has no services")?
        {
            let image = definition["image"]
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| format!("{}-{service}", self.project));
            if self.is_ingress(service) {
                let mut inspect = docker_command(true);
                inspect.args([
                    "image",
                    "inspect",
                    "--format",
                    "{{json (index .Config \"ExposedPorts\")}}",
                    &image,
                ]);
                let output = engine_output(inspect, Duration::from_secs(30)).await?;
                let exposed: Value =
                    serde_json::from_slice(&output).map_err(|_| "Invalid image port metadata")?;
                ports.insert(
                    service.clone(),
                    (
                        0,
                        http_port(service, definition, &exposed, self.config.is_sdk(service))?,
                    ),
                );
            }
            images.insert(service.clone(), image);
        }
        let override_path = self.directory.join(&self.override_file);
        std::fs::create_dir_all(override_path.parent().unwrap()).map_err(|e| e.to_string())?;
        let tako = self.development_services(&ports)?;
        write_private(
            &override_path,
            render_development_override(
                &images,
                &ports,
                self.env.get(TAKO_SHARED_DATA_DIR_ENV).map(String::as_str),
                &tako,
            )
            .as_bytes(),
        )?;
        self.run(
            &[
                "up",
                "--detach",
                "--no-build",
                "--wait",
                "--wait-timeout",
                "120",
                "--remove-orphans",
            ],
            true,
            Duration::from_secs(1800),
        )
        .await?;
        for container in self.containers().await? {
            if !self.is_ingress(&container.service) {
                continue;
            }
            let ready = tokio::time::timeout(Duration::from_secs(30), async {
                loop {
                    if self.container_healthy(&container.id).await? {
                        return Ok::<_, String>(());
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            })
            .await;
            ready.map_err(|_| self.readiness_error(&container.service))??;
        }
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

/// The socket directory (read-only) and app data directory of SDK services.
/// Mounting the socket's directory, not the file, lets a successor server
/// re-bind it without breaking running containers.
fn sdk_mounts(sdk: &SdkRuntime) -> [Value; 2] {
    [
        json!({
            "type": "bind", "source": sdk.socket_dir, "target": SDK_SOCKET_MOUNT,
            "read_only": true, "bind": {"create_host_path": false}
        }),
        json!({
            "type": "bind", "source": sdk.data_dir, "target": SDK_DATA_MOUNT,
            "bind": {"create_host_path": false}
        }),
    ]
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
    process_output::check_cancellation().map_err(|e| e.to_string())?;
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
    if !ingress.is_empty() && !services.contains_key(ingress) {
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
    ports: &BTreeMap<String, (u16, u16)>,
) -> String {
    render_development_override(images, ports, None, &BTreeMap::new())
}

/// What a development override adds to one `x-tako` service.
#[derive(Default)]
struct DevelopmentService {
    /// Values are Compose-interpolated: `$$` is a literal dollar.
    environment: Vec<(String, String)>,
    volumes: Vec<Value>,
    group_add: Vec<String>,
}

fn render_development_override(
    images: &BTreeMap<String, String>,
    ports: &BTreeMap<String, (u16, u16)>,
    shared: Option<&str>,
    tako: &BTreeMap<String, DevelopmentService>,
) -> String {
    let mut text = String::from("services:\n");
    for (service, image) in images {
        text.push_str(&format!(
            "  {}:\n    image: {}\n    ports: !override ",
            json!(service),
            json!(image)
        ));
        if let Some((host_port, port)) = ports.get(service) {
            text.push_str(&format!(
                "[{}]\n    scale: 1\n    deploy:\n      replicas: 1\n",
                json!(format!("127.0.0.1:{host_port}:{port}"))
            ));
        } else {
            text.push_str("[]\n");
        }
        let mut environment = Vec::new();
        let mut volumes = Vec::new();
        let mut groups = Vec::new();
        if let Some(source) = shared {
            volumes.push(json!({
                "type": "bind", "source": source, "target": SHARED_DATA_MOUNT,
                "bind": {"create_host_path": false}
            }));
            environment.push((
                TAKO_SHARED_DATA_DIR_ENV.to_string(),
                SHARED_DATA_MOUNT.to_string(),
            ));
            #[cfg(unix)]
            if let Ok(metadata) = std::fs::metadata(source) {
                use std::os::unix::fs::MetadataExt;
                groups.push(metadata.gid().to_string());
            }
        }
        if let Some(service) = tako.get(service) {
            environment.extend(service.environment.iter().cloned());
            volumes.extend(service.volumes.iter().cloned());
            for group in &service.group_add {
                if !groups.contains(group) {
                    groups.push(group.clone());
                }
            }
        }
        if !volumes.is_empty() {
            text.push_str("    volumes:\n");
            for volume in volumes {
                text.push_str(&format!("      - {volume}\n"));
            }
        }
        if !environment.is_empty() {
            text.push_str("    environment:\n");
            for (key, value) in environment {
                text.push_str(&format!("      {}: {}\n", json!(key), json!(value)));
            }
        }
        if !groups.is_empty() {
            text.push_str(&format!("    group_add: {}\n", json!(groups)));
        }
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_variables_cannot_select_buildkit_client_state_or_builders() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(
            directory.path().join("compose.yaml"),
            "services: {web: {build: .}}\n",
        )
        .unwrap();
        std::fs::create_dir_all(directory.path().join(".buildx/instances")).unwrap();
        std::fs::write(directory.path().join(".buildx/instances/evil"),
            r#"{"Name":"evil","Driver":"docker-container","Nodes":[{"DriverOpts":{"image":"attacker/builder"}}]}"#).unwrap();
        for (key, value) in [
            ("BUILDX_CONFIG", ".buildx"),
            ("BUILDX_BUILDER", "evil"),
            ("BUILDX_BAKE_FILE", "attacker.json"),
            ("BUILDKIT_HOST", "tcp://attacker:1234"),
            ("EXPERIMENTAL_BUILDKIT_SOURCE_POLICY", "attacker.json"),
            ("SSL_CERT_FILE", "attacker-ca.pem"),
            ("HTTPS_PROXY", "https://attacker:1234"),
        ] {
            let env = HashMap::from([
                (
                    TAKO_APP_DATA_DIR_ENV.into(),
                    directory.path().join("data").display().to_string(),
                ),
                (key.into(), value.into()),
            ]);
            let error = ComposeRuntime::new(directory.path().into(), config(), env).unwrap_err();
            assert!(
                error.contains("reserved for the Compose client"),
                "{key}: {error}"
            );
        }
    }

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
            services: Default::default(),
        }
    }

    #[test]
    fn shared_mount_is_managed_and_preserves_service_environment_and_groups() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("compose.yaml"), "services: {}\n").unwrap();
        let runtime = ComposeRuntime::new(
            directory.path().into(),
            config(),
            HashMap::from([
                (
                    TAKO_APP_DATA_DIR_ENV.into(),
                    directory.path().display().to_string(),
                ),
                (
                    TAKO_SHARED_DATA_DIR_ENV.into(),
                    directory.path().join("shared").display().to_string(),
                ),
            ]),
        )
        .unwrap();
        std::fs::create_dir(directory.path().join("shared")).unwrap();
        let mut service = json!({"environment":{"APP":"web"}, "volumes":[{"type":"volume", "source":"uploads", "target":"/uploads"}], "group_add":["123"]});
        runtime.inject_shared_data(&mut service);
        assert_eq!(service["environment"]["APP"], "web");
        assert_eq!(
            service["environment"][TAKO_SHARED_DATA_DIR_ENV],
            SHARED_DATA_MOUNT
        );
        assert_eq!(service["volumes"][0]["target"], "/uploads");
        assert_eq!(
            service["volumes"][1]["source"],
            runtime.env[TAKO_SHARED_DATA_DIR_ENV]
        );
        assert_eq!(service["volumes"][1]["bind"]["create_host_path"], false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let gid = std::fs::metadata(directory.path().join("shared"))
                .unwrap()
                .gid()
                .to_string();
            assert_eq!(service["group_add"], json!(["123", gid]));
        }
        let development = render_development_override(
            &BTreeMap::from([("web".into(), "image".into())]),
            &BTreeMap::from([("web".into(), (1234, 8080))]),
            Some(&runtime.env[TAKO_SHARED_DATA_DIR_ENV]),
            &BTreeMap::new(),
        );
        assert!(development.contains("\"TAKO_SHARED_DATA_DIR\": \"/tako/shared-data\""));
        assert!(development.contains("\"create_host_path\":false"));
    }

    #[test]
    fn development_override_references_the_envelope_without_writing_secrets() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("compose.yaml"), "services: {}\n").unwrap();
        let config = ComposeConfig {
            file: "compose.yaml".into(),
            services: serde_json::from_value(json!({
                "web": {"sdk": true, "envs": {"development": {"route": "shop.test"}}},
                "worker": {"sdk": true},
                "grafana": {"http": true}
            }))
            .unwrap(),
        };
        let runtime = ComposeRuntime::development(
            directory.path().into(),
            config,
            HashMap::new(),
            "sdk-test",
        )
        .unwrap()
        .with_sdk(SdkRuntime {
            app_name: "shop".into(),
            token: "dev-token".into(),
            envelope: json!({"token":"dev-token","secrets":{"API_KEY":"top-secret"}}).to_string(),
            build: "dev".into(),
            environment: "development".into(),
            socket_dir: directory.path().join("socket"),
            data_dir: directory.path().join(".tako/data/app"),
            health_host: "shop.tako".into(),
        });
        // Only the development client carries the envelope.
        assert!(runtime.env[DEV_BOOTSTRAP_ENV].contains("top-secret"));
        let ports = BTreeMap::from([("web".into(), (0, 3000)), ("grafana".into(), (0, 3000))]);
        let images: BTreeMap<String, String> = ["web", "worker", "grafana", "redis"]
            .into_iter()
            .map(|name| (name.to_string(), format!("{name}:dev")))
            .collect();
        let text = render_development_override(
            &images,
            &ports,
            None,
            &runtime.development_services(&ports).unwrap(),
        );
        assert!(!text.contains("top-secret"), "{text}");
        assert!(!text.contains("dev-token"), "{text}");
        assert!(text.contains("\"TAKO_BOOTSTRAP_DATA\": \"${TAKO_DEV_BOOTSTRAP_DATA}\""));
        assert!(text.contains("\"TAKO_INTERNAL_SOCKET\": \"/run/tako/internal.sock\""));
        assert!(text.contains("\"PORT\": \"3000\""));
        assert!(text.contains("\"TAKO_COMPOSE_SERVICE\": \"grafana\""));
        let parsed: serde_yaml_ng::Value = serde_yaml_ng::from_str(&text).unwrap();
        let services = &parsed["services"];
        assert!(services["worker"]["environment"].get("PORT").is_none());
        assert!(
            services["grafana"]["environment"]
                .get("TAKO_APP_NAME")
                .is_none()
        );
        assert!(services["redis"].get("environment").is_none());
    }

    #[test]
    fn a_reload_without_sdk_services_drops_the_runtime_and_its_envelope() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(
            directory.path().join("compose.yaml"),
            "services:\n  web:\n    image: web\n    x-tako: {sdk: true, http: true}\n",
        )
        .unwrap();
        let config = load_config(directory.path(), "compose.yaml").unwrap();
        let runtime = ComposeRuntime::development(
            directory.path().into(),
            config,
            HashMap::new(),
            "sdk-reload-test",
        )
        .unwrap()
        .with_sdk(SdkRuntime {
            app_name: "shop".into(),
            token: "dev-token".into(),
            envelope: json!({"token":"dev-token","secrets":{"API_KEY":"top-secret"}}).to_string(),
            build: "dev".into(),
            environment: "development".into(),
            socket_dir: directory.path().join("socket"),
            data_dir: directory.path().join(".tako/data/app"),
            health_host: "shop.tako".into(),
        });
        // The Compose file drops sdk: true; the reload clones the old runtime.
        std::fs::write(
            directory.path().join("compose.yaml"),
            "services:\n  web:\n    image: web\n    x-tako: {http: true}\n",
        )
        .unwrap();
        let reloaded = runtime.reload_development().unwrap();
        assert!(!reloaded.config.has_sdk_services());
        assert!(reloaded.sdk().is_some(), "a reload alone keeps the binding");
        let reloaded = reloaded.without_sdk();
        assert!(reloaded.sdk().is_none());
        assert!(!reloaded.env.contains_key(DEV_BOOTSTRAP_ENV));
    }

    #[tokio::test]
    #[ignore = "requires Docker Compose with shared host/container loopback and project paths"]
    async fn development_sdk_services_start_with_the_runtime_and_pass_sdk_readiness() {
        let directory = tempfile::tempdir().unwrap();
        let project = directory.path();
        std::fs::write(
            project.join("compose.yaml"),
            "services:\n  web:\n    build: .\n    command: [sh, -c, 'exec nc -lk -p \"$$PORT\" -e /handler.sh']\n    x-tako: {sdk: true, http: true}\n  worker:\n    build: .\n    command: [sleep, '3600']\n    x-tako: {sdk: true}\n  plain:\n    build: .\n    command: [sleep, '3600']\n",
        )
        .unwrap();
        std::fs::write(
            project.join("Dockerfile"),
            "FROM docker.io/library/busybox:1.37\nCOPY handler.sh /handler.sh\nRUN chmod 0755 /handler.sh\n",
        )
        .unwrap();
        // Answers Tako's SDK check only with the token from the envelope.
        std::fs::write(
            project.join("handler.sh"),
            r#"#!/bin/sh
token=$(printf '%s' "$TAKO_BOOTSTRAP_DATA" | sed -n 's/.*"token":"\([^"]*\)".*/\1/p')
auth=
read -r method path version
while IFS= read -r line; do
  line=$(printf '%s' "$line" | tr -d '\r')
  [ -z "$line" ] && break
  case "$line" in
    [Xx]-[Tt]ako-[Ii]nternal-[Tt]oken:*) auth=$(printf '%s' "${line#*:}" | tr -d ' ') ;;
  esac
done
if [ -n "$token" ] && [ "$auth" = "$token" ]; then
  printf 'HTTP/1.1 200 OK\r\nx-tako-internal-token: %s\r\nContent-Length: 0\r\nConnection: close\r\n\r\n' "$token"
else
  printf 'HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n'
fi
"#,
        )
        .unwrap();
        let data = project.join(".tako/data/app");
        std::fs::create_dir_all(&data).unwrap();
        let socket_dir = project.join("socket");
        std::fs::create_dir_all(&socket_dir).unwrap();
        let _listener =
            std::os::unix::net::UnixListener::bind(socket_dir.join(SDK_SOCKET_FILE)).unwrap();
        let config = load_config(project, "compose.yaml").unwrap();
        let runtime = ComposeRuntime::development(
            project.into(),
            config,
            HashMap::from([
                (TAKO_APP_DATA_DIR_ENV.into(), data.display().to_string()),
                ("API_KEY".into(), "top-secret".into()),
            ]),
            "sdk-development-test",
        )
        .unwrap()
        .with_development_hosts("sdk.test".into(), None)
        .with_sdk(SdkRuntime {
            app_name: "sdk".into(),
            token: "dev-token".into(),
            envelope: json!({"token":"dev-token","secrets":{"API_KEY":"top-secret"},"storages":{},"databases":{}}).to_string(),
            build: "dev".into(),
            environment: "development".into(),
            socket_dir: socket_dir.clone(),
            data_dir: data.clone(),
            health_host: "sdk.tako".into(),
        });
        struct Cleanup(ComposeRuntime);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = self
                    .0
                    .command(true)
                    .into_std()
                    .args(["down", "--volumes", "--remove-orphans"])
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status();
            }
        }
        let _cleanup = Cleanup(runtime.clone());
        let _logs = runtime.start_development().await.unwrap();
        let generated = std::fs::read_to_string(project.join(&runtime.override_file)).unwrap();
        assert!(!generated.contains("top-secret"), "{generated}");
        assert!(!generated.contains("dev-token"), "{generated}");
        let containers = runtime.containers().await.unwrap();
        let id = |service: &str| {
            containers
                .iter()
                .find(|c| c.service == service)
                .unwrap()
                .id
                .clone()
        };
        let exec = |id: String, script: &'static str| async move {
            let output = tokio::process::Command::new("docker")
                .args(["exec", &id, "sh", "-c", script])
                .output()
                .await
                .unwrap();
            (
                output.status.success(),
                String::from_utf8_lossy(&output.stdout).trim().to_string(),
            )
        };
        let (ok, envelope) = exec(id("web"), "printenv TAKO_BOOTSTRAP_DATA").await;
        assert!(ok && envelope.contains("top-secret"));
        assert_eq!(
            exec(
                id("worker"),
                "test -S /run/tako/internal.sock && printenv TAKO_APP_NAME"
            )
            .await,
            (true, "sdk".into())
        );
        assert!(!exec(id("plain"), "printenv TAKO_BOOTSTRAP_DATA").await.0);
        runtime.stop().await.unwrap();
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
