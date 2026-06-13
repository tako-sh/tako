use crate::container_runtime::{
    build_container_run_env, build_container_workflow_run_args, detect_container_engine,
    image_tag_for_manifest,
};
use crate::instances::{AppManager, clamp_instances_to_limit};
use crate::lb::LoadBalancer;
use crate::release::{apply_release_runtime_to_config, release_app_path};
use crate::release::{
    ensure_app_runtime_data_dirs, inject_app_data_dir_env, resolve_release_runtime_bin,
};
use crate::routing::RouteTable;
use crate::socket::{AppState, Response};
use crate::state_store::{SqliteStateStore, StateStoreError, load_or_create_device_key};
use crate::tls::{AcmeClient, CertManager, ChallengeTokens};
use parking_lot::RwLock as SyncRwLock;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tako_core::{ServerRuntimeInfo, UpgradeMode};
use tokio::sync::RwLock as AsyncRwLock;

#[derive(Debug, Clone)]
pub struct ServerRuntimeConfig {
    pub(crate) pid: u32,
    pub(crate) process_started_at_unix_secs: Option<i64>,
    pub(crate) socket: String,
    pub(crate) data_dir: PathBuf,
    pub(crate) http_port: u16,
    pub(crate) https_port: u16,
    pub(crate) no_acme: bool,
    pub(crate) acme_staging: bool,
    pub(crate) renewal_interval_hours: u64,
    pub(crate) standby: bool,
    pub(crate) metrics_port: Option<u16>,
    pub(crate) server_name: Option<String>,
    pub(crate) server_identity: Option<String>,
}

impl ServerRuntimeConfig {
    pub(crate) fn for_defaults(data_dir: PathBuf) -> Self {
        Self {
            pid: std::process::id(),
            process_started_at_unix_secs: None,
            socket: "/var/run/tako/tako.sock".to_string(),
            data_dir,
            http_port: 80,
            https_port: 443,
            no_acme: false,
            acme_staging: false,
            renewal_interval_hours: 12,
            standby: false,
            metrics_port: Some(9898),
            server_name: None,
            server_identity: None,
        }
    }

    pub(crate) fn to_runtime_info(&self, mode: UpgradeMode) -> ServerRuntimeInfo {
        ServerRuntimeInfo {
            pid: self.pid,
            mode,
            process_started_at_unix_secs: self.process_started_at_unix_secs,
            socket: self.socket.clone(),
            data_dir: self.data_dir.to_string_lossy().to_string(),
            http_port: self.http_port,
            https_port: self.https_port,
            no_acme: self.no_acme,
            acme_staging: self.acme_staging,
            acme_email: None,
            renewal_interval_hours: self.renewal_interval_hours,
            standby: self.standby,
            metrics_port: self.metrics_port,
            server_name: self.server_name.clone(),
            server_identity: self.server_identity.clone(),
        }
    }
}

