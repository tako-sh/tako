//! Continuous backups of hosted databases.
//!
//! While a database has backup storage, this server captures its committed
//! changes every few seconds and uploads them, encrypted with the database's
//! key, under `_tako/databases/<name>/<generation>/` (see `tako-backup`). Any
//! transaction of the last seven days can be restored. Full images (bases)
//! are captured when capture starts, once a day while the database changes,
//! and by `tako sqlite backup`; retention keeps some of them for four weeks.
//! Deploys, migrations, and replacements name the transaction before them
//! with a restore point.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use object_store::{ObjectStore, ObjectStoreExt, path::Path as ObjectPath};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tako_backup::{Capture, Point, Replica, Stored, Target};
use tako_core::{DatabaseBackupStatus, DatabaseSnapshot, SnapshotReason, StorageBinding};
use tako_database::DatabaseId;

use super::now_unix_secs;
use crate::ServerState;
use crate::operations::{METADATA_TIMEOUT, backup_store, put_json_object, with_deadline};
use crate::state_store::StoredDatabase;

const SCHEDULER_TICK: Duration = tako_backup::SYNC_INTERVAL;
/// Housekeeping runs once every this many ticks, about once a minute.
const HOUSEKEEPING_TICKS: u64 = 6;
/// Databases backed up at once, so slow storage delays only its own.
const CONCURRENCY: usize = 8;
/// Without known changes, capture still looks at the database this often.
const IDLE_SYNC: Duration = Duration::from_secs(60);
/// Longest an upload of pending captures may take before it is retried,
/// beyond the time its size needs (see `upload_timeout`).
const UPLOAD_TIMEOUT: Duration = Duration::from_secs(10 * 60);
/// How often retention runs for a database.
const PRUNE_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);
/// How long stopping waits for the databases' final uploads.
const STOP_TIMEOUT: Duration = Duration::from_secs(20);
/// Objects deleted per request when a database's backups are deleted, the
/// most an S3 `DeleteObjects` request takes.
const DELETE_BATCH: usize = 1000;
/// A work directory this old belongs to an operation that a restart or crash
/// interrupted: restores and exports finish well within it, and import and
/// export sessions expire after an hour.
const STALE_WORK_AGE: Duration = Duration::from_secs(24 * 60 * 60);
/// How often long work refreshes its directory's age, well within
/// [`STALE_WORK_AGE`].
const WORK_REFRESH: Duration = Duration::from_secs(60 * 60);
/// Free space kept on the data disk beyond restores' working files.
const DISK_RESERVE_BYTES: u64 = 256 * 1024 * 1024;

/// What `status` reports about a database's backups. The object store is
/// the record; this is what this process last saw.
#[derive(Clone, Debug, Default)]
pub(crate) struct BackupState {
    pub last_snapshot: Option<DatabaseSnapshot>,
    pub replicated_at_unix_secs: Option<u64>,
    pub last_attempt_unix_secs: Option<u64>,
    pub last_error: Option<String>,
    /// For a KV store's snapshots: whether `last_snapshot` was read from
    /// the object store.
    pub loaded: bool,
}

/// One database's capture, guarded by its own lock: the periodic job,
/// restore points, and replacements take turns.
#[derive(Default)]
pub(crate) struct Slot {
    open: Option<Open>,
    /// After a failure, remote work waits until then; capture continues.
    retry_at: Option<Instant>,
    backoff: Duration,
    pruned_at: Option<Instant>,
}

struct Open {
    capture: Capture,
    generation: String,
    backup: StorageBinding,
    base_at: Instant,
    synced_at: Instant,
    changed_since_base: bool,
    /// Points to publish once their segments are uploaded.
    points: Vec<Point>,
    /// Whether an upload since opening confirmed that no other history
    /// lies above this capture's.
    checked: bool,
}

/// Every database's slot.
pub(crate) type Slots = parking_lot::Mutex<HashMap<String, Arc<tokio::sync::Mutex<Slot>>>>;

fn slot(state: &ServerState, name: &str) -> Arc<tokio::sync::Mutex<Slot>> {
    state
        .databases
        .slots
        .lock()
        .entry(name.to_owned())
        .or_default()
        .clone()
}

/// A database's prefix in its backup storage. The generation keeps a
/// database created again under an earlier one's name from seeing its
/// backups.
fn prefix(stored: &StoredDatabase) -> String {
    format!("_tako/databases/{}/{}", stored.name, stored.generation)
}

/// Names the server that holds the database. A copy a move left behind sees
/// another server there and stops uploading, even when it was unreachable
/// during the move.
fn holder_key(stored: &StoredDatabase) -> String {
    format!("{}/holder", prefix(stored))
}

fn spool_dir(state: &ServerState, name: &str) -> PathBuf {
    state.databases.database_dir(name).join("replica")
}

#[derive(Serialize, Deserialize)]
struct Holder {
    server: String,
    since_unix_secs: u64,
}

/// The holder a deleted database's prefix keeps: no server is named so, so a
/// copy that a delete could not reach uploads nothing into the deleted
/// history when it comes back.
const DELETED_HOLDER: &str = "(deleted)";

/// The holder a given-up claim leaves. It names no server and bars none,
/// like a prefix without a holder. A claim is given up by replacing it
/// conditionally: a delete could remove a newer claim made after the read.
const UNCLAIMED_HOLDER: &str = "(none)";

/// A holder object as read: the server it names, if any, and the object's
/// version, for a write that must not replace a newer claim.
struct Claim {
    server: Option<String>,
    version: object_store::UpdateVersion,
}

async fn read_holder(
    remote: &dyn ObjectStore,
    stored: &StoredDatabase,
) -> Result<Option<String>, String> {
    let key = ObjectPath::parse(holder_key(stored)).map_err(|e| e.to_string())?;
    Ok(read_claim(remote, &key)
        .await?
        .and_then(|claim| claim.server))
}

async fn read_claim(remote: &dyn ObjectStore, key: &ObjectPath) -> Result<Option<Claim>, String> {
    with_deadline(METADATA_TIMEOUT, async {
        let object = match remote.get(key).await {
            Ok(object) => object,
            Err(object_store::Error::NotFound { .. }) => return Ok(None),
            Err(error) => return Err(format!("read the database's holder: {error}")),
        };
        let version = object_store::UpdateVersion {
            e_tag: object.meta.e_tag.clone(),
            version: object.meta.version.clone(),
        };
        let bytes = object
            .bytes()
            .await
            .map_err(|e| format!("read the database's holder: {e}"))?;
        let server = serde_json::from_slice::<Holder>(&bytes)
            .ok()
            .map(|holder| holder.server)
            .filter(|server| server != UNCLAIMED_HOLDER);
        Ok(Some(Claim { server, version }))
    })
    .await
}

/// Replaces `claim` with a holder naming `server`, or none, unless the
/// holder changed since `claim` was read. Returns whether it replaced it.
async fn replace_claim(
    remote: &dyn ObjectStore,
    key: &ObjectPath,
    claim: Claim,
    server: Option<&str>,
) -> Result<bool, String> {
    let holder = Holder {
        server: server.unwrap_or(UNCLAIMED_HOLDER).to_owned(),
        since_unix_secs: now_unix_secs(),
    };
    let body = serde_json::to_vec(&holder).map_err(|e| e.to_string())?;
    let options = object_store::PutOptions {
        mode: object_store::PutMode::Update(claim.version),
        ..Default::default()
    };
    with_deadline(METADATA_TIMEOUT, async {
        match remote.put_opts(key, body.into(), options).await {
            Ok(_) => Ok(true),
            Err(object_store::Error::Precondition { .. }) => Ok(false),
            Err(error) => Err(format!("record the database's holder: {error}")),
        }
    })
    .await
}

/// Why this server may not upload a database's backups.
enum Refused {
    /// Another server holds the database: this copy stops backing up until
    /// the holder changes.
    Moved(String),
    /// The database was deleted: this copy stops too.
    Deleted(String),
    /// The holder could not be read.
    Unavailable(String),
}

