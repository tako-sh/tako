//! Tako server protocol types for management socket communication
//!
//! These types are shared between the CLI and tako-server for
//! communication via the Unix management socket.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};

use crate::storage::StorageBinding;

pub const PROTOCOL_VERSION: u32 = 0;
pub const UPGRADE_RELOAD_MARKER_FILE: &str = ".upgrade-reload";
pub const MANAGEMENT_AUTH_NAMESPACE: &str = "tako-management-rpc-v0";
pub const DEFAULT_BACKUP_RETENTION_DAYS: u16 = 30;
pub const BACKUP_INTERVAL_SECS: i64 = 24 * 60 * 60;
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

pub fn release_artifact_upload_auth_body(
    app: &str,
    version: &str,
    size_bytes: u64,
    sha256_hex: &str,
) -> Vec<u8> {
    format!("release_artifact_upload\n{app}\n{version}\n{size_bytes}\n{sha256_hex}").into_bytes()
}

pub fn logs_request_auth_body(
    app: &str,
    previous_offset: u64,
    current_offset: u64,
    since_unix_secs: Option<i64>,
    max_bytes: usize,
) -> Vec<u8> {
    let since = since_unix_secs
        .map(|value| value.to_string())
        .unwrap_or_default();
    format!("logs\n{app}\n{previous_offset}\n{current_offset}\n{since}\n{max_bytes}").into_bytes()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReleaseUploadPlan {
    pub path: String,
    pub upload_required: bool,
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
    PrepareRelease {
        app: String,
        path: String,
        /// This deployment's secrets, used for preparation without persisting them.
        secrets: HashMap<String, String>,
        #[serde(default)]
        force: bool,
    },

    /// Plan a release artifact upload. Returns the server-side release path and
    /// whether the artifact still needs to be uploaded.
    PrepareReleaseUpload { app: String, version: String },

    /// Validate and stage deploy metadata that the server must use before the
    /// final deploy command. This keeps provider credentials on the server side
    /// after validation, so the final deploy command can avoid resending them.
    PrepareDeploy {
        app: String,
        version: String,
        path: String,
        /// Route patterns for this app (host, wildcard, optional path).
        routes: Vec<String>,
        /// SSL certificate provider and optional provider credentials.
        #[serde(default)]
        ssl: SslBinding,
        #[serde(default)]
        force: bool,
    },

    /// Clear staged deploy metadata without deleting the release artifact.
    CleanupPreparedDeploy { app: String, version: String },

    /// Remove a partially uploaded/extracted release.
    CleanupRelease { app: String, version: String },

    /// Point the app's current symlink at a successfully deployed release and
    /// prune old releases.
    FinalizeRelease { app: String, version: String },

    /// Check that the server has enough free space for deploy work.
    CheckDeploySpace { min_free_bytes: u64 },

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
        #[serde(default)]
        force: bool,
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
        /// Tako runtime/provider credentials used by the server only. These
        /// are not injected into app processes.
        #[serde(default)]
        runtime_credentials: Option<HashMap<String, String>>,
        /// Storage bindings injected into app processes at spawn time.
        /// When `None`, the server keeps existing storage bindings for this app.
        #[serde(default)]
        storages: Option<HashMap<String, StorageBinding>>,
        /// SSL certificate provider and optional provider credentials.
        /// Defaults to Let's Encrypt when omitted.
        #[serde(default)]
        ssl: SslBinding,
        /// Private backup target for this app/environment. When absent, any
        /// existing backup configuration for the app is cleared.
        #[serde(default)]
        backup: Option<Box<BackupBinding>>,
        #[serde(default)]
        force: bool,
    },

    /// Create a backup for the app immediately. When backup is provided, the
    /// server stores it before running the backup.
    BackupNow {
        app: String,
        #[serde(default)]
        backup: Option<Box<BackupBinding>>,
    },

    /// List backups recorded in the app's remote backup index.
    ListBackups { app: String },

    /// Get backup configuration and latest backup status for an app.
    BackupStatus { app: String },

    /// Create a short-lived download URL for a backup archive.
    BackupDownloadUrl { app: String, backup_id: String },

    /// Restore a full backup, or the SQLite databases of a database snapshot, on this server.
    RestoreBackup { app: String, backup_id: String },

    /// Update the desired minimum number of instances for an app.
    Scale {
        app: String,
        instances: Option<u8>,
        #[serde(default)]
        service: Option<String>,
        #[serde(default)]
        reset: bool,
    },

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

    /// Persist drain intent, reject new app traffic, and stop local work within a deadline.
    DrainServer { timeout_seconds: u64 },

    /// Restore persisted apps and workflows before accepting application traffic again.
    ResumeServer,

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
    /// The server inserts a row into the app's workflow store and the worker
    /// process claims it when runnable. A supplied `id` identifies the run.
    /// Repeating it with the same workflow and payload returns the retained run;
    /// a different workflow or payload is a conflict.
    EnqueueRun {
        app: String,
        name: String,
        payload: serde_json::Value,
        #[serde(default)]
        opts: EnqueueOpts,
    },

    /// Read the retained outcome of a run owned by the named workflow.
    GetRunResult {
        app: String,
        name: String,
        id: String,
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

    /// Worker: extend the lease on a running run. Requires the claim
    /// owner returned by ClaimRun so a stale attempt cannot heartbeat a
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
        result: serde_json::Value,
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
    /// Authenticated app-to-app call. `app` is the caller, never the destination.
    NetworkCall {
        app: String,
        address: String,
        request: crate::network::NetworkRequest,
    },

    /// Operator: list an app's workflow runs on this server's store, newest
    /// first. `limit` defaults to [`WORKFLOW_RUNS_DEFAULT_LIMIT`] and must be
    /// between 1 and [`WORKFLOW_RUNS_MAX_LIMIT`].
    ListWorkflowRuns {
        app: String,
        #[serde(default)]
        status: Option<WorkflowRunStatus>,
        /// Only runs of this workflow.
        #[serde(default)]
        name: Option<String>,
        #[serde(default)]
        limit: Option<u32>,
    },

    /// Operator: read one run with its saved steps. Large values are capped.
    GetWorkflowRun { app: String, id: String },

    /// Operator: return a `dead` or `cancelled` run to `pending`, due now,
    /// with a fresh retry budget. Saved steps are kept.
    RetryWorkflowRun { app: String, id: String },

    /// Operator: cancel a `pending` or `running` run. Unlike the worker's
    /// `CancelRun`, it carries no claim owner: a running attempt is fenced
    /// and its later writes are rejected as stale.
    CancelWorkflowRun { app: String, id: String },
}

/// A single cron schedule for a workflow.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ScheduleSpec {
    /// Workflow name. Must match a registered handler.
    pub name: String,
    /// Cron expression. 6-field format (sec min hour day month dayofweek) per
    /// the `cron` crate, or 5-field (the worker SDK normalizes).
    pub cron: String,
    /// The workflow's attempt budget (inclusive of the first) for the runs
    /// this schedule creates. Defaults to 3.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_attempts: Option<u32>,
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

    /// Caller-supplied run ID, unique within the app and environment.
    /// A retained run with the same ID, workflow, and payload is reused in any status.
    /// Omit to generate an ID. IDs must contain 1–255 bytes and no NUL characters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
}

