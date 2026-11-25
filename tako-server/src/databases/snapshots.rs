//! Snapshot backups of KV stores. SQLite databases are backed up
//! continuously instead (see [`super::backups`]), which also owns the
//! scheduler, disk-space reservations and work directories these use.
//!
//! A snapshot is a verified export of the store, compressed with zstd and
//! encrypted with the store's backup key, then
//! uploaded next to a JSON manifest. The manifest is written last, so only
//! complete snapshots are ever listed. Snapshots run hourly while the
//! database changes, before deploy release commands, before restores, and on
//! demand. Retention keeps every snapshot of the last 24 hours and the newest
//! of each of the last 7 days and 4 weeks.

use std::collections::{BTreeSet, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use futures_util::TryStreamExt;
use object_store::{ObjectStore, ObjectStoreExt, path::Path as ObjectPath};
use serde::{Deserialize, Serialize};
use tako_core::{
    BackupEncryptionInfo, BackupKeyBinding, DatabaseBackupStatus, DatabaseEngine, DatabaseSnapshot,
    SnapshotReason, StorageBinding,
};
use tako_db::DatabaseId;

use super::backups::{SpaceReservation, WorkDir, fresh_work_dir, sha256_file};
use super::encryption::{decrypt_backup_file, encrypt_backup_file};
use super::now_unix_secs;
use super::objects::{
    METADATA_TIMEOUT, backup_store, download_object, put_json_object, upload_file, with_deadline,
};
use crate::ServerState;
use crate::state_store::StoredDatabase;

/// How often a changing database is snapshotted.
const SNAPSHOT_INTERVAL_SECS: u64 = 60 * 60;
/// How soon a failed snapshot is retried.
const RETRY_INTERVAL_SECS: u64 = 5 * 60;
const SCHEDULER_TICK: Duration = Duration::from_secs(60);
/// How many databases the scheduler snapshots at once, so a database whose
/// storage is slow delays only its own snapshot.
const SCHEDULER_CONCURRENCY: usize = 4;
/// How long a data object may go without a manifest before pruning deletes
/// it. An upload still in progress has none yet.
const ORPHAN_AGE_SECS: i64 = 24 * 60 * 60;
const MANIFEST_FORMAT: u32 = 1;

#[derive(Debug, Serialize, Deserialize)]
struct Manifest {
    format: u32,
    database: String,
    generation: String,
    snapshot: Snapshot,
    encryption: BackupEncryptionInfo,
}

/// A snapshot as its manifest records it.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Snapshot {
    id: String,
    created_at_unix_secs: u64,
    reason: SnapshotReason,
    /// Size of the file the snapshot restores.
    database_bytes: u64,
    /// Size of the uploaded, compressed and encrypted object.
    stored_bytes: u64,
    /// SHA-256 of the file, hex encoded.
    sha256: String,
}

impl Snapshot {
    /// As `ListDatabaseSnapshots` lists it: every snapshot is a full copy.
    fn listed(&self) -> DatabaseSnapshot {
        DatabaseSnapshot {
            id: self.id.clone(),
            created_at_unix_secs: self.created_at_unix_secs,
            reason: self.reason,
            full: true,
            stored_bytes: self.stored_bytes,
        }
    }
}

/// Snapshots live under the database's name and generation, so a database
/// created again under an earlier one's name never sees its snapshots.
fn prefix(stored: &StoredDatabase) -> String {
    format!(
        "_tako/databases/{}/{}/snapshots",
        stored.name, stored.generation
    )
}

fn manifest_key(stored: &StoredDatabase, snapshot: &str) -> String {
    format!("{}/{snapshot}.json", prefix(stored))
}

fn data_key(stored: &StoredDatabase, snapshot: &str) -> String {
    format!(
        "{}/{snapshot}.{}.zst.enc",
        prefix(stored),
        extension(stored)
    )
}

/// The file extension of a database's copies: SQLite files, or KV exports.
pub(crate) fn extension(stored: &StoredDatabase) -> &'static str {
    match stored.engine {
        DatabaseEngine::Sqlite => "sqlite",
        DatabaseEngine::Kv => "redb",
    }
}

/// Whether the database may differ from its last snapshot.
pub(crate) fn has_changes(state: &ServerState, stored: &StoredDatabase) -> bool {
    match stored.engine {
        DatabaseEngine::Sqlite => {
            DatabaseId::parse(&stored.name).is_ok_and(|id| state.databases.host.has_changes(&id))
        }
        DatabaseEngine::Kv => tako_kv::StoreId::parse(&stored.name)
            .is_ok_and(|id| state.databases.kv.has_changes(&id)),
    }
}

/// Records that the database differs from its last snapshot.
pub(crate) fn mark_changed(state: &ServerState, stored: &StoredDatabase) {
    match stored.engine {
        DatabaseEngine::Sqlite => {
            if let Ok(id) = DatabaseId::parse(&stored.name) {
                state.databases.host.mark_changed(&id);
            }
        }
        DatabaseEngine::Kv => {
            if let Ok(id) = tako_kv::StoreId::parse(&stored.name) {
                state.databases.kv.mark_changed(&id);
            }
        }
    }
}

/// The size of the database's file on disk.
pub(crate) fn size_bytes(state: &ServerState, stored: &StoredDatabase) -> u64 {
    match stored.engine {
        DatabaseEngine::Sqlite => DatabaseId::parse(&stored.name)
            .ok()
            .and_then(|id| state.databases.host.size_bytes(&id)),
        DatabaseEngine::Kv => tako_kv::StoreId::parse(&stored.name)
            .ok()
            .and_then(|id| state.databases.kv.size_bytes(&id)),
    }
    .unwrap_or(0)
}