impl Refused {
    fn fenced(&self) -> bool {
        !matches!(self, Self::Unavailable(_))
    }
}

impl From<Refused> for String {
    fn from(refused: Refused) -> Self {
        match refused {
            Refused::Moved(message) | Refused::Deleted(message) | Refused::Unavailable(message) => {
                message
            }
        }
    }
}

/// Fails when another server holds the database.
async fn check_holder(
    remote: &dyn ObjectStore,
    stored: &StoredDatabase,
    own: Option<&str>,
) -> Result<(), Refused> {
    let Some(own) = own else {
        return Ok(());
    };
    match read_holder(remote, stored)
        .await
        .map_err(Refused::Unavailable)?
    {
        Some(holder) if holder == DELETED_HOLDER => Err(Refused::Deleted(format!(
            "database {} was deleted; this copy no longer uploads backups. Remove it with `tako sqlite delete {} --server <this server>`",
            stored.name, stored.name
        ))),
        Some(holder) if holder != own => Err(Refused::Moved(format!(
            "database {} was moved to another server ({holder}); this copy no longer uploads backups. Remove it with `tako sqlite delete {} --server <this server>`",
            stored.name, stored.name
        ))),
        _ => Ok(()),
    }
}

/// Marks a copy created by `CreateDatabase` until it is restored: a copy
/// created for a move must not back up its empty file into the history it
/// is about to restore from. A new database clears it on its first backup,
/// when its prefix has no history yet.
fn unrestored_marker(state: &ServerState, name: &str) -> PathBuf {
    state.databases.database_dir(name).join("unrestored")
}

pub(crate) fn mark_unrestored(state: &ServerState, name: &str) -> Result<(), String> {
    write_durably(&unrestored_marker(state, name), b"")
        .map_err(|e| format!("mark the new copy of {name}: {e}"))
}

/// Writes `bytes` to `path` through a synced temporary file renamed over
/// it, then syncs the directory, so a crash leaves the old contents or the
/// new ones, never a truncated file.
fn write_durably(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    let dir = path
        .parent()
        .ok_or_else(|| std::io::Error::other("the path has no directory"))?;
    let mut file = tempfile::NamedTempFile::new_in(dir)?;
    file.write_all(bytes)?;
    file.as_file().sync_all()?;
    file.persist(path).map_err(|error| error.error)?;
    std::fs::File::open(dir)?.sync_all()
}

/// Marks a copy that found another server holding its database, or the
/// database deleted. It captures nothing until a check of the holder passes,
/// also after a restart.
fn fenced_marker(state: &ServerState, name: &str) -> PathBuf {
    state.databases.database_dir(name).join("fenced")
}

fn remove_marker(path: &Path) -> Result<(), String> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("clear {}: {error}", path.display())),
    }
}

fn clear_unrestored(state: &ServerState, name: &str) -> Result<(), String> {
    match std::fs::remove_file(unrestored_marker(state, name)) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("clear the restore marker of {name}: {error}")),
    }
}

/// Gives up this server's claim on the database's backups, when it holds
/// it: removing a copy that an unfinished move left here lets the copy the
/// network names back up again.
pub(crate) async fn release_holder(
    state: &ServerState,
    stored: &StoredDatabase,
) -> Result<(), String> {
    let (Some(backup), Some(own)) = (&stored.secrets.backup, state.databases.server_id()) else {
        return Ok(());
    };
    let remote = store(backup)?;
    let key = ObjectPath::parse(holder_key(stored)).map_err(|e| e.to_string())?;
    let Some(claim) = read_claim(remote.as_ref(), &key).await? else {
        return Ok(());
    };
    if claim.server.as_deref() != Some(own.as_str()) {
        return Ok(());
    }
    // A newer claim from another server stands.
    replace_claim(remote.as_ref(), &key, claim, None)
        .await
        .map(|_| ())
        .map_err(|e| format!("release the backups of {}: {e}", stored.name))
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

#[cfg(not(test))]
fn store(binding: &StorageBinding) -> Result<Arc<dyn ObjectStore>, String> {
    Ok(Arc::new(backup_store(binding)?))
}

/// Tests keep backups in memory, one store per bucket.
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
        parking_lot::Mutex<HashMap<String, Arc<object_store::memory::InMemory>>>,
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
    Ok(store)
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

/// The database's history in its backup storage, read with `key_override`
/// when this server does not have the key (a replacement server).
fn replica(
    stored: &StoredDatabase,
    backup: &StorageBinding,
    key_override: Option<&str>,
) -> Result<(Replica, Arc<dyn ObjectStore>), String> {
    let remote = store(backup)?;
    let key = super::decode_backup_key(key_override.unwrap_or(&stored.secrets.backup_key_base64))?;
    let key = tako_backup::encryption::Key::new(format!("database-{}", stored.generation), key)
        .map_err(|e| e.to_string())?;
    let replica = Replica::new(remote.clone(), &prefix(stored), key).map_err(|e| e.to_string())?;
    Ok((replica, remote))
}

fn now_unix_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX)
        })
}

fn reason_name(reason: SnapshotReason) -> &'static str {
    match reason {
        SnapshotReason::Scheduled => "scheduled",
        SnapshotReason::Manual => "manual",
        SnapshotReason::Release => "release",
        SnapshotReason::Restore => "restore",
        SnapshotReason::Migration => "migration",
    }
}

fn parse_reason(name: &str) -> SnapshotReason {
    match name {
        "manual" => SnapshotReason::Manual,
        "release" => SnapshotReason::Release,
        "restore" => SnapshotReason::Restore,
        "migration" => SnapshotReason::Migration,
        _ => SnapshotReason::Scheduled,
    }
}

/// Runs `operation` on the capture on a blocking worker. A panic loses the
/// capture; the next tick opens it again.
async fn on_capture<R: Send + 'static>(
    slot: &mut Slot,
    operation: impl FnOnce(&mut Capture) -> tako_backup::anyhow::Result<R> + Send + 'static,
) -> Result<R, String> {
    let mut open = slot.open.take().ok_or("capture is not open")?;
    let (open, result) = tokio::task::spawn_blocking(move || {
        let result = operation(&mut open.capture);
        (open, result)
    })
    .await
    .map_err(|e| format!("capture stopped: {e}"))?;
    slot.open = Some(open);
    result.map_err(|e| format!("capture: {e:#}"))
}

/// Opens capture of a database for its current destination. Capture starts
/// with a full image, which the next upload sends.
///
/// A spool that holds a position resumes on this machine alone, so capture
/// continues while storage is unavailable; the first upload checks that this
/// server still holds the database. A new spool needs the history's position
/// to start above it.
async fn ensure_open(
    state: &ServerState,
    stored: &StoredDatabase,
    backup: &StorageBinding,
    slot: &mut Slot,
) -> Result<(), String> {
    if let Some(open) = slot.open.as_mut() {
        if open.generation == stored.generation
            && tako_core::same_backup_destination(&open.backup, backup)
        {
            // The credentials can change without the destination changing.
            open.backup = backup.clone();
            return Ok(());
        }
        // Another destination, or a database created again: its history
        // starts over there.
        close(slot).await;
        wipe_spool(state, &stored.name)?;
    }
    let spool = spool_dir(state, &stored.name);
    if !spool_matches(&spool, stored, backup) {
        wipe_spool(state, &stored.name)?;
    }
    let unrestored = unrestored_marker(state, &stored.name).exists();
    let fenced = fenced_marker(state, &stored.name);
    let positioned = tako_backup::spool_has_segments(&spool).map_err(|e| e.to_string())?;
    let floor = if positioned && !unrestored && !fenced.exists() {
        0
    } else {
        let (replica, remote) = replica(stored, backup, None)?;
        check_holder(
            remote.as_ref(),
            stored,
            state.databases.server_id().as_deref(),
        )
        .await?;
        remove_marker(&fenced)?;
        let newest = with_deadline(METADATA_TIMEOUT, async {
            replica
                .segments()
                .await
                .map_err(|e| format!("read the backup history: {e:#}"))
        })
        .await?
        .iter()
        .map(Stored::txid)
        .max();
        if unrestored {
            if newest.is_some() {
                return Err(format!(
                    "this copy of {} was created on top of existing backups and is not backed up until it is restored from them (`tako sqlite restore {} --latest`)",
                    stored.name, stored.name
                ));
            }
            clear_unrestored(state, &stored.name)?;
        }
        // Above every history there: a new spool's, and a fenced copy's
        // whose holder let it go again (an abandoned move may have left the
        // other copy's history above it).
        newest.map_or(0, |newest| newest.saturating_add(tako_backup::OWNER_JUMP))
    };
    remember_spool(&spool, stored, backup)?;
    let path = state.databases.database_path(&stored.name);
    let (capture, base, captured_at_ms) = tokio::task::spawn_blocking(
        move || -> tako_backup::anyhow::Result<(Capture, u64, i64)> {
            let mut capture = Capture::open(&path, &spool, floor)?;
            let (base, captured_at) = full_image(&mut capture)?;
            Ok((capture, base, captured_at))
        },
    )
    .await
    .map_err(|e| format!("capture stopped: {e}"))?
    .map_err(|e| format!("start capture: {e:#}"))?;
    let now = Instant::now();
    slot.open = Some(Open {
        capture,
        generation: stored.generation.clone(),
        backup: backup.clone(),
        base_at: now,
        synced_at: now,
        changed_since_base: false,
        // Names the full image, with its capture time: storage may take it
        // much later.
        points: vec![Point {
            created_at_ms: captured_at_ms,
            txid: base,
            reason: reason_name(SnapshotReason::Scheduled).into(),
        }],
        checked: false,
    });
    Ok(())
}