/// Response payload for `Command::EnqueueRun`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EnqueueRunResponse {
    /// The run id (newly-created or existing if the request was a dedup hit).
    pub id: String,
    /// True when the supplied ID matched a retained run.
    pub deduplicated: bool,
}

/// Retained workflow outcome; null at the command level means no matching run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunResult {
    pub status: String,
    pub result: serde_json::Value,
    pub error: Option<String>,
}

/// Response payload for `Command::ClaimRun`. `None` when nothing is due.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RunPayload {
    pub id: String,
    /// Opaque ownership identifier for this claim. Echo this as `worker_id`
    /// on every mutation; it is not the process ID supplied to ClaimRun.
    pub worker_id: String,
    pub name: String,
    pub payload: serde_json::Value,
    /// pending | running | succeeded | cancelled | dead
    pub status: String,
    pub attempts: u32,
    pub max_attempts: u32,
    pub run_at_ms: i64,
    pub step_state: serde_json::Value,
}

/// Default page size of `ListWorkflowRuns`.
pub const WORKFLOW_RUNS_DEFAULT_LIMIT: u32 = 50;
/// Largest page `ListWorkflowRuns` accepts.
pub const WORKFLOW_RUNS_MAX_LIMIT: u32 = 200;
/// `last_error` is capped at this many bytes in `ListWorkflowRuns`.
pub const WORKFLOW_LIST_ERROR_MAX_BYTES: usize = 1024;
/// `last_error` is capped at this many bytes in `GetWorkflowRun`.
pub const WORKFLOW_RUN_ERROR_MAX_BYTES: usize = 8 * 1024;
/// The run payload and result are capped at this many bytes of JSON text.
pub const WORKFLOW_RUN_VALUE_MAX_BYTES: usize = 32 * 1024;
/// Each saved step result is capped at this many bytes of JSON text.
pub const WORKFLOW_STEP_VALUE_MAX_BYTES: usize = 2 * 1024;
/// `GetWorkflowRun` returns at most this many of the most recent steps, and
/// at most this many event waits.
pub const WORKFLOW_RUN_MAX_STEPS: usize = 100;
/// Workflow, step and event names and claim owners are cut to at most this
/// many bytes in `ListWorkflowRuns` and `GetWorkflowRun`.
pub const WORKFLOW_NAME_MAX_BYTES: usize = 256;
/// `ListWorkflowRuns` and `GetWorkflowRun` replies serialize to at most this
/// many bytes, inside the 1 MiB frame. A list stops early and sets
/// `has_more`; a run drops its oldest steps.
pub const WORKFLOW_REPLY_MAX_BYTES: usize = 768 * 1024;

