//! Snapshot backups of hosted databases.
//!
//! A snapshot is a compact, verified copy of the database (`VACUUM INTO`),
//! compressed with zstd and encrypted with the database's backup key, then
//! uploaded next to a JSON manifest. The manifest is written last, so only
//! complete snapshots are ever listed. Snapshots run hourly while the
//! database changes, before deploy release commands, before restores, and on
//! demand. Retention keeps every snapshot of the last 24 hours and the newest
//! of each of the last 7 days and 4 weeks.

use std::collections::BTreeSet;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use futures_util::TryStreamExt;
use object_store::{ObjectStore, ObjectStoreExt, path::Path as ObjectPath};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tako_core::{
    BackupEncryptionInfo, BackupKeyBinding, DatabaseBackupStatus, DatabaseSnapshot, SnapshotReason,
    StorageBinding,
};
use tako_database::DatabaseId;

use super::now_unix_secs;
use crate::ServerState;
use crate::operations::{
    METADATA_TIMEOUT, backup_store, decrypt_backup_file, download_object, encrypt_backup_file,
    put_json_object, upload_file, with_deadline,
};
use crate::state_store::StoredDatabase;

/// How often a changing database is snapshotted.
const SNAPSHOT_INTERVAL_SECS: u64 = 60 * 60;
/// How soon a failed snapshot is retried.
const RETRY_INTERVAL_SECS: u64 = 5 * 60;
const SCHEDULER_TICK: Duration = Duration::from_secs(60);
/// A work directory this old belongs to an operation that a restart or crash
/// interrupted: snapshots, restores, and exports finish well within it, and
/// import and export sessions expire after an hour.
const STALE_WORK_AGE: Duration = Duration::from_secs(24 * 60 * 60);
/// Free space kept on the data disk beyond a snapshot's working files.
/// How long a data object may go without a manifest before pruning deletes
/// it. An upload still in progress has none yet.
const ORPHAN_AGE_SECS: i64 = 24 * 60 * 60;
const DISK_RESERVE_BYTES: u64 = 256 * 1024 * 1024;
const MANIFEST_FORMAT: u32 = 1;

/// In-memory backup state of one database. The object store is the record;
/// this only avoids listing it on every scheduler tick.
#[derive(Clone, Debug, Default)]
pub(crate) struct BackupState {
    pub last_snapshot: Option<DatabaseSnapshot>,
    pub last_attempt_unix_secs: Option<u64>,
    pub last_error: Option<String>,
    /// Whether `last_snapshot` was read from the object store.
    pub loaded: bool,
}

#[derive(Debug, Serialize, Deserialize)]
struct Manifest {
    format: u32,
    database: String,
    generation: String,
    snapshot: DatabaseSnapshot,
    encryption: BackupEncryptionInfo,
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
    format!("{}/{snapshot}.sqlite.zst.enc", prefix(stored))
}

/// Names the server that last restored the database. A copy a move left
/// behind sees another server there and stops publishing snapshots, even
/// when it was unreachable during the move.
fn holder_key(stored: &StoredDatabase) -> String {
    format!("{}/holder", prefix(stored))
}

#[derive(Serialize, Deserialize)]
struct Holder {
    server: String,
    since_unix_secs: u64,
}

async fn read_holder(
    remote: &dyn ObjectStore,
    stored: &StoredDatabase,
) -> Result<Option<String>, String> {
    let key = ObjectPath::parse(holder_key(stored)).map_err(|e| e.to_string())?;
    let bytes = with_deadline(METADATA_TIMEOUT, async {
        match remote.get(&key).await {
            Ok(object) => object
                .bytes()
                .await
                .map(Some)
                .map_err(|e| format!("read the database's holder: {e}")),
            Err(object_store::Error::NotFound { .. }) => Ok(None),
            Err(error) => Err(format!("read the database's holder: {error}")),
        }
    })
    .await?;
    Ok(bytes
        .and_then(|bytes| serde_json::from_slice::<Holder>(&bytes).ok())
        .map(|holder| holder.server))
}

/// Fails when another server holds the database.
async fn check_holder(
    remote: &dyn ObjectStore,
    stored: &StoredDatabase,
    own: Option<&str>,
) -> Result<(), String> {
    let Some(own) = own else {
        return Ok(());
    };
    match read_holder(remote, stored).await? {
        Some(holder) if holder != own => Err(format!(
            "database {} was moved to another server ({holder}); this copy no longer publishes snapshots. Remove it with `tako sqlite delete {} --server <this server>`",
            stored.name, stored.name
        )),
        _ => Ok(()),
    }
}

