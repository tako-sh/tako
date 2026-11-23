//! Tako server protocol types for management socket communication
//!
//! These types are shared between the CLI and tako-server for
//! communication via the Unix management socket.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};

use crate::database::{DatabaseBinding, DatabaseMigration, SnapshotReason};
use crate::storage::StorageBinding;

pub const PROTOCOL_VERSION: u32 = 0;
pub const UPGRADE_RELOAD_MARKER_FILE: &str = ".upgrade-reload";
pub const MANAGEMENT_AUTH_NAMESPACE: &str = "tako-management-rpc-v0";
pub const MAX_RELEASE_ARTIFACT_BYTES: u64 = 512 * 1024 * 1024;
pub const DEFAULT_BACKUP_RETENTION_DAYS: u16 = 30;
/// Operations on the authenticated caller's server-local cache.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
pub enum CacheRequest {
    Get {
        key: String,
    },
    Put {
        key: String,
        json: String,
        ttl_ms: u64,
    },
    Delete {
        key: String,
    },
}

/// The longest KV key, in bytes of UTF-8.
pub const KV_MAX_KEY_BYTES: usize = 1024;
/// The longest KV value, in bytes of serialized JSON.
pub const KV_MAX_VALUE_BYTES: usize = 256 * 1024;

/// Operations on one key of a KV store. Values are JSON text, stored as
/// given. A key without `ttl_ms` stays until it is deleted.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
pub enum KvRequest {
    /// Replies with the value's JSON text, or null when the key is missing
    /// or expired.
    Get {
        key: String,
    },
    Put {
        key: String,
        json: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ttl_ms: Option<u64>,
    },
    Delete {
        key: String,
    },
}

impl KvRequest {
    pub fn key(&self) -> &str {
        match self {
            Self::Get { key } | Self::Put { key, .. } | Self::Delete { key } => key,
        }
    }

    pub fn writes(&self) -> bool {
        !matches!(self, Self::Get { .. })
    }

    /// Checks the key and value limits and that the value is JSON.
    pub fn validate(&self) -> Result<(), String> {
        let key = self.key();
        if key.is_empty() {
            return Err("KV key cannot be empty.".into());
        }
        if key.len() > KV_MAX_KEY_BYTES {
            return Err(format!("KV key exceeds {KV_MAX_KEY_BYTES} bytes."));
        }
        if let Self::Put { json, ttl_ms, .. } = self {
            if json.len() > KV_MAX_VALUE_BYTES {
                return Err(format!(
                    "KV value exceeds {} KiB.",
                    KV_MAX_VALUE_BYTES / 1024
                ));
            }
            if serde_json::from_str::<serde::de::IgnoredAny>(json).is_err() {
                return Err("KV value must be JSON.".into());
            }
            if *ttl_ms == Some(0) {
                return Err("KV ttl must be a positive number of milliseconds.".into());
            }
        }
        Ok(())
    }
}

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