/// Captures a full image and returns its transaction with the time its
/// header records, which is when its read began: a restore point for it
/// must not claim an earlier time than its contents.
fn full_image(capture: &mut Capture) -> tako_backup::anyhow::Result<(u64, i64)> {
    let txid = capture.snapshot()?;
    Ok((txid, capture.captured_at(txid)?))
}

/// The destination a spool's captures belong to, recorded next to them, so a
/// spool is never uploaded somewhere else.
#[derive(Serialize, Deserialize, PartialEq)]
struct SpoolOwner {
    generation: String,
    destination: tako_core::BackupDestination,
}

fn spool_matches(spool: &Path, stored: &StoredDatabase, backup: &StorageBinding) -> bool {
    let Ok(bytes) = std::fs::read(spool.join("owner.json")) else {
        return !spool.exists();
    };
    serde_json::from_slice::<SpoolOwner>(&bytes).is_ok_and(|owner| {
        owner
            == SpoolOwner {
                generation: stored.generation.clone(),
                destination: tako_core::BackupDestination::of(backup),
            }
    })
}

fn remember_spool(
    spool: &Path,
    stored: &StoredDatabase,
    backup: &StorageBinding,
) -> Result<(), String> {
    super::create_private_dir(spool)?;
    let owner = SpoolOwner {
        generation: stored.generation.clone(),
        destination: tako_core::BackupDestination::of(backup),
    };
    let bytes = serde_json::to_vec(&owner).map_err(|e| e.to_string())?;
    write_durably(&spool.join("owner.json"), &bytes)
        .map_err(|e| format!("record the spool's destination: {e}"))
}

fn wipe_spool(state: &ServerState, name: &str) -> Result<(), String> {
    match std::fs::remove_dir_all(spool_dir(state, name)) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("clear the capture spool of {name}: {error}")),
    }
}

/// Captures what is left and stops capture. Segments stay in the spool for
/// the next upload.
async fn close(slot: &mut Slot) -> Option<u64> {
    let open = slot.open.take()?;
    let closed = tokio::task::spawn_blocking(move || open.capture.close()).await;
    match closed {
        Ok(Ok(txid)) => Some(txid),
        Ok(Err(error)) => {
            tracing::warn!(error = %format!("{error:#}"), "Final database capture failed");
            None
        }
        Err(error) => {
            tracing::warn!(%error, "Final database capture stopped");
            None
        }
    }
}

/// Captures recent changes on this machine.
async fn sync(
    state: &ServerState,
    stored: &StoredDatabase,
    slot: &mut Slot,
    force: bool,
) -> Result<u64, String> {
    let id = DatabaseId::parse(&stored.name)?;
    let open = slot.open.as_ref().ok_or("capture is not open")?;
    // Cleared first, so a write during the capture marks it again.
    let changed = state.databases.host.has_changes(&id);
    if changed {
        state.databases.host.clear_changed(&id);
    }
    if !force && !changed && open.synced_at.elapsed() < IDLE_SYNC {
        return Ok(open.capture.position());
    }
    let result = on_capture(slot, Capture::sync).await;
    match &result {
        Ok(_) => {
            if let Some(open) = slot.open.as_mut() {
                open.synced_at = Instant::now();
                open.changed_since_base |= changed;
            }
        }
        Err(_) if changed => state.databases.host.mark_changed(&id),
        Err(_) => {}
    }
    result
}

/// Uploads captured segments, then the points that name them. Checks first
/// that this server still holds the database.
async fn upload(
    state: &ServerState,
    stored: &StoredDatabase,
    slot: &mut Slot,
) -> Result<(), String> {
    let spool = spool_dir(state, &stored.name);
    let pending = tako_backup::Replica::pending(&spool).map_err(|e| e.to_string())?;
    let open = slot.open.as_mut().ok_or("capture is not open")?;
    if pending.is_empty() && open.points.is_empty() {
        return Ok(());
    }
    let (replica, remote) = replica(stored, &open.backup, None)?;
    let own = state.databases.server_id();
    let checked = check_holder(remote.as_ref(), stored, own.as_deref()).await;
    if let Err(refused) = checked {
        return Err(refuse(state, stored, slot, refused));
    }
    let uploading = with_deadline(upload_timeout(&spool, &pending), async {
        replica
            .upload(&spool)
            .await
            .map(|_| ())
            .map_err(|e| format!("upload backups: {e:#}"))
    })
    .await;
    // A move or a delete can land while the upload runs, also one that
    // fails part way. What reached a deleted history is taken back. After a
    // move the captures stay: they are this copy's history before the move,
    // which the new holder may be restoring from.
    let sent = tako_backup::Replica::receipted(&spool, &pending);
    // A segment whose acknowledgement was lost to the deadline has no
    // receipt but may be stored all the same.
    if !sent.is_empty() || uploading.is_err() {
        let rechecked = check_holder(remote.as_ref(), stored, own.as_deref()).await;
        match rechecked {
            Err(refused @ Refused::Deleted(_)) => {
                let removed = replica.remove(&pending).await;
                if let Err(cleanup) = removed {
                    tracing::warn!(database = %stored.name, error = %format!("{cleanup:#}"), "Could not remove backups uploaded after the database was deleted");
                }
                return Err(refuse(state, stored, slot, refused));
            }
            Err(refused @ Refused::Moved(_)) => return Err(refuse(state, stored, slot, refused)),
            Err(Refused::Unavailable(_)) | Ok(()) => {}
        }
    }
    uploading?;
    // The points name segments uploaded now or before.
    let points = std::mem::take(&mut slot.open.as_mut().expect("still open").points);
    for (index, point) in points.iter().enumerate() {
        let published = with_deadline(METADATA_TIMEOUT, async {
            replica
                .put_point(point)
                .await
                .map_err(|e| format!("record a restore point: {e:#}"))
        })
        .await;
        if let Err(error) = published {
            // Kept for the next upload.
            if let Some(open) = slot.open.as_mut() {
                open.points.extend(points[index..].iter().cloned());
            }
            return Err(error);
        }
    }
    // A capture that resumed from its spool continues below any history
    // another copy uploaded meanwhile, which a latest restore would then
    // prefer. It starts again above it, with a full image.
    let open = slot.open.as_ref().expect("still open");
    if !open.checked {
        let position = open.capture.position();
        let newest = with_deadline(METADATA_TIMEOUT, async {
            replica
                .segments()
                .await
                .map_err(|e| format!("read the backup history: {e:#}"))
        })
        .await?
        .iter()
        .map(Stored::txid)
        .max();
        if newest.is_some_and(|newest| newest > position) {
            tracing::warn!(database = %stored.name, "Another copy's backups are newer; capture starts again above them");
            close(slot).await;
            wipe_spool(state, &stored.name)?;
            return Ok(());
        }
        slot.open.as_mut().expect("still open").checked = true;
    }
    Ok(())
}

