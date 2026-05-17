//! Tako server protocol types for management socket communication
//!
//! These types are shared between the CLI and tako-server for
//! communication via the Unix management socket.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};

use crate::storage::StorageBinding;

pub const PROTOCOL_VERSION: u32 = 0;
pub const MANAGEMENT_AUTH_NAMESPACE: &str = "tako-management-rpc-v0";
const DEPLOYMENT_APP_ID_SEPARATOR: char = '/';

pub fn management_auth_message(timestamp: &str, nonce: &str, body: &[u8]) -> Vec<u8> {
    let mut message = Vec::with_capacity(
        MANAGEMENT_AUTH_NAMESPACE.len() + timestamp.len() + nonce.len() + body.len() + 4,
    );
    message.extend_from_slice(MANAGEMENT_AUTH_NAMESPACE.as_bytes());
    message.push(b'\n');
    message.extend_from_slice(timestamp.as_bytes());
    message.push(b'\n');
    message.extend_from_slice(nonce.as_bytes());
    message.push(b'\n');
    message.extend_from_slice(body);
    message
}

pub fn deployment_app_id(app_name: &str, env_name: &str) -> String {
    format!("{app_name}{DEPLOYMENT_APP_ID_SEPARATOR}{env_name}")
}

pub fn split_deployment_app_id(app_id: &str) -> Option<(&str, &str)> {
    let (app_name, env_name) = app_id.split_once(DEPLOYMENT_APP_ID_SEPARATOR)?;
    if app_name.is_empty() || env_name.is_empty() || env_name.contains(DEPLOYMENT_APP_ID_SEPARATOR)
    {
        return None;
    }
    Some((app_name, env_name))
}

pub fn deployment_app_id_filename(app_id: &str) -> String {
    app_id.replace(DEPLOYMENT_APP_ID_SEPARATOR, "%2F")
}

