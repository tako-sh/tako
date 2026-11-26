//! One KV store: a redb file and the thread that writes it.
//!
//! Writes from every caller go to one writer thread, which applies all the
//! work waiting for it in one transaction and commits it durably before
//! replying: an acknowledged write survives a crash, and concurrent writers
//! share each commit's flush. Reads run on read transactions, which see the
//! last commit.
//!
//! The file is opened in redb's multi-writer mode, so the process taking
//! over during a server reload can open it while the old one drains.
//!
//! The data tables are named for the store's generation (`entries.0` and
//! so on). Replacing the contents, as an import or restore does, moves the
//! store to a new generation of tables in the same transaction; the old
//! one is freed a bounded step at a time afterwards, so other writes never
//! wait for the whole deletion.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TrySendError, sync_channel};
use std::time::{Duration, Instant};

use parking_lot::{Mutex, RwLock};
use redb::{
    ConcurrencyMode, Database, ReadableDatabase, ReadableTable, ReadableTableMetadata, Table,
    TableDefinition, backends::FileBackend,
};

use crate::export;

/// Values by key: `u64be(expires_at_ms) || json`, with 0 for no expiry.
const ENTRIES: &str = "entries";
/// Keys with an expiry, by `u64be(expires_at_ms) || key`.
const EXPIRY: &str = "expiry";
/// Totals, the current generation, and the generation being freed.
const META: TableDefinition<&str, u64> = TableDefinition::new("meta");

type EntryTable<'n> = TableDefinition<'n, &'static str, &'static [u8]>;
type IndexTable<'n> = TableDefinition<'n, &'static [u8], ()>;

/// Bytes counted for each key besides its key and value, roughly what an
/// entry costs on disk, so a store's size reflects many tiny keys too.
pub const KEY_OVERHEAD: u64 = 64;
/// Writes waiting for the writer thread before callers wait to queue more.
const QUEUE: usize = 1024;
/// Work applied in one transaction.
const BATCH_JOBS: usize = 256;
const SWEEP_INTERVAL: Duration = Duration::from_millis(250);
/// Expired keys removed alongside other writes, and by a periodic sweep.
const SWEEP_WITH_WRITES: usize = 32;
const SWEEP_ALONE: usize = 10_000;
/// Bytes of expired keys one sweep copies out of the index at most.
const SWEEP_BYTES: usize = 1024 * 1024;
/// Entries of a replaced generation freed alongside other writes, and by a
/// periodic step, with at most `DRAIN_BYTES` of keys and values in a step.
const DRAIN_WITH_WRITES: usize = 1_000;
const DRAIN_ALONE: usize = 10_000;
const DRAIN_BYTES: usize = 4 * 1024 * 1024;

pub type Clock = Arc<dyn Fn() -> u64 + Send + Sync>;

/// Milliseconds since the Unix epoch.
pub fn system_clock() -> Clock {
    Arc::new(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64
    })
}

#[derive(Clone)]
pub struct StoreConfig {
    /// Bytes of redb page cache.
    pub cache_bytes: usize,
    pub clock: Clock,
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("{0}")]
    Storage(String),
    #[error("the store is closed")]
    Closed,
    /// An import file that is not a KV export, or holds keys or values past
    /// the limits.
    #[error("{0}")]
    Invalid(String),
}

pub(crate) fn storage(error: impl std::fmt::Display) -> StoreError {
    StoreError::Storage(error.to_string())
}

type Result<T, E = StoreError> = std::result::Result<T, E>;

/// Totals kept in the `meta` table.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct Stats {
    /// Bytes of keys, values and per-key overhead.
    pub bytes: u64,
    pub keys: u64,
    /// Keys with an expiry.
    pub expires: u64,
}

fn table_name(table: &str, generation: u64) -> String {
    format!("{table}.{generation}")
}

fn open_entries<'t>(
    txn: &'t redb::WriteTransaction,
    generation: u64,
) -> Result<Table<'t, &'static str, &'static [u8]>> {
    txn.open_table(EntryTable::new(&table_name(ENTRIES, generation)))
        .map_err(storage)
}