/// Writes a consistent copy of the database to `dest`. With `backup`, the
/// database counts as unchanged until its next write, unless this fails.
pub(crate) async fn copy_contents(
    state: &ServerState,
    stored: &StoredDatabase,
    dest: &Path,
    backup: bool,
) -> Result<(), String> {
    match stored.engine {
        DatabaseEngine::Sqlite => {
            let id = DatabaseId::parse(&stored.name)?;
            let copied = state.databases.host.snapshot(&id, dest).await;
            if !backup {
                // An export is not a backup: the scheduler still uploads the
                // contents, even a write that lands while the copy is made.
                state.databases.host.mark_changed(&id);
            }
            copied.map_err(|e| format!("copy database: {e}"))
        }
        DatabaseEngine::Kv => {
            let id = tako_kv::StoreId::parse(&stored.name)?;
            let kv = state.databases.kv.clone();
            let dest = dest.to_owned();
            tokio::task::spawn_blocking(move || {
                if backup {
                    kv.snapshot(&id, &dest)
                } else {
                    kv.export(&id, &dest)
                }
            })
            .await
            .map_err(|e| e.to_string())?
            .map(drop)
            .map_err(|e| format!("export KV store: {e}"))
        }
    }
}

/// Checks that `path` holds a usable copy for the database's engine. Blocks.
pub(crate) fn verify_copy(engine: DatabaseEngine, path: &Path) -> Result<(), String> {
    match engine {
        DatabaseEngine::Sqlite => tako_db::verify_database(path),
        DatabaseEngine::Kv => tako_kv::export::Reader::open(path)
            .and_then(|reader| reader.check())
            .map(drop)
            .map_err(|e| e.to_string()),
    }
}

fn key_binding(stored: &StoredDatabase, override_key: Option<&str>) -> BackupKeyBinding {
    BackupKeyBinding {
        id: format!("database-{}", stored.generation),
        key_base64: override_key
            .unwrap_or(&stored.secrets.backup_key_base64)
            .to_owned(),
    }
}

#[cfg(not(test))]
fn store(binding: &StorageBinding) -> Result<Arc<dyn ObjectStore>, String> {
    Ok(Arc::new(backup_store(binding)?))
}

/// Tests keep snapshots in memory, one store per bucket.
#[cfg(test)]
fn store(binding: &StorageBinding) -> Result<Arc<dyn ObjectStore>, String> {
    let _ = backup_store;
    test_store(binding.bucket.as_deref().unwrap_or_default())
}

/// The bucket `unreachable` fails at once; writes to `stalled` never finish,
/// like storage that accepts connections but stops answering.
#[cfg(test)]
pub(crate) fn test_store(bucket: &str) -> Result<Arc<dyn ObjectStore>, String> {
    if bucket == "unreachable" {
        return Err("backup storage is unreachable".into());
    }
    static STORES: std::sync::OnceLock<
        parking_lot::Mutex<std::collections::HashMap<String, Arc<object_store::memory::InMemory>>>,
    > = std::sync::OnceLock::new();
    let store = STORES
        .get_or_init(Default::default)
        .lock()
        .entry(bucket.to_owned())
        .or_insert_with(|| Arc::new(object_store::memory::InMemory::new()))
        .clone();
    if bucket == "stalled" {
        return Ok(Arc::new(StalledStore(store)));
    }
    if bucket == "lost-response" {
        return Ok(Arc::new(LostResponseStore(store)));
    }
    Ok(store)
}

/// Writes manifests, then reports failure, like storage whose response is
/// lost after it committed the object. Everything else passes through.
#[cfg(test)]
#[derive(Debug)]
struct LostResponseStore(Arc<object_store::memory::InMemory>);

#[cfg(test)]
impl std::fmt::Display for LostResponseStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("LostResponseStore")
    }
}

#[cfg(test)]
#[async_trait::async_trait]
impl ObjectStore for LostResponseStore {
    async fn put_opts(
        &self,
        location: &ObjectPath,
        payload: object_store::PutPayload,
        opts: object_store::PutOptions,
    ) -> object_store::Result<object_store::PutResult> {
        let result = self.0.put_opts(location, payload, opts).await?;
        if location.as_ref().ends_with(".json") {
            return Err(object_store::Error::Generic {
                store: "LostResponseStore",
                source: "the response was lost".into(),
            });
        }
        Ok(result)
    }

    async fn put_multipart_opts(
        &self,
        location: &ObjectPath,
        opts: object_store::PutMultipartOptions,
    ) -> object_store::Result<Box<dyn object_store::MultipartUpload>> {
        self.0.put_multipart_opts(location, opts).await
    }

    async fn get_opts(
        &self,
        location: &ObjectPath,
        options: object_store::GetOptions,
    ) -> object_store::Result<object_store::GetResult> {
        self.0.get_opts(location, options).await
    }

    fn delete_stream(
        &self,
        locations: futures_util::stream::BoxStream<'static, object_store::Result<ObjectPath>>,
    ) -> futures_util::stream::BoxStream<'static, object_store::Result<ObjectPath>> {
        self.0.delete_stream(locations)
    }

    fn list(
        &self,
        prefix: Option<&ObjectPath>,
    ) -> futures_util::stream::BoxStream<'static, object_store::Result<object_store::ObjectMeta>>
    {
        self.0.list(prefix)
    }

    async fn list_with_delimiter(
        &self,
        prefix: Option<&ObjectPath>,
    ) -> object_store::Result<object_store::ListResult> {
        self.0.list_with_delimiter(prefix).await
    }

    async fn copy_opts(
        &self,
        from: &ObjectPath,
        to: &ObjectPath,
        options: object_store::CopyOptions,
    ) -> object_store::Result<()> {
        self.0.copy_opts(from, to, options).await
    }
}