/// Server state shared across components
pub struct ServerState {
    pub(crate) app_manager: Arc<AppManager>,
    pub(crate) load_balancer: Arc<LoadBalancer>,
    pub(crate) cert_manager: Arc<CertManager>,
    pub(crate) acme_client: AsyncRwLock<Option<Arc<AcmeClient>>>,
    pub(crate) challenge_tokens: ChallengeTokens,
    pub(crate) routes: Arc<SyncRwLock<RouteTable>>,
    pub(crate) deploy_locks: AsyncRwLock<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    prepared_deploy_ssl: AsyncRwLock<HashMap<PreparedDeployKey, PreparedDeploySsl>>,
    pub(crate) cold_start: Arc<crate::scaling::ColdStartManager>,
    pub(crate) state_store: Arc<SqliteStateStore>,
    pub(crate) server_mode: AsyncRwLock<UpgradeMode>,
    pub(crate) runtime: ServerRuntimeConfig,
    pub(crate) workflows: Arc<crate::workflows::WorkflowManager>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct PreparedDeployKey {
    app: String,
    path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PreparedDeploySsl {
    routes: Vec<String>,
    ssl: tako_core::SslBinding,
}

impl ServerState {
    pub fn new(
        data_dir: PathBuf,
        cert_manager: Arc<CertManager>,
        acme_client: Option<Arc<AcmeClient>>,
        challenge_tokens: ChallengeTokens,
    ) -> Result<Self, StateStoreError> {
        let runtime = ServerRuntimeConfig::for_defaults(data_dir.clone());
        Self::new_with_runtime(
            data_dir,
            cert_manager,
            acme_client,
            challenge_tokens,
            runtime,
        )
    }

    pub fn new_with_runtime(
        data_dir: PathBuf,
        cert_manager: Arc<CertManager>,
        acme_client: Option<Arc<AcmeClient>>,
        challenge_tokens: ChallengeTokens,
        runtime: ServerRuntimeConfig,
    ) -> Result<Self, StateStoreError> {
        let app_manager = Arc::new(AppManager::new(data_dir.clone()));
        let load_balancer = Arc::new(LoadBalancer::new(app_manager.clone()));
        let device_key = load_or_create_device_key(&data_dir.join("secret.key"))?;
        let state_store = Arc::new(SqliteStateStore::new(
            data_dir.join("state.sqlite"),
            device_key,
        ));
        state_store.init()?;
        let server_mode = state_store.server_mode()?;
        if server_mode == UpgradeMode::Upgrading {
            state_store.set_server_mode(UpgradeMode::Normal)?;
            if let Some(owner) = state_store.upgrade_lock_owner()? {
                let _ = state_store.release_upgrade_lock(&owner);
            }
        }
        let server_mode = UpgradeMode::Normal;

        let workflows = Arc::new(crate::workflows::WorkflowManager::new(data_dir.clone()));
        {
            let state_store = state_store.clone();
            workflows.set_postgres_url_resolver(Arc::new(move |app| {
                state_store
                    .get_runtime_credentials(app)
                    .ok()
                    .and_then(|credentials| credentials.get("postgres_url").cloned())
            }));
        }

        // Server-side channel `.publish()` writes straight to the channel
        // store for the deployed app id via the shared internal socket.
        // Stores are opened lazily and cached so repeated publishes reuse
        // the same SQLite connection.
        {
            let data_dir = data_dir.clone();
            let state_store = state_store.clone();
            let stores: parking_lot::RwLock<HashMap<String, Arc<tako_channels::ChannelStore>>> =
                parking_lot::RwLock::new(HashMap::new());
            workflows.set_channel_publisher(std::sync::Arc::new(
                move |app: &str, channel: &str, payload: serde_json::Value| {
                    let typed: tako_channels::ChannelPublishPayload =
                        serde_json::from_value(payload)
                            .map_err(|e| format!("invalid payload: {e}"))?;

                    let store = if let Some(existing) = stores.read().get(app) {
                        existing.clone()
                    } else {
                        let mut guard = stores.write();
                        if let Some(existing) = guard.get(app) {
                            existing.clone()
                        } else {
                            let postgres_url = state_store
                                .get_runtime_credentials(app)
                                .ok()
                                .and_then(|credentials| credentials.get("postgres_url").cloned());
                            let config = crate::channels::app_channel_store_config_with_postgres(
                                &data_dir,
                                app,
                                postgres_url.as_deref(),
                            );
                            let opened = Arc::new(
                                tako_channels::ChannelStore::open_config(config)
                                    .map_err(|e| format!("open channel store: {e}"))?,
                            );
                            guard.insert(app.to_string(), opened.clone());
                            opened
                        }
                    };

                    store
                        .append(channel, &typed)
                        .map(|msg| serde_json::to_value(msg).unwrap_or(serde_json::Value::Null))
                        .map_err(|e| e.to_string())
                },
            ));
        }

        if tokio::runtime::Handle::try_current().is_ok() {
            workflows
                .start_socket()
                .map_err(|error| StateStoreError::InvalidData(error.to_string()))?;
        }

        Ok(Self {
            app_manager,
            load_balancer,
            cert_manager,
            acme_client: AsyncRwLock::new(acme_client),
            challenge_tokens,
            routes: Arc::new(SyncRwLock::new(RouteTable::default())),
            deploy_locks: AsyncRwLock::new(HashMap::new()),
            prepared_deploy_ssl: AsyncRwLock::new(HashMap::new()),
            cold_start: Arc::new(crate::scaling::ColdStartManager::new(
                crate::scaling::ColdStartConfig::default(),
            )),
            state_store,
            server_mode: AsyncRwLock::new(server_mode),
            runtime,
            workflows,
        })
    }

    pub(crate) fn app_manager(&self) -> Arc<AppManager> {
        self.app_manager.clone()
    }

    pub(crate) fn load_balancer(&self) -> Arc<LoadBalancer> {
        self.load_balancer.clone()
    }

    pub(crate) fn runtime_config(&self) -> &ServerRuntimeConfig {
        &self.runtime
    }

    pub fn cold_start(&self) -> Arc<crate::scaling::ColdStartManager> {
        self.cold_start.clone()
    }

    pub(crate) fn runtime_postgres_url(&self, app: &str) -> Option<String> {
        self.state_store
            .get_runtime_credentials(app)
            .ok()
            .and_then(|credentials| credentials.get("postgres_url").cloned())
    }

    pub async fn set_acme_client(&self, client: Arc<AcmeClient>) {
        *self.acme_client.write().await = Some(client);
    }

    pub(crate) async fn get_deploy_lock(&self, app_name: &str) -> Arc<tokio::sync::Mutex<()>> {
        let locks = self.deploy_locks.read().await;
        if let Some(lock) = locks.get(app_name) {
            return lock.clone();
        }
        drop(locks);

        let mut locks = self.deploy_locks.write().await;
        locks
            .entry(app_name.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }

    pub(crate) async fn stage_prepared_deploy_ssl(
        &self,
        app: &str,
        path: &std::path::Path,
        routes: Vec<String>,
        ssl: tako_core::SslBinding,
    ) {
        self.prepared_deploy_ssl.write().await.insert(
            PreparedDeployKey {
                app: app.to_string(),
                path: path.to_path_buf(),
            },
            PreparedDeploySsl { routes, ssl },
        );
    }

    pub(crate) async fn clear_prepared_deploy_ssl(&self, app: &str, path: &std::path::Path) {
        self.prepared_deploy_ssl
            .write()
            .await
            .remove(&PreparedDeployKey {
                app: app.to_string(),
                path: path.to_path_buf(),
            });
    }

    pub(crate) async fn resolve_deploy_ssl_binding(
        &self,
        app: &str,
        path: &std::path::Path,
        routes: &[String],
        ssl: tako_core::SslBinding,
    ) -> Result<tako_core::SslBinding, String> {
        if !ssl_binding_needs_cloudflare_token(ssl.provider, routes) {
            self.clear_prepared_deploy_ssl(app, path).await;
            return Ok(ssl);
        }

        if ssl
            .cloudflare_api_token
            .as_deref()
            .is_some_and(|token| !token.trim().is_empty())
        {
            self.clear_prepared_deploy_ssl(app, path).await;
            return Ok(ssl);
        }

        let prepared = self
            .prepared_deploy_ssl
            .write()
            .await
            .remove(&PreparedDeployKey {
                app: app.to_string(),
                path: path.to_path_buf(),
            })
            .ok_or_else(|| {
                "Prepared SSL credentials are missing. Re-run `tako deploy`.".to_string()
            })?;

        if prepared.routes != routes {
            return Err(
                "Prepared SSL credentials do not match the deploy routes. Re-run `tako deploy`."
                    .to_string(),
            );
        }
        if prepared.ssl.provider != ssl.provider {
            return Err(
                "Prepared SSL credentials do not match the deploy SSL provider. Re-run `tako deploy`."
                    .to_string(),
            );
        }
        if prepared
            .ssl
            .cloudflare_api_token
            .as_deref()
            .is_none_or(|token| token.trim().is_empty())
        {
            return Err(
                "Prepared SSL credentials did not include a Cloudflare API token. Re-run `tako deploy`."
                    .to_string(),
            );
        }

        Ok(prepared.ssl)
    }

    pub fn routes(&self) -> Arc<SyncRwLock<RouteTable>> {
        self.routes.clone()
    }

    pub async fn set_server_mode(&self, mode: UpgradeMode) -> Result<(), StateStoreError> {
        self.state_store.set_server_mode(mode)?;
        *self.server_mode.write().await = mode;
        Ok(())
    }

    pub async fn try_enter_upgrading(&self, owner: &str) -> Result<bool, StateStoreError> {
        if !self.state_store.try_acquire_upgrade_lock(owner)? {
            return Ok(false);
        }
        self.set_server_mode(UpgradeMode::Upgrading).await?;
        Ok(true)
    }

    pub async fn exit_upgrading(&self, owner: &str) -> Result<bool, StateStoreError> {
        if !self.state_store.release_upgrade_lock(owner)? {
            return Ok(false);
        }
        self.set_server_mode(UpgradeMode::Normal).await?;
        Ok(true)
    }

    pub(crate) fn ensure_internal_socket_started(&self) -> Result<(), StateStoreError> {
        self.workflows
            .start_socket()
            .map_err(|error| StateStoreError::InvalidData(error.to_string()))
    }

    pub(crate) async fn shutdown_runtime(&self, workflow_drain_timeout: Duration) {
        tracing::info!("Shutting down managed app runtime");
        self.workflows.shutdown_all(workflow_drain_timeout).await;
        self.app_manager.shutdown_all().await;
    }

    pub(crate) async fn reject_mutating_when_upgrading(&self, command: &str) -> Option<Response> {
        let mode = *self.server_mode.read().await;
        if mode == UpgradeMode::Upgrading {
            return Some(Response::error(format!(
                "Server is upgrading; '{}' is temporarily blocked. Please retry shortly.",
                command
            )));
        }
        None
    }

    pub async fn runtime_info(&self) -> ServerRuntimeInfo {
        let mode = *self.server_mode.read().await;
        self.runtime.to_runtime_info(mode)
    }

    /// Reconcile workflow + channel runtime support for the active release.
    ///
    /// Deploys, restores, and secret rotations all funnel through here so the
    /// worker runtime follows the current release and its current secrets. If a
    /// new release drops the configured workflows directory, any previously
    /// managed worker is drained and removed.
    pub(crate) async fn sync_app_workflows(
        &self,
        app_name: &str,
        release_path: &std::path::Path,
        runtime_bin_path: Option<&str>,
    ) {
        let manifest = match crate::app_command::load_release_manifest(release_path) {
            Ok(manifest) => manifest,
            Err(e) => {
                tracing::warn!(app = app_name, error = %e, "Skipping workflow engine: could not load release manifest");
                return;
            }
        };
        let app_path = match crate::app_command::safe_subdir(release_path, &manifest.app_dir) {
            Ok(path) => path,
            Err(e) => {
                tracing::warn!(app = app_name, error = %e, "Skipping workflow engine: invalid app_dir in manifest");
                return;
            }
        };
        let data_paths = match ensure_app_runtime_data_dirs(&self.runtime.data_dir, app_name) {
            Ok(paths) => paths,
            Err(error) => {
                tracing::warn!(app = app_name, error = %error, "Skipping workflow engine: failed to prepare app data dirs");
                return;
            }
        };
        if let Err(e) = self.workflows.start_socket() {
            tracing::warn!(error = %e, "Failed to start internal socket");
            return;
        }
        let internal_socket = self.workflows.socket_path();

        let secrets = self.state_store.get_secrets(app_name).unwrap_or_default();
        let storages = self.state_store.get_storages(app_name).unwrap_or_default();
        let mut worker_env = manifest.env_vars.clone();
        inject_app_data_dir_env(&mut worker_env, &data_paths);

        let worker_command = match manifest.runtime.as_str() {
            "container" => {
                let Some(run) = manifest.workflow_run.as_deref() else {
                    self.workflows.retire(app_name).await;
                    return;
                };
                let engine = match detect_container_engine() {
                    Ok(engine) => engine,
                    Err(error) => {
                        tracing::warn!(app = app_name, error = %error, "Skipping workflow engine: container engine not available");
                        self.workflows.retire(app_name).await;
                        return;
                    }
                };
                let image = match image_tag_for_manifest(&manifest) {
                    Ok(image) => image,
                    Err(error) => {
                        tracing::warn!(app = app_name, error = %error, "Skipping workflow engine: invalid container image tag");
                        self.workflows.retire(app_name).await;
                        return;
                    }
                };
                worker_env.insert(
                    tako_core::instance_env::TAKO_APP_NAME_ENV.to_string(),
                    app_name.to_string(),
                );
                worker_env.insert(
                    tako_core::instance_env::TAKO_INTERNAL_SOCKET_ENV.to_string(),
                    internal_socket.display().to_string(),
                );
                let token = nanoid::nanoid!(32);
                let container_env = build_container_run_env(
                    &worker_env,
                    &token,
                    &secrets,
                    &storages,
                    crate::container_runtime::DEFAULT_CONTAINER_PORT,
                )
                .into_iter()
                .collect();
                let args = match build_container_workflow_run_args(
                    &image,
                    &container_env,
                    &token,
                    &secrets,
                    &storages,
                    run,
                    &internal_socket,
                ) {
                    Ok(args) => args,
                    Err(error) => {
                        tracing::warn!(app = app_name, error = %error, "Skipping workflow engine: invalid container workflow run");
                        self.workflows.retire(app_name).await;
                        return;
                    }
                };
                worker_env = container_env;
                std::iter::once(std::ffi::OsString::from(engine.binary()))
                    .chain(args.into_iter().map(std::ffi::OsString::from))
                    .collect()
            }
            "go" => {
                let Some(worker_main) = manifest.workflow_worker_main.as_deref() else {
                    self.workflows.retire(app_name).await;
                    return;
                };
                let worker_bin = app_path.join(worker_main);
                if !worker_bin.exists() {
                    tracing::warn!(
                        app = app_name,
                        path = %worker_bin.display(),
                        "Skipping workflow engine: Go worker binary not found in release"
                    );
                    self.workflows.retire(app_name).await;
                    return;
                }
                match runtime_bin_path {
                    Some(bin) => vec![std::ffi::OsString::from(bin), worker_bin.into()],
                    None => vec![worker_bin.into()],
                }
            }
            _ => {
                let js_app_root = worker_env
                    .get("TAKO_APP_ROOT")
                    .map(String::as_str)
                    .filter(|root| !root.trim().is_empty())
                    .unwrap_or("src");
                let workflows_dir = if js_app_root == "." {
                    app_path.join("workflows")
                } else {
                    app_path.join(js_app_root).join("workflows")
                };
                if !workflows_dir.is_dir() {
                    self.workflows.retire(app_name).await;
                    return;
                }

                let worker_entry = app_path
                    .join("node_modules")
                    .join("tako.sh")
                    .join("dist")
                    .join("entrypoints")
                    .join("bun-worker.mjs");
                if !worker_entry.exists() {
                    tracing::warn!(
                        app = app_name,
                        path = %worker_entry.display(),
                        "Skipping workflow engine: worker entrypoint not found in release"
                    );
                    self.workflows.retire(app_name).await;
                    return;
                }

                let runtime_bin = runtime_bin_path
                    .map(std::path::PathBuf::from)
                    .unwrap_or_else(|| std::path::PathBuf::from("bun"));
                vec![runtime_bin.into(), worker_entry.into()]
            }
        };
        let isolation = if manifest.runtime == "container" {
            None
        } else {
            match crate::isolation::app_process_isolation(&self.runtime.data_dir, app_name) {
                Ok(isolation) => Some(isolation),
                Err(error) => {
                    tracing::warn!(app = app_name, error = %error, "Skipping workflow engine: failed to prepare app isolation");
                    return;
                }
            }
        };

        let app = app_name.to_string();
        let app_for_spec = app.clone();
        let worker_cwd = app_path;
        let manager = self.workflows.clone();
        let result = manager
            .ensure(&app, move |_db_path| {
                crate::workflows::worker_spec_for_command(
                    &app_for_spec,
                    0,       // workers (scale-to-zero)
                    500,     // concurrency
                    300_000, // idle_timeout_ms (5 min)
                    &internal_socket,
                    worker_command,
                    &worker_cwd,
                    worker_env,
                    secrets,
                    storages,
                    isolation,
                )
            })
            .await;

        if let Err(e) = result {
            tracing::warn!(
                app = app_name,
                error = %e,
                "Failed to bring up workflow engine"
            );
        }
    }

    pub async fn restore_from_state_store(&self) -> Result<(), StateStoreError> {
        let apps = self.state_store.load_apps()?;
        if apps.is_empty() {
            return Ok(());
        }

        tracing::info!(apps = apps.len(), "Restoring apps from durable state");

        for persisted in apps {
            let mut config = persisted.config.clone();
            let app_name = config.deployment_id();
            let routes = persisted.routes.clone();

            if self.runtime.standby && config.min_instances > 1 {
                config.min_instances = 1;
                config.max_instances = config.max_instances.max(1);
            }
            if let Some((requested_instances, max_instances)) =
                clamp_instances_to_limit(&mut config)
            {
                tracing::warn!(
                    app = %app_name,
                    requested_instances,
                    max_instances,
                    "Clamped restored app instance count to server maximum"
                );
            }

            let should_start = config.min_instances > 0;
            let release_path = release_app_path(&self.runtime.data_dir, &config);
            if let Err(error) =
                apply_release_runtime_to_config(&mut config, release_path.clone(), None)
            {
                tracing::error!(app = %app_name, "Failed to restore app config: {}", error);
                continue;
            }
            match ensure_app_runtime_data_dirs(&self.runtime.data_dir, &app_name) {
                Ok(paths) => inject_app_data_dir_env(&mut config.env_vars, &paths),
                Err(error) => {
                    tracing::error!(app = %app_name, "Failed to prepare app data dirs: {}", error);
                    continue;
                }
            }
            config.secrets = self.state_store.get_secrets(&app_name).unwrap_or_else(|e| {
                tracing::warn!(app = %app_name, "Failed to read secrets: {}", e);
                HashMap::new()
            });
            config.storages = self
                .state_store
                .get_storages(&app_name)
                .unwrap_or_else(|e| {
                    tracing::warn!(app = %app_name, "Failed to read storages: {}", e);
                    HashMap::new()
                });

            let app = self.app_manager.register_app(config.clone());
            self.load_balancer.register_app(app.clone());

            {
                let mut route_table = self.routes.write();
                route_table.set_app_routes_with_source_ip(
                    app_name.clone(),
                    routes,
                    config.source_ip,
                );
            }

            let runtime_bin_path =
                resolve_release_runtime_bin(&release_path, &self.runtime.data_dir)
                    .await
                    .ok()
                    .flatten();
            self.sync_app_workflows(&app_name, &release_path, runtime_bin_path.as_deref())
                .await;

            if should_start {
                match self.app_manager.start_app(&app_name).await {
                    Ok(()) => {
                        app.set_state(AppState::Running);
                        tracing::info!(app = %app_name, "Restored and started app");
                    }
                    Err(e) => {
                        app.set_state(AppState::Error);
                        app.set_last_error(format!("Restore startup failed: {}", e));
                        tracing::error!(app = %app_name, "Failed to start restored app: {}", e);
                    }
                }
            } else {
                app.set_state(AppState::Idle);
                self.cold_start.reset(&app_name);
                tracing::info!(app = %app_name, "Restored on-demand app in idle state");
            }
        }

        Ok(())
    }

    pub async fn persist_app_state(&self, app_name: &str) {
        let Some(app) = self.app_manager.get_app(app_name) else {
            return;
        };
        let config = app.config.read().clone();
        let routes = {
            let route_table = self.routes.read();
            route_table.routes_for_app(app_name)
        };
        if let Err(e) = self.state_store.upsert_app(&config, &routes) {
            tracing::warn!(app = app_name, "Failed to persist app state: {}", e);
        }
    }
}

pub(crate) fn ssl_binding_needs_cloudflare_token(
    provider: tako_core::SslProvider,
    routes: &[String],
) -> bool {
    provider == tako_core::SslProvider::Cloudflare
        || (provider == tako_core::SslProvider::LetsEncrypt
            && routes.iter().any(|route| route.starts_with("*.")))
}
