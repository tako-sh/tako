mod certificates;
mod recovery;
mod startup;
mod workflows;

pub(crate) use certificates::ssl_binding_needs_cloudflare_token;
pub(crate) use startup::native_channel_client;

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
use std::sync::atomic::{AtomicBool, Ordering};
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
            arch: std::env::consts::ARCH.into(),
            libc: if cfg!(target_env = "gnu") {
                "glibc"
            } else if cfg!(target_env = "musl") {
                "musl"
            } else {
                "unsupported"
            }
            .into(),
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
    pub(crate) management_lock: Arc<tokio::sync::Mutex<()>>,
    /// Drain holds this for writing while it stops instances. Cold starts and
    /// health replacements hold it for reading while they spawn, so an instance
    /// started concurrently with a drain is still stopped by it. Deployments do
    /// not take it: the management lock must not stall other apps' spawns.
    pub(crate) spawn_gate: Arc<tokio::sync::RwLock<()>>,
    /// Bumped whenever an app's runtime credentials change, so the proxy
    /// re-resolves channel backends only then instead of on every request.
    pub(crate) channel_credentials_generation: Arc<std::sync::atomic::AtomicU64>,
    /// Channel definitions per app build, shared by the proxy and the
    /// server-side publisher.
    pub(crate) channel_registry: Arc<crate::channels::registry::ChannelRegistry>,
    pub(crate) deploy_locks: AsyncRwLock<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    pub(crate) backup_locks: SyncRwLock<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// Latest hourly database snapshot attempt per app. Not persisted: the
    /// remote index keeps the cadence across restarts.
    pub(crate) database_snapshots:
        SyncRwLock<HashMap<String, crate::operations::DatabaseSnapshotState>>,
    prepared_deploy_ssl: AsyncRwLock<HashMap<PreparedDeployKey, PreparedDeploySsl>>,
    pub(crate) cold_start: Arc<crate::scaling::ColdStartManager>,
    pub(crate) state_store: Arc<SqliteStateStore>,
    pub(crate) server_mode: AsyncRwLock<UpgradeMode>,
    pub(crate) drain_requested: Arc<AtomicBool>,
    pub(crate) drain_activity: parking_lot::Mutex<Option<crate::state_store::DrainLease>>,
    pub(crate) drain_observer_started: AtomicBool,
    pub(crate) proxy_requests: Arc<crate::request_drain::RequestDrain>,
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
    pub(crate) fn app_manager(&self) -> Arc<AppManager> {
        self.app_manager.clone()
    }

    pub fn is_draining(&self) -> bool {
        match self.state_store.drain_requested() {
            Ok(true) => {
                self.drain_requested.store(true, Ordering::Release);
                true
            }
            Ok(false) => self.drain_requested.load(Ordering::Acquire),
            Err(error) => {
                // Fail closed for this caller only. Flipping the shared gate on a
                // transient read error would make the observer treat the unpersisted
                // in-memory drain as a resume and restart every application.
                tracing::warn!(%error, "Could not read persisted server drain state");
                true
            }
        }
    }

    pub fn drain_flag(&self) -> Arc<AtomicBool> {
        self.drain_requested.clone()
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

    pub(crate) fn runtime_postgres_url(&self, app: &str) -> Result<Option<String>, String> {
        self.state_store
            .get_runtime_credentials(app)
            .map(|credentials| credentials.get("postgres_url").cloned())
            .map_err(|_| "channel credentials unavailable".to_string())
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
}
