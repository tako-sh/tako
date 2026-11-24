//! Continuous backups of SQLite databases.
//!
//! [`Capture`] turns committed WAL pages into LTX files in a local spool.
//! [`Replica`] uploads them, encrypted, to object storage, restores a
//! database at any captured transaction, and applies retention.
//!
//! Remote layout under a replica's prefix:
//!
//! - `ltx/<level>/<min>-<max>-<captured>.ltx`: encrypted LTX segments, with
//!   16-digit hexadecimal transaction IDs and the capture time from the
//!   segment's header, in Unix milliseconds. Level 9 holds full images
//!   (bases) that start at transaction 1; level 0 holds one capture each.
//! - `points/<unix-ms>-<txid>-<reason>`: empty markers that name a captured
//!   transaction, such as the one before a deploy.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    fs::File,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result, bail, ensure};
use bytes::Bytes;
use futures::{StreamExt, TryStreamExt, stream};
use object_store::{
    GetOptions, ObjectStore, ObjectStoreExt, PutMode, PutOptions, path::Path as ObjectPath,
};
use tempfile::NamedTempFile;
use terse_litestream::{Database, Durability, FileStore, Options, SegmentWriter};

mod committed;
pub use committed::CommittedSpool;
pub mod encryption;
pub use anyhow;
pub use terse_litestream::{FileStore as Spool, ReplicaStore, Segment};

/// How often a changed database is captured and uploaded. Idle databases
/// make no remote requests.
pub const SYNC_INTERVAL: Duration = Duration::from_secs(10);
/// An unavailable object store is retried at most this far apart, while
/// local capture continues.
pub const MAX_RETRY_INTERVAL: Duration = Duration::from_secs(15 * 60);
/// Every transaction of the last seven days stays restorable.
pub const RETENTION: Duration = Duration::from_secs(7 * 24 * 60 * 60);
/// A changing database gets a new full image this often, so a restore
/// replays at most about a day of captures.
pub const SNAPSHOT_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);
/// How far above the existing history a capture with an empty spool starts.
/// A previous owner that has not yet noticed it lost the database can keep
/// uploading for a while; its transactions stay below this owner's, so no
/// restore chain mixes the two.
pub const OWNER_JUMP: u64 = 1 << 32;
/// Segments downloaded at once by a restore.
const DOWNLOADS: usize = 8;
/// Segments larger than this are uploaded in parts: single requests are
/// limited to 5 GiB on S3 and R2.
pub const MULTIPART_THRESHOLD: u64 = 1 << 30;
/// The size of each part of a multipart upload; 10,000 parts of it reach
/// 640 GiB.
pub const PART_BYTES: usize = 64 << 20;

/// One capture owner for an existing SQLite database in WAL mode. Run its
/// methods on a blocking worker.
pub struct Capture {
    database: Database,
    spool: FileStore,
    _lock: CaptureLock,
}

/// Held while capturing, or while uploading from a spool.
pub struct CaptureLock(File);

impl Drop for CaptureLock {
    fn drop(&mut self) {
        // Explicit unlock also releases descriptors briefly inherited by a
        // concurrent fork before exec, rather than waiting for the child.
        let _ = self.0.unlock();
    }
}

fn lock_file(path: &Path, owner: &str) -> Result<CaptureLock> {
    let lock = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    lock.try_lock()
        .with_context(|| format!("spool already has {owner}"))?;
    Ok(CaptureLock(lock))
}

impl Capture {
    /// Opens capture with its spool in `directory`, at a position of at
    /// least `floor` (see [`Replica::floor`]). Keep one spool and one remote
    /// prefix per database. The first capture after opening is a full image.
    pub fn open(database: &Path, directory: &Path, floor: u64) -> Result<Self> {
        let source = fs::canonicalize(database)?;
        fs::create_dir_all(directory)?;
        let lock = lock_file(&directory.join("capture.lock"), "a capture owner")?;
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
        let options = Options {
            busy_timeout: Duration::from_secs(5),
            ..Options::default()
        };
        let database =
            Database::with_store_from(database, options, Arc::new(spool.clone()), floor)?;
        Ok(Self {
            database,
            spool,
            _lock: lock,
        })
    }