async fn claim_holder(
    remote: &dyn ObjectStore,
    stored: &StoredDatabase,
    own: &str,
) -> Result<(), String> {
    put_json_object(
        remote,
        &holder_key(stored),
        &Holder {
            server: own.to_owned(),
            since_unix_secs: now_unix_secs(),
        },
    )
    .await
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
    Ok(store)
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
        last_snapshot: current.last_snapshot,
        last_attempt_unix_secs: current.last_attempt_unix_secs,
        last_error: current.last_error,
    })
}

pub(crate) fn spawn_scheduler(state: Arc<ServerState>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(SCHEDULER_TICK);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            if state.is_draining() {
                continue;
            }
            state.refresh_database_routes();
            super::transfer::expire(&state);
            let databases = match state.state_store.list_databases() {
                Ok(databases) => databases,
                Err(error) => {
                    tracing::warn!(%error, "Snapshot scheduler could not read databases");
                    continue;
                }
            };
            for stored in databases {
                // A database whose registration failed at startup (or later)
                // is retried, so a transient error does not keep it
                // unavailable until the next restart.
                state.retry_database_registration(&stored.name).await;
                let work = state.databases.work_dir(&stored.name);
                let removed = tokio::task::spawn_blocking(move || {
                    remove_stale_work_dirs(&work, std::time::SystemTime::now())
                })
                .await
                .unwrap_or(0);
                if removed > 0 {
                    tracing::info!(database = %stored.name, removed, "Removed interrupted database work directories");
                }
                if stored.secrets.backup.is_none() {
                    continue;
                }
                if !due(&state, &stored).await {
                    continue;
                }
                if let Err(error) = take(&state, &stored, SnapshotReason::Scheduled, true).await {
                    tracing::warn!(database = %stored.name, %error, "Scheduled database snapshot failed");
                }
            }
        }
    })
}

