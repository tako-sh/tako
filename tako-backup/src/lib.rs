//! Embedded SQLite WAL capture, encrypted uploads, retention, and recovery.

use std::{
    fs,
    fs::File,
    io::Write,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant, SystemTime},
};

use anyhow::{Context, Result, ensure};
use bytes::Bytes;
use futures::TryStreamExt;
use object_store::{GetOptions, ObjectStore, PutMode, PutOptions, path::Path as ObjectPath};
use tempfile::NamedTempFile;
use terse_litestream::{Database, FileStore, Options, Segment};

pub mod replica;
mod retention;
pub use terse_litestream::{FileStore as Spool, ReplicaStore};
pub mod encryption;
pub use retention::{RETENTION, RETENTION_INTERVAL, prune_expired};

/// Capture and upload together so busy databases normally produce one object per minute.
pub const BACKUP_INTERVAL: Duration = Duration::from_secs(60);
/// An unavailable object store is retried less often, while local capture continues.
pub const MAX_RETRY_INTERVAL: Duration = Duration::from_secs(15 * 60);

/// Caller-driven schedule with minute-sized capture batches and exponential upload backoff.
pub struct BackupLoop {
    capture_at: Instant,
    upload_at: Instant,
    retry: Duration,
    prune_at: Instant,
}

impl BackupLoop {
    /// The first tick captures and uploads immediately. Later captures batch one minute.
    pub fn new(now: Instant) -> Self {
        Self {
            capture_at: now,
            upload_at: now,
            retry: BACKUP_INTERVAL,
            prune_at: now,
        }
    }

    /// Capture keeps running once per minute during upload outages; idle ticks do no remote I/O.
    pub async fn tick(
        &mut self,
        now: Instant,
        capture: &mut Capture,
        remote: &dyn ObjectStore,
        prefix: &ObjectPath,
    ) -> Result<usize> {
        if now >= self.capture_at {
            capture.sync()?;
            self.capture_at = now + BACKUP_INTERVAL;
        }
        if now < self.upload_at {
            return Ok(0);
        }
        let result = async {
            let uploaded = upload_pending(&capture.spool(), remote, prefix).await?;
            if now >= self.prune_at {
                self.prune_at = now + RETENTION_INTERVAL;
                prune_expired(&capture.spool(), remote, prefix, SystemTime::now()).await?;
            }
            Ok(uploaded)
        }
        .await;
        let delay = if result.is_ok() {
            self.retry = BACKUP_INTERVAL;
            BACKUP_INTERVAL
        } else {
            let delay = self.retry;
            self.retry = self.retry.saturating_mul(2).min(MAX_RETRY_INTERVAL);
            delay
        };
        self.upload_at = now + delay;
        result
    }
}

/// One capture owner for an existing SQLite database. Run capture on a blocking worker.
pub struct Capture {
    database: Database,
    spool: FileStore,
    _lock: CaptureLock,
}

/// Held while capturing or while replacing the source during a restore.
pub struct CaptureLock(File);

impl Drop for CaptureLock {
    fn drop(&mut self) {
        // Explicit unlock also releases descriptors briefly inherited by a
        // concurrent fork before exec, rather than waiting for the child.
        let _ = self.0.unlock();
    }
}

pub fn lock_capture(directory: &Path) -> Result<CaptureLock> {
    fs::create_dir_all(directory)?;
    let lock = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(directory.join("capture.lock"))?;
    lock.try_lock()
        .context("spool already has a capture owner")?;
    Ok(CaptureLock(lock))
}