/// Serves reads from the store it wraps; writes wait forever.
#[cfg(test)]
#[derive(Debug)]
struct StalledStore(Arc<object_store::memory::InMemory>);

#[cfg(test)]
impl std::fmt::Display for StalledStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("StalledStore")
    }
}

#[cfg(test)]
#[async_trait::async_trait]
impl ObjectStore for StalledStore {
    async fn put_opts(
        &self,
        _location: &ObjectPath,
        _payload: object_store::PutPayload,
        _opts: object_store::PutOptions,
    ) -> object_store::Result<object_store::PutResult> {
        std::future::pending().await
    }

    async fn put_multipart_opts(
        &self,
        _location: &ObjectPath,
        _opts: object_store::PutMultipartOptions,
    ) -> object_store::Result<Box<dyn object_store::MultipartUpload>> {
        std::future::pending().await
    }

    async fn get_opts(
        &self,
        location: &ObjectPath,
        options: object_store::GetOptions,
    ) -> object_store::Result<object_store::GetResult> {
        self.0.get_opts(location, options).await
    }

    fn delete_stream(
        &self,
        locations: futures_util::stream::BoxStream<'static, object_store::Result<ObjectPath>>,
    ) -> futures_util::stream::BoxStream<'static, object_store::Result<ObjectPath>> {
        self.0.delete_stream(locations)
    }

    fn list(
        &self,
        prefix: Option<&ObjectPath>,
    ) -> futures_util::stream::BoxStream<'static, object_store::Result<object_store::ObjectMeta>>
    {
        self.0.list(prefix)
    }

    async fn list_with_delimiter(
        &self,
        prefix: Option<&ObjectPath>,
    ) -> object_store::Result<object_store::ListResult> {
        self.0.list_with_delimiter(prefix).await
    }

    async fn copy_opts(
        &self,
        _from: &ObjectPath,
        _to: &ObjectPath,
        _options: object_store::CopyOptions,
    ) -> object_store::Result<()> {
        std::future::pending().await
    }
}

pub(crate) fn status(state: &ServerState, stored: &StoredDatabase) -> Option<DatabaseBackupStatus> {
    let backup = stored.secrets.backup.as_ref()?;
    let current = state
        .databases
        .backups
        .lock()
        .get(&stored.name)
        .cloned()
        .unwrap_or_default();
    Some(DatabaseBackupStatus {
        bucket: backup.bucket.clone().unwrap_or_default(),
        endpoint: backup.endpoint.clone().unwrap_or_default(),
        destination: Some(tako_core::BackupDestination::of(backup)),
        last_snapshot: current.last_snapshot,
        // Snapshots only: no continuous history.
        replicated_at_unix_secs: None,
        last_attempt_unix_secs: current.last_attempt_unix_secs,
        last_error: current.last_error,
    })
}

pub(crate) fn spawn_scheduler(state: Arc<ServerState>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(SCHEDULER_TICK);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // Owned by this task, so stopping the scheduler stops its snapshots.
        let mut jobs = SnapshotJobs::new(SCHEDULER_CONCURRENCY);
        loop {
            interval.tick().await;
            schedule(&state, &mut jobs).await;
        }
    })
}

/// One scheduler tick: a snapshot job for each changed KV store with
/// backups. Returns without waiting for the snapshots.
pub(crate) async fn schedule(state: &Arc<ServerState>, jobs: &mut SnapshotJobs) {
    jobs.reap();
    if state.is_draining() {
        return;
    }
    // Routes, registrations and work directories are kept by the backup
    // scheduler, for both engines (see `super::backups::housekeeping`).
    let databases = match state.state_store.list_databases() {
        Ok(databases) => databases,
        Err(error) => {
            tracing::warn!(%error, "Snapshot scheduler could not read KV stores");
            return;
        }
    };
    for stored in databases {
        if state
            .databases
            .raft
            .resource(&stored.generation)
            .is_some_and(|r| !r.leads())
        {
            continue;
        }
        let Ok(stored) = state.databases.committed_settings(stored) else {
            continue;
        };
        if stored.engine == DatabaseEngine::Kv
            && stored.secrets.backup.is_some()
            && has_changes(state, &stored)
        {
            jobs.start(state, stored.name);
        }
    }
}

/// The scheduler's snapshots in progress. Each database's snapshot runs as
/// its own task, at most `limit` at once; dropping this aborts them.
pub(crate) struct SnapshotJobs {
    tasks: tokio::task::JoinSet<()>,
    permits: Arc<tokio::sync::Semaphore>,
    /// Databases with a job waiting or running, so a slow one is not queued
    /// again on every tick.
    pending: Arc<parking_lot::Mutex<HashSet<String>>>,
}

impl SnapshotJobs {
    pub(crate) fn new(limit: usize) -> Self {
        Self {
            tasks: tokio::task::JoinSet::new(),
            permits: Arc::new(tokio::sync::Semaphore::new(limit)),
            pending: Default::default(),
        }
    }

    /// Queues a snapshot of `name` if it is due once a slot is free, unless
    /// one is already waiting or running.
    fn start(&mut self, state: &Arc<ServerState>, name: String) {
        if !self.pending.lock().insert(name.clone()) {
            return;
        }
        let pending = Pending {
            set: self.pending.clone(),
            name,
        };
        let permits = self.permits.clone();
        let state = state.clone();
        self.tasks.spawn(async move {
            let Ok(_permit) = permits.acquire_owned().await else {
                return;
            };
            if state.is_draining() {
                return;
            }
            // Read again: the database may have been deleted, or its backups
            // changed, while this job waited for a slot.
            let Ok(stored) = state.hosted_database(&pending.name) else {
                return;
            };
            if stored.secrets.backup.is_none() || !due(&state, &stored).await {
                return;
            }
            if let Err(error) = take(&state, &stored, SnapshotReason::Scheduled, true).await {
                tracing::warn!(database = %stored.name, %error, "Scheduled database snapshot failed");
            }
        });
    }