fn open_index(
    txn: &redb::WriteTransaction,
    generation: u64,
) -> Result<Table<'_, &'static [u8], ()>> {
    txn.open_table(IndexTable::new(&table_name(EXPIRY, generation)))
        .map_err(storage)
}

/// Deletes a generation's tables, which must not be open.
fn delete_generation(txn: &redb::WriteTransaction, generation: u64) -> Result<()> {
    txn.delete_table(EntryTable::new(&table_name(ENTRIES, generation)))
        .map_err(storage)?;
    txn.delete_table(IndexTable::new(&table_name(EXPIRY, generation)))
        .map_err(storage)?;
    Ok(())
}

fn read_generations<T: ReadableTable<&'static str, u64>>(meta: &T) -> Result<(u64, Option<u64>)> {
    let get = |name: &str| -> Result<Option<u64>> {
        Ok(meta.get(name).map_err(storage)?.map(|v| v.value()))
    };
    Ok((get("generation")?.unwrap_or(0), get("trash")?))
}

fn read_stats<T: ReadableTable<&'static str, u64>>(meta: &T) -> Result<Stats> {
    let get = |name: &str| -> Result<u64> {
        Ok(meta.get(name).map_err(storage)?.map_or(0, |v| v.value()))
    };
    Ok(Stats {
        bytes: get("bytes")?,
        keys: get("keys")?,
        expires: get("expires")?,
    })
}

/// The result of [`Store::replace_from`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Replaced {
    /// Totals of the new contents.
    pub stats: Stats,
    /// Whether the replaced contents differed from the last snapshot, read
    /// with no write in between, so a backup of them is needed.
    pub previous_changed: bool,
}

/// What a key is charged.
pub(crate) fn weight(key: &str, json_len: usize) -> u64 {
    KEY_OVERHEAD + key.len() as u64 + json_len as u64
}

fn encode(expires_at: u64, json: &[u8]) -> Vec<u8> {
    let mut value = Vec::with_capacity(8 + json.len());
    value.extend_from_slice(&expires_at.to_be_bytes());
    value.extend_from_slice(json);
    value
}

/// A stored value's expiry and JSON.
fn decode(value: &[u8]) -> Result<(u64, &[u8])> {
    if value.len() < 8 {
        return Err(StoreError::Storage("corrupt KV entry".into()));
    }
    let expires_at = u64::from_be_bytes(value[..8].try_into().unwrap());
    Ok((expires_at, &value[8..]))
}

fn expiry_id(expires_at: u64, key: &str) -> Vec<u8> {
    let mut id = Vec::with_capacity(8 + key.len());
    id.extend_from_slice(&expires_at.to_be_bytes());
    id.extend_from_slice(key.as_bytes());
    id
}

fn live(expires_at: u64, now: u64) -> bool {
    expires_at == 0 || expires_at > now
}

/// The absolute expiry of a TTL from `now`, or 0 for none.
fn expiry_of(ttl_ms: Option<u64>, now: u64) -> u64 {
    ttl_ms.map_or(0, |ttl| now.saturating_add(ttl.max(1)))
}

enum Work {
    Put {
        key: String,
        json: Vec<u8>,
        ttl_ms: Option<u64>,
    },
    Delete {
        key: String,
    },
    /// Replaces every key with the entries of an export file, after
    /// exporting the contents it replaces to `previous`.
    Replace {
        path: PathBuf,
        previous: Option<PathBuf>,
    },
}

#[derive(Debug)]
enum Outcome {
    Done,
    Deleted(bool),
    Replaced(Replaced),
}

struct Job {
    work: Work,
    reply: SyncSender<Result<Outcome>>,
}

pub struct Store {
    path: PathBuf,
    replicated: Option<Arc<Replicated>>,
    db: RwLock<Option<Arc<Database>>>,
    writer: Mutex<Option<SyncSender<Job>>>,
    thread: Mutex<Option<std::thread::JoinHandle<()>>>,
    config: Arc<StoreConfig>,
    /// Whether the contents may differ from the last snapshot. Set when the
    /// store opens, so the first check after a restart snapshots it.
    changed: Arc<AtomicBool>,
}