/// Commands that can be sent to the server
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum Command {
    /// Query protocol version and supported capabilities.
    Hello { protocol_version: u32 },

    /// Download runtime and install production dependencies for a release.
    /// Called before `Deploy` so that the deploy step only does app registration
    /// and instance startup, keeping it fast.
    PrepareRelease { app: String, path: String },

    /// Run a one-shot release command on this server for the given
    /// release. Used during deploy to run migrations / cache invalidation
    /// before any rolling update starts. The CLI sends this only to the
    /// leader server when a `release` command is configured.
    RunRelease {
        app: String,
        version: String,
        path: String,
        /// Shell command line; runs as `sh -c "<command_line>"`.
        command_line: String,
        /// Non-secret env vars (matches the env that HTTP instances receive).
        #[serde(default)]
        vars: HashMap<String, String>,
        /// Decrypted secrets, injected as env vars for the duration of
        /// this one-shot. Long-running app processes still receive secrets
        /// via fd 3; release commands are short-lived and use env vars
        /// instead.
        #[serde(default)]
        secrets: HashMap<String, String>,
    },

    /// Deploy a new version of an app
    Deploy {
        app: String,
        version: String,
        path: String,
        /// Route patterns for this app (host, wildcard, optional path).
        routes: Vec<String>,

        /// How the proxy derives the client IP for requests to this app.
        /// Defaults to automatic Cloudflare detection with direct peer fallback.
        #[serde(default)]
        source_ip: SourceIpMode,

        /// Secret environment variables injected into app processes at spawn time.
        /// Non-secret env vars are read by the server from app.json in the release dir.
        /// When `None`, the server keeps existing secrets for this app.
        #[serde(default)]
        secrets: Option<HashMap<String, String>>,
        /// Storage bindings injected into app processes at spawn time.
        /// When `None`, the server keeps existing storage bindings for this app.
        #[serde(default)]
        storages: Option<HashMap<String, StorageBinding>>,
        /// DNS credentials used for app route wildcard certificate issuance.
        /// When `None`, app DNS credentials are cleared.
        #[serde(default)]
        dns: Option<DnsBinding>,
    },

    /// Update the desired minimum number of instances for an app.
    Scale { app: String, instances: u8 },

    /// Stop an app
    Stop { app: String },

    /// Delete an app from runtime state
    Delete { app: String },

    /// Get status of an app
    Status { app: String },

    /// List all apps
    List,

    /// List release/build history for an app
    ListReleases { app: String },

    /// Roll back an app to a previously deployed release/build
    Rollback { app: String, version: String },

    /// List all configured routes (all apps)
    Routes,

    /// Update secrets for an app
    UpdateSecrets {
        app: String,
        secrets: HashMap<String, String>,
    },

    /// Get the SHA-256 hash of an app's current secrets
    GetSecretsHash { app: String },

    /// Get server runtime information (ports, data dir, upgrade mode).
    ServerInfo,

    /// Enter upgrading mode with a durable lock owner.
    EnterUpgrading { owner: String },

    /// Exit upgrading mode for the lock owner.
    ExitUpgrading { owner: String },

    /// Inject an ACME challenge token (for testing HTTP-01 challenge serving).
    InjectChallengeToken {
        token: String,
        key_authorization: String,
    },

    /// Enqueue a run of the named workflow.
    ///
    /// The server inserts a row into `{data_dir}/apps/{app}/runs.db` and the
    /// worker process polls it up within ~1s. If `unique_key` collides with
    /// an existing non-terminal run, this is a no-op and the existing run id
    /// is returned.
    EnqueueRun {
        app: String,
        name: String,
        payload: serde_json::Value,
        #[serde(default)]
        opts: EnqueueOpts,
    },

    /// Register (or replace) the full set of cron schedules for an app.
    ///
    /// The worker sends this on startup; the server writes rows into
    /// `schedules` and the cron ticker reads from there. Re-sending is
    /// idempotent — any schedule not present in the new list is removed.
    RegisterSchedules {
        app: String,
        schedules: Vec<ScheduleSpec>,
    },

    /// Worker: atomically claim the oldest eligible run for one of `names`.
    /// Bumps `attempts`. Returns null when nothing is due. `app` selects
    /// which app's queue this worker is claiming from.
    ClaimRun {
        app: String,
        worker_id: String,
        names: Vec<String>,
        lease_ms: u64,
    },

    /// Worker: extend the lease on a running run. Requires the original
    /// `worker_id` so a stale worker (past its lease) can't heartbeat a
    /// run another worker has since claimed.
    HeartbeatRun {
        app: String,
        id: String,
        worker_id: String,
        lease_ms: u64,
    },

    /// Worker: persist a single completed step's result. First-write-wins
    /// on `(run_id, step_name)` — duplicate saves are ignored so a retried
    /// RPC after a successful insert doesn't overwrite.
    SaveStep {
        app: String,
        id: String,
        worker_id: String,
        step_name: String,
        result: serde_json::Value,
    },

    /// Worker: mark a run succeeded. Guarded by `worker_id` so a stale
    /// worker can't silently mark a run succeeded after its lease expired.
    CompleteRun {
        app: String,
        id: String,
        worker_id: String,
    },

    /// Worker: cancel a run cleanly (status becomes `cancelled`). Triggered
    /// by the user via `ctx.bail(reason?)`.
    CancelRun {
        app: String,
        id: String,
        worker_id: String,
        #[serde(default)]
        reason: Option<String>,
    },

    /// Worker: record a failure. When `finalize = true` the run becomes
    /// dead; otherwise it goes back to pending with `next_run_at_ms` as the
    /// new `run_at`.
    FailRun {
        app: String,
        id: String,
        worker_id: String,
        error: String,
        #[serde(default)]
        next_run_at_ms: Option<i64>,
        finalize: bool,
    },

    /// Worker: park the run for later resumption (durable `ctx.sleep` /
    /// `ctx.waitFor`). When `wake_at_ms` is None the run is parked until a
    /// matching `Signal` arrives. Does not consume retry budget.
    DeferRun {
        app: String,
        id: String,
        worker_id: String,
        #[serde(default)]
        wake_at_ms: Option<i64>,
    },

    /// Worker: park a run waiting for a named event. Resumed by `Signal`.
    WaitForEvent {
        app: String,
        id: String,
        worker_id: String,
        step_name: String,
        event_name: String,
        #[serde(default)]
        timeout_at_ms: Option<i64>,
    },

    /// Anyone: deliver an event payload. Wakes every parked waiter with
    /// matching `event_name`. The payload is materialized as the step
    /// result for the waiter's `step_name`.
    Signal {
        app: String,
        event_name: String,
        payload: serde_json::Value,
    },

    /// Server-side channel publish. Used by app/workflow code inside a
    /// Tako-managed process — goes straight to the channel store instead
    /// of round-tripping through the HTTPS proxy. `payload` mirrors the
    /// `ChannelPublishPayload` wire shape (`{ type, data }`).
    ChannelPublish {
        app: String,
        channel: String,
        payload: serde_json::Value,
    },
}