/// How long an upload of `pending` may take: [`UPLOAD_TIMEOUT`], plus time
/// for its bytes at 4 MiB/s, so a large full image is not cut off.
fn upload_timeout(spool: &Path, pending: &[tako_backup::Segment]) -> Duration {
    let store = tako_backup::Spool::new(spool);
    let bytes: u64 = pending
        .iter()
        .filter_map(|segment| std::fs::metadata(store.path(segment)).ok())
        .map(|metadata| metadata.len())
        .sum();
    UPLOAD_TIMEOUT + Duration::from_secs(bytes / (4 << 20))
}

/// Stops capture of a fenced copy, which would otherwise keep filling its
/// spool with captures it can never upload. It stops without a final
/// capture, and stays stopped across restarts until the holder lets it go.
fn refuse(
    state: &ServerState,
    stored: &StoredDatabase,
    slot: &mut Slot,
    refused: Refused,
) -> String {
    if refused.fenced() {
        if let Err(error) = write_durably(&fenced_marker(state, &stored.name), b"") {
            tracing::warn!(database = %stored.name, %error, "Could not record that this copy no longer backs up");
        }
        // Dropping capture captures nothing more.
        slot.open = None;
    }
    refused.into()
}

/// Starts the periodic backup of every database, and housekeeping.
pub(crate) fn spawn_scheduler(state: Arc<ServerState>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(SCHEDULER_TICK);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // Owned by this task, so stopping the scheduler stops its jobs.
        let mut jobs = BackupJobs::new(CONCURRENCY);
        let mut ticks: u64 = 0;
        loop {
            interval.tick().await;
            if ticks.is_multiple_of(HOUSEKEEPING_TICKS) {
                housekeeping(&state).await;
            }
            ticks = ticks.wrapping_add(1);
            schedule(&state, &mut jobs);
        }
    })
}

/// Retries failed registrations, refreshes routes, expires transfers, and
/// removes interrupted work directories.
pub(crate) async fn housekeeping(state: &Arc<ServerState>) {
    if state.is_draining() {
        return;
    }
    state.refresh_database_routes();
    super::transfer::expire(state);
    let databases = match state.state_store.list_databases() {
        Ok(databases) => databases,
        Err(error) => {
            tracing::warn!(%error, "Database housekeeping could not read databases");
            return;
        }
    };
    for stored in databases {
        // A database whose registration failed at startup (or later) is
        // retried, so a transient error does not keep it unavailable until
        // the next restart.
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
    }
}

/// Starts a backup job for every SQLite database, without waiting for them.
pub(crate) fn schedule(state: &Arc<ServerState>, jobs: &mut BackupJobs) {
    jobs.reap();
    if state.is_draining() {
        return;
    }
    let databases = match state.state_store.list_databases() {
        Ok(databases) => databases,
        Err(error) => {
            tracing::warn!(%error, "Backup scheduler could not read databases");
            return;
        }
    };
    // KV stores keep snapshot backups (see `super::snapshots`).
    for stored in databases {
        if stored.engine == tako_core::DatabaseEngine::Sqlite {
            jobs.start(state, stored.name);
        }
    }
}

/// The scheduler's backup jobs in progress. Each database's job runs as its
/// own task, at most `limit` at once; dropping this aborts them.
pub(crate) struct BackupJobs {
    tasks: tokio::task::JoinSet<()>,
    permits: Arc<tokio::sync::Semaphore>,
    /// Databases with a job waiting or running, so a slow one is not queued
    /// again on every tick.
    pending: Arc<parking_lot::Mutex<HashSet<String>>>,
}

impl BackupJobs {
    pub(crate) fn new(limit: usize) -> Self {
        Self {
            tasks: tokio::task::JoinSet::new(),
            permits: Arc::new(tokio::sync::Semaphore::new(limit)),
            pending: Arc::default(),
        }
    }

    fn reap(&mut self) {
        while let Some(result) = self.tasks.try_join_next() {
            if let Err(error) = result
                && error.is_panic()
            {
                tracing::error!(%error, "Database backup job panicked");
            }
        }
    }

    fn start(&mut self, state: &Arc<ServerState>, name: String) {
        if !self.pending.lock().insert(name.clone()) {
            return;
        }
        let state = state.clone();
        let permits = self.permits.clone();
        let pending = self.pending.clone();
        self.tasks.spawn(async move {
            let _pending = Pending {
                pending,
                name: name.clone(),
            };
            let Ok(_permit) = permits.acquire().await else {
                return;
            };
            back_up(&state, &name).await;
        });
    }

    #[cfg(test)]
    pub(crate) fn is_pending(&self, name: &str) -> bool {
        self.pending.lock().contains(name)
    }
}

struct Pending {
    pending: Arc<parking_lot::Mutex<HashSet<String>>>,
    name: String,
}

impl Drop for Pending {
    fn drop(&mut self) {
        self.pending.lock().remove(&self.name);
    }
}

/// One periodic backup step for a database: capture, upload, a daily full
/// image, and retention.
pub(crate) async fn back_up(state: &ServerState, name: &str) {
    let Ok(id) = DatabaseId::parse(name) else {
        return;
    };
    if !state.databases.host.is_registered(&id) {
        return;
    }
    let slot = slot(state, name);
    // A restore point or a replacement has it; this tick has nothing to add.
    let Ok(mut slot) = slot.try_lock() else {
        return;
    };
    // Read under the slot's lock (see `current`).
    let Ok(stored) = state.hosted_database(name) else {
        return;
    };
    let Some(backup) = stored.secrets.backup.clone() else {
        if slot.open.is_some() {
            close(&mut slot).await;
        }
        return;
    };
    if slot.retry_at.is_some_and(|at| Instant::now() < at) {
        // Storage failed recently; keep capturing on this machine.
        if slot.open.is_some()
            && let Err(error) = sync(state, &stored, &mut slot, false).await
        {
            tracing::warn!(database = %name, %error, "Database capture failed");
        }
        return;
    }
    let result = step(state, &stored, &backup, &mut slot).await;
    match &result {
        Ok(()) => {
            slot.retry_at = None;
            slot.backoff = Duration::ZERO;
        }
        Err(error) => {
            slot.backoff =
                (slot.backoff * 2).clamp(SCHEDULER_TICK, tako_backup::MAX_RETRY_INTERVAL);
            slot.retry_at = Some(Instant::now() + slot.backoff);
            tracing::warn!(database = %name, %error, "Database backup failed");
        }
    }
    record(state, name, result.err());
    if slot.open.is_some()
        && slot
            .pruned_at
            .is_none_or(|at| at.elapsed() >= PRUNE_INTERVAL)
    {
        slot.pruned_at = Some(Instant::now());
        if let Err(error) = prune(&stored, &backup).await {
            tracing::warn!(database = %name, %error, "Pruning database backups failed");
        }
    }
}

async fn step(
    state: &ServerState,
    stored: &StoredDatabase,
    backup: &StorageBinding,
    slot: &mut Slot,
) -> Result<(), String> {
    let opened = slot.open.is_none();
    ensure_open(state, stored, backup, slot).await?;
    if opened {
        let txid = slot.open.as_ref().map(|open| open.capture.position());
        if let Some(txid) = txid {
            note_base(state, stored, txid, SnapshotReason::Scheduled);
        }
    }
    sync(state, stored, slot, false).await?;
    let open = slot.open.as_mut().expect("opened above");
    if open.changed_since_base && open.base_at.elapsed() >= tako_backup::SNAPSHOT_INTERVAL {
        let (txid, captured_at_ms) = on_capture(slot, full_image).await?;
        let open = slot.open.as_mut().expect("still open");
        open.base_at = Instant::now();
        open.changed_since_base = false;
        open.points.push(Point {
            created_at_ms: captured_at_ms,
            txid,
            reason: reason_name(SnapshotReason::Scheduled).into(),
        });
        note_base(state, stored, txid, SnapshotReason::Scheduled);
    }
    upload(state, stored, slot).await
}