async fn due(state: &ServerState, stored: &StoredDatabase) -> bool {
    let Ok(id) = DatabaseId::parse(&stored.name) else {
        return false;
    };
    if !state.databases.host.has_changes(&id) {
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
            current.last_snapshot = latest;
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
    let id = DatabaseId::parse(&stored.name)?;
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
    if only_if_changed && !state.databases.host.has_changes(&id) {
        return Ok(None);
    }
    let started = now_unix_secs();
    let own = state.databases.server_id();
    let result = match store(&backup) {
        Ok(remote) => match check_holder(remote.as_ref(), stored, own.as_deref()).await {
            Ok(()) => match copy_database(state, stored).await {
                Ok((work, copy)) => {
                    upload_copy(stored, &backup, &work, &copy, reason, own.as_deref()).await
                }
                Err(error) => Err(error),
            },
            Err(error) => Err(error),
        },
        Err(error) => Err(error),
    };
    if result.is_err() {
        // The copy cleared the changed flag; keep the scheduler retrying.
        state.databases.host.mark_changed(&id);
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
    result: Result<DatabaseSnapshot, String>,
) -> Result<DatabaseSnapshot, String> {
    {
        let mut backups = state.databases.backups.lock();
        let current = backups.entry(stored.name.clone()).or_default();
        current.last_attempt_unix_secs = Some(started);
        match &result {
            Ok(snapshot) => {
                current.last_snapshot = Some(snapshot.clone());
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
    Ok(snapshot)
}

/// Copies the database into a fresh work directory, after checking there is
/// room for the copy and its compressed and encrypted forms.
async fn copy_database(
    state: &ServerState,
    stored: &StoredDatabase,
) -> Result<(WorkDir, PathBuf), String> {
    let id = DatabaseId::parse(&stored.name)?;
    let size = state.databases.host.size_bytes(&id).unwrap_or(0);
    let work = fresh_work_dir(state, &stored.name, "snapshot")?;
    let available = crate::operations::available_bytes(&work.path)?;
    if available < size.saturating_mul(3).saturating_add(DISK_RESERVE_BYTES) {
        return Err(format!(
            "not enough free disk space for a snapshot ({available} bytes available)"
        ));
    }
    let copy = work.path.join("snapshot.sqlite");
    state
        .databases
        .host
        .snapshot(&id, &copy)
        .await
        .map_err(|e| format!("copy database: {e}"))?;
    Ok((work, copy))
}

/// Compresses, encrypts and uploads a copy, then its manifest. With `own`,
/// the manifest is published only while no other server holds the database.
async fn upload_copy(
    stored: &StoredDatabase,
    backup: &StorageBinding,
    work: &WorkDir,
    copy: &Path,
    reason: SnapshotReason,
    own: Option<&str>,
) -> Result<DatabaseSnapshot, String> {
    let created = now_unix_secs();
    let snapshot_id = format!(
        "{created}-{}",
        nanoid::nanoid!(8, &nanoid::alphabet::SAFE[..])
    );
    let copy = copy.to_path_buf();
    let key = key_binding(stored, None);
    let compressed = work.path.join("snapshot.sqlite.zst");
    let encrypted = work.path.join("snapshot.sqlite.zst.enc");
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
    let snapshot = DatabaseSnapshot {
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
    // Checked again now: the database may have moved during the upload.
    let published = match check_holder(remote.as_ref(), stored, own).await {
        Ok(()) => {
            put_json_object(
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
            .await
        }
        Err(error) => Err(error),
    };
    if let Err(error) = published {
        // Listing and retention find data through manifests; without one the
        // object would never be removed.
        if let Ok(path) = ObjectPath::parse(&data) {
            let _ = with_deadline(METADATA_TIMEOUT, async {
                remote.delete(&path).await.map_err(|e| e.to_string())
            })
            .await;
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
    manifests.sort_by_key(|m| std::cmp::Reverse(m.snapshot.created_at_unix_secs));
    Ok(manifests)
}

async fn list(stored: &StoredDatabase) -> Result<Vec<DatabaseSnapshot>, String> {
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
    let snapshots = list(&stored).await?;
    Ok(serde_json::json!({"database": stored.name, "snapshots": snapshots}))
}

/// Which snapshots retention keeps: the newest overall, every snapshot of the
/// last 24 hours (so a release or manual snapshot survives the hourly ones
/// after it), and the newest of each of the last 7 days and 4 weeks.
/// `snapshots` is newest first.
fn retained(snapshots: &[DatabaseSnapshot], now: u64) -> BTreeSet<String> {
    let mut keep = BTreeSet::new();
    if let Some(newest) = snapshots.first() {
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

/// Deletes every snapshot object of a database from its backup storage and
/// returns how many objects were removed.
pub(crate) async fn delete_all(stored: &StoredDatabase) -> Result<usize, String> {
    let Some(backup) = &stored.secrets.backup else {
        return Ok(0);
    };
    let remote = store(backup)?;
    let keys = list_objects(remote.as_ref(), stored)
        .await?
        .into_iter()
        .map(|(key, _)| key);
    // Manifests first, so a partial deletion lists no snapshot without data.
    let (manifests, data): (Vec<_>, Vec<_>) = keys.partition(|key| key.as_ref().ends_with(".json"));
    let mut deleted = 0;
    for key in manifests.into_iter().chain(data) {
        with_deadline(METADATA_TIMEOUT, async {
            match remote.delete(&key).await {
                Ok(()) | Err(object_store::Error::NotFound { .. }) => Ok(()),
                Err(error) => Err(format!("delete {key}: {error}")),
            }
        })
        .await?;
        deleted += 1;
    }
    Ok(deleted)
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
                .and_then(|name| name.strip_suffix(".sqlite.zst.enc"))
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
    let stored = state.hosted_database(id)?;
    if let Some(key) = &key_override {
        super::decode_backup_key(key)?;
    }
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

    // Fetch and verify the snapshot before anything changes.
    let work = fresh_work_dir(state, &stored.name, "restore")?;
    // The download, the restored file, and a copy of what it replaces.
    let needed = manifest
        .snapshot
        .stored_bytes
        .saturating_add(manifest.snapshot.database_bytes.saturating_mul(2))
        .saturating_add(DISK_RESERVE_BYTES);
    let available = crate::operations::available_bytes(&work.path)?;
    if available < needed {
        return Err(format!(
            "Not enough free disk space to restore {}: {needed} bytes needed, {available} available.",
            stored.name
        ));
    }
    let encrypted = work.path.join("snapshot.sqlite.zst.enc");
    download_object(
        remote.as_ref(),
        &data_key(&stored, &manifest.snapshot.id),
        &encrypted,
    )
    .await?;
    let key = key_binding(&stored, key_override.as_deref());
    let restored = work.path.join("restored.sqlite");
    {
        let (encrypted, restored) = (encrypted.clone(), restored.clone());
        let compressed = work.path.join("snapshot.sqlite.zst");
        let encryption = manifest.encryption.clone();
        let expected = manifest.snapshot.sha256.clone();
        tokio::task::spawn_blocking(move || -> Result<(), String> {
            decrypt_backup_file(&encrypted, &compressed, &key, &encryption)?;
            decompress(&compressed, &restored)?;
            let (sha256, _) = sha256_file(&restored)?;
            if sha256 != expected {
                return Err("the downloaded snapshot does not match its checksum".into());
            }
            tako_database::verify_database(&restored)
        })
        .await
        .map_err(|e| e.to_string())??;
    }

    let mut warnings = replace(state, &stored, restored, "database is being restored").await?;
    // This server now holds the database; a copy elsewhere that a move left
    // behind stops publishing snapshots.
    if let Some(own) = state.databases.server_id()
        && let Err(error) = claim_holder(remote.as_ref(), &stored, &own).await
    {
        warnings.push(format!(
            "could not record this server as the database's holder; a copy left elsewhere may keep publishing snapshots until it is removed: {error}"
        ));
    }
    tracing::info!(database = %stored.name, snapshot = %manifest.snapshot.id, "Restored database");
    Ok(serde_json::json!({
        "database": stored.name,
        "name": stored.name,
        "snapshot": manifest.snapshot,
        "warnings": warnings,
    }))
}

/// Replaces a database's file with a verified `replacement` while its
/// requests are paused. When backups are configured and the database changed,
/// the exact contents being replaced are copied during the pause and uploaded
/// once requests resume. Returns warnings about that snapshot; the replaced
/// file is also kept as `previous.sqlite`.
pub(crate) async fn replace(
    state: &ServerState,
    stored: &StoredDatabase,
    replacement: PathBuf,
    pause_reason: &str,
) -> Result<Vec<String>, String> {
    let id = DatabaseId::parse(&stored.name)?;
    let lock = state.databases.lock(&stored.name);
    let _operation = lock.lock().await;
    let guard = state
        .databases
        .host
        .pause(&id, pause_reason)
        .await
        .map_err(|e| e.to_string())?;
    let mut warnings = Vec::new();
    let backup = stored.secrets.backup.clone();
    let copy = match &backup {
        Some(_) if state.databases.host.has_changes(&id) => {
            match copy_database(state, stored).await {
                Ok(copy) => Some(copy),
                Err(error) => {
                    warnings.push(format!("could not snapshot the replaced contents: {error}"));
                    None
                }
            }
        }
        _ => None,
    };
    let path = guard.path().to_path_buf();
    let previous = state
        .databases
        .database_dir(&stored.name)
        .join("previous.sqlite");
    let swapped = tokio::task::spawn_blocking(move || {
        tako_database::replace_database(&path, &replacement, &previous)
    })
    .await
    .map_err(|e| e.to_string())?;
    guard.mark_changed();
    drop(guard);
    swapped?;

    if let (Some(backup), Some((work, copy))) = (backup, copy) {
        let started = now_unix_secs();
        // Not fenced: copies of replaced contents are left out of latest
        // restores, and a move's new copy uploads one before it claims the
        // database.
        let result =
            upload_copy(stored, &backup, &work, &copy, SnapshotReason::Restore, None).await;
        if let Err(error) = record(
            state,
            stored,
            &backup,
            started,
            SnapshotReason::Restore,
            result,
        )
        .await
        {
            warnings.push(format!(
                "could not upload a snapshot of the replaced contents (kept as previous.sqlite on the server): {error}"
            ));
        }
    }
    Ok(warnings)
}

/// Removes the work directories under `root` last modified more than
/// [`STALE_WORK_AGE`] before `now`, and returns how many it removed.
pub(crate) fn remove_stale_work_dirs(root: &Path, now: std::time::SystemTime) -> usize {
    let Ok(entries) = std::fs::read_dir(root) else {
        return 0;
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        let stale = entry
            .metadata()
            .and_then(|metadata| metadata.modified())
            .is_ok_and(|modified| {
                now.duration_since(modified)
                    .is_ok_and(|age| age > STALE_WORK_AGE)
            });
        if stale && std::fs::remove_dir_all(entry.path()).is_ok() {
            removed += 1;
        }
    }
    removed
}

/// A work directory removed when dropped.
pub(crate) struct WorkDir {
    pub path: PathBuf,
}

impl Drop for WorkDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

pub(crate) fn fresh_work_dir(
    state: &ServerState,
    id: &str,
    label: &str,
) -> Result<WorkDir, String> {
    let root = state.databases.work_dir(id);
    super::create_private_dir(&root)?;
    let path = root.join(format!(
        "{label}-{}",
        nanoid::nanoid!(12, &nanoid::alphabet::SAFE[..])
    ));
    super::create_private_dir(&path)?;
    Ok(WorkDir { path })
}

pub(crate) fn sha256_file(path: &Path) -> Result<(String, u64), String> {
    let mut file =
        std::fs::File::open(path).map_err(|e| format!("open {}: {e}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 256 * 1024];
    let mut total = 0u64;
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|e| format!("read {}: {e}", path.display()))?;
        if read == 0 {
            break;
        }
        total += read as u64;
        hasher.update(&buffer[..read]);
    }
    Ok((hex::encode(hasher.finalize()), total))
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

    fn snapshot(id: &str, created: u64) -> DatabaseSnapshot {
        DatabaseSnapshot {
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
        ];
        assert_eq!(
            orphaned_data(&objects, now),
            vec![key("2-b.sqlite.zst.enc")]
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
