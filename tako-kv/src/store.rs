//! One key-value store: a redb file and the thread that writes it.
//!
//! Writes from every connection go to one writer thread, which applies all
//! the work waiting for it in one transaction and commits it durably before
//! replying: an acknowledged write survives a crash, and concurrent writers
//! share each commit's flush. Reads run on read transactions, which see the
//! last commit.
//!
//! The file is opened in redb's multi-writer mode, so the process taking
//! over during a server reload can open it while the old one drains.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::time::{Duration, Instant};

use bytes::Bytes;
use parking_lot::{Mutex, RwLock};
use redb::{
    ConcurrencyMode, Database, ReadableDatabase, ReadableTable, Table, TableDefinition,
    backends::FileBackend,
};

use crate::command::{
    self, Cmd, HASH_NOT_FLOAT, HASH_NOT_INTEGER, KeyType, Limits, NAN_OR_INFINITY, NO_SUCH_KEY,
    NOT_FLOAT, NOT_INTEGER, OOM, OVERFLOW, SetCondition, Ttl, TtlQuery, WRONGTYPE,
};
use crate::glob;
use crate::resp::Reply;

/// Keys by `slot || key`, where the slot is a hash of the key, so `SCAN`
/// cursors are numbers. Values encode the key's type, expiry and version.
const KEYS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("keys");
/// Hash fields by `len(key) || key || field`.
const FIELDS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("fields");
/// Keys with an expiry, by `expires_at || key`.
const EXPIRY: TableDefinition<&[u8], ()> = TableDefinition::new("expiry");
const META: TableDefinition<&str, u64> = TableDefinition::new("meta");

const ENTRY_HEADER: usize = 17;
/// Overhead charged for each key and each hash field, so many tiny entries
/// still count against the budget.
const KEY_OVERHEAD: u64 = 64;
const FIELD_OVERHEAD: u64 = 32;
/// Writes waiting for the writer thread before callers wait to queue more.
const QUEUE: usize = 1024;
/// Work applied in one transaction.
const BATCH_JOBS: usize = 256;
/// Replies a batch accumulates before it stops early; the connection sends
/// the rest of its commands in the next batch.
const BATCH_REPLY_BYTES: usize = 8 * 1024 * 1024;
const SWEEP_INTERVAL: Duration = Duration::from_millis(250);
/// Expired keys removed alongside other writes, and by a periodic sweep.
const SWEEP_WITH_WRITES: usize = 32;
const SWEEP_ALONE: usize = 10_000;

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
    /// Bytes of keys, values and per-entry overhead. Writes that add data
    /// are refused once a store holds this much.
    pub max_bytes: u64,
    pub limits: Limits,
    /// Bytes of redb page cache.
    pub cache_bytes: usize,
    /// The largest single reply, such as `KEYS` or `HGETALL`.
    pub max_reply_bytes: usize,
    pub clock: Clock,
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("{0}")]
    Storage(String),
    #[error("the store is closed")]
    Closed,
}

fn storage(error: impl std::fmt::Display) -> StoreError {
    StoreError::Storage(error.to_string())
}

type Result<T, E = StoreError> = std::result::Result<T, E>;

/// Totals kept in the `meta` table.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub bytes: u64,
    pub keys: u64,
    pub expires: u64,
    /// Incremented by every change; a key's version is its value then.
    seq: u64,
}

/// A transaction's commands, as the connection parsed them when `EXEC` ran.
pub(crate) enum Op {
    Cmd(Cmd),
    /// A command that failed to parse; its error is its reply.
    Failed(Reply),
}

/// A key a connection watches: its version when `WATCH` read it (`None`
/// while absent), and the store's change counter then.
pub(crate) struct Watched {
    pub key: Bytes,
    pub version: Option<u64>,
    pub seq: u64,
}

/// Keys removed while some connection watches keys. A key that was absent
/// when watched has no version to compare, so without this a transaction
/// would miss it being created and removed again before `EXEC`; Redis
/// aborts then.
#[derive(Default)]
struct Removals {
    /// The `seq` of each active `WATCH`, with how many share it. Removals no
    /// older than the oldest are kept.
    watching: BTreeMap<u64, usize>,
    /// The keys some `WATCH` holds, with how many hold each. Only their
    /// removals are kept, so what is kept is bounded by what is watched.
    keys: HashMap<Vec<u8>, usize>,
    /// The `seq` of each watched key's latest removal.
    removed: HashMap<Vec<u8>, u64>,
    /// The `seq` of the latest `FLUSHDB`, which removes every key.
    flushed: u64,
}

impl Removals {
    fn hold(&mut self, keys: &[Bytes]) {
        for key in keys {
            *self.keys.entry(key.to_vec()).or_insert(0) += 1;
        }
    }

    fn release(&mut self, keys: &[Bytes]) {
        for key in keys {
            if let Some(count) = self.keys.get_mut(key.as_ref()) {
                *count -= 1;
                if *count == 0 {
                    self.keys.remove(key.as_ref());
                    self.removed.remove(key.as_ref());
                }
            }
        }
    }

    /// When `key` was last removed, if after anything still watched.
    fn removed_at(&self, key: &[u8]) -> u64 {
        self.removed
            .get(key)
            .copied()
            .unwrap_or(0)
            .max(self.flushed)
    }

    fn unwatch(&mut self, seq: u64) {
        if let Some(count) = self.watching.get_mut(&seq) {
            *count -= 1;
            if *count == 0 {
                self.watching.remove(&seq);
            }
        }
        match self.watching.keys().next() {
            None => *self = Self::default(),
            Some(&oldest) => self.removed.retain(|_, at| *at > oldest),
        }
    }
}

/// Keeps the removals a connection's watched keys may need until dropped.
/// One guard covers every key the connection watches, from its first
/// `WATCH` on.
pub(crate) struct WatchGuard {
    removals: Arc<Mutex<Removals>>,
    seq: u64,
    keys: Vec<Bytes>,
}

impl Drop for WatchGuard {
    fn drop(&mut self) {
        let mut removals = self.removals.lock();
        removals.release(&self.keys);
        removals.unwatch(self.seq);
    }
}

enum Work {
    /// One connection's commands, in order, whose replies have `room`
    /// bytes. The writer may run only a prefix of them; see [`Ran`].
    Batch { cmds: Vec<Cmd>, room: usize },
    /// A transaction. Aborted, with a nil reply, when a watched key changed.
    /// Replies past `room` bytes become errors.
    Exec {
        ops: Vec<Op>,
        watched: Vec<Watched>,
        room: usize,
    },
}

struct Job {
    work: Work,
    reply: tokio::sync::oneshot::Sender<Result<Ran, String>>,
}

/// What a batch of commands returned: the replies of a prefix of them, all
/// unless the replies grew large or the next one did not fit.
#[derive(Debug)]
pub(crate) struct Ran {
    pub(crate) replies: Vec<Reply>,
    /// The batch stopped before a command, without running it, because its
    /// reply needs this many bytes and fewer were left of the room.
    pub(crate) short: Option<usize>,
}

/// One command's outcome in a batch.
enum Step {
    Done(Reply),
    /// Not run: its reply would need this many bytes.
    Short(usize),
}

pub struct Store {
    path: PathBuf,
    db: RwLock<Option<Arc<Database>>>,
    writer: Mutex<Option<SyncSender<Job>>>,
    thread: Mutex<Option<std::thread::JoinHandle<()>>>,
    config: Arc<StoreConfig>,
    removals: Arc<Mutex<Removals>>,
}