    /// Captures committed changes durably on this machine and returns the
    /// latest local transaction.
    pub fn sync(&mut self) -> Result<u64> {
        self.database.sync()?;
        Ok(self.database.position())
    }

    /// Captures a full image of the database at a new transaction, which
    /// restores without any earlier segment.
    pub fn snapshot(&mut self) -> Result<u64> {
        self.database.snapshot()?;
        Ok(self.database.position())
    }

    /// When a segment still in the spool was captured, from its header.
    pub fn captured_at(&self, txid: u64) -> Result<i64> {
        for level in [9, 0] {
            if let Some(segment) = self
                .spool
                .list(level)?
                .into_iter()
                .find(|segment| segment.max_txid == txid)
            {
                let decoder = terse_ltx::Decoder::new(self.spool.open(&segment)?)?;
                return Ok(decoder.header().timestamp);
            }
        }
        bail!("transaction {txid} is not in the spool")
    }

    pub fn position(&self) -> u64 {
        self.database.position()
    }

    /// Captures what is left and releases the database and the spool.
    pub fn close(self) -> Result<u64> {
        let position = self.database.close()?;
        Ok(position)
    }

    /// The spool, for uploads; cloning does not borrow the capture.
    pub fn spool(&self) -> FileStore {
        self.spool.clone()
    }
}

/// Whether a spool holds any segment, so capture resumes its position.
pub fn spool_has_segments(directory: &Path) -> Result<bool> {
    let spool = FileStore::new(directory);
    for level in 0..=9 {
        if !spool.list(level)?.is_empty() {
            return Ok(true);
        }
    }
    Ok(false)
}

/// A segment in the remote store.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stored {
    pub segment: Segment,
    /// Size of the encrypted object.
    pub size: u64,
    /// When the segment was captured, in Unix milliseconds: its header's
    /// time, which its object name carries, so a listing dates it.
    pub captured_at_ms: i64,
    /// When the object was written, in Unix milliseconds.
    pub uploaded_at_ms: i64,
}

impl Stored {
    /// A full image, which restores on its own.
    pub fn is_base(&self) -> bool {
        self.segment.min_txid == 1
    }

    pub fn txid(&self) -> u64 {
        self.segment.max_txid
    }
}

/// A run of consecutive transactions that can each be restored: a base,
/// then the captures after it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Window {
    /// Starts with the base; one segment per transaction.
    pub members: Vec<Stored>,
}

impl Window {
    pub fn first(&self) -> &Stored {
        &self.members[0]
    }

    pub fn last(&self) -> &Stored {
        self.members.last().expect("windows are not empty")
    }

    pub fn contains(&self, txid: u64) -> bool {
        (self.first().txid()..=self.last().txid()).contains(&txid)
    }
}

/// The restorable windows of a replica, oldest first. Segments in a run
/// without a base (none yet uploaded, or one retention removed) belong to
/// no window.
pub fn windows(segments: &[Stored]) -> Vec<Window> {
    let mut by_txid: BTreeMap<u64, Stored> = BTreeMap::new();
    for stored in segments {
        by_txid
            .entry(stored.txid())
            .and_modify(|current| {
                if stored.segment.level > current.segment.level {
                    *current = *stored;
                }
            })
            .or_insert(*stored);
    }
    let mut windows = Vec::new();
    let mut run: Vec<Stored> = Vec::new();
    let mut flush = |run: &mut Vec<Stored>| {
        if let Some(start) = run.iter().position(Stored::is_base) {
            windows.push(Window {
                members: run[start..].to_vec(),
            });
        }
        run.clear();
    };
    for stored in by_txid.into_values() {
        if run
            .last()
            .is_some_and(|last| last.txid().checked_add(1) != Some(stored.txid()))
        {
            flush(&mut run);
        }
        run.push(stored);
    }
    flush(&mut run);
    windows
}

/// A named transaction: the state before a deploy, a manual snapshot, and
/// the like.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Point {
    pub created_at_ms: i64,
    pub txid: u64,
    /// Lowercase letters and `_`.
    pub reason: String,
}

/// What to restore.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    /// The newest restorable transaction.
    Latest,
    Txid(u64),
    /// The newest transaction captured at or before this time (Unix ms).
    Time(i64),
}