/// Lifecycle status of a workflow run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowRunStatus {
    Pending,
    Running,
    Succeeded,
    Cancelled,
    Dead,
}

impl WorkflowRunStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::Cancelled => "cancelled",
            Self::Dead => "dead",
        }
    }
}

impl std::fmt::Display for WorkflowRunStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for WorkflowRunStatus {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "pending" => Ok(Self::Pending),
            "running" => Ok(Self::Running),
            "succeeded" => Ok(Self::Succeeded),
            "cancelled" => Ok(Self::Cancelled),
            "dead" => Ok(Self::Dead),
            other => Err(format!("unknown workflow run status '{other}'")),
        }
    }
}

/// Which workflow store answered. A `postgres` store is shared by every
/// server of the app/environment; a `sqlite` store belongs to one server.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowStoreKind {
    Sqlite,
    Postgres,
}

/// A stored JSON value, capped for management replies.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CappedJson {
    /// The value, or null when it was truncated.
    pub value: serde_json::Value,
    /// True when the value exceeded its cap and only `preview` is returned.
    pub truncated: bool,
    /// Size of the stored JSON text in bytes.
    pub bytes: u64,
    /// When truncated: the start of the stored JSON text, cut at a UTF-8
    /// boundary within the cap.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
}

/// One row of `ListWorkflowRuns`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkflowRunSummary {
    pub id: String,
    pub name: String,
    pub status: WorkflowRunStatus,
    /// Attempts started with the current budget.
    pub attempts: u32,
    pub max_attempts: u32,
    pub created_at_ms: i64,
    /// When the run is next due (pending) or was last due. Null while it is
    /// parked until a signal with no timeout.
    pub run_at_ms: Option<i64>,
    pub finished_at_ms: Option<i64>,
    pub last_error: Option<String>,
    #[serde(default)]
    pub last_error_truncated: bool,
}

/// Response payload for `Command::ListWorkflowRuns`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ListWorkflowRunsResponse {
    pub app: String,
    pub store: WorkflowStoreKind,
    pub runs: Vec<WorkflowRunSummary>,
    /// True when more runs match than `runs` holds.
    pub has_more: bool,
}

/// A completed step saved for a run.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WorkflowRunStep {
    pub name: String,
    pub completed_at_ms: i64,
    pub result: CappedJson,
}