/// Allow five minutes plus six seconds per started MiB of compressed data.
/// Bound the total even if an unauthenticated sender advertises a larger size.
pub fn release_artifact_upload_timeout(size_bytes: u64) -> std::time::Duration {
    let size = size_bytes.min(MAX_RELEASE_ARTIFACT_BYTES);
    std::time::Duration::from_secs(5 * 60 + 6 * size.div_ceil(1024 * 1024))
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
    pub preparation_required: bool,
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
        /// The app environment's secret snapshot to prepare with, from
        /// [`Command::SecretsDigest`]. The server resolves its values and uses
        /// them without persisting them. Without one, preparation gets no
        /// secrets.
        #[serde(default)]
        secrets_digest: Option<String>,
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
        /// SSL certificate provider. The server supplies the provider credential
        /// from the secret snapshot.
        #[serde(default)]
        ssl: SslBinding,
        /// The app environment's secret snapshot, from [`Command::SecretsDigest`].
        #[serde(default)]
        secrets_digest: Option<String>,
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
        /// The secret snapshot whose app secrets are injected as env vars for
        /// the duration of this one-shot, from [`Command::SecretsDigest`].
        /// Long-running app processes still receive secrets via fd 3;
        /// release commands are short-lived and use env vars instead.
        #[serde(default)]
        secrets_digest: Option<String>,
        /// Databases of the release. The command receives a bootstrap
        /// envelope (`TAKO_BOOTSTRAP_DATA`) with a connection for each.
        #[serde(default)]
        databases: HashMap<String, DatabaseBinding>,
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

        /// The app environment's secret snapshot, from [`Command::SecretsDigest`].
        /// The server resolves app secrets and provider credentials from it
        /// and fills the secret parts of the bindings below. When `None`, the
        /// server keeps the secrets and credentials it last applied.
        /// Non-secret env vars are read by the server from app.json in the
        /// release dir.
        #[serde(default)]
        secrets_digest: Option<String>,
        /// Storage bindings injected into app processes at spawn time. S3
        /// bindings name their storage resource and carry no access keys.
        /// When `None`, the server keeps existing storage bindings for this app.
        #[serde(default)]
        storages: Option<HashMap<String, StorageBinding>>,
        /// Database bindings, by binding name. Each bound process receives a
        /// connection for each binding. When `None`, the server keeps the
        /// existing database bindings for this app.
        #[serde(default)]
        databases: Option<HashMap<String, DatabaseBinding>>,
        /// SSL certificate provider. Defaults to Let's Encrypt when omitted.
        #[serde(default)]
        ssl: SslBinding,
        /// Private backup target for this app/environment, without access
        /// keys or backup keys. When absent, any existing backup configuration
        /// for the app is cleared.
        #[serde(default)]
        backup: Option<Box<BackupBinding>>,
        #[serde(default)]
        force: bool,
    },

    /// Create a backup for the app immediately. When backup is provided, the
    /// server fills its keys from the app environment's current secrets and
    /// stores it before running the backup.
    BackupNow {
        app: String,
        #[serde(default)]
        backup: Option<Box<BackupBinding>>,
        /// The app environment's secret snapshot to take the backup keys and
        /// storage credentials from, from [`Command::SecretsDigest`]. Every
        /// target server applies the same snapshot.
        #[serde(default)]
        secrets_digest: Option<String>,
    },

    /// List backups recorded in the app's remote backup index.
    ListBackups { app: String },

    /// Get backup configuration and latest backup status for an app.
    BackupStatus { app: String },

    /// Create a short-lived download URL for a backup archive.
    BackupDownloadUrl { app: String, backup_id: String },

    /// Restore a full backup, or the SQLite databases of a database snapshot, on this server.
    RestoreBackup { app: String, backup_id: String },

    /// Create a database hosted on this server.
    CreateDatabase {
        name: String,
        #[serde(default)]
        engine: crate::database::DatabaseEngine,
        /// Tells this database apart from earlier ones with the same name.
        generation: String,
        /// 32-byte key, base64, that encrypts the database's snapshots.
        backup_key_base64: String,
        /// Private S3 storage that receives snapshots.
        #[serde(default)]
        backup: Option<StorageBinding>,
    },

    /// Delete a database hosted on this server. Refused while bound. Its
    /// snapshots are deleted too, unless `keep_snapshots` is set: a copy
    /// left behind by a move, or one whose recovery failed, shares them
    /// with the database that lives on.
    DeleteDatabase {
        name: String,
        /// The generation the caller means. When set, a database of that
        /// name created again meanwhile is not deleted.
        #[serde(default)]
        generation: Option<String>,
        #[serde(default)]
        keep_snapshots: bool,
    },

    /// Describe databases hosted on this server: one, or all when `None`.
    DatabaseStatus {
        #[serde(default)]
        name: Option<String>,
    },

    /// Set or clear where a database's snapshots are uploaded.
    ConfigureDatabaseBackup {
        name: String,
        backup: Option<StorageBinding>,
    },

    /// Let an app environment running on other servers use databases hosted
    /// on this server. Replaces the environment's previous grants.
    /// With `keep_existing`, the databases already granted stay granted: a
    /// deploy widens grants before its rollout, while the previous release
    /// still runs, and narrows them once the rollout succeeds.
    GrantDatabases {
        /// `<app>/<env>` for application SQL, or its separate `#channels` service scope.
        app: String,
        databases: Vec<String>,
        #[serde(default)]
        keep_existing: bool,
    },

    /// Record a restore point now: a full image for `manual` and
    /// `scheduled`, otherwise a named transaction of the continuous backup.
    /// `app` names the app environment that asks (before its release
    /// command), which must use the database.
    SnapshotDatabase {
        name: String,
        reason: SnapshotReason,
        #[serde(default)]
        app: Option<String>,
    },

    /// List a database's restore points and its windows of restorable time.
    ListDatabaseSnapshots { name: String },

    /// Replace a database with its backed-up state: at the restore point
    /// `snapshot` (a transaction), at the newest transaction captured at or
    /// before `at_unix_ms`, or, with neither, at the newest one. The key is
    /// needed when this server no longer has it (a replacement server).
    RestoreDatabase {
        name: String,
        #[serde(default)]
        snapshot: Option<String>,
        #[serde(default)]
        at_unix_ms: Option<i64>,
        #[serde(default)]
        backup_key_base64: Option<String>,
    },

    /// Run one SQL statement as an administrator, for `tako db shell`.
    QueryDatabase { name: String, sql: String },

    /// Read or write one key of a KV store this server hosts, as an
    /// administrator, for `tako kv get|put|delete`. Replies as the internal
    /// socket's `Kv` does: the value's JSON text or null.
    QueryKv { name: String, request: KvRequest },

    /// Apply an app's SQL migrations: each pending file in order, once,
    /// after a snapshot when any are pending. Replies with a
    /// `DatabaseMigrationReport`. `app` names the app environment that asks
    /// (a deployer), which must use the database and own the migrations;
    /// administrators send none.
    MigrateDatabase {
        name: String,
        /// The app the migrations belong to. Apps that share a database
        /// keep separate histories.
        owner: String,
        migrations: Vec<DatabaseMigration>,
        /// Report the pending migrations without applying them.
        #[serde(default)]
        dry_run: bool,
        #[serde(default)]
        app: Option<String>,
    },

    /// Replace a database with an empty one, after a snapshot of its
    /// contents when backups are configured. `migrations` (of `owner`) are
    /// applied to the empty database before it replaces the old one, so
    /// apps never see it without its schema, and a failing migration
    /// leaves the old one in place.
    ResetDatabase {
        name: String,
        #[serde(default)]
        owner: Option<String>,
        #[serde(default)]
        migrations: Vec<DatabaseMigration>,
    },

    /// Start replacing a database with an uploaded SQLite file.
    BeginDatabaseImport {
        name: String,
        size_bytes: u64,
        sha256: String,
    },

    /// Append one chunk of an import at `offset`.
    DatabaseImportChunk {
        name: String,
        upload_id: String,
        offset: u64,
        data_base64: String,
    },

    /// Verify a completed upload and replace the database with it.
    FinishDatabaseImport { name: String, upload_id: String },

    /// Write a consistent copy of a database for download.
    BeginDatabaseExport { name: String },

    /// Read one chunk of an export.
    DatabaseExportChunk {
        name: String,
        export_id: String,
        offset: u64,
    },

    /// Remove an export's copy.
    FinishDatabaseExport { name: String, export_id: String },

    /// Update the desired minimum number of instances for an app.
    Scale {
        app: String,
        instances: Option<u8>,
        #[serde(default)]
        service: Option<String>,
        #[serde(default)]
        reset: bool,
    },

    /// Start an app or resume a Compose group, preserving individually stopped apps.
    Start {
        app: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        service: Option<String>,
    },

    /// Stop an app or a Compose group without changing replica counts.
    Stop {
        app: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        service: Option<String>,
    },

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

    /// Pin the app environment's current secret snapshot for a deploy. The
    /// server first waits for the listed versions, which the deploying client
    /// just published, so a lagging server cannot pin older values. Returns
    /// `{ "digest": "..." }` for the deploy's other commands.
    SecretsDigest {
        app: String,
        #[serde(default)]
        published: Vec<PublishedSecret>,
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
    /// Each worker group sends every schedule. The active release replaces
    /// the app's schedule set once, including when the list is empty.
    RegisterSchedules {
        app: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        build: Option<String>,
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
    /// When the run's inbox already holds a signal for `event_name`, it is
    /// consumed as the step result in the same transaction and the run is
    /// due again at once instead of parked. Replies with
    /// [`WaitForEventResponse`].
    WaitForEvent {
        app: String,
        id: String,
        worker_id: String,
        step_name: String,
        event_name: String,
        #[serde(default)]
        timeout_at_ms: Option<i64>,
    },

    /// Worker: park the run until another run of the same app finishes, and
    /// save that run's outcome (`RunResult` JSON) as the step result. When
    /// the target has already finished, is missing, or belongs to another
    /// workflow than `target_name`, the outcome is saved at once and the run
    /// is due again immediately. A timeout saves `null`. Replies with
    /// [`WaitForEventResponse`].
    WaitForRun {
        app: String,
        id: String,
        worker_id: String,
        step_name: String,
        target_name: String,
        target_id: String,
        #[serde(default)]
        timeout_at_ms: Option<i64>,
    },

    /// Anyone: deliver an event payload.
    ///
    /// Without `run_id`, wakes every parked waiter with matching
    /// `event_name`; the payload is materialized as the step result for the
    /// waiter's `step_name`, and nothing is kept for runs that are not
    /// waiting yet. Replies with [`SignalBroadcastResponse`].
    ///
    /// With `run_id`, delivers to that run only: a waiting run is woken, and
    /// a run that has not reached its wait yet keeps the payload in its
    /// inbox until it does. Replies with [`SignalRunResponse`].
    Signal {
        app: String,
        event_name: String,
        payload: serde_json::Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        run_id: Option<String>,
    },

    /// Server-side channel publish. Used by app/workflow code inside a
    /// Tako-managed process — goes straight to the channel store instead
    /// of round-tripping through the HTTPS proxy. `payload` mirrors the
    /// `ChannelPublishPayload` wire shape (`{ type, data }`). `channel` and
    /// `params` together select the stream; omitted params select the
    /// channel's unparameterized stream. `build` is the publisher's
    /// `TAKO_BUILD`: the server checks `params` against that build's channel
    /// definition when it has loaded it.
    ChannelPublish {
        app: String,
        channel: String,
        #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
        params: serde_json::Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        build: Option<String>,
        payload: serde_json::Value,
    },
    /// Server-side close of a channel stream: appends a durable terminal
    /// marker after which the stream accepts no publishes. `channel`, `params`
    /// and `build` select and check the stream as for `ChannelPublish`.
    /// `lifecycle` is the channel definition's lifecycle, the shape a publish
    /// payload's `lifecycle` has; ephemeral streams cannot be closed. Closing
    /// a closed stream succeeds.
    ChannelClose {
        app: String,
        channel: String,
        #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
        params: serde_json::Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        build: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        lifecycle: Option<serde_json::Value>,
    },
    Cache {
        app: String,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        shared: bool,
        #[serde(flatten)]
        request: CacheRequest,
    },
    /// One key of a KV store the caller binds. `token` is the binding's,
    /// from the app's bootstrap envelope; the server serves the store or
    /// forwards the request to the store's server.
    Kv {
        app: String,
        token: String,
        #[serde(flatten)]
        request: KvRequest,
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
    /// Five-field Unix cron (0 and 7 are Sunday), or the `cron` crate's
    /// seconds-first format with an optional year or a named alias.
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
/// A run holds at most this many buffered signals: payloads delivered with
/// `Signal { run_id }` before the run started waiting for them. A signal
/// over the limit is rejected with `signal_inbox_full`, never dropped.
pub const WORKFLOW_SIGNAL_INBOX_MAX_ENTRIES: usize = 1000;
/// A run's buffered signal payloads total at most this many bytes of JSON
/// text. A signal over the limit is rejected with `signal_inbox_full`.
pub const WORKFLOW_SIGNAL_INBOX_MAX_BYTES: usize = 8 * 1024 * 1024;
/// Event names are 1–255 bytes with no NUL. Names starting with `__` are
/// reserved for Tako.
pub const WORKFLOW_EVENT_NAME_MAX_BYTES: usize = 255;
/// `GetWorkflowRun` lists at most this many buffered signals, oldest first.
pub const WORKFLOW_RUN_MAX_SIGNALS: usize = 100;

/// Reply to a broadcast `Signal` (one without `run_id`).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct SignalBroadcastResponse {
    /// Parked runs woken by this signal.
    pub woken: u64,
}

/// What a `Signal` addressed to one run did with its payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SignalRunStatus {
    /// The run was waiting for this event and was woken with the payload.
    Delivered,
    /// The run is not waiting for this event yet; the payload is stored in
    /// its inbox and is consumed when the run waits for it.
    Buffered,
    /// The run already holds a value for this event name, either a buffered
    /// signal or a completed wait. The first value wins; this one is not stored.
    Duplicate,
    /// The run has finished; the payload is not stored.
    Finished,
    /// No retained run has this ID; the payload is not stored.
    Missing,
}

/// Reply to a `Signal` addressed to one run.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct SignalRunResponse {
    pub status: SignalRunStatus,
}