/// A single cron schedule for a workflow.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ScheduleSpec {
    /// Workflow name. Must match a registered handler.
    pub name: String,
    /// Cron expression. 6-field format (sec min hour day month dayofweek) per
    /// the `cron` crate, or 5-field (the worker SDK normalizes).
    pub cron: String,
}

/// Options for `Command::EnqueueRun`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct EnqueueOpts {
    /// Unix-ms timestamp to run at. Defaults to now.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_at_ms: Option<i64>,

    /// Max attempts (inclusive of the first). Defaults to 3.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_attempts: Option<u32>,

    /// Deduplication key. If another non-terminal task with this key exists,
    /// enqueue is a no-op and the existing task id is returned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unique_key: Option<String>,
}

/// Response payload for `Command::EnqueueRun`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EnqueueRunResponse {
    /// The run id (newly-created or existing if the request was a dedup hit).
    pub id: String,
    /// True when the request collapsed onto a pre-existing run via unique_key.
    pub deduplicated: bool,
}

/// Response payload for `Command::ClaimRun`. `None` when nothing is due.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RunPayload {
    pub id: String,
    pub name: String,
    pub payload: serde_json::Value,
    /// pending | running | succeeded | cancelled | dead
    pub status: String,
    pub attempts: u32,
    pub max_attempts: u32,
    pub run_at_ms: i64,
    pub step_state: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HelloResponse {
    pub protocol_version: u32,
    pub server_version: String,
    pub capabilities: Vec<String>,
    #[serde(default)]
    pub server_identity: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UpgradeMode {
    Normal,
    Upgrading,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[derive(Default)]
pub enum SourceIpMode {
    #[default]
    Auto,
    Direct,
    CloudflareProxy,
    TrustedProxy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DnsProvider {
    Cloudflare,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DnsBinding {
    pub provider: DnsProvider,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cloudflare_api_token: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerRuntimeInfo {
    pub pid: u32,
    pub mode: UpgradeMode,
    #[serde(default)]
    pub process_started_at_unix_secs: Option<i64>,
    pub socket: String,
    pub data_dir: String,
    pub http_port: u16,
    pub https_port: u16,
    pub no_acme: bool,
    pub acme_staging: bool,
    #[serde(default)]
    pub acme_email: Option<String>,
    pub renewal_interval_hours: u64,
    #[serde(default, alias = "worker")]
    pub standby: bool,
    #[serde(default)]
    pub metrics_port: Option<u16>,
    #[serde(default)]
    pub server_name: Option<String>,
    #[serde(default)]
    pub server_identity: Option<String>,
}

/// Response from the server
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Response {
    /// Command succeeded
    Ok { data: serde_json::Value },

    /// Command failed
    Error { message: String },
}

impl Response {
    pub fn ok(data: impl Serialize) -> Self {
        Self::Ok {
            data: serde_json::to_value(data).expect("Response::ok data must serialize"),
        }
    }

    pub fn error(message: impl Into<String>) -> Self {
        Self::Error {
            message: message.into(),
        }
    }

    /// Check if response is Ok
    pub fn is_ok(&self) -> bool {
        matches!(self, Self::Ok { .. })
    }

    /// Get data from Ok response
    pub fn data(&self) -> Option<&serde_json::Value> {
        match self {
            Self::Ok { data } => Some(data),
            Self::Error { .. } => None,
        }
    }

    /// Get error message from Error response
    pub fn error_message(&self) -> Option<&str> {
        match self {
            Self::Ok { .. } => None,
            Self::Error { message } => Some(message),
        }
    }
}

/// App status information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppStatus {
    pub name: String,
    pub version: String,
    pub instances: Vec<InstanceStatus>,
    #[serde(default)]
    pub builds: Vec<BuildStatus>,
    pub state: AppState,

    pub last_error: Option<String>,
}

/// Runtime status for a specific build/version of an app.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuildStatus {
    pub version: String,
    pub state: AppState,
    pub instances: Vec<InstanceStatus>,
}

/// Instance status information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstanceStatus {
    pub id: String,
    pub state: InstanceState,
    pub pid: Option<u32>,
    pub uptime_secs: u64,
    pub requests_total: u64,
}

/// App state
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AppState {
    Running,
    Idle,
    Deploying,
    Stopped,
    Error,
}

impl std::fmt::Display for AppState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AppState::Running => write!(f, "running"),
            AppState::Idle => write!(f, "idle"),
            AppState::Deploying => write!(f, "deploying"),
            AppState::Stopped => write!(f, "stopped"),
            AppState::Error => write!(f, "error"),
        }
    }
}