/// What retention removed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Pruned {
    pub segments: usize,
    pub points: usize,
}

/// A database's history in object storage, encrypted with its key.
pub struct Replica {
    /// Encrypts segments; markers go to `raw`.
    store: Arc<dyn ObjectStore>,
    encrypted: Arc<encryption::EncryptedStore>,
    raw: Arc<dyn ObjectStore>,
    prefix: ObjectPath,
    multipart_threshold: u64,
    part_bytes: usize,
}

impl Replica {
    /// `prefix` is the database's own prefix; it is also authenticated with
    /// every segment, so a segment cannot be substituted from elsewhere.
    pub fn new(raw: Arc<dyn ObjectStore>, prefix: &str, key: encryption::Key) -> Result<Self> {
        let prefix = ObjectPath::parse(prefix)?;
        let encrypted = Arc::new(encryption::EncryptedStore::new(
            raw.clone(),
            vec![key],
            prefix.to_string(),
        )?);
        Ok(Self {
            store: encrypted.clone(),
            encrypted,
            raw,
            prefix,
            multipart_threshold: MULTIPART_THRESHOLD,
            part_bytes: PART_BYTES,
        })
    }

    /// Uploads segments larger than `threshold` in parts of `part_bytes`.
    pub fn with_multipart(mut self, threshold: u64, part_bytes: usize) -> Self {
        self.multipart_threshold = threshold;
        self.part_bytes = part_bytes;
        self
    }

    pub fn prefix(&self) -> &ObjectPath {
        &self.prefix
    }

    /// `ltx/<level>/<min>-<max>-<captured ms>.ltx`. The name is
    /// authenticated with the contents, so a renamed object fails to
    /// decrypt.
    fn segment_key(&self, segment: &Segment, captured_at_ms: i64) -> ObjectPath {
        let name = segment.filename();
        let stem = name.strip_suffix(".ltx").unwrap_or(&name);
        self.prefix
            .clone()
            .join("ltx")
            .join(segment.level.to_string())
            .join(format!("{stem}-{captured_at_ms}.ltx"))
    }

    fn key(&self, stored: &Stored) -> ObjectPath {
        self.segment_key(&stored.segment, stored.captured_at_ms)
    }

    fn point_key(&self, point: &Point) -> ObjectPath {
        self.prefix.clone().join("points").join(format!(
            "{:013}-{:016x}-{}",
            point.created_at_ms, point.txid, point.reason
        ))
    }

    /// The position a capture with the spool in `directory` must start
    /// above: none when the spool already has a position, otherwise far past
    /// the remote history ([`OWNER_JUMP`]).
    pub async fn floor(&self, directory: &Path) -> Result<u64> {
        if spool_has_segments(directory)? {
            return Ok(0);
        }
        Ok(self
            .segments()
            .await?
            .iter()
            .map(Stored::txid)
            .max()
            .map_or(0, |max| max.saturating_add(OWNER_JUMP)))
    }

    /// Uploads pending segments in transaction order, recording each only
    /// once the remote object is durable, then removes uploaded segments from
    /// the spool except the newest, which holds the capture's position.
    ///
    /// No remote LIST or HEAD is used. A segment is created only where none
    /// exists; an existing object is compared before it is accepted.
    pub async fn upload(&self, directory: &Path) -> Result<Vec<Segment>> {
        let spool = FileStore::new(directory);
        let _lock = self.upload_lock(&spool)?;
        let mut pending = Vec::new();
        for level in [0, 9] {
            for segment in spool.list(level)? {
                if !receipt_path(&spool, &segment).exists() {
                    pending.push(segment);
                }
            }
        }
        pending.sort_by_key(|segment| (segment.max_txid, segment.level));
        for segment in &pending {
            self.upload_segment(&spool, segment).await?;
        }
        trim(&spool)?;
        Ok(pending)
    }

    /// The local segments not yet uploaded.
    pub fn pending(directory: &Path) -> Result<Vec<Segment>> {
        let spool = FileStore::new(directory);
        let mut pending = Vec::new();
        for level in 0..=9 {
            for segment in spool.list(level)? {
                if !receipt_path(&spool, &segment).exists() {
                    pending.push(segment);
                }
            }
        }
        Ok(pending)
    }