    /// Collects finished jobs.
    fn reap(&mut self) {
        while let Some(finished) = self.tasks.try_join_next() {
            if let Err(error) = finished
                && error.is_panic()
            {
                tracing::error!(%error, "Scheduled database snapshot panicked");
            }
        }
    }
}

/// Marks a database's job as pending until the job ends, however it ends.
struct Pending {
    set: Arc<parking_lot::Mutex<HashSet<String>>>,
    name: String,
}

impl Drop for Pending {
    fn drop(&mut self) {
        self.set.lock().remove(&self.name);
    }
}

async fn due(state: &ServerState, stored: &StoredDatabase) -> bool {
    if !has_changes(state, stored) {
        return false;
    }
    let loaded = state
        .databases
        .backups
        .lock()
        .get(&stored.name)
        .is_some_and(|current| current.loaded);
    if !loaded {
        // Learn when the last snapshot was taken, so a restart does not
        // snapshot again right away.
        let latest = list(stored).await.ok().and_then(|mut all| {
            all.sort_by_key(|s| std::cmp::Reverse(s.created_at_unix_secs));
            all.into_iter().next()
        });
        let mut backups = state.databases.backups.lock();
        let current = backups.entry(stored.name.clone()).or_default();
        current.loaded = true;
        if current.last_snapshot.is_none() {
            current.last_snapshot = latest.as_ref().map(Snapshot::listed);
        }
    }
    let now = now_unix_secs();
    let current = state
        .databases
        .backups
        .lock()
        .get(&stored.name)
        .cloned()
        .unwrap_or_default();
    if current.last_error.is_some()
        && current
            .last_attempt_unix_secs
            .is_some_and(|at| now < at + RETRY_INTERVAL_SECS)
    {
        return false;
    }
    current
        .last_snapshot
        .as_ref()
        .is_none_or(|last| now >= last.created_at_unix_secs + SNAPSHOT_INTERVAL_SECS)
}

/// Takes and uploads a snapshot. With `only_if_changed`, an unchanged
/// database returns `None`.
pub(crate) async fn take(
    state: &ServerState,
    stored: &StoredDatabase,
    reason: SnapshotReason,
    only_if_changed: bool,
) -> Result<Option<DatabaseSnapshot>, String> {
    let lock = state.databases.lock(&stored.name);
    let _guard = lock.lock().await;
    // The destination may have changed while this run waited for the lock.
    let stored = &state.hosted_database(&stored.name)?;
    let Some(backup) = stored.secrets.backup.clone() else {
        return Err(format!(
            "Backups are not configured for database {}.",
            stored.name
        ));
    };
    if only_if_changed && !has_changes(state, stored) {
        return Ok(None);
    }
    let started = now_unix_secs();
    let result = match copy_database(state, stored).await {
        Ok(copy) => upload_copy(stored, &backup, &copy, reason).await,
        Err(error) => Err(error),
    };
    if result.is_err() {
        // The copy cleared the changed flag; keep the scheduler retrying.
        mark_changed(state, stored);
    }
    record(state, stored, &backup, started, reason, result)
        .await
        .map(Some)
}

/// Records a snapshot attempt and prunes after a success.
async fn record(
    state: &ServerState,
    stored: &StoredDatabase,
    backup: &StorageBinding,
    started: u64,
    reason: SnapshotReason,
    result: Result<Snapshot, String>,
) -> Result<DatabaseSnapshot, String> {
    {
        let mut backups = state.databases.backups.lock();
        let current = backups.entry(stored.name.clone()).or_default();
        // A failure counts from when it ended, so the retry waits its
        // interval even after an attempt that ran until it timed out.
        current.last_attempt_unix_secs = Some(if result.is_ok() {
            started
        } else {
            now_unix_secs()
        });
        match &result {
            Ok(snapshot) => {
                current.last_snapshot = Some(snapshot.listed());
                current.last_error = None;
                tracing::info!(database = %stored.name, snapshot = %snapshot.id, ?reason, "Uploaded database snapshot");
            }
            Err(error) => current.last_error = Some(error.clone()),
        }
    }
    let snapshot = result?;
    if let Err(error) = prune(stored, backup).await {
        tracing::warn!(database = %stored.name, %error, "Pruning database snapshots failed");
    }
    Ok(snapshot.listed())
}

/// A work directory with the disk space reserved for its files. The space is
/// released once the directory is removed.
struct ReservedWork {
    work: WorkDir,
    _space: SpaceReservation,
}

/// A copy of a database in its own work directory.
struct SnapshotCopy {
    work: WorkDir,
    path: PathBuf,
    /// Space for the copy and its compressed and encrypted forms. Declared
    /// after `work`, so it is released once the directory is removed.
    _space: SpaceReservation,
}

/// Copies the database into a fresh work directory, after reserving room for
/// the copy and its compressed and encrypted forms.
async fn copy_database(
    state: &ServerState,
    stored: &StoredDatabase,
) -> Result<SnapshotCopy, String> {
    let size = size_bytes(state, stored);
    let work = fresh_work_dir(state, &stored.name, "snapshot")?;
    let available = crate::operations::available_bytes(&work.path)?;
    let space =
        state
            .databases
            .snapshot_space
            .reserve(size.saturating_mul(3), available, "a snapshot")?;
    let path = work.path.join(format!("snapshot.{}", extension(stored)));
    copy_contents(state, stored, &path, true).await?;
    Ok(SnapshotCopy {
        work,
        path,
        _space: space,
    })
}