impl Store {
    /// Opens the store at `path`, creating the file if needed.
    pub fn open(path: &Path, config: StoreConfig) -> Result<Self> {
        let db = open_database(path, config.cache_bytes)?;
        let db = Arc::new(db);
        let config = Arc::new(config);
        let (sender, receiver) = std::sync::mpsc::sync_channel(QUEUE);
        let removals = Arc::new(Mutex::new(Removals::default()));
        let thread = {
            let db = db.clone();
            let config = config.clone();
            let removals = removals.clone();
            std::thread::Builder::new()
                .name("tako-kv-writer".into())
                .spawn(move || writer(db, receiver, config, removals))
                .map_err(storage)?
        };
        Ok(Self {
            path: path.to_owned(),
            db: RwLock::new(Some(db)),
            writer: Mutex::new(Some(sender)),
            thread: Mutex::new(Some(thread)),
            config,
            removals,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn db(&self) -> Result<Arc<Database>> {
        self.db.read().clone().ok_or(StoreError::Closed)
    }

    pub fn is_closed(&self) -> bool {
        self.db.read().is_none()
    }

    /// Stops accepting work, finishes what is queued, and closes the file
    /// once in-flight reads end.
    pub async fn close(&self) {
        drop(self.writer.lock().take());
        drop(self.db.write().take());
        let thread = self.thread.lock().take();
        if let Some(thread) = thread {
            let _ = tokio::task::spawn_blocking(move || thread.join()).await;
        }
    }

    async fn submit(&self, work: Work) -> Result<Ran> {
        let (reply, receive) = tokio::sync::oneshot::channel();
        let mut job = Job { work, reply };
        loop {
            let sender = self.writer.lock().clone().ok_or(StoreError::Closed)?;
            match sender.try_send(job) {
                Ok(()) => break,
                Err(TrySendError::Full(back)) => {
                    job = back;
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
                Err(TrySendError::Disconnected(_)) => return Err(StoreError::Closed),
            }
        }
        match receive.await {
            Ok(Ok(ran)) => Ok(ran),
            Ok(Err(error)) => Err(StoreError::Storage(error)),
            Err(_) => Err(StoreError::Closed),
        }
    }

    /// Runs one connection's commands in order, while their replies fit in
    /// `room` bytes, and returns the replies of a prefix of them.
    pub(crate) async fn run(&self, cmds: Vec<Cmd>, room: usize) -> Result<Ran> {
        if cmds.iter().any(Cmd::writes) {
            return self.submit(Work::Batch { cmds, room }).await;
        }
        let db = self.db()?;
        let config = self.config.clone();
        tokio::task::spawn_blocking(move || read_batch(&db, &cmds, room, &config))
            .await
            .map_err(storage)?
    }

    /// Runs a transaction whose replies have `room` bytes. The reply is its
    /// array, or nil when a watched key changed since `watched` was read.
    pub(crate) async fn exec(
        &self,
        ops: Vec<Op>,
        watched: Vec<Watched>,
        room: usize,
    ) -> Result<Reply> {
        let mut ran = self.submit(Work::Exec { ops, watched, room }).await?;
        Ok(ran.replies.pop().unwrap_or(Reply::NilArray))
    }

    /// Reads `keys` for `WATCH`, which `guard` then covers: the
    /// connection's guard, or a new one. It keeps the removals their
    /// transaction needs until it is dropped.
    pub(crate) async fn watch(
        &self,
        keys: Vec<Bytes>,
        guard: &mut Option<WatchGuard>,
    ) -> Result<Vec<Watched>> {
        let db = self.db()?;
        let clock = self.config.clock.clone();
        if guard
            .as_ref()
            .is_some_and(|held| !Arc::ptr_eq(&held.removals, &self.removals))
        {
            // Another store's guard covers nothing here.
            *guard = None;
        }
        // Registered before the read, so a removal committed after the
        // snapshot is kept. A new guard starts at 0 and moves to the
        // snapshot's counter below; an existing one is older than it.
        let fresh = guard.is_none();
        {
            let mut removals = self.removals.lock();
            removals.hold(&keys);
            if fresh {
                *removals.watching.entry(0).or_insert(0) += 1;
            }
        }
        let held = guard.get_or_insert_with(|| WatchGuard {
            removals: self.removals.clone(),
            seq: 0,
            keys: Vec::new(),
        });
        let start = held.keys.len();
        held.keys.extend(keys.iter().cloned());
        let read = tokio::task::spawn_blocking(move || {
            let view = ReadView::open(&db)?;
            let now = clock();
            let seq = view.stats.seq;
            keys.into_iter()
                .map(|key| {
                    let version = live(&view, &key, now)?.map(|entry| entry.version);
                    Ok(Watched { key, version, seq })
                })
                .collect::<Result<Vec<_>>>()
        })
        .await
        .map_err(storage)
        .and_then(|read| read);
        let read = match read {
            Ok(read) => read,
            Err(error) => {
                if fresh {
                    *guard = None;
                } else if let Some(held) = guard {
                    let added = held.keys.split_off(start);
                    held.removals.lock().release(&added);
                }
                return Err(error);
            }
        };
        if let (true, Some(held), Some(seq)) = (fresh, guard.as_mut(), read.first().map(|w| w.seq))
        {
            let mut removals = self.removals.lock();
            *removals.watching.entry(seq).or_insert(0) += 1;
            removals.unwatch(0);
            held.seq = seq;
        }
        Ok(read)
    }

    pub async fn stats(&self) -> Result<Stats> {
        let db = self.db()?;
        tokio::task::spawn_blocking(move || Ok(ReadView::open(&db)?.stats))
            .await
            .map_err(storage)?
    }
}

impl Drop for Store {
    fn drop(&mut self) {
        // The writer exits once its channel closes; it holds no lock anyone
        // waits for, so it is not joined here.
        self.writer.lock().take();
    }
}

/// Opens (or creates) a store file with every table in place, so read
/// transactions find them.
fn open_database(path: &Path, cache_bytes: usize) -> Result<Database> {
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
    let txn = db.begin_write().map_err(storage)?;
    txn.open_table(KEYS).map_err(storage)?;
    txn.open_table(FIELDS).map_err(storage)?;
    txn.open_table(EXPIRY).map_err(storage)?;
    txn.open_table(META).map_err(storage)?;
    txn.commit().map_err(storage)?;
    Ok(db)
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

/// Where a key sorts: a stable 64-bit hash, never zero, so a `SCAN` cursor
/// of 0 means the start and the end.
fn slot(key: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in key {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash.max(1)
}

fn key_id(key: &[u8]) -> Vec<u8> {
    let mut id = Vec::with_capacity(8 + key.len());
    id.extend_from_slice(&slot(key).to_be_bytes());
    id.extend_from_slice(key);
    id
}

fn field_prefix(key: &[u8]) -> Vec<u8> {
    let mut id = Vec::with_capacity(4 + key.len());
    id.extend_from_slice(&(key.len() as u32).to_be_bytes());
    id.extend_from_slice(key);
    id
}

/// A field sorts by its slot within its hash, so `HSCAN` pages like `SCAN`.
fn field_id(key: &[u8], field: &[u8]) -> Vec<u8> {
    let mut id = field_prefix(key);
    id.extend_from_slice(&slot(field).to_be_bytes());
    id.extend_from_slice(field);
    id
}

/// The first id after every id starting with `prefix`. Prefixes start with
/// a length below 2^24, so the increment always fits.
fn prefix_end(prefix: &[u8]) -> Vec<u8> {
    let mut end = prefix.to_vec();
    while let Some(last) = end.last_mut() {
        if *last == u8::MAX {
            end.pop();
        } else {
            *last += 1;
            break;
        }
    }
    end
}

fn expiry_id(when: u64, key: &[u8]) -> Vec<u8> {
    let mut id = Vec::with_capacity(8 + key.len());
    id.extend_from_slice(&when.to_be_bytes());
    id.extend_from_slice(key);
    id
}

#[derive(Clone, Debug)]
struct Entry {
    kind: KeyType,
    /// Unix milliseconds, or 0 for none.
    expires: u64,
    version: u64,
    /// A string's value, or a hash's field count and field bytes.
    data: Vec<u8>,
}

impl Entry {
    fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < ENTRY_HEADER {
            return Err(StoreError::Storage("corrupt key entry".into()));
        }
        let kind = match bytes[0] {
            0 => KeyType::String,
            1 => KeyType::Hash,
            _ => return Err(StoreError::Storage("corrupt key entry".into())),
        };
        let entry = Self {
            kind,
            expires: u64::from_be_bytes(bytes[1..9].try_into().unwrap()),
            version: u64::from_be_bytes(bytes[9..17].try_into().unwrap()),
            data: bytes[ENTRY_HEADER..].to_vec(),
        };
        if kind == KeyType::Hash && entry.data.len() != 16 {
            return Err(StoreError::Storage("corrupt hash entry".into()));
        }
        Ok(entry)
    }

    fn encode(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(ENTRY_HEADER + self.data.len());
        bytes.push(match self.kind {
            KeyType::String => 0,
            KeyType::Hash => 1,
        });
        bytes.extend_from_slice(&self.expires.to_be_bytes());
        bytes.extend_from_slice(&self.version.to_be_bytes());
        bytes.extend_from_slice(&self.data);
        bytes
    }

    fn string(value: &[u8], expires: u64) -> Self {
        Self {
            kind: KeyType::String,
            expires,
            version: 0,
            data: value.to_vec(),
        }
    }

    fn hash() -> Self {
        Self {
            kind: KeyType::Hash,
            expires: 0,
            version: 0,
            data: vec![0; 16],
        }
    }

    fn live(&self, now: u64) -> bool {
        self.expires == 0 || self.expires > now
    }

    /// A hash's field count and the bytes its fields are charged.
    fn fields(&self) -> (u64, u64) {
        (
            u64::from_be_bytes(self.data[..8].try_into().unwrap()),
            u64::from_be_bytes(self.data[8..16].try_into().unwrap()),
        )
    }

    fn set_fields(&mut self, count: u64, bytes: u64) {
        self.data[..8].copy_from_slice(&count.to_be_bytes());
        self.data[8..16].copy_from_slice(&bytes.to_be_bytes());
    }

    fn ttl(&self) -> Option<i64> {
        (self.expires != 0).then_some(self.expires as i64)
    }
}

fn key_weight(key: &[u8], entry: &Entry) -> u64 {
    KEY_OVERHEAD + 2 * key.len() as u64 + entry.data.len() as u64
}

fn field_weight(key: &[u8], field: &[u8], value: &[u8]) -> u64 {
    FIELD_OVERHEAD + (key.len() + field.len() + value.len()) as u64
}

/// Called with a field's slot, the field, and its value.
type FieldVisit<'a> = dyn FnMut(u64, &[u8], &[u8]) -> bool + 'a;

/// What both kinds of transaction can read.
trait View {
    fn entry(&self, key: &[u8]) -> Result<Option<Entry>>;
    fn field(&self, key: &[u8], field: &[u8]) -> Result<Option<Vec<u8>>>;
    /// Visits a hash's fields from `slot` on, in slot order, until `visit`
    /// returns false.
    fn each_field(&self, key: &[u8], from: u64, visit: &mut FieldVisit) -> Result<()>;
    /// Visits keys from `slot` on, in slot order, until `visit` returns false.
    fn each_key(&self, from: u64, visit: &mut dyn FnMut(u64, &[u8], &Entry) -> bool) -> Result<()>;
    fn stats(&self) -> Stats;
}

fn get_entry<T: ReadableTable<&'static [u8], &'static [u8]>>(
    keys: &T,
    key: &[u8],
) -> Result<Option<Entry>> {
    keys.get(key_id(key).as_slice())
        .map_err(storage)?
        .map(|value| Entry::decode(value.value()))
        .transpose()
}

fn get_field<T: ReadableTable<&'static [u8], &'static [u8]>>(
    fields: &T,
    key: &[u8],
    field: &[u8],
) -> Result<Option<Vec<u8>>> {
    Ok(fields
        .get(field_id(key, field).as_slice())
        .map_err(storage)?
        .map(|value| value.value().to_vec()))
}

fn each_field<T: ReadableTable<&'static [u8], &'static [u8]>>(
    fields: &T,
    key: &[u8],
    from: u64,
    visit: &mut FieldVisit,
) -> Result<()> {
    let prefix = field_prefix(key);
    let end = prefix_end(&prefix);
    let mut start = prefix.clone();
    start.extend_from_slice(&from.to_be_bytes());
    for item in fields
        .range(start.as_slice()..end.as_slice())
        .map_err(storage)?
    {
        let (id, value) = item.map_err(storage)?;
        let Some(rest) = id
            .value()
            .get(prefix.len()..)
            .filter(|rest| rest.len() >= 8)
        else {
            return Err(StoreError::Storage("corrupt field id".into()));
        };
        let slot = u64::from_be_bytes(rest[..8].try_into().unwrap());
        if !visit(slot, &rest[8..], value.value()) {
            break;
        }
    }
    Ok(())
}

fn each_key<T: ReadableTable<&'static [u8], &'static [u8]>>(
    keys: &T,
    from: u64,
    visit: &mut dyn FnMut(u64, &[u8], &Entry) -> bool,
) -> Result<()> {
    let start = from.to_be_bytes();
    for item in keys.range(start.as_slice()..).map_err(storage)? {
        let (id, value) = item.map_err(storage)?;
        let id = id.value();
        if id.len() < 8 {
            return Err(StoreError::Storage("corrupt key id".into()));
        }
        let slot = u64::from_be_bytes(id[..8].try_into().unwrap());
        let entry = Entry::decode(value.value())?;
        if !visit(slot, &id[8..], &entry) {
            break;
        }
    }
    Ok(())
}

fn read_stats<T: ReadableTable<&'static str, u64>>(meta: &T) -> Result<Stats> {
    let get = |name: &str| -> Result<u64> {
        Ok(meta.get(name).map_err(storage)?.map_or(0, |v| v.value()))
    };
    Ok(Stats {
        bytes: get("bytes")?,
        keys: get("keys")?,
        expires: get("expires")?,
        seq: get("seq")?,
    })
}

struct ReadView {
    keys: redb::ReadOnlyTable<&'static [u8], &'static [u8]>,
    fields: redb::ReadOnlyTable<&'static [u8], &'static [u8]>,
    stats: Stats,
}

impl ReadView {
    fn open(db: &Database) -> Result<Self> {
        let txn = db.begin_read().map_err(storage)?;
        Ok(Self {
            keys: txn.open_table(KEYS).map_err(storage)?,
            fields: txn.open_table(FIELDS).map_err(storage)?,
            stats: read_stats(&txn.open_table(META).map_err(storage)?)?,
        })
    }
}

impl View for ReadView {
    fn entry(&self, key: &[u8]) -> Result<Option<Entry>> {
        get_entry(&self.keys, key)
    }
    fn field(&self, key: &[u8], field: &[u8]) -> Result<Option<Vec<u8>>> {
        get_field(&self.fields, key, field)
    }
    fn each_field(&self, key: &[u8], from: u64, visit: &mut FieldVisit) -> Result<()> {
        each_field(&self.fields, key, from, visit)
    }
    fn each_key(&self, from: u64, visit: &mut dyn FnMut(u64, &[u8], &Entry) -> bool) -> Result<()> {
        each_key(&self.keys, from, visit)
    }
    fn stats(&self) -> Stats {
        self.stats
    }
}

struct WriteView<'t> {
    keys: Table<'t, &'static [u8], &'static [u8]>,
    fields: Table<'t, &'static [u8], &'static [u8]>,
    expiry: Table<'t, &'static [u8], ()>,
    meta: Table<'t, &'static str, u64>,
    stats: Stats,
    dirty: bool,
    /// Keys this transaction removed, with the counter then, for
    /// [`Removals`] once it commits.
    removed: Vec<(Vec<u8>, u64)>,
    flushed: Option<u64>,
}

impl View for WriteView<'_> {
    fn entry(&self, key: &[u8]) -> Result<Option<Entry>> {
        get_entry(&self.keys, key)
    }
    fn field(&self, key: &[u8], field: &[u8]) -> Result<Option<Vec<u8>>> {
        get_field(&self.fields, key, field)
    }
    fn each_field(&self, key: &[u8], from: u64, visit: &mut FieldVisit) -> Result<()> {
        each_field(&self.fields, key, from, visit)
    }
    fn each_key(&self, from: u64, visit: &mut dyn FnMut(u64, &[u8], &Entry) -> bool) -> Result<()> {
        each_key(&self.keys, from, visit)
    }
    fn stats(&self) -> Stats {
        self.stats
    }
}

impl<'t> WriteView<'t> {
    fn open(txn: &'t redb::WriteTransaction) -> Result<Self> {
        let meta = txn.open_table(META).map_err(storage)?;
        let stats = read_stats(&meta)?;
        Ok(Self {
            keys: txn.open_table(KEYS).map_err(storage)?,
            fields: txn.open_table(FIELDS).map_err(storage)?,
            expiry: txn.open_table(EXPIRY).map_err(storage)?,
            meta,
            stats,
            dirty: false,
            removed: Vec::new(),
            flushed: None,
        })
    }

    /// When this transaction last removed `key`, or 0.
    fn removed_at(&self, key: &[u8]) -> u64 {
        self.removed
            .iter()
            .rev()
            .find(|(removed, _)| removed == key)
            .map_or(0, |(_, at)| *at)
            .max(self.flushed.unwrap_or(0))
    }

    fn flush(&mut self) -> Result<()> {
        if self.dirty {
            for (name, value) in [
                ("bytes", self.stats.bytes),
                ("keys", self.stats.keys),
                ("expires", self.stats.expires),
                ("seq", self.stats.seq),
            ] {
                self.meta.insert(name, value).map_err(storage)?;
            }
        }
        Ok(())
    }

    /// The key's entry, removing it first when it has expired.
    fn load(&mut self, key: &[u8], now: u64) -> Result<Option<Entry>> {
        match self.entry(key)? {
            Some(entry) if !entry.live(now) => {
                self.remove(key, &entry)?;
                Ok(None)
            }
            entry => Ok(entry),
        }
    }

    fn remove(&mut self, key: &[u8], entry: &Entry) -> Result<()> {
        self.dirty = true;
        self.stats.seq += 1;
        self.removed.push((key.to_vec(), self.stats.seq));
        self.keys.remove(key_id(key).as_slice()).map_err(storage)?;
        if entry.expires != 0 {
            self.expiry
                .remove(expiry_id(entry.expires, key).as_slice())
                .map_err(storage)?;
            self.stats.expires -= 1;
        }
        let mut weight = key_weight(key, entry);
        if entry.kind == KeyType::Hash {
            let prefix = field_prefix(key);
            let end = prefix_end(&prefix);
            self.fields
                .retain_in(prefix.as_slice()..end.as_slice(), |_, _| false)
                .map_err(storage)?;
            weight += entry.fields().1;
        }
        self.stats.bytes = self.stats.bytes.saturating_sub(weight);
        self.stats.keys = self.stats.keys.saturating_sub(1);
        Ok(())
    }

    /// Writes `entry` for `key`, replacing `old` (which must be the same
    /// type, or already removed), with a new version.
    fn put(&mut self, key: &[u8], old: Option<&Entry>, mut entry: Entry) -> Result<()> {
        self.dirty = true;
        self.stats.seq += 1;
        entry.version = self.stats.seq;
        let old_expires = old.map_or(0, |old| old.expires);
        if old_expires != entry.expires {
            if old_expires != 0 {
                self.expiry
                    .remove(expiry_id(old_expires, key).as_slice())
                    .map_err(storage)?;
                self.stats.expires -= 1;
            }
            if entry.expires != 0 {
                self.expiry
                    .insert(expiry_id(entry.expires, key).as_slice(), ())
                    .map_err(storage)?;
                self.stats.expires += 1;
            }
        }
        match old {
            Some(old) => {
                self.stats.bytes = self.stats.bytes.saturating_sub(key_weight(key, old));
            }
            None => self.stats.keys += 1,
        }
        self.stats.bytes += key_weight(key, &entry);
        self.keys
            .insert(key_id(key).as_slice(), entry.encode().as_slice())
            .map_err(storage)?;
        Ok(())
    }

    /// Sets a string, replacing whatever the key held.
    fn put_string(
        &mut self,
        key: &[u8],
        old: Option<Entry>,
        value: &[u8],
        expires: u64,
    ) -> Result<()> {
        let old = match old {
            Some(old) if old.kind != KeyType::String => {
                self.remove(key, &old)?;
                None
            }
            old => old,
        };
        self.put(key, old.as_ref(), Entry::string(value, expires))
    }

    fn set_field(
        &mut self,
        key: &[u8],
        hash: &mut Entry,
        field: &[u8],
        value: &[u8],
    ) -> Result<bool> {
        self.dirty = true;
        let previous = self
            .fields
            .insert(field_id(key, field).as_slice(), value)
            .map_err(storage)?
            .map(|v| field_weight(key, field, v.value()));
        let (mut count, mut bytes) = hash.fields();
        if previous.is_none() {
            count += 1;
        }
        let added = field_weight(key, field, value);
        bytes = bytes - previous.unwrap_or(0) + added;
        hash.set_fields(count, bytes);
        self.stats.bytes = self.stats.bytes.saturating_sub(previous.unwrap_or(0)) + added;
        Ok(previous.is_none())
    }

    fn delete_field(&mut self, key: &[u8], hash: &mut Entry, field: &[u8]) -> Result<bool> {
        let removed = self
            .fields
            .remove(field_id(key, field).as_slice())
            .map_err(storage)?
            .map(|v| field_weight(key, field, v.value()));
        let Some(weight) = removed else {
            return Ok(false);
        };
        self.dirty = true;
        let (count, bytes) = hash.fields();
        hash.set_fields(count - 1, bytes - weight);
        self.stats.bytes = self.stats.bytes.saturating_sub(weight);
        Ok(true)
    }

    fn flush_all(&mut self) -> Result<()> {
        self.dirty = true;
        self.keys.retain(|_, _| false).map_err(storage)?;
        self.fields.retain(|_, _| false).map_err(storage)?;
        self.expiry.retain(|_, _| false).map_err(storage)?;
        self.stats = Stats {
            seq: self.stats.seq + 1,
            ..Stats::default()
        };
        self.flushed = Some(self.stats.seq);
        Ok(())
    }

    /// Removes up to `limit` expired keys. Returns whether more remain.
    fn sweep(&mut self, now: u64, limit: usize) -> Result<bool> {
        let mut due = Vec::new();
        let mut more = false;
        for item in self.expiry.iter().map_err(storage)? {
            let (id, _) = item.map_err(storage)?;
            let id = id.value();
            let when = u64::from_be_bytes(id[..8].try_into().unwrap());
            if when > now {
                break;
            }
            if due.len() == limit {
                more = true;
                break;
            }
            due.push(id.to_vec());
        }
        for id in due {
            let key = &id[8..];
            match self.entry(key)? {
                Some(entry) if !entry.live(now) => self.remove(key, &entry)?,
                // A stale index entry; it should not exist, but must not
                // stop the sweep.
                _ => {
                    self.dirty = true;
                    self.expiry.remove(id.as_slice()).map_err(storage)?;
                }
            }
        }
        Ok(more)
    }

    /// The live hash at `key`, or a new one; `Err` replies for a string.
    fn hash_for_write(
        &mut self,
        key: &[u8],
        now: u64,
    ) -> Result<Result<(Option<Entry>, Entry), Reply>> {
        Ok(match self.load(key, now)? {
            Some(entry) if entry.kind != KeyType::Hash => Err(Reply::error(WRONGTYPE)),
            Some(entry) => Ok((Some(entry.clone()), entry)),
            None => Ok((None, Entry::hash())),
        })
    }

    /// Stores a changed hash entry, or removes the key once it is empty.
    fn finish_hash(&mut self, key: &[u8], old: Option<Entry>, hash: Entry) -> Result<()> {
        if hash.fields().0 == 0 {
            if let Some(old) = old {
                // Its fields are already gone; remove the entry itself.
                let mut empty = old;
                empty.set_fields(0, 0);
                self.remove(key, &empty)?;
            }
            return Ok(());
        }
        self.put(key, old.as_ref(), hash)
    }
}

fn live<V: View + ?Sized>(view: &V, key: &[u8], now: u64) -> Result<Option<Entry>> {
    Ok(view.entry(key)?.filter(|entry| entry.live(now)))
}

/// A string's value; `Err` replies for another type.
fn string<V: View + ?Sized>(
    view: &V,
    key: &[u8],
    now: u64,
) -> Result<Result<Option<Vec<u8>>, Reply>> {
    Ok(match live(view, key, now)? {
        Some(entry) if entry.kind == KeyType::String => Ok(Some(entry.data)),
        Some(_) => Err(Reply::error(WRONGTYPE)),
        None => Ok(None),
    })
}

/// A live hash; `Err` replies for another type.
fn hash<V: View + ?Sized>(view: &V, key: &[u8], now: u64) -> Result<Result<Option<Entry>, Reply>> {
    Ok(match live(view, key, now)? {
        Some(entry) if entry.kind == KeyType::Hash => Ok(Some(entry)),
        Some(_) => Err(Reply::error(WRONGTYPE)),
        None => Ok(None),
    })
}

/// Bytes of a `SCAN` or `HSCAN` reply besides its items, at most: the
/// arrays and the cursor.
const PAGE_FRAMING: usize = 72;

fn too_large(limit: usize) -> Reply {
    Reply::Error(format!(
        "ERR reply would exceed {limit} bytes; use SCAN, HSCAN or smaller requests"
    ))
}

fn format_float(value: f64) -> String {
    let text = format!("{value}");
    if text == "-0" { "0".into() } else { text }
}

macro_rules! tri {
    ($e:expr) => {
        match $e? {
            Ok(value) => value,
            Err(reply) => return Ok(reply),
        }
    };
}

/// Runs a command that only reads. `SCAN` and `HSCAN` pages end once they
/// pass `page` bytes.
fn read<V: View + ?Sized>(
    view: &V,
    cmd: &Cmd,
    now: u64,
    config: &StoreConfig,
    page: usize,
) -> Result<Reply> {
    let limit = config.max_reply_bytes;
    // What a page's items may take: its framing is the rest.
    let page = page.min(limit).saturating_sub(PAGE_FRAMING);
    Ok(match cmd {
        Cmd::Get(key) => match tri!(string(view, key, now)) {
            Some(value) => Reply::bulk(value),
            None => Reply::Nil,
        },
        Cmd::MGet(keys) => {
            let mut items = Vec::with_capacity(keys.len());
            let mut size = 0;
            for key in keys {
                let item = match live(view, key, now)? {
                    Some(entry) if entry.kind == KeyType::String => Reply::bulk(entry.data),
                    _ => Reply::Nil,
                };
                size += item.size();
                if size > limit {
                    return Ok(too_large(limit));
                }
                items.push(item);
            }
            Reply::Array(items)
        }
        Cmd::Strlen(key) => Reply::Int(tri!(string(view, key, now)).map_or(0, |v| v.len() as i64)),
        Cmd::Exists(keys) => {
            let mut count = 0;
            for key in keys {
                count += i64::from(live(view, key, now)?.is_some());
            }
            Reply::Int(count)
        }
        Cmd::Type(key) => Reply::Simple(
            live(view, key, now)?
                .map_or("none", |entry| entry.kind.name())
                .to_owned(),
        ),
        Cmd::Ttl { key, query } => Reply::Int(match live(view, key, now)? {
            None => -2,
            Some(entry) if entry.expires == 0 => -1,
            Some(entry) => {
                let remaining = entry.expires - now;
                match query {
                    TtlQuery::Seconds => ((remaining + 500) / 1000) as i64,
                    TtlQuery::Millis => remaining as i64,
                    // Rounded, as Redis rounds `TTL` and `EXPIRETIME`.
                    TtlQuery::UnixSeconds => ((entry.expires + 500) / 1000) as i64,
                    TtlQuery::UnixMillis => entry.expires as i64,
                }
            }
        }),
        Cmd::Keys(pattern) => {
            let mut items = Vec::new();
            let mut size = 0;
            let mut over = false;
            let mut budget = glob::Budget::new();
            let mut complex = false;
            view.each_key(0, &mut |_, key, entry| {
                if !entry.live(now) {
                    return true;
                }
                match glob::matches(pattern, key, &mut budget) {
                    None => {
                        complex = true;
                        return false;
                    }
                    Some(false) => return true,
                    Some(true) => {}
                }
                size += key.len() + 16;
                if size > limit {
                    over = true;
                    return false;
                }
                items.push(Reply::bulk(Bytes::copy_from_slice(key)));
                true
            })?;
            if complex {
                return Ok(Reply::error(glob::TOO_COMPLEX));
            }
            if over {
                return Ok(too_large(limit));
            }
            Reply::Array(items)
        }
        Cmd::Scan {
            cursor,
            pattern,
            count,
            kind,
        } => {
            let mut items = Vec::new();
            let mut visited = 0;
            let mut size = 0;
            let mut last = 0;
            let mut next = 0;
            let mut budget = glob::Budget::new();
            let mut complex = false;
            view.each_key(*cursor, &mut |slot, key, entry| {
                // A page ends at COUNT keys, or before a key its reply has
                // no room for, and only at the first key of another slot:
                // keys sharing a slot are returned together, so the next
                // cursor always moves forward.
                if visited >= *count && slot != last {
                    next = slot;
                    return false;
                }
                let mut matched =
                    entry.live(now) && kind.is_none_or(|kind| kind == Some(entry.kind));
                if matched && let Some(pattern) = pattern {
                    match glob::matches(pattern, key, &mut budget) {
                        None => {
                            complex = true;
                            return false;
                        }
                        Some(found) => matched = found,
                    }
                }
                let cost = if matched { key.len() + 16 } else { 0 };
                if size + cost > page && visited > 0 && slot != last {
                    next = slot;
                    return false;
                }
                visited += 1;
                last = slot;
                if matched {
                    size += cost;
                    items.push(Reply::bulk(Bytes::copy_from_slice(key)));
                }
                true
            })?;
            if complex {
                return Ok(Reply::error(glob::TOO_COMPLEX));
            }
            Reply::Array(vec![Reply::bulk(next.to_string()), Reply::Array(items)])
        }
        Cmd::DbSize => Reply::Int(view.stats().keys as i64),
        Cmd::HGet { key, field } => match tri!(hash(view, key, now)) {
            Some(_) => view.field(key, field)?.map_or(Reply::Nil, Reply::bulk),
            None => Reply::Nil,
        },
        Cmd::HMGet { key, fields } => {
            let present = tri!(hash(view, key, now)).is_some();
            let mut items = Vec::with_capacity(fields.len());
            let mut size = 0;
            for field in fields {
                let item = match present {
                    true => view.field(key, field)?.map_or(Reply::Nil, Reply::bulk),
                    false => Reply::Nil,
                };
                size += item.size();
                if size > limit {
                    return Ok(too_large(limit));
                }
                items.push(item);
            }
            Reply::Array(items)
        }
        Cmd::HScan {
            key,
            cursor,
            pattern,
            count,
            values,
        } => {
            let mut items = Vec::new();
            let mut next = 0;
            if tri!(hash(view, key, now)).is_some() {
                let mut visited = 0;
                let mut size = 0;
                let mut last = 0;
                let mut budget = glob::Budget::new();
                let mut complex = false;
                view.each_field(key, *cursor, &mut |slot, field, value| {
                    // Pages end like SCAN's, at a slot boundary.
                    if visited >= *count && slot != last {
                        next = slot;
                        return false;
                    }
                    let matched = match pattern {
                        Some(pattern) => match glob::matches(pattern, field, &mut budget) {
                            None => {
                                complex = true;
                                return false;
                            }
                            Some(matched) => matched,
                        },
                        None => true,
                    };
                    let cost = match matched {
                        true => 32 + field.len() + if *values { value.len() } else { 0 },
                        false => 0,
                    };
                    // Large values end a page early instead of failing it.
                    if size + cost > page && visited > 0 && slot != last {
                        next = slot;
                        return false;
                    }
                    visited += 1;
                    last = slot;
                    if matched {
                        size += cost;
                        items.push(Reply::bulk(Bytes::copy_from_slice(field)));
                        if *values {
                            items.push(Reply::bulk(Bytes::copy_from_slice(value)));
                        }
                    }
                    true
                })?;
                if complex {
                    return Ok(Reply::error(glob::TOO_COMPLEX));
                }
            }
            Reply::Array(vec![Reply::bulk(next.to_string()), Reply::Array(items)])
        }
        Cmd::HGetAll(key) | Cmd::HKeys(key) | Cmd::HVals(key) => {
            let mut pairs = Vec::new();
            if tri!(hash(view, key, now)).is_some() {
                let (keys, values) = match cmd {
                    Cmd::HKeys(_) => (true, false),
                    Cmd::HVals(_) => (false, true),
                    _ => (true, true),
                };
                let mut size = 0;
                let mut over = false;
                view.each_field(key, 0, &mut |_, field, value| {
                    size += 32
                        + if keys { field.len() } else { 0 }
                        + if values { value.len() } else { 0 };
                    if size > limit {
                        over = true;
                        return false;
                    }
                    pairs.push((
                        keys.then(|| Bytes::copy_from_slice(field)),
                        values.then(|| Bytes::copy_from_slice(value)),
                    ));
                    true
                })?;
                if over {
                    return Ok(too_large(limit));
                }
            }
            match cmd {
                Cmd::HGetAll(_) => Reply::Map(
                    pairs
                        .into_iter()
                        .map(|(f, v)| (Reply::bulk(f.unwrap()), Reply::bulk(v.unwrap())))
                        .collect(),
                ),
                _ => Reply::Array(
                    pairs
                        .into_iter()
                        .map(|(f, v)| Reply::bulk(f.or(v).unwrap()))
                        .collect(),
                ),
            }
        }
        Cmd::HLen(key) => {
            Reply::Int(tri!(hash(view, key, now)).map_or(0, |entry| entry.fields().0 as i64))
        }
        Cmd::HExists { key, field } => Reply::Int(match tri!(hash(view, key, now)) {
            Some(_) => i64::from(view.field(key, field)?.is_some()),
            None => 0,
        }),
        Cmd::HStrlen { key, field } => Reply::Int(match tri!(hash(view, key, now)) {
            Some(_) => view.field(key, field)?.map_or(0, |v| v.len() as i64),
            None => 0,
        }),
        _ => unreachable!("{cmd:?} writes"),
    })
}

fn grows(cmd: &Cmd) -> bool {
    matches!(
        cmd,
        Cmd::Set { .. }
            | Cmd::SetNx { .. }
            | Cmd::MSet { .. }
            | Cmd::Append { .. }
            | Cmd::IncrBy { .. }
            | Cmd::IncrByFloat { .. }
            | Cmd::HSet { .. }
            | Cmd::HSetNx { .. }
            | Cmd::HIncrBy { .. }
            | Cmd::HIncrByFloat { .. }
    )
}

fn invalid_expire(name: &str) -> Reply {
    Reply::Error(format!("ERR invalid expire time in '{name}' command"))
}

/// Runs any command in a write transaction.
fn write(
    w: &mut WriteView,
    cmd: &Cmd,
    now: u64,
    config: &StoreConfig,
    page: usize,
) -> Result<Reply> {
    if !cmd.writes() {
        return read(w, cmd, now, config, page);
    }
    if grows(cmd) && w.stats.bytes >= config.max_bytes {
        return Ok(Reply::error(OOM));
    }
    let max_value = config.limits.max_value_bytes;
    Ok(match cmd {
        Cmd::Set {
            key,
            value,
            condition,
            get,
            ttl,
            name,
        } => {
            let old = w.load(key, now)?;
            if *get && old.as_ref().is_some_and(|old| old.kind != KeyType::String) {
                return Ok(Reply::error(WRONGTYPE));
            }
            let previous = old.as_ref().map(|old| Bytes::copy_from_slice(&old.data));
            let reply_for = |applied: bool| match (*get, applied) {
                (true, _) => previous.clone().map_or(Reply::Nil, Reply::Bulk),
                (false, true) => Reply::Ok,
                (false, false) => Reply::Nil,
            };
            let allowed = match condition {
                SetCondition::Always => true,
                SetCondition::IfMissing => old.is_none(),
                SetCondition::IfExists => old.is_some(),
            };
            if !allowed {
                return Ok(reply_for(false));
            }
            let expires = match ttl {
                Ttl::Clear => 0,
                Ttl::Keep => old.as_ref().map_or(0, |old| old.expires),
                Ttl::Set(when) => match when.resolve(now) {
                    Some(at) if at <= now as i64 => {
                        // Already expired: the key ends up absent.
                        if let Some(old) = &old {
                            w.remove(key, old)?;
                        }
                        return Ok(reply_for(true));
                    }
                    Some(at) => at as u64,
                    None => return Ok(invalid_expire(name)),
                },
            };
            w.put_string(key, old, value, expires)?;
            reply_for(true)
        }
        Cmd::SetNx { key, value } => match w.load(key, now)? {
            Some(_) => Reply::Int(0),
            None => {
                w.put_string(key, None, value, 0)?;
                Reply::Int(1)
            }
        },
        Cmd::GetDel(key) => match w.load(key, now)? {
            Some(entry) if entry.kind != KeyType::String => Reply::error(WRONGTYPE),
            Some(entry) => {
                w.remove(key, &entry)?;
                Reply::bulk(entry.data)
            }
            None => Reply::Nil,
        },
        Cmd::GetEx { key, ttl } => match w.load(key, now)? {
            Some(entry) if entry.kind != KeyType::String => Reply::error(WRONGTYPE),
            Some(entry) => {
                let value = Reply::bulk(entry.data.clone());
                match ttl {
                    Ttl::Keep => {}
                    Ttl::Clear => {
                        if entry.expires != 0 {
                            let mut changed = entry.clone();
                            changed.expires = 0;
                            w.put(key, Some(&entry), changed)?;
                        }
                    }
                    Ttl::Set(when) => match when.resolve(now) {
                        Some(at) if at <= now as i64 => w.remove(key, &entry)?,
                        Some(at) => {
                            let mut changed = entry.clone();
                            changed.expires = at as u64;
                            w.put(key, Some(&entry), changed)?;
                        }
                        None => return Ok(invalid_expire("getex")),
                    },
                }
                value
            }
            None => Reply::Nil,
        },
        Cmd::MSet { pairs, only_new } => {
            if *only_new {
                for (key, _) in pairs {
                    if w.load(key, now)?.is_some() {
                        return Ok(Reply::Int(0));
                    }
                }
            }
            for (key, value) in pairs {
                let old = w.load(key, now)?;
                w.put_string(key, old, value, 0)?;
            }
            if *only_new { Reply::Int(1) } else { Reply::Ok }
        }
        Cmd::Append { key, value } => match w.load(key, now)? {
            Some(entry) if entry.kind != KeyType::String => Reply::error(WRONGTYPE),
            Some(entry) => {
                if entry.data.len() + value.len() > max_value {
                    return Ok(Reply::error("ERR string exceeds maximum allowed size"));
                }
                let mut changed = entry.clone();
                changed.data.extend_from_slice(value);
                let len = changed.data.len();
                w.put(key, Some(&entry), changed)?;
                Reply::Int(len as i64)
            }
            None => {
                w.put_string(key, None, value, 0)?;
                Reply::Int(value.len() as i64)
            }
        },
        Cmd::IncrBy { key, by } => {
            let old = w.load(key, now)?;
            let current = match &old {
                Some(entry) if entry.kind != KeyType::String => return Ok(Reply::error(WRONGTYPE)),
                Some(entry) => match command::int(&entry.data) {
                    Some(value) => value,
                    None => return Ok(Reply::error(NOT_INTEGER)),
                },
                None => 0,
            };
            let Some(next) = current.checked_add(*by) else {
                return Ok(Reply::error(OVERFLOW));
            };
            let expires = old.as_ref().map_or(0, |old| old.expires);
            w.put_string(key, old, next.to_string().as_bytes(), expires)?;
            Reply::Int(next)
        }
        Cmd::IncrByFloat { key, by } => {
            let old = w.load(key, now)?;
            let current = match &old {
                Some(entry) if entry.kind != KeyType::String => return Ok(Reply::error(WRONGTYPE)),
                Some(entry) => match command::float(&entry.data) {
                    Some(value) => value,
                    None => return Ok(Reply::error(NOT_FLOAT)),
                },
                None => 0.0,
            };
            let next = current + by;
            if !next.is_finite() {
                return Ok(Reply::error(NAN_OR_INFINITY));
            }
            let text = format_float(next);
            let expires = old.as_ref().map_or(0, |old| old.expires);
            w.put_string(key, old, text.as_bytes(), expires)?;
            Reply::bulk(text)
        }
        Cmd::Del(keys) => {
            let mut count = 0;
            for key in keys {
                if let Some(entry) = w.load(key, now)? {
                    w.remove(key, &entry)?;
                    count += 1;
                }
            }
            Reply::Int(count)
        }
        Cmd::Expire {
            key,
            when,
            condition,
            name,
        } => {
            let Some(entry) = w.load(key, now)? else {
                return Ok(Reply::Int(0));
            };
            let Some(at) = when.resolve(now) else {
                return Ok(invalid_expire(name));
            };
            if !condition.allows(entry.ttl(), at) {
                return Ok(Reply::Int(0));
            }
            if at <= now as i64 {
                w.remove(key, &entry)?;
            } else {
                let mut changed = entry.clone();
                changed.expires = at as u64;
                w.put(key, Some(&entry), changed)?;
            }
            Reply::Int(1)
        }
        Cmd::Persist(key) => match w.load(key, now)? {
            Some(entry) if entry.expires != 0 => {
                let mut changed = entry.clone();
                changed.expires = 0;
                w.put(key, Some(&entry), changed)?;
                Reply::Int(1)
            }
            _ => Reply::Int(0),
        },
        Cmd::Rename { from, to, only_new } => {
            let Some(entry) = w.load(from, now)? else {
                return Ok(Reply::error(NO_SUCH_KEY));
            };
            if from == to {
                return Ok(if *only_new { Reply::Int(0) } else { Reply::Ok });
            }
            if let Some(existing) = w.load(to, now)? {
                if *only_new {
                    return Ok(Reply::Int(0));
                }
                w.remove(to, &existing)?;
            }
            let mut fields = Vec::new();
            if entry.kind == KeyType::Hash {
                w.each_field(from, 0, &mut |_, field, value| {
                    fields.push((field.to_vec(), value.to_vec()));
                    true
                })?;
            }
            w.remove(from, &entry)?;
            let mut moved = entry.clone();
            if entry.kind == KeyType::Hash {
                moved.set_fields(0, 0);
                for (field, value) in &fields {
                    w.set_field(to, &mut moved, field, value)?;
                }
            }
            w.put(to, None, moved)?;
            if *only_new { Reply::Int(1) } else { Reply::Ok }
        }
        Cmd::Flush => {
            w.flush_all()?;
            Reply::Ok
        }
        Cmd::HSet { key, pairs, ok } => {
            let (old, mut hash) = tri!(w.hash_for_write(key, now));
            let mut added = 0;
            for (field, value) in pairs {
                added += i64::from(w.set_field(key, &mut hash, field, value)?);
            }
            w.finish_hash(key, old, hash)?;
            if *ok { Reply::Ok } else { Reply::Int(added) }
        }
        Cmd::HSetNx { key, field, value } => {
            let (old, mut hash) = tri!(w.hash_for_write(key, now));
            if old.is_some() && w.field(key, field)?.is_some() {
                return Ok(Reply::Int(0));
            }
            w.set_field(key, &mut hash, field, value)?;
            w.finish_hash(key, old, hash)?;
            Reply::Int(1)
        }
        Cmd::HDel { key, fields } => {
            let old = match w.load(key, now)? {
                Some(entry) if entry.kind != KeyType::Hash => return Ok(Reply::error(WRONGTYPE)),
                Some(entry) => entry,
                None => return Ok(Reply::Int(0)),
            };
            let mut hash = old.clone();
            let mut removed = 0;
            for field in fields {
                removed += i64::from(w.delete_field(key, &mut hash, field)?);
            }
            if removed > 0 {
                w.finish_hash(key, Some(old), hash)?;
            }
            Reply::Int(removed)
        }
        Cmd::HIncrBy { key, field, by } => {
            let (old, mut hash) = tri!(w.hash_for_write(key, now));
            let current = match old
                .is_some()
                .then(|| w.field(key, field))
                .transpose()?
                .flatten()
            {
                Some(value) => match command::int(&value) {
                    Some(value) => value,
                    None => return Ok(Reply::error(HASH_NOT_INTEGER)),
                },
                None => 0,
            };
            let Some(next) = current.checked_add(*by) else {
                return Ok(Reply::error(OVERFLOW));
            };
            w.set_field(key, &mut hash, field, next.to_string().as_bytes())?;
            w.finish_hash(key, old, hash)?;
            Reply::Int(next)
        }
        Cmd::HIncrByFloat { key, field, by } => {
            let (old, mut hash) = tri!(w.hash_for_write(key, now));
            let current = match old
                .is_some()
                .then(|| w.field(key, field))
                .transpose()?
                .flatten()
            {
                Some(value) => match command::float(&value) {
                    Some(value) => value,
                    None => return Ok(Reply::error(HASH_NOT_FLOAT)),
                },
                None => 0.0,
            };
            let next = current + by;
            if !next.is_finite() {
                return Ok(Reply::error(NAN_OR_INFINITY));
            }
            let text = format_float(next);
            w.set_field(key, &mut hash, field, text.as_bytes())?;
            w.finish_hash(key, old, hash)?;
            Reply::bulk(text)
        }
        _ => unreachable!("{cmd:?} only reads"),
    })
}

/// Runs commands until they are done, their replies reach the batch limit,
/// or the next reply does not fit in what is left of `room`. `step` gets
/// each command with the bytes left.
fn run_prefix(
    cmds: &[Cmd],
    room: usize,
    mut step: impl FnMut(&Cmd, usize) -> Result<Step>,
) -> Result<Ran> {
    let mut replies = Vec::with_capacity(cmds.len());
    let mut size = 0;
    for cmd in cmds {
        let left = room.saturating_sub(size);
        let reply = match step(cmd, left)? {
            Step::Done(reply) => reply,
            Step::Short(need) => {
                return Ok(Ran {
                    replies,
                    short: Some(need),
                });
            }
        };
        // A read whose reply does not fit is dropped: it changed nothing,
        // so it runs again once there is room. A write's reply is small,
        // or was checked before it ran; see [`returned_value`].
        if !cmd.writes() && reply.size() > left {
            return Ok(Ran {
                replies,
                short: Some(reply.size()),
            });
        }
        size += reply.size();
        replies.push(reply);
        if size > BATCH_REPLY_BYTES {
            break;
        }
    }
    Ok(Ran {
        replies,
        short: None,
    })
}

/// The reply size of a write that returns a stored value, so a batch can
/// stop before running it when the reply would not fit; 0 for others.
fn returned_value<V: View + ?Sized>(view: &V, cmd: &Cmd, now: u64) -> Result<usize> {
    let key = match cmd {
        Cmd::GetDel(key) | Cmd::GetEx { key, .. } | Cmd::Set { key, get: true, .. } => key,
        _ => return Ok(0),
    };
    Ok(match live(view, key, now)? {
        Some(entry) if entry.kind == KeyType::String => Reply::Nil.size() + entry.data.len(),
        _ => 0,
    })
}

fn read_batch(db: &Database, cmds: &[Cmd], room: usize, config: &StoreConfig) -> Result<Ran> {
    let view = ReadView::open(db)?;
    let now = (config.clock)();
    run_prefix(cmds, room, |cmd, left| {
        read(&view, cmd, now, config, left).map(Step::Done)
    })
}

fn writer(
    db: Arc<Database>,
    jobs: Receiver<Job>,
    config: Arc<StoreConfig>,
    removals: Arc<Mutex<Removals>>,
) {
    let mut next_sweep = Instant::now() + SWEEP_INTERVAL;
    loop {
        let wait = next_sweep.saturating_duration_since(Instant::now());
        let mut batch = Vec::new();
        match jobs.recv_timeout(wait) {
            Ok(job) => batch.push(job),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
        while batch.len() < BATCH_JOBS {
            match jobs.try_recv() {
                Ok(job) => batch.push(job),
                Err(_) => break,
            }
        }
        let sweep = Instant::now() >= next_sweep;
        if batch.is_empty() && (!sweep || !has_expired(&db, (config.clock)())) {
            if sweep {
                next_sweep = Instant::now() + SWEEP_INTERVAL;
            }
            continue;
        }
        let (works, replies): (Vec<_>, Vec<_>) =
            batch.into_iter().map(|job| (job.work, job.reply)).unzip();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            apply(&db, &works, sweep, &config, &removals)
        }))
        .unwrap_or_else(|_| Err(StoreError::Storage("internal error".into())));
        match result {
            Ok((outcomes, more)) => {
                if sweep {
                    next_sweep = if more {
                        Instant::now()
                    } else {
                        Instant::now() + SWEEP_INTERVAL
                    };
                }
                for (reply, outcome) in replies.into_iter().zip(outcomes) {
                    let _ = reply.send(Ok(outcome));
                }
            }
            Err(error) => {
                if sweep {
                    next_sweep = Instant::now() + SWEEP_INTERVAL;
                }
                tracing::warn!(%error, "KV store write failed");
                for reply in replies {
                    let _ = reply.send(Err(error.to_string()));
                }
            }
        }
    }
}

