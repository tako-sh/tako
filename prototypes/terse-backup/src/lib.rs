//! Isolated experiment: terse captures locally; async uploads never list the bucket.

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
use terse_litestream::{Database, FileStore, Options, ReplicaStore, Segment};

mod retention;
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
    _lock: File,
}

impl Capture {
    /// Opens a durable local spool. Keep one dedicated spool and remote prefix per database.
    pub fn open(database: &Path, directory: &Path) -> Result<Self> {
        let source = fs::canonicalize(database)?;
        fs::create_dir_all(directory)?;
        let lock = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(directory.join("capture.lock"))?;
        lock.try_lock()
            .context("spool already has a capture owner")?;
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

fn upload_lock(spool: &FileStore, remote: &dyn ObjectStore, prefix: &ObjectPath) -> Result<File> {
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
    // shortcut: buffer one segment; stream large transactions before production use.
    let bytes = Bytes::from(fs::read(spool.path(segment))?);
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
    let prefix = prefix.clone().join("ltx");
    let mut objects = remote.list(Some(&prefix));
    while let Some(object) = objects.try_next().await? {
        let relative = object
            .location
            .as_ref()
            .strip_prefix(&format!("{prefix}/"))
            .context("object is outside restore prefix")?;
        let (level, name) = relative.split_once('/').context("invalid segment key")?;
        let stem = name
            .strip_suffix(".ltx")
            .context("invalid segment extension")?;
        ensure!(
            stem.len() == 33 && stem.as_bytes()[16] == b'-',
            "invalid segment name"
        );
        let segment = Segment::new(
            level.parse()?,
            u64::from_str_radix(stem.get(..16).context("invalid TXID")?, 16)?,
            u64::from_str_radix(stem.get(17..).context("invalid TXID")?, 16)?,
        )?;
        let bytes = remote
            .get_opts(&object.location, GetOptions::default())
            .await?
            .bytes()
            .await?;
        let mut writer = store.create(&segment)?;
        writer.write_all(&bytes)?;
        writer.commit()?;
    }
    terse_litestream::restore(&store, output, target)
}

fn persist(directory: &Path, destination: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = NamedTempFile::new_in(directory)?;
    file.write_all(bytes)?;
    file.as_file().sync_all()?;
    file.persist_noclobber(destination)?;
    File::open(directory)?.sync_all()?;
    Ok(())
}