/// A writer's private file can contain an unacknowledged redb commit.
/// All readers therefore use the last applied image, including local reads.
struct Replicated {
    resource: tako_raft::Resource,
    reader: Mutex<Option<(Arc<tempfile::NamedTempFile>, Arc<Database>)>>,
}
impl Replicated {
    fn read(&self, cache_bytes: usize) -> Result<Arc<Database>> {
        let file = self.resource.local_file().map_err(storage)?;
        let mut cached = self.reader.lock();
        if let Some((existing, db)) = cached.as_ref()
            && Arc::ptr_eq(existing, &file)
        {
            return Ok(db.clone());
        }
        let db = Arc::new(open_database(file.path(), cache_bytes)?);
        *cached = Some((file, db.clone()));
        Ok(db)
    }
}

struct Primary {
    db: Database,
    file: tako_raft::RedbFile,
    writer: tako_raft::Writer,
}
impl Primary {
    fn open(resource: &tako_raft::Resource, cache_bytes: usize) -> Result<Self> {
        let writer = resource.writer().map_err(storage)?;
        let file = tako_raft::RedbFile::open(&writer).map_err(storage)?;
        let db = Database::builder()
            .set_cache_size(cache_bytes)
            .create_with_backend(file.clone())
            .map_err(storage)?;
        init_tables(&db)?;
        Ok(Self { db, file, writer })
    }
}

impl Store {
    /// Opens the store at `path`, creating the file if needed.
    pub fn open(path: &Path, config: StoreConfig) -> Result<Self> {
        Self::open_inner(path, config, None)
    }

    pub fn open_replicated(
        path: &Path,
        config: StoreConfig,
        resource: tako_raft::Resource,
    ) -> Result<Self> {
        let empty = resource.file_size().map_err(storage)? == 0;
        let replicated = Arc::new(Replicated {
            resource,
            reader: Default::default(),
        });
        let store = Self::open_inner(path, config, Some(replicated))?;
        if empty {
            store.clear_changed();
        }
        Ok(store)
    }