impl Capture {
    /// Opens a durable local spool. Keep one dedicated spool and remote prefix per database.
    pub fn open(database: &Path, directory: &Path) -> Result<Self> {
        let source = fs::canonicalize(database)?;
        let lock = lock_capture(directory)?;
        let identity = directory.join("source");
        let source = source.to_str().context("source path must be UTF-8")?;
        if identity.exists() {
            ensure!(
                fs::read_to_string(&identity)? == source,
                "spool belongs to another database"
            );
        } else {
            persist(directory, &identity, source.as_bytes())?;
        }
        let spool = FileStore::new(directory);
        let database = Database::with_store(database, Options::default(), Arc::new(spool.clone()))?;
        Ok(Self {
            database,
            spool,
            _lock: lock,
        })
    }

    /// Captures committed changes durably on this machine, returning the latest local TXID.
    pub fn sync(&mut self) -> Result<u64> {
        self.database.sync()?;
        Ok(self.database.position())
    }

    /// Uploading a cloned spool does not borrow the SQLite capture handle.
    pub fn spool(&self) -> FileStore {
        self.spool.clone()
    }
}

/// Uploads pending files, acknowledging locally only after the remote object is durable.
///
/// No remote LIST or HEAD is used. An ambiguous PUT is retried with create-only semantics;
/// an existing object is fetched and compared before its receipt is persisted.
pub async fn upload_pending(
    spool: &FileStore,
    remote: &dyn ObjectStore,
    prefix: &ObjectPath,
) -> Result<usize> {
    let _lock = upload_lock(spool, remote, prefix)?;
    let mut uploaded = 0;
    for level in [9, 0] {
        for segment in spool.list(level)? {
            uploaded += usize::from(upload_segment(spool, remote, prefix, &segment).await?);
        }
    }
    Ok(uploaded)
}

fn upload_lock(
    spool: &FileStore,
    remote: &dyn ObjectStore,
    prefix: &ObjectPath,
) -> Result<CaptureLock> {
    let receipts = spool.root().join("uploaded");
    fs::create_dir_all(&receipts)?;
    File::open(spool.root())?.sync_all()?;
    let lock = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(receipts.join("upload.lock"))?;
    lock.try_lock().context("spool already has an uploader")?;
    let lock = CaptureLock(lock);
    let destination = receipts.join("destination");
    let identity = format!("{remote}\n{prefix}");
    if destination.exists() {
        ensure!(
            fs::read_to_string(&destination)? == identity,
            "upload destination changed"
        );
    } else {
        persist(&receipts, &destination, identity.as_bytes())?;
    }
    Ok(lock)
}

fn receipt_path(spool: &FileStore, segment: &Segment) -> PathBuf {
    spool
        .root()
        .join("uploaded")
        .join(format!("{}-{}", segment.level, segment.filename()))
}

fn segment_key(prefix: &ObjectPath, segment: &Segment) -> ObjectPath {
    prefix
        .clone()
        .join("ltx")
        .join(segment.level.to_string())
        .join(segment.filename())
}

async fn upload_segment(
    spool: &FileStore,
    remote: &dyn ObjectStore,
    prefix: &ObjectPath,
    segment: &Segment,
) -> Result<bool> {
    let receipt = receipt_path(spool, segment);
    if receipt.exists() {
        return Ok(false);
    }
    let key = segment_key(prefix, segment);
    // Segment files are immutable and the upload lease excludes retention.
    // Mapping lets the kernel reclaim pages during a large upload.
    let file = File::open(spool.path(segment))?;
    let bytes = unsafe { Bytes::from_owner(memmap2::Mmap::map(&file)?) };
    let options = PutOptions {
        mode: PutMode::Create,
        ..Default::default()
    };
    match remote.put_opts(&key, bytes.clone().into(), options).await {
        Ok(_) => {}
        Err(object_store::Error::AlreadyExists { .. }) => {
            let existing = remote
                .get_opts(&key, GetOptions::default())
                .await?
                .bytes()
                .await?;
            ensure!(
                existing == bytes,
                "remote object conflicts with capture: {key}"
            );
        }
        Err(error) => return Err(error.into()),
    }
    persist(&spool.root().join("uploaded"), &receipt, b"")?;
    Ok(true)
}