fn note_base(state: &ServerState, stored: &StoredDatabase, txid: u64, reason: SnapshotReason) {
    let mut backups = state.databases.backups.lock();
    let current = backups.entry(stored.name.clone()).or_default();
    current.last_snapshot = Some(DatabaseSnapshot {
        id: txid.to_string(),
        created_at_unix_secs: now_unix_secs(),
        reason,
        full: true,
        stored_bytes: 0,
    });
}

/// Records the outcome of a backup attempt for `status`.
fn record(state: &ServerState, name: &str, error: Option<String>) {
    let mut backups = state.databases.backups.lock();
    let current = backups.entry(name.to_owned()).or_default();
    let now = now_unix_secs();
    current.last_attempt_unix_secs = Some(now);
    match error {
        None => {
            current.replicated_at_unix_secs = Some(now);
            current.last_error = None;
        }
        Some(error) => current.last_error = Some(error),
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
        replicated_at_unix_secs: current.replicated_at_unix_secs,
        last_attempt_unix_secs: current.last_attempt_unix_secs,
        last_error: current.last_error,
    })
}

fn require_backup(stored: &StoredDatabase) -> Result<StorageBinding, String> {
    stored
        .secrets
        .backup
        .clone()
        .ok_or_else(|| format!("Backups are not configured for database {}.", stored.name))
}

/// The database's configuration as it is now, read while holding its slot's
/// lock. A change of backups takes that lock before it takes effect (see
/// `forget`), so work under the lock never uploads to a destination the
/// database no longer has.
fn current(state: &ServerState, stored: &StoredDatabase) -> Result<StoredDatabase, String> {
    let current = state.hosted_database(&stored.name)?;
    if current.generation != stored.generation {
        return Err(format!(
            "Database {} was deleted and created again meanwhile.",
            stored.name
        ));
    }
    Ok(current)
}

/// Captures and uploads everything so far, then names the transaction it
/// reached, as before a release command or migrations.
pub(crate) async fn mark(
    state: &ServerState,
    stored: &StoredDatabase,
    reason: SnapshotReason,
) -> Result<DatabaseSnapshot, String> {
    let slot = slot(state, &stored.name);
    let mut slot = slot.lock().await;
    let stored = &current(state, stored)?;
    let backup = require_backup(stored)?;
    let result = async {
        ensure_open(state, stored, &backup, &mut slot).await?;
        let txid = sync(state, stored, &mut slot, true).await?;
        let created_at_ms = now_unix_ms();
        slot.open
            .as_mut()
            .expect("opened above")
            .points
            .push(Point {
                created_at_ms,
                txid,
                reason: reason_name(reason).into(),
            });
        upload(state, stored, &mut slot).await?;
        Ok(DatabaseSnapshot {
            id: txid.to_string(),
            created_at_unix_secs: u64::try_from(created_at_ms / 1000).unwrap_or_default(),
            reason,
            full: false,
            stored_bytes: 0,
        })
    }
    .await;
    record(state, &stored.name, result.as_ref().err().cloned());
    result
}

/// Captures a full image now and uploads it, as `tako sqlite backup` does.
/// It restores on its own and is kept like the daily ones.
pub(crate) async fn full_snapshot(
    state: &ServerState,
    stored: &StoredDatabase,
    reason: SnapshotReason,
) -> Result<DatabaseSnapshot, String> {
    let slot = slot(state, &stored.name);
    let mut slot = slot.lock().await;
    let stored = &current(state, stored)?;
    let backup = require_backup(stored)?;
    let result = async {
        let opened = slot.open.is_none();
        ensure_open(state, stored, &backup, &mut slot).await?;
        // Opening captured a full image just now.
        let (txid, created_at_ms) = if opened {
            let open = slot.open.as_ref().expect("opened above");
            let txid = open.capture.position();
            let at = open
                .points
                .iter()
                .find(|point| point.txid == txid)
                .map_or_else(now_unix_ms, |point| point.created_at_ms);
            (txid, at)
        } else {
            sync(state, stored, &mut slot, true).await?;
            let taken = on_capture(&mut slot, full_image).await?;
            let open = slot.open.as_mut().expect("still open");
            open.base_at = Instant::now();
            open.changed_since_base = false;
            taken
        };
        let open = slot.open.as_mut().expect("still open");
        // The image opening captured is this one.
        open.points.retain(|point| point.txid != txid);
        open.points.push(Point {
            created_at_ms,
            txid,
            reason: reason_name(reason).into(),
        });
        upload(state, stored, &mut slot).await?;
        let (replica, _) = replica(stored, &backup, None)?;
        let stored_bytes = with_deadline(METADATA_TIMEOUT, async {
            replica.segments().await.map_err(|e| format!("{e:#}"))
        })
        .await?
        .iter()
        .find(|segment| segment.is_base() && segment.txid() == txid)
        .map_or(0, |segment| segment.size);
        let snapshot = DatabaseSnapshot {
            id: txid.to_string(),
            created_at_unix_secs: u64::try_from(created_at_ms / 1000).unwrap_or_default(),
            reason,
            full: true,
            stored_bytes,
        };
        state
            .databases
            .backups
            .lock()
            .entry(stored.name.clone())
            .or_default()
            .last_snapshot = Some(snapshot.clone());
        Ok(snapshot)
    }
    .await;
    record(state, &stored.name, result.as_ref().err().cloned());
    result
}

/// Stops capture of a database, as before it is deleted or its backups
/// move elsewhere, and clears its spool when `wipe`.
pub(crate) async fn forget(state: &ServerState, name: &str, wipe: bool) -> Result<(), String> {
    let slot = slot(state, name);
    let mut slot = slot.lock().await;
    close(&mut slot).await;
    *slot = Slot::default();
    state.databases.backups.lock().remove(name);
    if wipe {
        wipe_spool(state, name)?;
    }
    Ok(())
}

/// Captures and uploads what each database still has, then stops capture,
/// before this process hands over or exits. The databases share one
/// deadline, [`STOP_TIMEOUT`], and up to [`CONCURRENCY`] stop at once.
pub(crate) async fn stop_all(state: &ServerState) {
    use futures_util::StreamExt;
    let slots: Vec<(String, Arc<tokio::sync::Mutex<Slot>>)> = state
        .databases
        .slots
        .lock()
        .iter()
        .map(|(name, slot)| (name.clone(), slot.clone()))
        .collect();
    let deadline = tokio::time::Instant::now() + STOP_TIMEOUT;
    let stop = |(name, slot): (String, Arc<tokio::sync::Mutex<Slot>>)| async move {
        let stopped = tokio::time::timeout_at(deadline, async {
            let mut slot = slot.lock().await;
            if slot.open.is_none() {
                return;
            }
            let Ok(stored) = state.hosted_database(&name) else {
                close(&mut slot).await;
                return;
            };
            if let Err(error) = sync(state, &stored, &mut slot, true).await {
                tracing::warn!(database = %name, %error, "Final database capture failed");
            }
            if let Err(error) = upload(state, &stored, &mut slot).await {
                tracing::warn!(database = %name, %error, "Final database backup upload failed; it resumes on the next start");
            }
            close(&mut slot).await;
        })
        .await;
        if stopped.is_err() {
            tracing::warn!(database = %name, "Final database backup did not finish in time; it resumes on the next start");
        }
    };
    futures_util::stream::iter(slots)
        .for_each_concurrent(CONCURRENCY, stop)
        .await;
}