/// Compresses, encrypts and uploads a committed copy before publishing its manifest.
async fn upload_copy(
    stored: &StoredDatabase,
    backup: &StorageBinding,
    copy: &SnapshotCopy,
    reason: SnapshotReason,
) -> Result<Snapshot, String> {
    let created = now_unix_secs();
    // Milliseconds lead the ID, so IDs order snapshots taken in the same
    // second (snapshots of a database are taken one at a time).
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis());
    let snapshot_id = format!(
        "{millis:013}-{}",
        nanoid::nanoid!(8, &nanoid::alphabet::SAFE[..])
    );
    let key = key_binding(stored, None);
    let compressed = copy.work.path.join("snapshot.zst");
    let encrypted = copy.work.path.join("snapshot.zst.enc");
    let copy = copy.path.clone();
    let (sha256, database_bytes, encryption) = {
        let (copy, compressed, encrypted) = (copy.clone(), compressed.clone(), encrypted.clone());
        tokio::task::spawn_blocking(move || -> Result<_, String> {
            let (sha256, database_bytes) = sha256_file(&copy)?;
            compress(&copy, &compressed)?;
            let encryption = encrypt_backup_file(&compressed, &encrypted, &key)?;
            Ok((sha256, database_bytes, encryption))
        })
        .await
        .map_err(|e| e.to_string())??
    };
    let stored_bytes = std::fs::metadata(&encrypted)
        .map(|m| m.len())
        .map_err(|e| e.to_string())?;
    let snapshot = Snapshot {
        id: snapshot_id.clone(),
        created_at_unix_secs: created,
        reason,
        database_bytes,
        stored_bytes,
        sha256,
    };
    let remote = store(backup)?;
    let data = data_key(stored, &snapshot_id);
    upload_file(remote.clone(), &data, &encrypted).await?;
    let published = put_json_object(
        remote.as_ref(),
        &manifest_key(stored, &snapshot_id),
        &Manifest {
            format: MANIFEST_FORMAT,
            database: stored.name.clone(),
            generation: stored.generation.clone(),
            snapshot: snapshot.clone(),
            encryption,
        },
    )
    .await;
    if let Err(error) = published {
        // The manifest may have been written although the call failed (a
        // response lost to a timeout), so it goes first, then the data:
        // listing and retention find data only through manifests.
        for key in [manifest_key(stored, &snapshot_id), data] {
            if let Ok(path) = ObjectPath::parse(&key) {
                let _ = with_deadline(METADATA_TIMEOUT, async {
                    match remote.delete(&path).await {
                        Ok(()) | Err(object_store::Error::NotFound { .. }) => Ok(()),
                        Err(error) => Err(error.to_string()),
                    }
                })
                .await;
            }
        }
        return Err(error);
    }
    Ok(snapshot)
}

/// Every object under the database's snapshot prefix, with the time it was
/// last written (Unix seconds).
async fn list_objects(
    remote: &dyn ObjectStore,
    stored: &StoredDatabase,
) -> Result<Vec<(ObjectPath, i64)>, String> {
    let prefix = ObjectPath::parse(prefix(stored)).map_err(|e| e.to_string())?;
    with_deadline(METADATA_TIMEOUT, async {
        remote
            .list(Some(&prefix))
            .map_ok(|meta| (meta.location, meta.last_modified.timestamp()))
            .try_collect::<Vec<_>>()
            .await
            .map_err(|e| format!("list database snapshots: {e}"))
    })
    .await
}

async fn list_manifests(
    remote: &dyn ObjectStore,
    stored: &StoredDatabase,
) -> Result<Vec<Manifest>, String> {
    let objects = list_objects(remote, stored).await?;
    read_manifests(remote, stored, &objects).await
}

async fn read_manifests(
    remote: &dyn ObjectStore,
    stored: &StoredDatabase,
    objects: &[(ObjectPath, i64)],
) -> Result<Vec<Manifest>, String> {
    let mut manifests = Vec::new();
    for (key, _) in objects
        .iter()
        .filter(|(key, _)| key.as_ref().ends_with(".json"))
    {
        let bytes = with_deadline(METADATA_TIMEOUT, async {
            remote
                .get(key)
                .await
                .map_err(|e| format!("read snapshot manifest: {e}"))?
                .bytes()
                .await
                .map_err(|e| format!("read snapshot manifest: {e}"))
        })
        .await?;
        match serde_json::from_slice::<Manifest>(&bytes) {
            Ok(manifest)
                if manifest.database == stored.name && manifest.generation == stored.generation =>
            {
                manifests.push(manifest)
            }
            Ok(_) => tracing::warn!(%key, "Ignoring a snapshot manifest of another database"),
            Err(error) => tracing::warn!(%key, %error, "Ignoring an unreadable snapshot manifest"),
        }
    }
    // Newest first; within a second, by the millisecond-ordered ID.
    manifests.sort_by(|a, b| {
        (b.snapshot.created_at_unix_secs, &b.snapshot.id)
            .cmp(&(a.snapshot.created_at_unix_secs, &a.snapshot.id))
    });
    Ok(manifests)
}

async fn list(stored: &StoredDatabase) -> Result<Vec<Snapshot>, String> {
    let Some(backup) = &stored.secrets.backup else {
        return Err(format!(
            "Backups are not configured for database {}.",
            stored.name
        ));
    };
    let remote = store(backup)?;
    Ok(list_manifests(remote.as_ref(), stored)
        .await?
        .into_iter()
        .map(|m| m.snapshot)
        .collect())
}