    fn open_inner(
        path: &Path,
        config: StoreConfig,
        replicated: Option<Arc<Replicated>>,
    ) -> Result<Self> {
        let db = if replicated.is_some() {
            None
        } else {
            Some(Arc::new(open_database(path, config.cache_bytes)?))
        };
        let config = Arc::new(config);
        let changed = Arc::new(AtomicBool::new(true));
        let (sender, receiver) = sync_channel(QUEUE);
        let thread = {
            let db = db.clone();
            let replicated = replicated.clone();
            let config = config.clone();
            let changed = changed.clone();
            std::thread::Builder::new()
                .name("tako-kv-writer".into())
                .spawn(move || writer(db, replicated, receiver, config, changed))
                .map_err(storage)?
        };
        Ok(Self {
            path: path.to_owned(),
            db: RwLock::new(db),
            replicated,
            writer: Mutex::new(Some(sender)),
            thread: Mutex::new(Some(thread)),
            config,
            changed,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn db(&self) -> Result<Arc<Database>> {
        if let Some(replica) = &self.replicated {
            if self.writer.lock().is_none() {
                return Err(StoreError::Closed);
            }
            return replica.read(self.config.cache_bytes);
        }
        self.db.read().clone().ok_or(StoreError::Closed)
    }

    /// Stops accepting work, finishes what is queued, and closes the file
    /// once in-flight reads end. Blocks until the writer has stopped.
    pub fn close(&self) {
        drop(self.writer.lock().take());
        drop(self.db.write().take());
        if let Some(thread) = self.thread.lock().take() {
            let _ = thread.join();
        }
    }

    fn submit(&self, work: Work) -> Result<Outcome> {
        let (reply, receive) = sync_channel(1);
        let mut job = Job { work, reply };
        loop {
            let sender = self.writer.lock().clone().ok_or(StoreError::Closed)?;
            match sender.try_send(job) {
                Ok(()) => break,
                Err(TrySendError::Full(back)) => {
                    job = back;
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(TrySendError::Disconnected(_)) => return Err(StoreError::Closed),
            }
        }
        receive.recv().map_err(|_| StoreError::Closed)?
    }

    /// The value's JSON, or `None` when the key is missing or expired.
    pub fn get(&self, key: &str) -> Result<Option<Vec<u8>>> {
        if let Some(replica) = &self.replicated {
            replica.resource.leader_barrier().map_err(storage)?;
        }
        self.get_local(key)
    }

    pub(crate) fn replica_grant(&self, scope: &str, hash: &[u8; 32]) -> Option<bool> {
        self.replicated
            .as_ref()
            .map(|r| r.resource.grant_matches(scope, hash))
    }
    pub fn is_replicated(&self) -> bool {
        self.replicated.is_some()
    }

    pub fn get_local(&self, key: &str) -> Result<Option<Vec<u8>>> {
        let db = self.db()?;
        let txn = db.begin_read().map_err(storage)?;
        let (generation, _) = read_generations(&txn.open_table(META).map_err(storage)?)?;
        let entries = txn
            .open_table(EntryTable::new(&table_name(ENTRIES, generation)))
            .map_err(storage)?;
        let now = (self.config.clock)();
        let Some(value) = entries.get(key).map_err(storage)? else {
            return Ok(None);
        };
        let (expires_at, json) = decode(value.value())?;
        Ok(live(expires_at, now).then(|| json.to_vec()))
    }

    /// Stores `json` at `key`, replacing what it held. Without a TTL the key
    /// stays until it is deleted.
    pub fn put(&self, key: &str, json: Vec<u8>, ttl_ms: Option<u64>) -> Result<()> {
        self.submit(Work::Put {
            key: key.to_owned(),
            json,
            ttl_ms,
        })
        .map(drop)
    }

    /// Removes `key`. Returns whether it held a live value.
    pub fn delete(&self, key: &str) -> Result<bool> {
        match self.submit(Work::Delete {
            key: key.to_owned(),
        })? {
            Outcome::Deleted(deleted) => Ok(deleted),
            _ => Ok(false),
        }
    }

    pub fn stats(&self) -> Result<Stats> {
        let db = self.db()?;
        let txn = db.begin_read().map_err(storage)?;
        read_stats(&txn.open_table(META).map_err(storage)?)
    }

    /// Writes every live key to a new export file at `path`, from one
    /// consistent read of the store.
    pub fn export_to(&self, path: &Path) -> Result<Stats> {
        export_database(&*self.db()?, path, (self.config.clock)())
    }

    /// Whether the contents may differ from the last [`Self::snapshot_to`].
    pub fn has_changes(&self) -> bool {
        self.changed.load(Ordering::Relaxed)
    }

    /// Records that the contents differ from the last snapshot, as after a
    /// snapshot that was not kept.
    pub fn mark_changed(&self) {
        self.changed.store(true, Ordering::Relaxed);
    }

    /// Records that the contents match the last snapshot, as a new, empty
    /// store does.
    pub fn clear_changed(&self) {
        self.changed.store(false, Ordering::Relaxed);
    }

    /// [`Self::export_to`] for a backup: the store counts as unchanged until
    /// the next write, unless the export fails.
    pub fn snapshot_to(&self, path: &Path) -> Result<Stats> {
        // Cleared before the read, so a write that lands meanwhile marks the
        // store changed again.
        let was_changed = self.changed.swap(false, Ordering::Relaxed);
        let exported = self.export_to(path);
        if exported.is_err() && was_changed {
            self.mark_changed();
        }
        exported
    }

    /// Replaces every key with the entries of the export file at `path`, in
    /// one transaction: readers see the old contents or the new ones.
    /// Entries already expired are skipped. With `previous`, the contents
    /// being replaced are exported there first, with no write in between.
    pub fn replace_from(&self, path: &Path, previous: Option<&Path>) -> Result<Replaced> {
        // Checked before queueing, so a bad file never holds up writes.
        export::Reader::open(path)?.check()?;
        match self.submit(Work::Replace {
            path: path.to_owned(),
            previous: previous.map(Path::to_owned),
        })? {
            Outcome::Replaced(replaced) => Ok(replaced),
            _ => Err(StoreError::Storage("unexpected reply to a replace".into())),
        }
    }
}

impl Drop for Store {
    fn drop(&mut self) {
        // The writer exits once its channel closes; it holds no lock anyone
        // waits for, so it is not joined here.
        self.writer.lock().take();
    }
}

/// Opens (or creates) a store file with its current generation's tables in
/// place, so read transactions find them.
fn open_database(path: &Path, cache_bytes: usize) -> Result<Database> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(storage)?;
    }
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .map_err(storage)?;
    set_private(path);
    let db = Database::builder()
        .set_concurrency_mode(ConcurrencyMode::MultiWriter)
        .set_cache_size(cache_bytes)
        .create_with_backend(FileBackend::new(file))
        .map_err(storage)?;
    init_tables(&db)?;
    Ok(db)
}

fn init_tables(db: &Database) -> Result<()> {
    let txn = db.begin_write().map_err(storage)?;
    let (generation, _) = read_generations(&txn.open_table(META).map_err(storage)?)?;
    open_entries(&txn, generation)?;
    open_index(&txn, generation)?;
    txn.commit().map_err(storage)?;
    Ok(())
}

/// Writes every live key of `db`'s last commit to a new export file.
fn export_database(db: &Database, path: &Path, now: u64) -> Result<Stats> {
    let txn = db.begin_read().map_err(storage)?;
    let (generation, _) = read_generations(&txn.open_table(META).map_err(storage)?)?;
    let entries = txn
        .open_table(EntryTable::new(&table_name(ENTRIES, generation)))
        .map_err(storage)?;
    let mut writer = export::Writer::create(path)?;
    for item in entries.iter().map_err(storage)? {
        let (key, value) = item.map_err(storage)?;
        let (expires_at, json) = decode(value.value())?;
        if live(expires_at, now) {
            writer.insert(key.value(), expires_at, json)?;
        }
    }
    writer.finish(now)
}

/// Creates an empty store file, closed again before this returns.
pub fn create_file(path: &Path) -> Result<()> {
    drop(open_database(path, 1024 * 1024)?);
    Ok(())
}

fn set_private(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}

struct WriteView<'t> {
    txn: &'t redb::WriteTransaction,
    generation: u64,
    trash: Option<u64>,
    entries: Table<'t, &'static str, &'static [u8]>,
    expiry: Table<'t, &'static [u8], ()>,
    meta: Table<'t, &'static str, u64>,
    stats: Stats,
    dirty: bool,
}

impl<'t> WriteView<'t> {
    fn open(txn: &'t redb::WriteTransaction) -> Result<Self> {
        let meta = txn.open_table(META).map_err(storage)?;
        let stats = read_stats(&meta)?;
        let (generation, trash) = read_generations(&meta)?;
        Ok(Self {
            txn,
            generation,
            trash,
            entries: open_entries(txn, generation)?,
            expiry: open_index(txn, generation)?,
            meta,
            stats,
            dirty: false,
        })
    }