/// Every full image and named transaction of a database, with the windows of
/// restorable time.
pub(crate) async fn list_command(
    state: &ServerState,
    id: &str,
) -> Result<serde_json::Value, String> {
    let stored = state.hosted_database(id)?;
    let backup = require_backup(&stored)?;
    let (replica, _) = replica(&stored, &backup, None)?;
    let (segments, points) = with_deadline(METADATA_TIMEOUT, async {
        let segments = replica
            .segments()
            .await
            .map_err(|e| format!("list backups: {e:#}"))?;
        let points = replica
            .points()
            .await
            .map_err(|e| format!("list backups: {e:#}"))?;
        Ok((segments, points))
    })
    .await?;
    // Captures can be uploaded long after they were captured, when storage
    // was unavailable: the bounds are the capture times `restore --at` goes
    // by.
    let windows: Vec<serde_json::Value> = tako_backup::windows(&segments)
        .iter()
        .map(|window| {
            let (first, last) = (window.first(), window.last());
            serde_json::json!({
                "from_transaction": first.txid(),
                "to_transaction": last.txid(),
                "from_unix_ms": first.captured_at_ms,
                "to_unix_ms": last.captured_at_ms,
            })
        })
        .collect();
    let mut snapshots = snapshots(&segments, &points);
    snapshots.sort_by(|a, b| {
        (b.created_at_unix_secs, b.id.parse::<u64>().unwrap_or(0))
            .cmp(&(a.created_at_unix_secs, a.id.parse::<u64>().unwrap_or(0)))
    });
    Ok(serde_json::json!({
        "database": stored.name,
        "windows": windows,
        "snapshots": snapshots,
    }))
}

/// A Unix-millisecond time in UTC, for messages.
fn format_ms(at_ms: i64) -> String {
    time::OffsetDateTime::from_unix_timestamp(at_ms.div_euclid(1000)).map_or_else(
        |_| format!("{at_ms} (Unix ms)"),
        |at| {
            format!(
                "{:04}-{:02}-{:02} {:02}:{:02}:{:02} UTC",
                at.year(),
                at.month() as u8,
                at.day(),
                at.hour(),
                at.minute(),
                at.second()
            )
        },
    )
}

/// Full images and named transactions, one entry per transaction and
/// reason, in no particular order.
fn snapshots(segments: &[Stored], points: &[Point]) -> Vec<DatabaseSnapshot> {
    let bases: BTreeMap<u64, &Stored> = segments
        .iter()
        .filter(|stored| stored.is_base())
        .map(|stored| (stored.txid(), stored))
        .collect();
    let scheduled = reason_name(SnapshotReason::Scheduled);
    // A transaction named for a reason is listed for it, not also as the
    // full image capture took there.
    let named_for: HashSet<u64> = points
        .iter()
        .filter(|point| point.reason != scheduled)
        .map(|point| point.txid)
        .collect();
    let mut seen = HashSet::new();
    let mut snapshots = Vec::new();
    for point in points {
        if !seen.insert((point.txid, point.reason.clone()))
            || (point.reason == scheduled && named_for.contains(&point.txid))
        {
            continue;
        }
        let base = bases.get(&point.txid);
        snapshots.push(DatabaseSnapshot {
            id: point.txid.to_string(),
            created_at_unix_secs: u64::try_from(point.created_at_ms / 1000).unwrap_or_default(),
            reason: parse_reason(&point.reason),
            full: base.is_some(),
            stored_bytes: base.map_or(0, |base| base.size),
        });
    }
    let named: HashSet<u64> = points.iter().map(|point| point.txid).collect();
    for (txid, base) in bases {
        if named.contains(&txid) {
            continue;
        }
        // Captured when capture started.
        snapshots.push(DatabaseSnapshot {
            id: txid.to_string(),
            created_at_unix_secs: u64::try_from(base.captured_at_ms / 1000).unwrap_or_default(),
            reason: SnapshotReason::Scheduled,
            full: true,
            stored_bytes: base.size,
        });
    }
    snapshots
}

/// Applies retention to a database's backups: seven days of history, plus
/// the full images [`retained`] keeps.
async fn prune(stored: &StoredDatabase, backup: &StorageBinding) -> Result<(), String> {
    let (replica, _) = replica(stored, backup, None)?;
    let pruned = with_deadline(UPLOAD_TIMEOUT, async {
        let segments = replica.segments().await.map_err(|e| format!("{e:#}"))?;
        let points = replica.points().await.map_err(|e| format!("{e:#}"))?;
        let full: Vec<DatabaseSnapshot> = {
            let mut full: Vec<_> = snapshots(&segments, &points)
                .into_iter()
                .filter(|snapshot| snapshot.full)
                .collect();
            full.sort_by_key(|snapshot| std::cmp::Reverse(snapshot.created_at_unix_secs));
            full
        };
        let keep: BTreeSet<u64> = retained(&full, now_unix_secs())
            .iter()
            .filter_map(|id| id.parse().ok())
            .collect();
        replica
            .prune(&segments, &points, now_unix_ms(), &keep)
            .await
            .map_err(|e| format!("{e:#}"))
    })
    .await?;
    if pruned.segments + pruned.points > 0 {
        tracing::info!(database = %stored.name, segments = pruned.segments, points = pruned.points, "Pruned database backups");
    }
    Ok(())
}