pub(crate) async fn list_command(
    state: &ServerState,
    id: &str,
) -> Result<serde_json::Value, String> {
    let stored = state.hosted_database(id)?;
    let snapshots: Vec<DatabaseSnapshot> =
        list(&stored).await?.iter().map(Snapshot::listed).collect();
    // A KV store has no continuous history to restore into.
    Ok(serde_json::json!({"database": stored.name, "windows": [], "snapshots": snapshots}))
}

/// Which snapshots retention keeps: the newest overall, the newest one a
/// restore picks by default (copies taken before restores and imports are
/// not), every snapshot of the last 24 hours (so a release or manual
/// snapshot survives the hourly ones after it), and the newest of each of the
/// last 7 days and 4 weeks. `snapshots` is newest first.
fn retained(snapshots: &[Snapshot], now: u64) -> BTreeSet<String> {
    let mut keep = BTreeSet::new();
    if let Some(newest) = snapshots.first() {
        keep.insert(newest.id.clone());
    }
    if let Some(newest) = snapshots
        .iter()
        .find(|snapshot| snapshot.reason != SnapshotReason::Restore)
    {
        keep.insert(newest.id.clone());
    }
    for snapshot in snapshots {
        if now.saturating_sub(snapshot.created_at_unix_secs) < 86_400 {
            keep.insert(snapshot.id.clone());
        }
    }
    for (window, count) in [(86_400u64, 7u64), (604_800, 4)] {
        let mut buckets = BTreeSet::new();
        for snapshot in snapshots {
            let age = now.saturating_sub(snapshot.created_at_unix_secs);
            if age >= window * count {
                continue;
            }
            if buckets.insert(snapshot.created_at_unix_secs / window) {
                keep.insert(snapshot.id.clone());
            }
        }
    }
    keep
}

async fn prune(stored: &StoredDatabase, backup: &StorageBinding) -> Result<(), String> {
    let remote = store(backup)?;
    let objects = list_objects(remote.as_ref(), stored).await?;
    let manifests = read_manifests(remote.as_ref(), stored, &objects).await?;
    let snapshots: Vec<_> = manifests.iter().map(|m| m.snapshot.clone()).collect();
    let keep = retained(&snapshots, now_unix_secs());
    let delete = |path: ObjectPath| {
        let remote = remote.clone();
        async move {
            with_deadline(METADATA_TIMEOUT, async {
                match remote.delete(&path).await {
                    Ok(()) | Err(object_store::Error::NotFound { .. }) => Ok(()),
                    Err(error) => Err(format!("delete {path}: {error}")),
                }
            })
            .await
        }
    };
    for snapshot in snapshots.iter().filter(|s| !keep.contains(&s.id)) {
        // The manifest goes first, so a listed snapshot always has its data.
        for key in [
            manifest_key(stored, &snapshot.id),
            data_key(stored, &snapshot.id),
        ] {
            delete(ObjectPath::parse(&key).map_err(|e| e.to_string())?).await?;
        }
    }
    let now = i64::try_from(now_unix_secs()).unwrap_or(i64::MAX);
    for orphan in orphaned_data(&objects, now) {
        tracing::info!(database = %stored.name, key = %orphan, "Deleting snapshot data without a manifest");
        delete(orphan).await?;
    }
    Ok(())
}

/// Data objects without a manifest, last written at least `ORPHAN_AGE_SECS`
/// before `now`. An upload that stopped before publishing its manifest
/// leaves one, as does pruning that deleted a manifest but not its data.
/// Listing and restore never see them, so nothing else would delete them.
fn orphaned_data(objects: &[(ObjectPath, i64)], now: i64) -> Vec<ObjectPath> {
    let manifests: BTreeSet<&str> = objects
        .iter()
        .filter_map(|(key, _)| key.filename()?.strip_suffix(".json"))
        .collect();
    objects
        .iter()
        .filter(|(key, modified)| {
            key.filename()
                .and_then(|name| {
                    // Either engine's data: SQLite copies or KV exports.
                    name.strip_suffix(".sqlite.zst.enc")
                        .or_else(|| name.strip_suffix(".redb.zst.enc"))
                })
                .is_some_and(|id| !manifests.contains(id))
                && now.saturating_sub(*modified) >= ORPHAN_AGE_SECS
        })
        .map(|(key, _)| key.clone())
        .collect()
}

/// Replaces a database with a snapshot, taking a snapshot of the current
/// contents first when they changed.
pub(crate) async fn restore(
    state: &ServerState,
    id: &str,
    snapshot: Option<&str>,
    key_override: Option<String>,
) -> Result<serde_json::Value, String> {
    state.hosted_database(id)?;
    if let Some(key) = &key_override {
        super::decode_backup_key(key)?;
    }
    // Held from choosing the snapshot through the swap, so a scheduled
    // snapshot of the contents being replaced cannot be published meanwhile
    // and become the newest one (a later `--latest` would undo the restore).
    let lock = state.databases.lock(id);
    let _operation = lock.lock().await;
    // Read under the lock: a delete and create, or a change of backups, may
    // have finished while this waited for it.
    let stored = state.hosted_database(id)?;
    let Some(backup) = stored.secrets.backup.clone() else {
        return Err(format!(
            "Backups are not configured for database {}.",
            stored.name
        ));
    };
    let remote = store(&backup)?;
    let manifests = list_manifests(remote.as_ref(), &stored).await?;
    let manifest = match snapshot {
        Some(wanted) => manifests
            .into_iter()
            .find(|m| m.snapshot.id == wanted)
            .ok_or_else(|| {
                format!(
                    "Snapshot {wanted} of database {} was not found.",
                    stored.name
                )
            })?,
        // The newest snapshot of the database's own history: the safety
        // copies that restores and imports take of what they replace are
        // left out, so repeating a restore does not undo it.
        None => {
            let fallback = manifests.first().map(|m| m.snapshot.id.clone());
            let mut manifests = manifests.into_iter();
            match manifests
                .by_ref()
                .find(|m| m.snapshot.reason != SnapshotReason::Restore)
            {
                Some(manifest) => manifest,
                None => {
                    let id = fallback
                        .ok_or_else(|| format!("Database {} has no snapshots yet.", stored.name))?;
                    return Err(format!(
                        "Database {} has only snapshots taken before restores or imports; pass --snapshot {id} to restore one.",
                        stored.name
                    ));
                }
            }
        }
    };

    restore_from(state, &stored, remote.as_ref(), manifest, key_override).await
}