/// Reply to `WaitForEvent`.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct WaitForEventResponse {
    /// True when a buffered signal satisfied the wait at once: the run is
    /// due again immediately instead of parked.
    #[serde(default)]
    pub delivered: bool,
}

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

    /// True once the run has finished and will not run again unless an
    /// operator retries it.
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Succeeded | Self::Cancelled | Self::Dead)
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

/// A signal delivered to a run before it waited for it, kept until a wait
/// for `event_name` consumes it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkflowRunSignal {
    pub event_name: String,
    pub received_at_ms: i64,
    /// Stored payload size in bytes of JSON text.
    pub bytes: u64,
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
    /// The oldest buffered signals not yet consumed by a wait.
    #[serde(default)]
    pub signals: Vec<WorkflowRunSignal>,
    /// Buffered signals in the store; more than `signals.len()` when some were left out.
    #[serde(default)]
    pub signals_total: u64,
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

/// A secret record version a client published for an app environment:
/// its record kind (`app`, `credential`, `storage`, `backup_key` or
/// `backup_active`), name and the revision the server accepted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublishedSecret {
    pub kind: String,
    pub name: String,
    pub revision: u64,
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
    /// File name for saving a download of the encrypted archive. Built from
    /// the id and kind, never from the storage object key: the index is
    /// remote text that anyone with write access to the bucket can change.
    /// Fails unless the id has the form Tako creates, `b<secs>-<nanoid>` for
    /// a full archive or `d<secs>-<nanoid>` for a database snapshot.
    pub fn archive_file_name(&self) -> Result<String, String> {
        let prefix = match self.kind {
            BackupKind::Full => 'b',
            BackupKind::Databases => 'd',
        };
        let valid = self.id.strip_prefix(prefix).is_some_and(|rest| {
            rest.split_once('-').is_some_and(|(secs, suffix)| {
                !secs.is_empty()
                    && secs.bytes().all(|byte| byte.is_ascii_digit())
                    && !suffix.is_empty()
                    && suffix
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
            })
        });
        if !valid {
            return Err(format!(
                "Backup id {:?} is not a valid {} backup id",
                self.id,
                self.kind.as_str()
            ));
        }
        Ok(match self.kind {
            BackupKind::Full => format!("{}.tar.zst.enc", self.id),
            BackupKind::Databases => format!("{}.databases.tar.zst.enc", self.id),
        })
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
    /// The latest failed manual full archive attempt.
    #[serde(default)]
    pub last_backup_failure: Option<BackupFailure>,
    /// Restore records that still need attention, even if backups are disabled.
    #[serde(default)]
    pub unfinished_restores: Vec<BackupRestoreStatus>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BackupRestoreStatus {
    pub path: String,
    pub state: BackupRestoreState,
    pub blocks_app: bool,
    pub message: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BackupRestoreState {
    InProgress,
    Unfinished,
    Abandoned,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BackupFailureReason {
    /// Creating or uploading the full archive failed.
    Failed,
    /// Too little free disk for the archive's temporary copies.
    InsufficientDisk,
    /// The remote archive index could not be read.
    IndexUnreadable,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BackupFailure {
    pub at_unix_secs: i64,
    pub reason: BackupFailureReason,
    pub message: String,
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
    /// What the server's machine has and uses, read when the info is asked
    /// for. Absent when none of it could be read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<HostStats>,
}

/// Resources of the machine a server runs on. Each part is absent when the
/// server could not read it.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct HostStats {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uptime_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu: Option<CpuStats>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory: Option<MemoryStats>,
    /// The filesystem holding the server's data directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk: Option<DiskStats>,
}

impl HostStats {
    pub fn is_empty(&self) -> bool {
        self.uptime_secs.is_none()
            && self.cpu.is_none()
            && self.memory.is_none()
            && self.disk.is_none()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CpuStats {
    /// Online logical CPUs.
    pub cores: u32,
    /// Run-queue load averaged over 1, 5 and 15 minutes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub load_average: Option<[f64; 3]>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MemoryStats {
    pub total_bytes: u64,
    /// Memory that can be given to new work without swapping, page cache
    /// included.
    pub available_bytes: u64,
    #[serde(default)]
    pub swap_total_bytes: u64,
    #[serde(default)]
    pub swap_free_bytes: u64,
}

/// Sizes as `df` reports them: `used` excludes blocks reserved for root, so
/// `used + available` can be less than `total`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DiskStats {
    pub path: String,
    pub total_bytes: u64,
    pub used_bytes: u64,
    pub available_bytes: u64,
}

/// Response from the server
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Response {
    /// Command succeeded
    Ok { data: serde_json::Value },

    /// Command failed. `code` is present only when an SDK must be able to
    /// branch on the failure; management errors carry free text alone.
    Error {
        message: String,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "crate::error_code::lenient_code"
        )]
        code: Option<crate::ErrorCode>,
    },
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
            code: None,
        }
    }

    /// An error reply an SDK can branch on by `code`.
    pub fn coded_error(code: crate::ErrorCode, message: impl Into<String>) -> Self {
        Self::Error {
            message: message.into(),
            code: Some(code),
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
            Self::Error { message, .. } => Some(message),
        }
    }

    /// Get the machine-readable code of an Error response, when it has one
    pub fn error_code(&self) -> Option<crate::ErrorCode> {
        match self {
            Self::Ok { .. } => None,
            Self::Error { code, .. } => *code,
        }
    }
}

impl From<crate::SocketError> for Response {
    fn from(error: crate::SocketError) -> Self {
        Self::Error {
            message: error.message,
            code: error.code,
        }
    }
}

/// App status information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppStatus {
    #[serde(default)]
    pub kind: ApplicationKind,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub applications: Vec<ComposeApplicationStatus>,
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

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApplicationKind {
    #[default]
    Application,
    ApplicationGroup,
}

/// An x-tako service within a Compose group. Identity and replicas are service-scoped;
/// releases, networks, volumes and lifecycle transactions belong to the group.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComposeApplicationStatus {
    pub name: String,
    pub service: String,
    /// `http` for apps with a backend pool, `worker` for SDK workers and jobs.
    pub kind: crate::compose::ComposeServiceKind,
    /// Whether the service receives the Tako SDK runtime (`x-tako.sdk`).
    pub sdk: bool,
    pub state: AppState,
    pub routes: Vec<String>,
    pub instances: Vec<InstanceStatus>,
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