/// Which full images retention keeps beyond the seven days of history: the
/// newest, every one of the last 24 hours, and the newest of each of the
/// last 7 days and 4 weeks. `snapshots` is newest first.
pub(crate) fn retained(snapshots: &[DatabaseSnapshot], now: u64) -> BTreeSet<String> {
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

/// Deletes every backup object of a database and returns how many were
/// removed. A holder naming no server stays behind as the deletion's fence.
pub(crate) async fn delete_all(stored: &StoredDatabase) -> Result<usize, String> {
    let Some(backup) = &stored.secrets.backup else {
        return Ok(0);
    };
    let remote = store(backup)?;
    // First, and kept: a copy on a server the delete could not reach stops
    // uploading, also when it comes back after the delete.
    claim_holder(remote.as_ref(), stored, DELETED_HOLDER)
        .await
        .map_err(|e| format!("mark the backups deleted: {e}"))?;
    let holder = holder_key(stored);
    let root = ObjectPath::parse(prefix(stored)).map_err(|e| e.to_string())?;
    let keys: Vec<ObjectPath> = with_deadline(METADATA_TIMEOUT, async {
        use futures_util::TryStreamExt;
        remote
            .list(Some(&root))
            .map_ok(|meta| meta.location)
            .try_collect::<Vec<_>>()
            .await
            .map_err(|e| format!("list database backups: {e}"))
    })
    .await?
    .into_iter()
    .filter(|key| key.as_ref() != holder)
    .collect();
    // Markers first, so a partial deletion lists no point without data.
    let (points, data): (Vec<_>, Vec<_>) = keys
        .into_iter()
        .partition(|key| key.as_ref().contains("/points/"));
    let mut deleted = 0;
    // In batches, which S3 deletes in one request each.
    let keys = [points, data];
    for batch in keys.iter().flat_map(|keys| keys.chunks(DELETE_BATCH)) {
        with_deadline(METADATA_TIMEOUT, async {
            use futures_util::StreamExt;
            // The store takes a stream that owns its paths.
            let paths: Vec<ObjectPath> = batch.to_vec();
            let mut deletions =
                remote.delete_stream(futures_util::stream::iter(paths.into_iter().map(Ok)).boxed());
            for _ in batch {
                match deletions.next().await {
                    Some(Ok(_) | Err(object_store::Error::NotFound { .. })) => {}
                    Some(Err(error)) => return Err(format!("delete database backups: {error}")),
                    None => return Err("the store did not acknowledge every deletion".into()),
                }
            }
            Ok(())
        })
        .await?;
        deleted += batch.len();
    }
    Ok(deleted)
}

/// Replaces a database with its state at `target`, after naming the current
/// state with a restore point.
pub(crate) async fn restore(
    state: &ServerState,
    id: &str,
    target: Target,
    key_override: Option<String>,
) -> Result<serde_json::Value, String> {
    state.hosted_database(id)?;
    if let Some(key) = &key_override {
        super::decode_backup_key(key)?;
    }
    let lock = state.databases.lock(id);
    let _operation = lock.lock().await;
    // Read under the lock: a delete and create, or a change of backups, may
    // have finished while this waited for it.
    let stored = state.hosted_database(id)?;
    let backup = require_backup(&stored)?;
    let (replica, remote) = replica(&stored, &backup, key_override.as_deref())?;
    // This server holds the database from now on, so a copy that a move
    // leaves behind stops uploading. Claiming before listing means anything
    // that copy still uploads is below this server's history (see
    // `tako_backup::OWNER_JUMP`). A failed restore gives the claim back.
    let own = state.databases.server_id();
    let previous = match &own {
        Some(own) => {
            let previous = read_holder(remote.as_ref(), &stored).await?;
            if previous.as_deref() != Some(own.as_str()) {
                claim_holder(remote.as_ref(), &stored, own)
                    .await
                    .map_err(|e| {
                        format!("record this server as the holder of {}: {e}", stored.name)
                    })?;
            }
            Some(previous)
        }
        None => None,
    };
    let moved = matches!((&previous, &own), (Some(previous), Some(own)) if previous.as_deref() != Some(own.as_str()));
    let restored = restore_from(state, &stored, &replica, target, moved).await;
    if let (Err(error), Some(previous)) = (&restored, previous)
        && previous != own
    {
        // Another server may have restored the database meanwhile; its
        // claim stands. Only a claim this server reads as its own is given
        // back, and only while it is unchanged since that read.
        let key = ObjectPath::parse(holder_key(&stored)).map_err(|e| e.to_string())?;
        let claim = match read_claim(remote.as_ref(), &key).await {
            Ok(Some(claim)) if claim.server.as_deref() == own.as_deref() => claim,
            Ok(_) => return restored,
            Err(read) => {
                return Err(format!(
                    "{error} (and this server may still be recorded as the holder of {}: {read}; run the restore again or `tako sqlite delete {} --server <this server>`)",
                    stored.name, stored.name
                ));
            }
        };
        let given_back = replace_claim(remote.as_ref(), &key, claim, previous.as_deref()).await;
        if let Err(revert) = given_back {
            return Err(format!(
                "{error} (and the database's previous holder could not be recorded again: {revert})"
            ));
        }
    }
    restored
}

/// Downloads, verifies, and swaps in the database's state at `target`.
async fn restore_from(
    state: &ServerState,
    stored: &StoredDatabase,
    replica: &Replica,
    target: Target,
    moved: bool,
) -> Result<serde_json::Value, String> {
    let segments = with_deadline(METADATA_TIMEOUT, async {
        replica
            .segments()
            .await
            .map_err(|e| format!("list backups: {e:#}"))
    })
    .await?;
    let txid = Replica::resolve(&segments, target)
        .map_err(
            |error| match error.downcast_ref::<tako_backup::TooEarly>() {
                Some(early) => format!(
                    "{error}; the earliest restorable time is {}",
                    format_ms(early.earliest_ms)
                ),
                None => format!("{error:#}"),
            },
        )
        .map_err(|e| format!("Cannot restore {}: {e}.", stored.name))?;
    let plan = Replica::plan(&segments, txid).map_err(|e| format!("{e:#}"))?;
    let captured_at_ms = plan.last().map_or(0, |last| last.captured_at_ms);
    // The downloads and their decrypted forms first; then the restored file,
    // which compressed pages can make far larger, once its size is known.
    // The segments stay on disk for the restore, where the free space
    // already counts them.
    let downloaded: u64 = plan.iter().map(|stored| stored.size).sum();
    let work = fresh_work_dir(state, &stored.name, "restore")?;
    let downloads = reserve_restore(state, stored, &work, downloaded.saturating_mul(2))?;
    let segments_dir = work.path.join("segments");
    let restored = work.path.join("restored.sqlite");
    // A large restore can outlast the age at which interrupted work is
    // cleaned up.
    let _restored_space = keeping_fresh(&work, async {
        let restored_bytes = replica
            .download(&plan, &segments_dir)
            .await
            .map_err(|e| format!("Cannot restore {}: {e:#}.", stored.name))?;
        drop(downloads);
        let space = reserve_restore(state, stored, &work, restored_bytes)?;
        Replica::apply(&segments_dir, txid, &restored)
            .await
            .map_err(|e| format!("Cannot restore {}: {e:#}.", stored.name))?;
        let verify = restored.clone();
        tokio::task::spawn_blocking(move || tako_database::verify_database(&verify))
            .await
            .map_err(|e| e.to_string())??;
        Ok::<_, String>(space)
    })
    .await?;
    let warnings =
        replace_inner(state, stored, restored, "database is being restored", moved).await?;
    tracing::info!(database = %stored.name, transaction = txid, "Restored database");
    Ok(serde_json::json!({
        "database": stored.name,
        "name": stored.name,
        "restored": {
            "transaction": txid,
            "captured_at_unix_ms": captured_at_ms,
        },
        "warnings": warnings,
    }))
}

/// Runs `work` while refreshing `dir`'s age every [`WORK_REFRESH`], so the
/// cleanup of interrupted work, which goes by age, leaves it alone.
async fn keeping_fresh<T>(dir: &WorkDir, work: impl std::future::Future<Output = T>) -> T {
    let mut work = std::pin::pin!(work);
    let mut refresh = tokio::time::interval(WORK_REFRESH);
    refresh.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            output = &mut work => return output,
            _ = refresh.tick() => dir.touch(),
        }
    }
}

fn reserve_restore(
    state: &ServerState,
    stored: &StoredDatabase,
    work: &WorkDir,
    needed: u64,
) -> Result<SpaceReservation, String> {
    let available = crate::operations::available_bytes(&work.path)?;
    state
        .databases
        .snapshot_space
        .reserve(needed, available, "the restore")
        .map_err(|error| format!("Cannot restore {}: {error}.", stored.name))
}

/// Replaces a database's file with a verified `replacement` while its
/// requests are paused. When backups are configured, the contents being
/// replaced are captured and uploaded first and named with a restore point.
/// Returns warnings about that backup; the replaced file is also kept as
/// `previous.sqlite`.
pub(crate) async fn replace(
    state: &ServerState,
    stored: &StoredDatabase,
    replacement: PathBuf,
    pause_reason: &str,
) -> Result<Vec<String>, String> {
    let lock = state.databases.lock(&stored.name);
    let _operation = lock.lock().await;
    replace_locked(state, stored, replacement, pause_reason).await
}

/// [`replace`], for a caller that already holds the database's lock.
pub(crate) async fn replace_locked(
    state: &ServerState,
    stored: &StoredDatabase,
    replacement: PathBuf,
    pause_reason: &str,
) -> Result<Vec<String>, String> {
    if stored.engine == tako_core::DatabaseEngine::Kv {
        return super::snapshots::replace_locked(state, stored, replacement).await;
    }
    replace_inner(state, stored, replacement, pause_reason, false).await
}