/// Downloads, verifies, and swaps in a snapshot.
async fn restore_from(
    state: &ServerState,
    stored: &StoredDatabase,
    remote: &dyn ObjectStore,
    manifest: Manifest,
    key_override: Option<String>,
) -> Result<serde_json::Value, String> {
    if manifest.format != MANIFEST_FORMAT {
        return Err(format!(
            "Snapshot {} is in format {}, which this server cannot read (it reads format {MANIFEST_FORMAT}); upgrade the server.",
            manifest.snapshot.id, manifest.format
        ));
    }
    // Fetch and verify the snapshot before anything changes.
    let work = fresh_work_dir(state, &stored.name, "restore")?;
    // The download, its decrypted form, and the restored file, held in the
    // ledger snapshots share; the copy of what it replaces reserves its own.
    let needed = manifest
        .snapshot
        .stored_bytes
        .saturating_mul(2)
        .saturating_add(manifest.snapshot.database_bytes);
    let available = crate::operations::available_bytes(&work.path)?;
    let space = state
        .databases
        .snapshot_space
        .reserve(needed, available, "the restore")
        .map_err(|error| format!("Cannot restore {}: {error}.", stored.name))?;
    let reserved = ReservedWork {
        work,
        _space: space,
    };
    let work = &reserved.work;
    let encrypted = work.path.join("snapshot.zst.enc");
    download_object(remote, &data_key(stored, &manifest.snapshot.id), &encrypted).await?;
    let key = key_binding(stored, key_override.as_deref());
    let restored = work.path.join(format!("restored.{}", extension(stored)));
    {
        let (encrypted, restored) = (encrypted.clone(), restored.clone());
        let compressed = work.path.join("snapshot.zst");
        let engine = stored.engine;
        let encryption = manifest.encryption.clone();
        let expected = manifest.snapshot.sha256.clone();
        tokio::task::spawn_blocking(move || -> Result<(), String> {
            decrypt_backup_file(&encrypted, &compressed, &key, &encryption)?;
            decompress(&compressed, &restored)?;
            let (sha256, _) = sha256_file(&restored)?;
            if sha256 != expected {
                return Err("the downloaded snapshot does not match its checksum".into());
            }
            verify_copy(engine, &restored)
        })
        .await
        .map_err(|e| e.to_string())??;
    }

    let warnings = replace_locked(state, stored, restored).await?;
    tracing::info!(database = %stored.name, snapshot = %manifest.snapshot.id, "Restored database");
    Ok(serde_json::json!({
        "database": stored.name,
        "name": stored.name,
        "snapshot": manifest.snapshot.listed(),
        "warnings": warnings,
    }))
}

/// Replaces a KV store's keys with a verified `replacement`, for a caller
/// that holds the store's lock. The replacement is applied in one
/// transaction while the store keeps serving, so readers see the old
/// contents or the new ones. The exact contents it replaces are exported to
/// `previous.redb` first, and uploaded as a snapshot when backups are
/// configured and they changed since the last one.
pub(crate) async fn replace_locked(
    state: &ServerState,
    stored: &StoredDatabase,
    replacement: PathBuf,
) -> Result<Vec<String>, String> {
    let id = tako_kv::StoreId::parse(&stored.name)?;
    // The caller read `stored` before it took the lock; a delete and create
    // of the same name meanwhile leaves a different store under it.
    let current = state.hosted_database(&stored.name)?;
    if current.generation != stored.generation {
        return Err(format!(
            "KV store {} was replaced by a new store of the same name meanwhile; nothing was changed.",
            stored.name
        ));
    }
    let previous = state
        .databases
        .database_dir(&stored.name)
        .join("previous.redb");
    // Whether the replaced contents need a backup is read in the store's
    // writer as it exports them, so a write racing the replace counts.
    let changed = {
        let kv = state.databases.kv.clone();
        let previous = previous.clone();
        tokio::task::spawn_blocking(move || kv.import(&id, &replacement, Some(&previous)))
            .await
            .map_err(|e| e.to_string())?
            .map_err(|e| format!("replace KV store {}: {e}", stored.name))?
            .previous_changed
    };
    let mut warnings = Vec::new();
    let Some(backup) = stored.secrets.backup.clone().filter(|_| changed) else {
        return Ok(warnings);
    };
    let copy = async {
        let size = std::fs::metadata(&previous)
            .map(|meta| meta.len())
            .map_err(|e| e.to_string())?;
        let work = fresh_work_dir(state, &stored.name, "snapshot")?;
        let available = crate::operations::available_bytes(&work.path)?;
        let space = state.databases.snapshot_space.reserve(
            size.saturating_mul(3),
            available,
            "a snapshot",
        )?;
        let path = work.path.join("snapshot.redb");
        std::fs::copy(&previous, &path).map_err(|e| e.to_string())?;
        Ok::<_, String>(SnapshotCopy {
            work,
            path,
            _space: space,
        })
    }
    .await;
    match copy {
        Ok(copy) => {
            let started = now_unix_secs();
            // Not fenced, like the SQLite copy of replaced contents.
            let result = upload_copy(stored, &backup, &copy, SnapshotReason::Restore).await;
            if let Err(error) =
                record(state, stored, &backup, started, SnapshotReason::Restore, result).await
            {
                warnings.push(format!(
                    "could not upload a snapshot of the replaced contents (kept as previous.redb on the server): {error}"
                ));
            }
        }
        Err(error) => warnings.push(format!(
            "could not snapshot the replaced contents (kept as previous.redb on the server): {error}"
        )),
    }
    Ok(warnings)
}