fn has_expired(db: &Database, now: u64) -> bool {
    let check = || -> Result<bool> {
        let txn = db.begin_read().map_err(storage)?;
        let expiry = txn.open_table(EXPIRY).map_err(storage)?;
        Ok(expiry.first().map_err(storage)?.is_some_and(|(id, _)| {
            let id = id.value();
            u64::from_be_bytes(id[..8].try_into().unwrap()) <= now
        }))
    };
    check().unwrap_or(false)
}

/// Applies a batch of work in one transaction, committed durably.
fn apply(
    db: &Database,
    works: &[Work],
    sweep: bool,
    config: &StoreConfig,
    removals: &Mutex<Removals>,
) -> Result<(Vec<Ran>, bool)> {
    let txn = db.begin_write().map_err(storage)?;
    let now = (config.clock)();
    let mut outcomes = Vec::with_capacity(works.len());
    let (dirty, more, removed, flushed) = {
        let mut w = WriteView::open(&txn)?;
        for work in works {
            outcomes.push(match work {
                Work::Batch { cmds, room } => run_prefix(cmds, *room, |cmd, left| {
                    let need = returned_value(&w, cmd, now)?;
                    if need > left {
                        return Ok(Step::Short(need));
                    }
                    write(&mut w, cmd, now, config, left).map(Step::Done)
                })?,
                Work::Exec { ops, watched, room } => {
                    let mut changed = false;
                    for watched in watched {
                        let current = live(&w, &watched.key, now)?.map(|e| e.version);
                        // Absent then and now: changed if it was removed
                        // since, having been created meanwhile.
                        let removed = current.is_none()
                            && w.removed_at(&watched.key)
                                .max(removals.lock().removed_at(&watched.key))
                                > watched.seq;
                        if current != watched.version || removed {
                            changed = true;
                            break;
                        }
                    }
                    let replies = if changed {
                        Reply::NilArray
                    } else {
                        // Every command runs, but once the replies fill the
                        // room the connection reserved, large ones are
                        // replaced with an error, so a transaction cannot
                        // buffer without bound.
                        let limit = (*room).min(config.max_reply_bytes);
                        let mut size = 0;
                        let mut replies = Vec::with_capacity(ops.len());
                        for op in ops {
                            let left = limit.saturating_sub(size);
                            let mut reply = match op {
                                Op::Cmd(cmd) => write(&mut w, cmd, now, config, left)?,
                                Op::Failed(reply) => reply.clone(),
                            };
                            if size + reply.size() > limit {
                                reply = too_large(limit);
                            }
                            size += reply.size();
                            replies.push(reply);
                        }
                        Reply::Array(replies)
                    };
                    Ran {
                        replies: vec![replies],
                        short: None,
                    }
                }
            });
        }
        let more = w.sweep(
            now,
            if sweep {
                SWEEP_ALONE
            } else {
                SWEEP_WITH_WRITES
            },
        )? && sweep;
        w.flush()?;
        (w.dirty, more, std::mem::take(&mut w.removed), w.flushed)
    };
    if dirty {
        txn.commit().map_err(storage)?;
    } else {
        txn.abort().map_err(storage)?;
    }
    // Only a `WATCH` registered now can need these removals; see
    // [`Store::watch`].
    let mut removals = removals.lock();
    if dirty && !removals.watching.is_empty() {
        for (key, seq) in removed {
            if removals.keys.contains_key(&key) {
                removals.removed.insert(key, seq);
            }
        }
        if let Some(flushed) = flushed {
            removals.flushed = flushed;
        }
    }
    Ok((outcomes, more))
}

#[cfg(test)]
mod tests;