/// A run parked waiting for an event.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkflowRunWait {
    pub step_name: String,
    pub event_name: String,
    pub expires_at_ms: Option<i64>,
}

/// Every stored field of a run.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WorkflowRunDetail {
    pub id: String,
    pub name: String,
    pub status: WorkflowRunStatus,
    pub attempts: u32,
    pub max_attempts: u32,
    pub created_at_ms: i64,
    pub run_at_ms: Option<i64>,
    pub finished_at_ms: Option<i64>,
    /// Lease expiry of a running attempt.
    pub lease_until_ms: Option<i64>,
    /// Claim owner of a running attempt.
    pub worker_id: Option<String>,
    pub last_error: Option<String>,
    #[serde(default)]
    pub last_error_truncated: bool,
    pub payload: CappedJson,
    /// Present once the run succeeded.
    pub result: Option<CappedJson>,
    /// The most recent saved steps, oldest first.
    pub steps: Vec<WorkflowRunStep>,
    /// Saved steps in the store; more than `steps.len()` when older ones were left out.
    pub steps_total: u64,
    pub waits: Vec<WorkflowRunWait>,
}

/// Response payload for `Command::GetWorkflowRun`. `run` is null when this
/// store has no run with that ID.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GetWorkflowRunResponse {
    pub app: String,
    pub store: WorkflowStoreKind,
    pub run: Option<WorkflowRunDetail>,
}