    fn flush(&mut self) -> Result<()> {
        if !self.dirty {
            return Ok(());
        }
        for (name, value) in [
            ("bytes", self.stats.bytes),
            ("keys", self.stats.keys),
            ("expires", self.stats.expires),
            ("generation", self.generation),
        ] {
            self.meta.insert(name, value).map_err(storage)?;
        }
        match self.trash {
            Some(trash) => self.meta.insert("trash", trash).map(drop),
            None => self.meta.remove("trash").map(drop),
        }
        .map_err(storage)?;
        Ok(())
    }

    /// The key's expiry and value length, removing it first when it has
    /// expired.
    fn load(&mut self, key: &str, now: u64) -> Result<Option<(u64, usize)>> {
        let found = match self.entries.get(key).map_err(storage)? {
            Some(value) => {
                let (expires_at, json) = decode(value.value())?;
                Some((expires_at, json.len()))
            }
            None => None,
        };
        match found {
            Some((expires_at, len)) if !live(expires_at, now) => {
                self.remove(key, expires_at, len)?;
                Ok(None)
            }
            found => Ok(found),
        }
    }

    fn remove(&mut self, key: &str, expires_at: u64, len: usize) -> Result<()> {
        self.dirty = true;
        self.entries.remove(key).map_err(storage)?;
        if expires_at != 0 {
            self.expiry
                .remove(expiry_id(expires_at, key).as_slice())
                .map_err(storage)?;
            self.stats.expires = self.stats.expires.saturating_sub(1);
        }
        self.stats.bytes = self.stats.bytes.saturating_sub(weight(key, len));
        self.stats.keys = self.stats.keys.saturating_sub(1);
        Ok(())
    }