/// Instance state
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InstanceState {
    Starting,
    Ready,
    Healthy,
    Unhealthy,
    Draining,
    Stopped,
}

impl std::fmt::Display for InstanceState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            InstanceState::Starting => write!(f, "starting"),
            InstanceState::Ready => write!(f, "ready"),
            InstanceState::Healthy => write!(f, "healthy"),
            InstanceState::Unhealthy => write!(f, "unhealthy"),
            InstanceState::Draining => write!(f, "draining"),
            InstanceState::Stopped => write!(f, "stopped"),
        }
    }
}

/// Server list response - list of app statuses
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListResponse {
    pub apps: Vec<AppStatus>,
}

/// Release/build metadata for an app.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReleaseInfo {
    pub version: String,
    pub current: bool,
    pub deployed_at_unix_secs: Option<i64>,
    #[serde(default)]
    pub commit_message: Option<String>,
    #[serde(default)]
    pub git_dirty: Option<bool>,
}

/// Response payload for `list_releases`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ListReleasesResponse {
    pub app: String,
    pub releases: Vec<ReleaseInfo>,
}

/// Compute a stable SHA-256 hash of a secrets map.
///
/// The hash is computed over sorted key-value pairs to ensure deterministic
/// output regardless of HashMap iteration order. Returns a hex-encoded digest.
/// An empty map produces a distinct hash (the SHA-256 of an empty input).
pub fn compute_secrets_hash(secrets: &HashMap<String, String>) -> String {
    let sorted: BTreeMap<&String, &String> = secrets.iter().collect();
    let mut hasher = Sha256::new();
    for (key, value) in &sorted {
        // Length-prefix each field to prevent key/value boundary collisions:
        // e.g. {"A=B":"C"} and {"A":"B=C"} must produce different hashes.
        hasher.update((key.len() as u64).to_le_bytes());
        hasher.update(key.as_bytes());
        hasher.update((value.len() as u64).to_le_bytes());
        hasher.update(value.as_bytes());
    }
    format!("{:x}", hasher.finalize())
}

#[cfg(test)]
mod tests;