/// Response payload for `Command::RetryWorkflowRun` and `Command::CancelWorkflowRun`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkflowRunTransition {
    pub app: String,
    pub store: WorkflowStoreKind,
    pub id: String,
    pub previous_status: WorkflowRunStatus,
    pub status: WorkflowRunStatus,
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
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum SourceIpMode {
    #[default]
    Auto,
    Direct,
    CloudflareProxy,
    TrustedProxy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum SslProvider {
    #[serde(rename = "letsencrypt")]
    #[default]
    LetsEncrypt,
    Cloudflare,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SslBinding {
    #[serde(default)]
    pub provider: SslProvider,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cloudflare_api_token: Option<String>,
}

impl Default for SslBinding {
    fn default() -> Self {
        Self {
            provider: SslProvider::LetsEncrypt,
            cloudflare_api_token: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackupBinding {
    pub storage: StorageBinding,
    #[serde(default)]
    pub backup_keys: Vec<BackupKeyBinding>,
    #[serde(default = "default_backup_retention_days")]
    pub retention_days: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackupKeyBinding {
    pub id: String,
    pub key_base64: String,
}

fn default_backup_retention_days() -> u16 {
    DEFAULT_BACKUP_RETENTION_DAYS
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BackupEncryptionInfo {
    pub algorithm: String,
    pub key_id: String,
    pub nonce_base64: String,
    pub tag_base64: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BackupInfo {
    pub id: String,
    pub app: String,
    pub environment: String,
    pub server: String,
    pub created_at_unix_secs: i64,
    pub size_bytes: u64,
    pub sha256_hex: String,
    pub archive_key: String,
    pub manifest_key: String,
    pub encryption: BackupEncryptionInfo,
    /// Entries written before kinds existed are full archives.
    #[serde(default)]
    pub kind: BackupKind,
    /// Keyed digest of the SQLite databases the entry captured, used to skip
    /// unchanged database snapshots. Absent on older entries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub database_digest: Option<String>,
    /// Paths, relative to the app data directory, of the SQLite databases a
    /// database snapshot contains. Empty for full archives.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub databases: Vec<String>,
}

impl BackupInfo {
    /// File name of the encrypted archive object, for saving a download.
    pub fn archive_file_name(&self) -> String {
        match self.archive_key.rsplit('/').next() {
            Some(name) if !name.is_empty() && name != "." && name != ".." => name.to_string(),
            _ => match self.kind {
                BackupKind::Full => format!("{}.tar.zst.enc", self.id),
                BackupKind::Databases => format!("{}.databases.tar.zst.enc", self.id),
            },
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BackupKind {
    /// The whole app data tree.
    #[default]
    Full,
    /// Only the SQLite databases of the app data tree.
    Databases,
}

impl BackupKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Databases => "databases",
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BackupSkipReason {
    Unchanged,
    NoDatabases,
    TooLarge,
    InsufficientDisk,
    DeployInProgress,
    BackupInProgress,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BackupSkip {
    pub at_unix_secs: i64,
    pub reason: BackupSkipReason,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BackupListResponse {
    pub app: String,
    pub backups: Vec<BackupInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BackupStatusResponse {
    pub app: String,
    pub enabled: bool,
    #[serde(default)]
    pub retention_days: Option<u16>,
    /// The latest full archive.
    #[serde(default)]
    pub last_backup: Option<BackupInfo>,
    #[serde(default)]
    pub next_backup_at_unix_secs: Option<i64>,
    #[serde(default)]
    pub last_database_snapshot_at_unix_secs: Option<i64>,
    /// The latest hourly database snapshot attempt that uploaded nothing.
    #[serde(default)]
    pub last_database_snapshot_skip: Option<BackupSkip>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BackupDownloadUrlResponse {
    pub backup: BackupInfo,
    pub url: String,
    pub expires_in_seconds: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BackupRestoreResponse {
    pub backup: BackupInfo,
    /// What a database snapshot restore changed. Absent for full archives.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub databases: Option<RestoredDatabases>,
}

/// Paths relative to the app data directory.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct RestoredDatabases {
    /// Live databases replaced by their snapshot copies.
    pub replaced: Vec<String>,
    /// Snapshot databases that did not exist live and were created.
    pub created: Vec<String>,
    /// Live databases the snapshot does not contain, left as they were.
    pub untouched: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerRuntimeInfo {
    pub pid: u32,
    pub arch: String,
    pub libc: String,
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
    #[serde(default)]
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
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub replica_overrides: std::collections::BTreeMap<String, u32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub services: Vec<ComposeServiceStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub services_error: Option<String>,
    pub name: String,
    pub version: String,
    pub instances: Vec<InstanceStatus>,
    pub builds: Vec<BuildStatus>,
    pub state: AppState,

    pub last_error: Option<String>,
    /// Present only while there is instance recovery activity to report.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recovery: Option<RecoveryStatus>,
}

/// Instance recovery activity since the app was deployed or last stayed healthy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryStatus {
    /// Replacement starts attempted by the server.
    pub restart_count: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_exit: Option<InstanceExit>,
    /// Set while the server backs off before the next replacement start.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_restart_at_unix_secs: Option<i64>,
}

/// The most recent instance the server lost.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstanceExit {
    pub reason: InstanceExitReason,
    /// Observed process exit code, when the process exited by itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// Signal that terminated the process, when one did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signal: Option<i32>,
    pub at_unix_secs: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InstanceExitReason {
    /// The process exited after it had become healthy.
    Crashed,
    /// Health probes failed until the instance was declared dead.
    Unhealthy,
    /// The instance never became healthy.
    FailedToStart,
}

impl std::fmt::Display for InstanceExitReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            InstanceExitReason::Crashed => write!(f, "crashed"),
            InstanceExitReason::Unhealthy => write!(f, "unhealthy"),
            InstanceExitReason::FailedToStart => write!(f, "failed to start"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComposeServiceStatus {
    pub service: String,
    pub state: String,
    pub health: String,
    pub exit_code: i64,
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

/// The Tako SDK a release was built against, as its manifest records it.
/// Informational only: no compatibility decision depends on it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReleaseSdk {
    /// `javascript`, `go`, or `rust`.
    pub language: String,
    /// Package, module, or crate name as the ecosystem spells it.
    pub name: String,
    pub version: String,
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
    pub description: Option<String>,
    #[serde(default)]
    pub git_dirty: Option<bool>,
    /// Absent when the release has no detectable Tako SDK.
    #[serde(default)]
    pub sdk: Option<ReleaseSdk>,
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
    hex::encode(hasher.finalize())
}

#[cfg(test)]
mod tests;