/// With `moved`, this server just took the database over: its own earlier
/// contents and spool are not part of the history it continues.
async fn replace_inner(
    state: &ServerState,
    stored: &StoredDatabase,
    replacement: PathBuf,
    pause_reason: &str,
    moved: bool,
) -> Result<Vec<String>, String> {
    let id = DatabaseId::parse(&stored.name)?;
    // The caller read `stored` before it took the lock; a delete and create
    // of the same name meanwhile leaves a different database under it.
    let current = state.hosted_database(&stored.name)?;
    if current.generation != stored.generation {
        return Err(format!(
            "Database {} was replaced by a new database of the same name meanwhile; nothing was changed.",
            stored.name
        ));
    }
    let guard = state
        .databases
        .host
        .pause(&id, pause_reason)
        .await
        .map_err(|e| e.to_string())?;
    let mut warnings = Vec::new();
    let backup = current.secrets.backup.clone();
    let slot = slot(state, &stored.name);
    let mut slot = slot.lock().await;
    if let Some(backup) = &backup
        && !moved
    {
        // Requests are paused, so this captures exactly what is replaced.
        let backed_up = async {
            ensure_open(state, &current, backup, &mut slot).await?;
            let txid = sync(state, &current, &mut slot, true).await?;
            slot.open
                .as_mut()
                .expect("opened above")
                .points
                .push(Point {
                    created_at_ms: now_unix_ms(),
                    txid,
                    reason: reason_name(SnapshotReason::Restore).into(),
                });
            upload(state, &current, &mut slot).await
        }
        .await;
        if let Err(error) = backed_up {
            warnings.push(format!(
                "could not back up the replaced contents (kept as previous.sqlite on the server): {error}"
            ));
        }
    }
    // Capture holds the file open; the swap checkpoints it alone.
    close(&mut slot).await;
    let path = guard.path().to_path_buf();
    let previous = state
        .databases
        .database_dir(&stored.name)
        .join("previous.sqlite");
    let swapped = tokio::task::spawn_blocking(move || {
        rotate_replay_epochs(&replacement)
            .map_err(|e| format!("prepare restored channel replay epochs: {e}"))?;
        tako_database::replace_database(&path, &replacement, &previous)
    })
    .await
    .map_err(|e| e.to_string())?;
    guard.mark_changed();
    drop(guard);
    swapped?;
    // The copy now holds restored (or deliberately replaced) contents.
    if let Err(error) = clear_unrestored(state, &stored.name) {
        warnings.push(error);
    }
    if moved {
        slot.open = None;
        if let Err(error) = wipe_spool(state, &stored.name) {
            warnings.push(error);
        }
    }
    // Capture starts again with a full image of the new contents.
    if let Some(backup) = &backup {
        let resumed = async {
            ensure_open(state, &current, backup, &mut slot).await?;
            let txid = slot.open.as_ref().expect("opened above").capture.position();
            note_base(state, &current, txid, SnapshotReason::Scheduled);
            upload(state, &current, &mut slot).await
        }
        .await;
        if let Err(error) = resumed {
            warnings.push(format!(
                "backups of the new contents will resume on their own: {error}"
            ));
        }
    }
    Ok(warnings)
}

/// Cursors issued before a restore cannot refer to the restored timeline.
/// Change the staged copy only, before its main file is renamed into place.
fn rotate_replay_epochs(path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    use rusqlite::{Connection, OpenFlags, config::DbConfig};

    let db = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
    db.set_db_config(DbConfig::SQLITE_DBCONFIG_TRUSTED_SCHEMA, false)?;
    db.set_db_config(DbConfig::SQLITE_DBCONFIG_ENABLE_TRIGGER, false)?;
    let replay: bool = db.query_row(
        "SELECT EXISTS (SELECT 1 FROM main.sqlite_schema
           WHERE type = 'table' AND name = 'tako_replay_apps' COLLATE NOCASE AND rootpage > 0)
         AND (SELECT count(*) FROM pragma_table_info('tako_replay_apps')
              WHERE name COLLATE NOCASE IN ('app', 'epoch') AND type = 'TEXT' COLLATE NOCASE) = 2",
        [],
        |row| row.get(0),
    )?;
    if replay {
        db.execute(
            "UPDATE main.tako_replay_apps SET epoch = lower(hex(randomblob(16)))",
            [],
        )?;
        // The swap moves only the main file; make the new epochs durable in it.
        let busy: i32 = db.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| row.get(0))?;
        if busy != 0 {
            return Err("could not checkpoint the replacement database".into());
        }
    }
    Ok(())
}

/// Disk space held for restores, resets, and transfers on disk. Several can
/// run at once, so each one checks the free space against what the others
/// still hold.
#[derive(Default)]
pub(crate) struct SnapshotSpace {
    reserved: Arc<parking_lot::Mutex<u64>>,
}

impl SnapshotSpace {
    /// Holds `needed` bytes for `purpose` until the returned reservation
    /// drops, if `available` covers them, the other reservations, and the
    /// disk reserve. Another reservation counts in full even after its files
    /// are partly written, so this errs toward refusing.
    pub(crate) fn reserve(
        &self,
        needed: u64,
        available: u64,
        purpose: &str,
    ) -> Result<SpaceReservation, String> {
        let mut reserved = self.reserved.lock();
        let required = reserved
            .saturating_add(needed)
            .saturating_add(DISK_RESERVE_BYTES);
        if available < required {
            return Err(if *reserved == 0 {
                format!(
                    "not enough free disk space for {purpose}: {required} bytes needed, {available} available"
                )
            } else {
                format!(
                    "not enough free disk space for {purpose}: {required} bytes needed, including {} held for other restores and transfers in progress; {available} available",
                    *reserved
                )
            });
        }
        *reserved = reserved.saturating_add(needed);
        Ok(SpaceReservation {
            reserved: self.reserved.clone(),
            bytes: needed,
        })
    }

    #[cfg(test)]
    pub(crate) fn reserved_bytes(&self) -> u64 {
        *self.reserved.lock()
    }
}

pub(crate) struct SpaceReservation {
    reserved: Arc<parking_lot::Mutex<u64>>,
    bytes: u64,
}

impl Drop for SpaceReservation {
    fn drop(&mut self) {
        let mut reserved = self.reserved.lock();
        *reserved = reserved.saturating_sub(self.bytes);
    }
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

impl WorkDir {
    /// Marks the directory as in use now, so the cleanup of interrupted
    /// work, which goes by its age, leaves it alone. Writing a file inside
    /// does not change the directory's own modification time.
    pub(crate) fn touch(&self) {
        let touched = std::fs::File::open(&self.path).and_then(|dir| {
            dir.set_times(std::fs::FileTimes::new().set_modified(std::time::SystemTime::now()))
        });
        if let Err(error) = touched {
            tracing::debug!(path = %self.path.display(), %error, "Could not refresh a work directory's age");
        }
    }
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
    use std::io::Read;
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

#[cfg(test)]
mod tests {
    use super::*;

    async fn holder_named(remote: &dyn ObjectStore, key: &ObjectPath) -> Option<String> {
        read_claim(remote, key).await.unwrap().unwrap().server
    }

    #[tokio::test(start_paused = true)]
    async fn long_work_keeps_its_directory_from_the_stale_cleanup() {
        let root = tempfile::tempdir().unwrap();
        let dir = WorkDir {
            path: root.path().join("restore-1"),
        };
        std::fs::create_dir(&dir.path).unwrap();
        let two_days_ago = std::time::SystemTime::now() - Duration::from_secs(2 * 24 * 60 * 60);
        let age_dir = |at| {
            std::fs::File::open(&dir.path)
                .unwrap()
                .set_times(std::fs::FileTimes::new().set_modified(at))
                .unwrap();
        };
        age_dir(two_days_ago);
        // Two hours of work on the paused clock.
        keeping_fresh(&dir, async {
            tokio::time::sleep(Duration::from_secs(90 * 60)).await;
            age_dir(two_days_ago);
            tokio::time::sleep(Duration::from_secs(40 * 60)).await;
        })
        .await;
        assert_eq!(
            remove_stale_work_dirs(root.path(), std::time::SystemTime::now()),
            0
        );
        assert!(dir.path.exists());
    }

    #[tokio::test]
    async fn a_claim_is_given_back_only_while_it_is_unchanged() {
        let remote = object_store::memory::InMemory::new();
        let key = ObjectPath::from("_tako/databases/main/1/holder");
        let claim = |server: &str| {
            serde_json::to_vec(&Holder {
                server: server.into(),
                since_unix_secs: 0,
            })
            .unwrap()
        };
        remote.put(&key, claim("server-a").into()).await.unwrap();

        // Another server claims after this one read its own claim.
        let read = read_claim(&remote, &key).await.unwrap().unwrap();
        remote.put(&key, claim("server-b").into()).await.unwrap();
        assert!(!replace_claim(&remote, &key, read, None).await.unwrap());
        assert_eq!(
            holder_named(&remote, &key).await.as_deref(),
            Some("server-b")
        );

        // Unchanged, the claim is given up, and the holder names no server.
        let read = read_claim(&remote, &key).await.unwrap().unwrap();
        assert!(replace_claim(&remote, &key, read, None).await.unwrap());
        assert_eq!(holder_named(&remote, &key).await, None);
    }
}