    fn put(&mut self, key: &str, json: &[u8], expires_at: u64, now: u64) -> Result<()> {
        let old = self.load(key, now)?;
        let added = weight(key, json.len());
        let freed = old.map_or(0, |(_, len)| weight(key, len));
        self.dirty = true;
        if let Some((old_expiry, _)) = old {
            if old_expiry != 0 {
                self.expiry
                    .remove(expiry_id(old_expiry, key).as_slice())
                    .map_err(storage)?;
                self.stats.expires -= 1;
            }
        } else {
            self.stats.keys += 1;
        }
        if expires_at != 0 {
            self.expiry
                .insert(expiry_id(expires_at, key).as_slice(), ())
                .map_err(storage)?;
            self.stats.expires += 1;
        }
        self.entries
            .insert(key, encode(expires_at, json).as_slice())
            .map_err(storage)?;
        self.stats.bytes = self.stats.bytes - freed + added;
        Ok(())
    }

    fn delete(&mut self, key: &str, now: u64) -> Result<bool> {
        match self.load(key, now)? {
            Some((expires_at, len)) => {
                self.remove(key, expires_at, len)?;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    /// Moves the store to a new, empty generation. The old one is left for
    /// [`Self::drain`]; one an earlier replace left is deleted now, so at
    /// most one generation waits to be freed.
    fn clear(&mut self) -> Result<()> {
        self.dirty = true;
        let old = self.generation;
        self.generation += 1;
        // Assigning closes the old tables, so they can be deleted.
        self.entries = open_entries(self.txn, self.generation)?;
        self.expiry = open_index(self.txn, self.generation)?;
        if let Some(trash) = self.trash.take() {
            delete_generation(self.txn, trash)?;
        }
        self.trash = Some(old);
        self.stats = Stats::default();
        Ok(())
    }

    /// Replaces every key with an export file's live entries.
    fn replace(&mut self, path: &Path, now: u64) -> Result<Stats> {
        let reader = export::Reader::open(path)?;
        self.clear()?;
        for entry in reader.entries()? {
            let (key, expires_at, json) = entry?;
            if !live(expires_at, now) {
                continue;
            }
            self.put(&key, &json, expires_at, now)?;
        }
        Ok(self.stats)
    }

    /// Frees up to `limit` entries of the generation a replace left, and its
    /// tables once they are empty. Returns whether any is left.
    fn drain(&mut self, limit: usize) -> Result<bool> {
        let Some(trash) = self.trash else {
            return Ok(false);
        };
        self.dirty = true;
        let mut budget = (limit, DRAIN_BYTES);
        let mut empty = {
            let mut table = open_entries(self.txn, trash)?;
            let mut removed = table.extract_if(.., |_, _| true).map_err(storage)?;
            while budget.0 > 0 && budget.1 > 0 {
                let Some(item) = removed.next() else {
                    break;
                };
                let (key, value) = item.map_err(storage)?;
                budget.0 -= 1;
                budget.1 = budget
                    .1
                    .saturating_sub(key.value().len() + value.value().len());
            }
            removed.close().map_err(storage)?;
            table.is_empty().map_err(storage)?
        };
        empty &= {
            let mut table = open_index(self.txn, trash)?;
            let mut removed = table.extract_if(.., |_, _| true).map_err(storage)?;
            while budget.0 > 0 && budget.1 > 0 {
                let Some(item) = removed.next() else {
                    break;
                };
                let (id, _) = item.map_err(storage)?;
                budget.0 -= 1;
                budget.1 = budget.1.saturating_sub(id.value().len());
            }
            removed.close().map_err(storage)?;
            table.is_empty().map_err(storage)?
        };
        if empty {
            delete_generation(self.txn, trash)?;
            self.trash = None;
        }
        Ok(!empty)
    }

    /// Removes up to `limit` expired keys. Returns whether more remain.
    fn sweep(&mut self, now: u64, limit: usize) -> Result<bool> {
        let mut due = Vec::new();
        let mut bytes = 0;
        let mut more = false;
        for item in self.expiry.iter().map_err(storage)? {
            let (id, _) = item.map_err(storage)?;
            let id = id.value();
            let when = u64::from_be_bytes(id[..8].try_into().unwrap());
            if when > now {
                break;
            }
            if due.len() == limit || bytes >= SWEEP_BYTES {
                more = true;
                break;
            }
            bytes += id.len();
            due.push(id.to_vec());
        }
        for id in due {
            let when = u64::from_be_bytes(id[..8].try_into().unwrap());
            let key = String::from_utf8_lossy(&id[8..]).into_owned();
            let current = match self.entries.get(key.as_str()).map_err(storage)? {
                Some(value) => {
                    let (expires_at, json) = decode(value.value())?;
                    Some((expires_at, json.len()))
                }
                None => None,
            };
            match current {
                Some((expires_at, len)) if expires_at == when => {
                    self.remove(&key, expires_at, len)?;
                }
                // The key is gone, or a later write moved its expiry: the
                // index entry is stale.
                _ => {
                    self.dirty = true;
                    self.expiry.remove(id.as_slice()).map_err(storage)?;
                }
            }
        }
        Ok(more)
    }
}

fn writer(
    db: Option<Arc<Database>>,
    replicated: Option<Arc<Replicated>>,
    jobs: Receiver<Job>,
    config: Arc<StoreConfig>,
    changed: Arc<AtomicBool>,
) {
    let mut primary: Option<Primary> = None;
    let mut next_sweep = Instant::now() + SWEEP_INTERVAL;
    // A replace runs in its own transaction; one received while batching
    // waits here for the next round.
    let mut held: Option<Job> = None;
    loop {
        let mut batch = Vec::new();
        if let Some(job) = held.take() {
            batch.push(job);
        } else {
            let wait = next_sweep.saturating_duration_since(Instant::now());
            match jobs.recv_timeout(wait) {
                Ok(job) => batch.push(job),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
        let alone = batch
            .first()
            .is_some_and(|job| matches!(job.work, Work::Replace { .. }));
        while !alone && batch.len() < BATCH_JOBS {
            match jobs.try_recv() {
                Ok(job) if matches!(job.work, Work::Replace { .. }) => {
                    held = Some(job);
                    break;
                }
                Ok(job) => batch.push(job),
                Err(_) => break,
            }
        }
        let sweep = Instant::now() >= next_sweep;
        let current = db.as_deref().or_else(|| primary.as_ref().map(|p| &p.db));
        if batch.is_empty()
            && (!sweep || !current.is_some_and(|db| has_cleanup(db, (config.clock)())))
        {
            if sweep {
                next_sweep = Instant::now() + SWEEP_INTERVAL;
            }
            continue;
        }
        let (works, replies): (Vec<_>, Vec<_>) =
            batch.into_iter().map(|job| (job.work, job.reply)).unzip();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            if let Some(replica) = &replicated {
                if primary
                    .as_ref()
                    .is_none_or(|p| p.writer.revision_hint().is_err())
                {
                    primary = None;
                    primary = Some(Primary::open(&replica.resource, config.cache_bytes)?);
                }
                let p = primary.as_mut().expect("opened replicated writer");
                let result = apply(&p.db, &works, sweep, &config, &changed)?;
                p.file.publish(&mut p.writer).map_err(storage)?;
                Ok(result)
            } else {
                apply(
                    db.as_ref().expect("opened local database"),
                    &works,
                    sweep,
                    &config,
                    &changed,
                )
            }
        }))
        .unwrap_or_else(|_| Err(StoreError::Storage("internal error".into())));
        match result {
            Ok((outcomes, more)) => {
                // Committed before the replies: a snapshot that starts once
                // a write is acknowledged sees the store changed.
                if outcomes.iter().any(|outcome| {
                    matches!(
                        outcome,
                        Ok(Outcome::Done | Outcome::Deleted(true) | Outcome::Replaced(_))
                    )
                }) {
                    changed.store(true, Ordering::Relaxed);
                }
                if sweep {
                    next_sweep = if more {
                        Instant::now()
                    } else {
                        Instant::now() + SWEEP_INTERVAL
                    };
                }
                for (reply, outcome) in replies.into_iter().zip(outcomes) {
                    let _ = reply.send(outcome);
                }
            }
            Err(error) => {
                if sweep {
                    next_sweep = Instant::now() + SWEEP_INTERVAL;
                }
                tracing::warn!(%error, "KV store write failed");
                let message = error.to_string();
                for reply in replies {
                    let _ = reply.send(Err(StoreError::Storage(message.clone())));
                }
            }
        }
    }
}

/// Whether a sweep has work: expired keys, or a replaced generation to free.
fn has_cleanup(db: &Database, now: u64) -> bool {
    let check = || -> Result<bool> {
        let txn = db.begin_read().map_err(storage)?;
        let (generation, trash) = read_generations(&txn.open_table(META).map_err(storage)?)?;
        if trash.is_some() {
            return Ok(true);
        }
        let expiry = txn
            .open_table(IndexTable::new(&table_name(EXPIRY, generation)))
            .map_err(storage)?;
        Ok(expiry.first().map_err(storage)?.is_some_and(|(id, _)| {
            let id = id.value();
            u64::from_be_bytes(id[..8].try_into().unwrap()) <= now
        }))
    };
    check().unwrap_or(false)
}

/// Applies a batch of work in one transaction, committed durably. A
/// failure fails the whole batch.
fn apply(
    db: &Database,
    works: &[Work],
    sweep: bool,
    config: &StoreConfig,
    changed: &AtomicBool,
) -> Result<(Vec<Result<Outcome>>, bool)> {
    let txn = db.begin_write().map_err(storage)?;
    let now = (config.clock)();
    let mut outcomes = Vec::with_capacity(works.len());
    let (dirty, more) = {
        let mut w = WriteView::open(&txn)?;
        for work in works {
            outcomes.push(match work {
                Work::Put { key, json, ttl_ms } => {
                    w.put(key, json, expiry_of(*ttl_ms, now), now)?;
                    Ok(Outcome::Done)
                }
                Work::Delete { key } => Ok(Outcome::Deleted(w.delete(key, now)?)),
                // Runs in a batch of its own, so its errors fail only it.
                Work::Replace { path, previous } => {
                    // Nothing else writes meanwhile, so the last commit is
                    // exactly what the replace removes. Every write before
                    // it already set the flag, as this thread commits them.
                    let previous_changed = changed.load(Ordering::Relaxed);
                    if let Some(previous) = previous {
                        export_database(db, previous, now)?;
                    }
                    Ok(Outcome::Replaced(Replaced {
                        stats: w.replace(path, now)?,
                        previous_changed,
                    }))
                }
            });
        }
        let (expired, trash) = if sweep {
            (SWEEP_ALONE, DRAIN_ALONE)
        } else {
            (SWEEP_WITH_WRITES, DRAIN_WITH_WRITES)
        };
        let more = w.sweep(now, expired)? && sweep;
        let more = w.drain(trash)? || more;
        w.flush()?;
        (w.dirty, more)
    };
    if dirty {
        txn.commit().map_err(storage)?;
    } else {
        txn.abort().map_err(storage)?;
    }
    Ok((outcomes, more))
}

#[cfg(test)]
mod tests;