fn compress(input: &Path, output: &Path) -> Result<(), String> {
    let mut reader = std::fs::File::open(input).map_err(|e| e.to_string())?;
    let writer = std::fs::File::create(output).map_err(|e| e.to_string())?;
    let mut encoder = zstd::stream::Encoder::new(writer, 3).map_err(|e| e.to_string())?;
    std::io::copy(&mut reader, &mut encoder).map_err(|e| format!("compress snapshot: {e}"))?;
    let mut file = encoder.finish().map_err(|e| e.to_string())?;
    file.flush().map_err(|e| e.to_string())?;
    file.sync_all().map_err(|e| e.to_string())
}

fn decompress(input: &Path, output: &Path) -> Result<(), String> {
    let reader = std::fs::File::open(input).map_err(|e| e.to_string())?;
    let mut decoder = zstd::stream::Decoder::new(reader).map_err(|e| e.to_string())?;
    let mut writer = std::fs::File::create(output).map_err(|e| e.to_string())?;
    std::io::copy(&mut decoder, &mut writer).map_err(|e| format!("decompress snapshot: {e}"))?;
    writer.sync_all().map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(id: &str, created: u64) -> Snapshot {
        Snapshot {
            id: id.into(),
            created_at_unix_secs: created,
            reason: SnapshotReason::Scheduled,
            database_bytes: 0,
            stored_bytes: 0,
            sha256: String::new(),
        }
    }

    #[test]
    fn retention_keeps_a_day_of_snapshots_then_daily_and_weekly() {
        let now = 100 * 86_400;
        let mut all = Vec::new();
        // One snapshot every 30 minutes for 40 days, newest first.
        for step in 0..(40 * 48) {
            let created = now - step * 1800;
            all.push(snapshot(&format!("s{created}"), created));
        }
        let keep = retained(&all, now);
        // Every one of the last 24 hours, plus older days and weeks.
        for recent in all.iter().take(48) {
            assert!(keep.contains(&recent.id), "{} is within a day", recent.id);
        }
        assert!(
            keep.len() > 48 && keep.len() <= 48 + 7 + 4,
            "{}",
            keep.len()
        );
        assert!(keep.contains(&all[0].id), "the newest is kept");
        let oldest_kept = all
            .iter()
            .filter(|s| keep.contains(&s.id))
            .map(|s| s.created_at_unix_secs)
            .min()
            .unwrap();
        assert!(now - oldest_kept < 4 * 604_800);
        assert!(now - oldest_kept > 3 * 604_800);
    }

    #[test]
    fn retention_keeps_the_snapshot_a_restore_picks_by_default() {
        let now = 365 * 86_400;
        // On the same, long gone day: a scheduled snapshot, then the copy a
        // restore took of what it replaced. The day keeps only its newest.
        let mut before_restore = snapshot("2-b", now - 30 * 86_400 + 60);
        before_restore.reason = SnapshotReason::Restore;
        let scheduled = snapshot("1-a", now - 30 * 86_400);
        let keep = retained(&[before_restore, scheduled], now);
        assert!(keep.contains("1-a"), "{keep:?}");
        assert!(keep.contains("2-b"), "{keep:?}");
    }

    #[test]
    fn retention_always_keeps_the_newest_snapshot() {
        let old = snapshot("old", 0);
        let keep = retained(std::slice::from_ref(&old), 365 * 86_400);
        assert_eq!(keep, BTreeSet::from(["old".to_string()]));
    }

    #[test]
    fn data_without_a_manifest_is_deleted_once_old() {
        let day = ORPHAN_AGE_SECS;
        let now = 10 * day;
        let key = |name: &str| ObjectPath::from(format!("_tako/databases/shop/g/snapshots/{name}"));
        let objects = vec![
            // A complete snapshot, however old.
            (key("1-a.json"), 0),
            (key("1-a.sqlite.zst.enc"), 0),
            // Data whose manifest pruning deleted.
            (key("2-b.sqlite.zst.enc"), now - 2 * day),
            // An upload that may still be publishing its manifest.
            (key("3-c.sqlite.zst.enc"), now - 60),
            // A manifest alone is not data.
            (key("4-d.json"), 0),
            // Something else written under the prefix.
            (key("notes.txt"), 0),
            // A KV store's data, with and without its manifest.
            (key("5-e.redb.zst.enc"), now - 2 * day),
            (key("6-f.redb.zst.enc"), 0),
            (key("6-f.json"), 0),
        ];
        assert_eq!(
            orphaned_data(&objects, now),
            vec![key("2-b.sqlite.zst.enc"), key("5-e.redb.zst.enc")]
        );
    }

    #[test]
    fn compression_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("in");
        std::fs::write(&input, b"tako".repeat(10_000)).unwrap();
        compress(&input, &dir.path().join("z")).unwrap();
        decompress(&dir.path().join("z"), &dir.path().join("out")).unwrap();
        assert_eq!(
            std::fs::read(dir.path().join("out")).unwrap(),
            b"tako".repeat(10_000)
        );
        assert_eq!(sha256_file(&input).unwrap().1, 40_000);
    }
}