    /// Those of `segments` that are uploaded now, such as the ones an
    /// upload that failed part way already sent.
    pub fn receipted(directory: &Path, segments: &[Segment]) -> Vec<Segment> {
        let spool = FileStore::new(directory);
        segments
            .iter()
            .filter(|segment| receipt_path(&spool, segment).exists())
            .copied()
            .collect()
    }

    /// Whether every local segment is uploaded.
    pub fn uploaded(directory: &Path) -> Result<bool> {
        let spool = FileStore::new(directory);
        for level in 0..=9 {
            for segment in spool.list(level)? {
                if !receipt_path(&spool, &segment).exists() {
                    return Ok(false);
                }
            }
        }
        Ok(true)
    }

    fn upload_lock(&self, spool: &FileStore) -> Result<CaptureLock> {
        let receipts = spool.root().join("uploaded");
        fs::create_dir_all(&receipts)?;
        File::open(spool.root())?.sync_all()?;
        let lock = lock_file(&receipts.join("upload.lock"), "an uploader")?;
        let destination = receipts.join("destination");
        let identity = format!("{}\n{}", self.raw, self.prefix);
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

    async fn upload_segment(&self, spool: &FileStore, segment: &Segment) -> Result<()> {
        let bytes = {
            let path = spool.path(segment);
            tokio::task::spawn_blocking(move || -> Result<Bytes> {
                let file = File::open(path)?;
                if file.metadata()?.len() == 0 {
                    return Ok(Bytes::new());
                }
                // Segment files are immutable and the upload lock excludes
                // trimming. Mapping lets the kernel reclaim pages during a
                // large upload.
                Ok(unsafe { Bytes::from_owner(memmap2::Mmap::map(&file)?) })
            })
            .await??
        };
        let captured_at_ms = terse_ltx::Decoder::new(std::io::Cursor::new(&bytes[..]))
            .with_context(|| format!("read the header of segment {}", segment.filename()))?
            .header()
            .timestamp;
        ensure!(
            captured_at_ms >= 0,
            "segment {} has a capture time before 1970",
            segment.filename()
        );
        encryption::ensure_sealable(bytes.len() as u64)
            .with_context(|| format!("segment {} cannot be backed up", segment.filename()))?;
        let key = self.segment_key(segment, captured_at_ms);
        let stored = if bytes.len() as u64 > self.multipart_threshold {
            // A multipart write cannot be create-only, so an existing object
            // is looked for first. Two owners never write one key (see
            // `OWNER_JUMP` and the callers' holder checks); this catches a
            // retry after a lost acknowledgement.
            match self.raw.head(&key).await {
                Ok(_) => Err(object_store::Error::AlreadyExists {
                    path: key.to_string(),
                    source: "the segment was uploaded before".into(),
                }),
                Err(object_store::Error::NotFound { .. }) => self
                    .encrypted
                    .put_sealed_in_parts(&key, bytes.clone().into(), self.part_bytes)
                    .await
                    .map(|_| ()),
                Err(error) => Err(error),
            }
        } else {
            let options = PutOptions {
                mode: PutMode::Create,
                ..Default::default()
            };
            self.store
                .put_opts(&key, bytes.clone().into(), options)
                .await
                .map(|_| ())
        };
        match stored {
            Ok(()) => {}
            Err(object_store::Error::AlreadyExists { .. }) => {
                let existing = self
                    .store
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
        persist(
            &spool.root().join("uploaded"),
            &receipt_path(spool, segment),
            b"",
        )?;
        Ok(())
    }

    /// Every segment in the remote store, in transaction order.
    pub async fn segments(&self) -> Result<Vec<Stored>> {
        let prefix = self.prefix.clone().join("ltx");
        let mut objects = self.raw.list(Some(&prefix));
        let mut segments = Vec::new();
        while let Some(object) = objects.try_next().await? {
            // Other objects under the prefix are not this replica's.
            let Some((segment, captured_at_ms)) = parse_segment(&self.prefix, &object.location)
            else {
                continue;
            };
            segments.push(Stored {
                segment,
                size: object.size,
                captured_at_ms,
                uploaded_at_ms: object.last_modified.timestamp_millis(),
            });
        }
        segments.sort_by_key(|stored| (stored.txid(), stored.segment.level));
        Ok(segments)
    }

    /// Names a transaction. Markers are not encrypted: they hold only a time,
    /// a transaction ID and a reason.
    pub async fn put_point(&self, point: &Point) -> Result<()> {
        ensure!(
            !point.reason.is_empty()
                && point.reason.len() <= 32
                && point
                    .reason
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b == b'_'),
            "invalid restore point reason"
        );
        self.raw
            .put_opts(
                &self.point_key(point),
                Bytes::new().into(),
                PutOptions::default(),
            )
            .await?;
        Ok(())
    }

    /// Every named transaction, oldest first.
    pub async fn points(&self) -> Result<Vec<Point>> {
        let prefix = self.prefix.clone().join("points");
        let mut objects = self.raw.list(Some(&prefix));
        let mut points = Vec::new();
        while let Some(object) = objects.try_next().await? {
            if let Some(point) = object.location.filename().and_then(parse_point) {
                points.push(point);
            }
        }
        points.sort_by_key(|point| (point.created_at_ms, point.txid));
        Ok(points)
    }

    /// The transaction a target selects, which is restorable. Times are
    /// the segments' capture times, which the listing carries.
    pub fn resolve(segments: &[Stored], target: Target) -> Result<u64> {
        let windows = windows(segments);
        match target {
            Target::Latest => Ok(windows.last().context("no backups yet")?.last().txid()),
            Target::Txid(txid) => {
                ensure!(
                    windows.iter().any(|window| window.contains(txid)),
                    "transaction {txid} is not in the backups"
                );
                Ok(txid)
            }
            Target::Time(at) => {
                for window in windows.iter().rev() {
                    if let Some(txid) = newest_at(window, at) {
                        return Ok(txid);
                    }
                }
                let first = windows.first().context("no backups yet")?.first();
                Err(TooEarly {
                    earliest_ms: first.captured_at_ms,
                }
                .into())
            }
        }
    }

    /// The segments a restore of `txid` downloads, in order.
    pub fn plan(segments: &[Stored], txid: u64) -> Result<Vec<Stored>> {
        let listing = Listing(segments.iter().map(|stored| stored.segment).collect());
        let plan = terse_litestream::restore_plan(&listing, Some(txid))?;
        Ok(plan
            .iter()
            .filter_map(|segment| {
                segments
                    .iter()
                    .find(|stored| stored.segment == *segment)
                    .copied()
            })
            .collect())
    }

    /// Restores transaction `txid` (see [`Self::resolve`]) to `output`, which
    /// must not exist, downloading only the segments it needs into `scratch`.
    pub async fn restore(
        &self,
        segments: &[Stored],
        txid: u64,
        output: &Path,
        scratch: &Path,
    ) -> Result<()> {
        let plan = Self::plan(segments, txid)?;
        self.download(&plan, scratch).await?;
        Self::apply(scratch, txid, output).await
    }

    /// Downloads a restore's segments (see [`Self::plan`]) into `scratch`
    /// and returns the size of the database they restore.
    pub async fn download(&self, plan: &[Stored], scratch: &Path) -> Result<u64> {
        let spool = FileStore::with_durability(scratch, Durability::Scratch);
        let segments: Vec<(Segment, ObjectPath)> = plan
            .iter()
            .map(|stored| (stored.segment, self.key(stored)))
            .collect();
        stream::iter(segments)
            .map(|(segment, key)| {
                let spool = spool.clone();
                async move {
                    let bytes = self
                        .store
                        .get_opts(&key, GetOptions::default())
                        .await?
                        .bytes()
                        .await?;
                    tokio::task::spawn_blocking(move || -> Result<()> {
                        let mut writer = spool.create(&segment)?;
                        writer.write_all(&bytes)?;
                        writer.commit()
                    })
                    .await?
                }
            })
            .buffer_unordered(DOWNLOADS)
            .try_collect::<Vec<()>>()
            .await?;
        let last = plan.last().context("empty restore plan")?.segment;
        let header = *terse_ltx::Decoder::new(spool.open(&last)?)?.header();
        Ok(u64::from(header.commit) * u64::from(header.page_size))
    }

    /// Restores transaction `txid` to `output`, which must not exist, from
    /// the segments [`Self::download`] put in `scratch`, after verifying
    /// their checksums.
    pub async fn apply(scratch: &Path, txid: u64, output: &Path) -> Result<()> {
        ensure!(!output.exists(), "restore output already exists");
        let spool = FileStore::with_durability(scratch, Durability::Scratch);
        let output = output.to_path_buf();
        let restored = tokio::task::spawn_blocking(move || {
            terse_litestream::restore(&spool, &output, Some(txid))
        })
        .await??;
        ensure!(
            restored == txid,
            "restore stopped at transaction {restored}"
        );
        Ok(())
    }

    /// Deletes segments, such as ones uploaded after this server lost the
    /// database. The listing names their objects.
    pub async fn remove(&self, segments: &[Segment]) -> Result<()> {
        if segments.is_empty() {
            return Ok(());
        }
        let paths: Vec<ObjectPath> = self
            .segments()
            .await?
            .iter()
            .filter(|stored| segments.contains(&stored.segment))
            .map(|stored| self.key(stored))
            .collect();
        if paths.is_empty() {
            return Ok(());
        }
        let count = paths.len();
        let mut deletions = self
            .raw
            .delete_stream(Box::pin(stream::iter(paths.into_iter().map(Ok))));
        for _ in 0..count {
            match deletions.try_next().await {
                Ok(Some(_)) | Err(object_store::Error::NotFound { .. }) => {}
                Ok(None) => bail!("object store did not acknowledge all deletions"),
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }

    /// Applies retention. Every transaction captured in the last
    /// [`RETENTION`] stays restorable: from the newest base captured before
    /// then, everything is kept. Before it, only the bases in `keep` (full
    /// snapshots kept longer) remain, with the points that name them. The
    /// newest segment always remains.
    pub async fn prune(
        &self,
        segments: &[Stored],
        points: &[Point],
        now_ms: i64,
        keep: &BTreeSet<u64>,
    ) -> Result<Pruned> {
        let cutoff = now_ms.saturating_sub(i64::try_from(RETENTION.as_millis())?);
        let windows = windows(segments);
        let newest = windows.len().checked_sub(1);
        // The transactions each window keeps, from the first one.
        let kept: Vec<(u64, u64)> = windows
            .iter()
            .enumerate()
            .filter(|(index, window)| {
                Some(*index) == newest || window.last().uploaded_at_ms > cutoff
            })
            .map(|(_, window)| {
                let anchor = window
                    .members
                    .iter()
                    .rfind(|stored| stored.is_base() && stored.uploaded_at_ms <= cutoff)
                    .unwrap_or(window.first());
                (anchor.txid(), window.last().txid())
            })
            .collect();
        let in_kept_range = |txid: u64| {
            kept.iter()
                .any(|(first, last)| (*first..=*last).contains(&txid))
        };
        let kept_base = |txid: u64| {
            keep.contains(&txid)
                && segments
                    .iter()
                    .any(|stored| stored.txid() == txid && stored.is_base())
        };
        let restorable = |txid: u64| in_kept_range(txid) || kept_base(txid);
        let in_window = |txid: u64| windows.iter().any(|window| window.contains(txid));
        let latest = segments.iter().map(Stored::txid).max();
        let obsolete: Vec<&Stored> = segments
            .iter()
            .filter(|stored| {
                let txid = stored.txid();
                if Some(txid) == latest || in_kept_range(txid) {
                    return false;
                }
                if stored.is_base() && keep.contains(&txid) {
                    return false;
                }
                // A run without a base restores nothing, but one may still
                // be uploading its base.
                in_window(txid) || stored.uploaded_at_ms <= cutoff
            })
            .collect();
        let stale_points: Vec<&Point> = points
            .iter()
            .filter(|point| !restorable(point.txid))
            .collect();
        // Markers go first, so a listed point is always restorable.
        let point_paths: Vec<ObjectPath> = stale_points
            .iter()
            .map(|point| self.point_key(point))
            .collect();
        let segment_paths: Vec<ObjectPath> =
            obsolete.iter().map(|stored| self.key(stored)).collect();
        for paths in [point_paths, segment_paths] {
            if paths.is_empty() {
                continue;
            }
            let count = paths.len();
            let mut deletions = self
                .raw
                .delete_stream(Box::pin(stream::iter(paths.into_iter().map(Ok))));
            for _ in 0..count {
                match deletions.try_next().await {
                    Ok(Some(_)) | Err(object_store::Error::NotFound { .. }) => {}
                    Ok(None) => bail!("object store did not acknowledge all deletions"),
                    Err(error) => return Err(error.into()),
                }
            }
        }
        Ok(Pruned {
            segments: obsolete.len(),
            points: stale_points.len(),
        })
    }
}

/// A time target earlier than every restorable transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TooEarly {
    /// When the earliest restorable transaction was captured (Unix ms).
    pub earliest_ms: i64,
}

impl std::fmt::Display for TooEarly {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("no backup was captured that early")
    }
}

impl std::error::Error for TooEarly {}

/// The newest transaction of `window` that includes nothing captured after
/// `at`. Capture times grow with transactions unless the clock was set
/// back, so the run stops at the first capture dated later.
fn newest_at(window: &Window, at: i64) -> Option<u64> {
    let captured = window
        .members
        .iter()
        .position(|stored| stored.captured_at_ms > at)
        .unwrap_or(window.members.len());
    captured
        .checked_sub(1)
        .map(|last| window.members[last].txid())
}

/// A listing, for planning a restore before downloading.
struct Listing(Vec<Segment>);

impl ReplicaStore for Listing {
    fn list(&self, level: u8) -> Result<Vec<Segment>> {
        Ok(self
            .0
            .iter()
            .filter(|segment| segment.level == level)
            .copied()
            .collect())
    }

    fn open(&self, _: &Segment) -> Result<Box<dyn Read + Send>> {
        bail!("a listing has no contents")
    }

    fn create(&self, _: &Segment) -> Result<Box<dyn SegmentWriter>> {
        bail!("a listing is read-only")
    }

    fn remove(&self, _: &Segment) -> Result<()> {
        bail!("a listing is read-only")
    }
}

fn receipt_path(spool: &FileStore, segment: &Segment) -> PathBuf {
    spool
        .root()
        .join("uploaded")
        .join(format!("{}-{}", segment.level, segment.filename()))
}

/// Removes uploaded segments except the newest, which holds the position.
fn trim(spool: &FileStore) -> Result<()> {
    let mut segments = Vec::new();
    for level in 0..=9 {
        segments.extend(spool.list(level)?);
    }
    let Some(newest) = segments
        .iter()
        .max_by_key(|segment| (segment.max_txid, segment.level))
        .copied()
    else {
        return Ok(());
    };
    let mut removed = false;
    for segment in segments {
        let receipt = receipt_path(spool, &segment);
        if segment == newest || !receipt.exists() {
            continue;
        }
        // The file goes before its receipt, so an interruption cannot
        // upload it again.
        spool.remove(&segment)?;
        fs::remove_file(receipt)?;
        removed = true;
    }
    if removed {
        File::open(spool.root().join("uploaded"))?.sync_all()?;
    }
    Ok(())
}

/// A segment and its capture time, from its object name (see
/// [`Replica::segment_key`]).
fn parse_segment(prefix: &ObjectPath, key: &ObjectPath) -> Option<(Segment, i64)> {
    let relative = key.as_ref().strip_prefix(&format!("{prefix}/ltx/"))?;
    let (level, name) = relative.split_once('/')?;
    let stem = name.strip_suffix(".ltx")?;
    let (range, captured) = (stem.get(..33)?, stem.get(33..)?.strip_prefix('-')?);
    if range.as_bytes()[16] != b'-'
        || captured.is_empty()
        || !captured.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    let level: u8 = level.parse().ok()?;
    if level != 0 && level != 9 {
        return None;
    }
    let segment = Segment::new(
        level,
        u64::from_str_radix(range.get(..16)?, 16).ok()?,
        u64::from_str_radix(range.get(17..)?, 16).ok()?,
    )
    .ok()?;
    Some((segment, captured.parse().ok()?))
}

fn parse_point(name: &str) -> Option<Point> {
    let (created, rest) = name.split_once('-')?;
    let (txid, reason) = rest.split_once('-')?;
    if created.len() != 13 || txid.len() != 16 || reason.is_empty() {
        return None;
    }
    Some(Point {
        created_at_ms: created.parse().ok()?,
        txid: u64::from_str_radix(txid, 16).ok()?,
        reason: reason.to_owned(),
    })
}

fn persist(directory: &Path, destination: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = NamedTempFile::new_in(directory)?;
    file.write_all(bytes)?;
    file.as_file().sync_all()?;
    file.persist_noclobber(destination)?;
    File::open(directory)?.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stored(level: u8, min: u64, max: u64, at: i64) -> Stored {
        Stored {
            segment: Segment::new(level, min, max).unwrap(),
            size: 1,
            captured_at_ms: at,
            uploaded_at_ms: at,
        }
    }

    #[test]
    fn segment_names_carry_the_capture_time() {
        let prefix = ObjectPath::from("db/1");
        let key =
            ObjectPath::from("db/1/ltx/0/0000000000000002-0000000000000003-1732345678901.ltx");
        assert_eq!(
            parse_segment(&prefix, &key),
            Some((Segment::new(0, 2, 3).unwrap(), 1_732_345_678_901))
        );
        for name in [
            "db/1/ltx/0/0000000000000002-0000000000000003.ltx",
            "db/1/ltx/0/0000000000000002-0000000000000003-.ltx",
            "db/1/ltx/0/0000000000000002-0000000000000003-12a.ltx",
            "db/1/ltx/3/0000000000000002-0000000000000003-1.ltx",
        ] {
            assert_eq!(
                parse_segment(&prefix, &ObjectPath::from(name)),
                None,
                "{name}"
            );
        }
    }

    #[test]
    fn a_time_selects_the_newest_transaction_captured_by_then() {
        let segments = [
            stored(9, 1, 1, 10),
            stored(0, 2, 2, 20),
            stored(0, 3, 3, 30),
        ];
        let resolve = |at| Replica::resolve(&segments, Target::Time(at));
        assert_eq!(resolve(25).unwrap(), 2);
        assert_eq!(resolve(30).unwrap(), 3);
        assert_eq!(resolve(10).unwrap(), 1);
        let early = resolve(9).unwrap_err();
        assert_eq!(
            early.downcast_ref::<TooEarly>(),
            Some(&TooEarly { earliest_ms: 10 })
        );
    }

    #[test]
    fn a_clock_set_back_never_restores_a_later_capture() {
        // Transaction 3 was captured at 30, then the clock went back.
        let segments = [
            stored(9, 1, 1, 10),
            stored(0, 2, 2, 20),
            stored(0, 3, 3, 30),
            stored(0, 4, 4, 15),
        ];
        let resolve = |at| Replica::resolve(&segments, Target::Time(at)).unwrap();
        assert_eq!(resolve(25), 2);
        assert_eq!(resolve(15), 1);
        assert_eq!(resolve(30), 4);
    }

    #[test]
    fn windows_start_at_a_base_and_end_at_a_gap() {
        let segments = [
            stored(9, 1, 1, 0),
            stored(0, 2, 2, 1),
            stored(0, 3, 3, 2),
            // A previous owner's tail, without a base of its own.
            stored(0, 5, 5, 3),
            stored(9, 1, 100, 4),
            stored(0, 101, 101, 5),
        ];
        let windows = windows(&segments);
        assert_eq!(windows.len(), 2);
        assert_eq!(
            (windows[0].first().txid(), windows[0].last().txid()),
            (1, 3)
        );
        assert_eq!(
            (windows[1].first().txid(), windows[1].last().txid()),
            (100, 101)
        );
        assert!(!windows.iter().any(|window| window.contains(5)));
    }

    #[test]
    fn points_round_trip_through_their_names() {
        let point = parse_point("1732345678901-00000000000000ff-release").unwrap();
        assert_eq!(
            point,
            Point {
                created_at_ms: 1_732_345_678_901,
                txid: 255,
                reason: "release".into()
            }
        );
        assert!(parse_point("1-ff-release").is_none());
    }
}