/// Restores from remote objects alone. Listing is confined to explicit restore operations.
/// The target is an available capture TXID, not an arbitrary SQL transaction or timestamp.
pub async fn restore_remote(
    remote: &dyn ObjectStore,
    prefix: &ObjectPath,
    output: &Path,
    target: Option<u64>,
) -> Result<u64> {
    ensure!(!output.exists(), "restore output already exists");
    let directory = tempfile::tempdir()?;
    let store = FileStore::new(directory.path());
    download_replica(remote, prefix, &store).await?;
    terse_litestream::restore(&store, output, target)
}

#[derive(Debug, Clone)]
pub struct RecoveryPoint {
    pub txid: u64,
    pub uploaded_at_unix_secs: i64,
    pub size_bytes: u64,
}

fn parse_segment(prefix: &ObjectPath, key: &ObjectPath) -> Result<Segment> {
    let relative = key
        .as_ref()
        .strip_prefix(&format!("{prefix}/ltx/"))
        .context("object is outside replica prefix")?;
    let (level, name) = relative.split_once('/').context("invalid segment key")?;
    let stem = name
        .strip_suffix(".ltx")
        .context("invalid segment extension")?;
    ensure!(
        stem.len() == 33 && stem.as_bytes()[16] == b'-',
        "invalid segment name"
    );
    let level: u8 = level.parse()?;
    ensure!(level == 0 || level == 9, "invalid replica level");
    Segment::new(
        level,
        u64::from_str_radix(stem.get(..16).context("invalid TXID")?, 16)?,
        u64::from_str_radix(stem.get(17..).context("invalid TXID")?, 16)?,
    )
}

pub async fn recovery_points(
    remote: &dyn ObjectStore,
    prefix: &ObjectPath,
) -> Result<Vec<RecoveryPoint>> {
    let mut objects = remote.list(Some(&prefix.clone().join("ltx")));
    let mut points = std::collections::BTreeMap::new();
    while let Some(object) = objects.try_next().await? {
        let segment = parse_segment(prefix, &object.location)?;
        points.insert(
            segment.max_txid,
            RecoveryPoint {
                txid: segment.max_txid,
                uploaded_at_unix_secs: object.last_modified.timestamp(),
                size_bytes: object.size,
            },
        );
    }
    Ok(points.into_values().rev().collect())
}

/// Seed an empty local spool before starting capture on a replacement server.
/// Reopening capture then writes a full snapshot above the remote high-water mark.
pub async fn download_replica(
    remote: &dyn ObjectStore,
    prefix: &ObjectPath,
    store: &FileStore,
) -> Result<()> {
    let mut objects = remote.list(Some(&prefix.clone().join("ltx")));
    while let Some(object) = objects.try_next().await? {
        let segment = parse_segment(prefix, &object.location)?;
        if store.path(&segment).exists() {
            continue;
        }
        let bytes = remote
            .get_opts(&object.location, GetOptions::default())
            .await?
            .bytes()
            .await?;
        let mut writer = store.create(&segment)?;
        writer.write_all(&bytes)?;
        writer.commit()?;
    }
    Ok(())
}

/// The greatest locally captured point and greatest acknowledged remote point.
pub fn positions(spool: &FileStore) -> Result<(Option<u64>, Option<u64>)> {
    let mut captured = None;
    let mut uploaded = None;
    for level in [0, 9] {
        for segment in spool.list(level)? {
            captured = captured.max(Some(segment.max_txid));
            if receipt_path(spool, &segment).exists() {
                uploaded = uploaded.max(Some(segment.max_txid));
            }
        }
    }
    Ok((captured, uploaded))
}

fn persist(directory: &Path, destination: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = NamedTempFile::new_in(directory)?;
    file.write_all(bytes)?;
    file.as_file().sync_all()?;
    file.persist_noclobber(destination)?;
    File::open(directory)?.sync_all()?;
    Ok(())
}
