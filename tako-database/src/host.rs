//! The database host: databases on this server, who may use them, and the
//! streams currently open on them.

use std::collections::{BTreeSet, HashMap};
use std::fs::File;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use parking_lot::{Mutex, RwLock};
use sha2::{Digest, Sha256};
use tokio::sync::{Notify, Semaphore};

use crate::engine::{self, EngineConfig};
use crate::proto::Error;

/// A database's identifier: its name, unique within a network. Names start
/// with a lowercase letter and contain only lowercase letters, digits, `-`
/// and `_`, at most 63 characters.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DatabaseId(String);

impl DatabaseId {
    pub fn parse(value: &str) -> Result<Self, String> {
        let valid = !value.is_empty()
            && value.len() <= 63
            && value.as_bytes()[0].is_ascii_lowercase()
            && value
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_');
        if valid {
            Ok(Self(value.to_owned()))
        } else {
            Err(format!(
                "database name `{value}` must start with a lowercase letter and contain only lowercase letters, digits, `-` and `_` (at most 63 characters)"
            ))
        }
    }

    /// A random name, for databases nobody names (tests, local tools).
    pub fn generate() -> Self {
        let mut bytes = [0u8; 16];
        getrandom::fill(&mut bytes).expect("operating system random source");
        Self(format!("db-{}", hex::encode(bytes)))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for DatabaseId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

pub type TokenHash = [u8; 32];

pub fn hash_token(token: &str) -> TokenHash {
    Sha256::digest(token.as_bytes()).into()
}

/// Generates a binding token: 32 random bytes, hex encoded.
pub fn generate_token() -> String {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).expect("operating system random source");
    hex::encode(bytes)
}

#[derive(Clone, Debug)]
pub struct HostConfig {
    pub engine: EngineConfig,
    /// Streams idle for longer are closed and their transactions rolled back.
    pub stream_idle_timeout: Duration,
    /// How long a cursor waits for its client to read more before it stops
    /// and closes its stream.
    pub cursor_send_timeout: Duration,
    pub max_streams_per_database: usize,
    pub max_streams: usize,
    pub max_request_bytes: usize,
    /// Request bodies, pipeline results and cursor output this host buffers
    /// at once, across every database. A request reserves its
    /// `Content-Length` (or `max_request_bytes`) before its body is read; a
    /// pipeline's results hold their bytes until the response has been sent,
    /// and a pipeline that finds none left fails with `503`.
    pub max_buffered_bytes: usize,
    /// How long an admitted request may take to send its body before it
    /// gives up its request slot.
    pub request_body_timeout: Duration,
    /// How long a request waits for one of the database's request slots
    /// before it is answered `503`.
    pub admission_timeout: Duration,
    pub max_response_bytes: usize,
    pub max_stored_sql_per_stream: usize,
    /// Total length of the SQL texts one stream may store.
    pub max_stored_sql_bytes_per_stream: usize,
    /// Total length of the SQL texts every open stream on this host may
    /// store together.
    pub max_stored_sql_bytes: usize,
    pub max_condition_depth: usize,
    /// Idle connections kept per database for stateless requests.
    pub pool_size: usize,
    /// Requests executing at once on one database.
    pub max_concurrent_per_database: usize,
    /// How long `pause` waits for in-flight requests.
    pub pause_timeout: Duration,
    /// Connections one listener holds open at once; further ones wait to be
    /// accepted.
    pub max_connections: usize,
    /// How long a connection may wait to send a request's headers, the
    /// first time and between requests, before it is closed.
    pub idle_connection_timeout: Duration,
    /// How long a response may go without any of it being written before
    /// its connection is closed: a client that stops reading must not hold
    /// the response, and the memory reserved for it, forever.
    pub write_stall_timeout: Duration,
}

impl Default for HostConfig {
    fn default() -> Self {
        Self {
            engine: EngineConfig::default(),
            stream_idle_timeout: Duration::from_secs(10),
            cursor_send_timeout: Duration::from_secs(30),
            max_streams_per_database: 128,
            max_streams: 4096,
            max_request_bytes: 32 * 1024 * 1024,
            max_buffered_bytes: 256 * 1024 * 1024,
            request_body_timeout: Duration::from_secs(30),
            admission_timeout: Duration::from_secs(30),
            max_response_bytes: 64 * 1024 * 1024,
            max_stored_sql_per_stream: 256,
            max_stored_sql_bytes_per_stream: 1024 * 1024,
            max_stored_sql_bytes: 64 * 1024 * 1024,
            max_condition_depth: 32,
            pool_size: 8,
            max_concurrent_per_database: 32,
            max_connections: 1024,
            idle_connection_timeout: Duration::from_secs(30),
            write_stall_timeout: Duration::from_secs(30),
            pause_timeout: Duration::from_secs(30),
        }
    }
}

/// Where a binding's database lives.
#[derive(Clone, PartialEq, Eq)]
pub enum Home {
    /// On this server.
    Local,
    /// On another server of the network, reached at `upstream`. `key` is
    /// the key that server issued with its grant to this app environment.
    Remote { upstream: SocketAddr, key: String },
    /// On another server whose address is not known right now.
    Unavailable { reason: String },
}

impl std::fmt::Debug for Home {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Local => f.write_str("Local"),
            Self::Remote { upstream, .. } => f
                .debug_struct("Remote")
                .field("upstream", upstream)
                .field("key", &"[REDACTED]")
                .finish(),
            Self::Unavailable { reason } => f
                .debug_struct("Unavailable")
                .field("reason", reason)
                .finish(),
        }
    }
}

/// What a server grants an app environment that runs on other servers.
#[derive(Clone)]
struct Grant {
    databases: BTreeSet<DatabaseId>,
    key_hash: TokenHash,
}

/// A binding of one app environment on this server.
#[derive(Clone, Debug)]
pub struct Binding {
    pub token_hash: TokenHash,
    pub database: DatabaseId,
    pub home: Home,
}

#[derive(Clone, Debug)]
struct ScopedBinding {
    scope: String,
    binding: Binding,
}

/// Who is executing a request.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Principal {
    /// An app on this server, identified by its binding token.
    Local(TokenHash),
    /// An app on another server, forwarded by that server.
    Remote { server: IpAddr, scope: String },
}

pub(crate) struct Database {
    pub(crate) id: DatabaseId,
    path: PathBuf,
    engine: EngineConfig,
    pool_size: usize,
    state: Mutex<DatabaseState>,
    idle: Notify,
    pool: Mutex<Vec<engine::Connection>>,
    /// Changed since the last snapshot. Starts set: the history is unknown.
    changed: AtomicBool,
    pub(crate) permits: Arc<Semaphore>,
    /// Shared while this process serves the file; exclusive during a swap.
    lock: Mutex<Option<File>>,
}

struct DatabaseState {
    paused: Option<String>,
    /// This process stopped serving the database; nothing enters again.
    closed: bool,
    active: usize,
    streams: usize,
}

impl Database {
    pub(crate) fn acquire(&self) -> Result<engine::Connection, Error> {
        if let Some(conn) = self.pool.lock().pop() {
            return Ok(conn);
        }
        engine::Connection::open(&self.path, &self.engine)
    }

    /// Returns a connection for reuse when it carries no stream state.
    pub(crate) fn release(&self, mut conn: engine::Connection) {
        conn.rollback();
        if conn.is_dirty() || !conn.is_autocommit() {
            return;
        }
        let mut pool = self.pool.lock();
        if pool.len() < self.pool_size {
            pool.push(conn);
        }
    }

    pub(crate) fn mark_changed(&self) {
        self.changed.store(true, Ordering::Relaxed);
    }

    fn enter(self: &Arc<Self>) -> Result<ActiveGuard, Unavailable> {
        let mut state = self.state.lock();
        if state.closed {
            return Err(Unavailable(
                "this server stopped serving the database".into(),
            ));
        }
        if let Some(reason) = &state.paused {
            return Err(Unavailable(reason.clone()));
        }
        state.active += 1;
        Ok(ActiveGuard {
            database: self.clone(),
        })
    }
}

pub(crate) struct Unavailable(pub String);

/// Counts an in-flight request on a database.
pub(crate) struct ActiveGuard {
    database: Arc<Database>,
}

impl Drop for ActiveGuard {
    fn drop(&mut self) {
        let mut state = self.database.state.lock();
        state.active -= 1;
        if state.active == 0 {
            self.database.idle.notify_waiters();
        }
    }
}

/// Bytes of SQL text stored by every open stream of a host.
pub(crate) struct StoredSqlBudget {
    used: std::sync::atomic::AtomicUsize,
    limit: usize,
}

impl StoredSqlBudget {
    pub(crate) fn new(limit: usize) -> Self {
        Self {
            used: std::sync::atomic::AtomicUsize::new(0),
            limit,
        }
    }

    /// Reserves `bytes`, or returns false when they do not fit.
    fn reserve(&self, bytes: usize) -> bool {
        let mut used = self.used.load(Ordering::Acquire);
        loop {
            let Some(total) = used.checked_add(bytes).filter(|total| *total <= self.limit) else {
                return false;
            };
            match self
                .used
                .compare_exchange_weak(used, total, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => return true,
                Err(current) => used = current,
            }
        }
    }

    fn release(&self, bytes: usize) {
        let mut used = self.used.load(Ordering::Acquire);
        loop {
            match self.used.compare_exchange_weak(
                used,
                used.saturating_sub(bytes),
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return,
                Err(current) => used = current,
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn used(&self) -> usize {
        self.used.load(Ordering::Acquire)
    }
}

/// An open stream: one connection plus its stored SQL texts.
pub(crate) struct Stream {
    pub(crate) conn: Option<engine::Connection>,
    pub(crate) stored_sql: HashMap<i32, String>,
    /// Where the stored texts' bytes are counted; released on drop.
    budget: Arc<StoredSqlBudget>,
}

impl Stream {
    pub(crate) fn new(budget: Arc<StoredSqlBudget>) -> Self {
        Self {
            conn: None,
            stored_sql: HashMap::new(),
            budget,
        }
    }

    fn stored_bytes(&self) -> usize {
        self.stored_sql.values().map(String::len).sum()
    }

    /// Stores `sql` as `sql_id`, counting its bytes against the host's
    /// budget. Returns false, storing nothing, when they do not fit.
    pub(crate) fn store_sql(&mut self, sql_id: i32, sql: String) -> bool {
        let previous = self.stored_sql.get(&sql_id).map_or(0, String::len);
        if sql.len() > previous {
            if !self.budget.reserve(sql.len() - previous) {
                return false;
            }
        } else {
            self.budget.release(previous - sql.len());
        }
        self.stored_sql.insert(sql_id, sql);
        true
    }

    pub(crate) fn close_sql(&mut self, sql_id: i32) {
        if let Some(sql) = self.stored_sql.remove(&sql_id) {
            self.budget.release(sql.len());
        }
    }

    /// Whether the stream carries client-visible state (spec section 4.3).
    pub(crate) fn has_state(&self) -> bool {
        !self.stored_sql.is_empty()
            || self
                .conn
                .as_ref()
                .is_some_and(|conn| !conn.is_autocommit() || conn.is_dirty())
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        self.budget.release(self.stored_bytes());
    }
}

pub(crate) struct StreamEntry {
    pub(crate) database: DatabaseId,
    pub(crate) principal: Principal,
    pub(crate) sequence: u64,
    pub(crate) last_used: Instant,
    /// `None` while a request is executing on the stream.
    pub(crate) stream: Option<Stream>,
}

pub(crate) struct HostInner {
    pub(crate) config: HostConfig,
    databases: RwLock<HashMap<DatabaseId, Arc<Database>>>,
    /// Bindings of apps on this server, by app environment scope.
    bindings: RwLock<HashMap<String, Vec<Binding>>>,
    by_token: RwLock<HashMap<TokenHash, ScopedBinding>>,
    /// Databases on this server that apps on other servers may use.
    remote_grants: RwLock<HashMap<String, Grant>>,
    pub(crate) streams: Mutex<HashMap<[u8; 16], StreamEntry>>,
    pub(crate) stored_sql: Arc<StoredSqlBudget>,
    /// Bytes of request bodies, responses and cursor output buffered; see
    /// [`HostConfig::max_buffered_bytes`].
    pub(crate) buffered: Arc<Semaphore>,
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// The app environment a binding scope belongs to. Temporary bindings, such
/// as a release command's, use `<app>/<env>#<purpose>`; grants on other
/// servers are per environment.
fn environment_scope(scope: &str) -> &str {
    scope
        .split_once('#')
        .map_or(scope, |(environment, _)| environment)
}

/// The database host. Cheap to clone.
#[derive(Clone)]
pub struct DatabaseHost {
    pub(crate) inner: Arc<HostInner>,
}

/// What a request resolved to.
pub(crate) enum Resolved {
    Serve {
        database: Arc<Database>,
        principal: Principal,
    },
    Forward {
        database: DatabaseId,
        upstream: SocketAddr,
        scope: String,
        key: String,
    },
    Unavailable(String),
}

#[derive(Debug, thiserror::Error)]
pub enum HostError {
    #[error("database {0} is not hosted on this server")]
    UnknownDatabase(DatabaseId),
    #[error("database {0} is already registered")]
    AlreadyRegistered(DatabaseId),
    #[error("{0}")]
    Engine(String),
    #[error("database {database} is still in use after {seconds}s")]
    Busy { database: DatabaseId, seconds: u64 },
    #[error("{0}")]
    Io(#[from] std::io::Error),
}

impl DatabaseHost {
    pub fn new(config: HostConfig) -> Self {
        let stored_sql = Arc::new(StoredSqlBudget::new(config.max_stored_sql_bytes));
        let buffered = Arc::new(Semaphore::new(config.max_buffered_bytes));
        Self {
            inner: Arc::new(HostInner {
                config,
                databases: RwLock::new(HashMap::new()),
                bindings: RwLock::new(HashMap::new()),
                by_token: RwLock::new(HashMap::new()),
                remote_grants: RwLock::new(HashMap::new()),
                streams: Mutex::new(HashMap::new()),
                stored_sql,
                buffered,
            }),
        }
    }

    pub fn config(&self) -> &HostConfig {
        &self.inner.config
    }

    /// Creates the database file at `path` (which must not exist).
    pub fn create_file(path: &Path) -> Result<(), HostError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        engine::create_database(path).map_err(HostError::Engine)
    }

    /// Serves an existing database file. Takes a shared lock on
    /// `<path>.lock` so a swap in another process waits for this one.
    pub fn register(&self, id: DatabaseId, path: PathBuf) -> Result<(), HostError> {
        let mut databases = self.inner.databases.write();
        if databases.contains_key(&id) {
            return Err(HostError::AlreadyRegistered(id));
        }
        let lock = open_lock(&path)?;
        lock.lock_shared()?;
        let config = &self.inner.config;
        databases.insert(
            id.clone(),
            Arc::new(Database {
                id,
                path,
                engine: config.engine.clone(),
                pool_size: config.pool_size,
                state: Mutex::new(DatabaseState {
                    paused: None,
                    closed: false,
                    active: 0,
                    streams: 0,
                }),
                idle: Notify::new(),
                pool: Mutex::new(Vec::new()),
                changed: AtomicBool::new(true),
                permits: Arc::new(Semaphore::new(config.max_concurrent_per_database)),
                lock: Mutex::new(Some(lock)),
            }),
        );
        Ok(())
    }

    pub fn is_registered(&self, id: &DatabaseId) -> bool {
        self.inner.databases.read().contains_key(id)
    }

    pub fn registered(&self) -> Vec<DatabaseId> {
        let mut ids: Vec<_> = self.inner.databases.read().keys().cloned().collect();
        ids.sort();
        ids
    }

    pub fn path(&self, id: &DatabaseId) -> Option<PathBuf> {
        self.inner
            .databases
            .read()
            .get(id)
            .map(|database| database.path.clone())
    }

    /// Stops serving a database: waits for in-flight requests, rolls back its
    /// streams, closes its connections, and releases the file lock.
    pub async fn unregister(&self, id: &DatabaseId) -> Result<(), HostError> {
        let mut guard = self.pause(id, "database is being removed").await?;
        guard.database.lock.lock().take();
        self.inner.databases.write().remove(id);
        guard.resume = false;
        Ok(())
    }

    /// Replaces the bindings of one app environment on this server.
    pub fn set_bindings(&self, scope: &str, bindings: Vec<Binding>) {
        let mut all = self.inner.bindings.write();
        let mut by_token = self.inner.by_token.write();
        if let Some(previous) = all.remove(scope) {
            for binding in previous {
                by_token.remove(&binding.token_hash);
            }
        }
        if !bindings.is_empty() {
            for binding in &bindings {
                by_token.insert(
                    binding.token_hash,
                    ScopedBinding {
                        scope: scope.to_owned(),
                        binding: binding.clone(),
                    },
                );
            }
            all.insert(scope.to_owned(), bindings);
        }
    }

    /// Replaces which databases on this server an app environment running on
    /// other servers may use. Their servers present `key` when forwarding.
    pub fn set_remote_grants(&self, scope: &str, databases: BTreeSet<DatabaseId>, key: &str) {
        let mut grants = self.inner.remote_grants.write();
        if databases.is_empty() {
            grants.remove(scope);
        } else {
            grants.insert(
                scope.to_owned(),
                Grant {
                    databases,
                    key_hash: hash_token(key),
                },
            );
        }
    }

    /// Whether a database changed since the last successful snapshot.
    pub fn has_changes(&self, id: &DatabaseId) -> bool {
        self.inner
            .databases
            .read()
            .get(id)
            .is_some_and(|database| database.changed.load(Ordering::Relaxed))
    }

    /// Records that a database matches its last snapshot, as a new, empty
    /// database does.
    pub fn clear_changed(&self, id: &DatabaseId) {
        if let Some(database) = self.inner.databases.read().get(id) {
            database.changed.store(false, Ordering::Relaxed);
        }
    }

    /// Records that a database differs from its last snapshot.
    pub fn mark_changed(&self, id: &DatabaseId) {
        if let Some(database) = self.inner.databases.read().get(id) {
            database.mark_changed();
        }
    }

    /// Writes a verified, compacted copy of a database to `dest`. Concurrent
    /// requests keep running; the copy is one consistent read transaction.
    pub async fn snapshot(&self, id: &DatabaseId, dest: &Path) -> Result<(), HostError> {
        let database = self.database(id)?;
        let was_changed = database.changed.swap(false, Ordering::Relaxed);
        let path = database.path.clone();
        let dest = dest.to_path_buf();
        let busy = self.inner.config.engine.busy_timeout;
        let result =
            tokio::task::spawn_blocking(move || engine::snapshot_database(&path, &dest, busy))
                .await
                .map_err(|e| HostError::Engine(e.to_string()))?;
        if result.is_err() && was_changed {
            database.mark_changed();
        }
        result.map_err(HostError::Engine)
    }

    /// Stops new requests on a database, waits for in-flight ones, rolls back
    /// and closes its streams and connections, and takes the file lock
    /// exclusively. Requests get `503` with `reason` until the guard drops.
    pub async fn pause(&self, id: &DatabaseId, reason: &str) -> Result<PauseGuard, HostError> {
        let database = self.database(id)?;
        let deadline = tokio::time::Instant::now() + self.inner.config.pause_timeout;
        {
            let mut state = database.state.lock();
            if state.paused.is_some() {
                return Err(HostError::Busy {
                    database: id.clone(),
                    seconds: 0,
                });
            }
            state.paused = Some(reason.to_owned());
        }
        let guard = PauseGuard {
            database: database.clone(),
            resume: true,
        };
        loop {
            let notified = database.idle.notified();
            if database.state.lock().active == 0 {
                break;
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                return Err(HostError::Busy {
                    database: id.clone(),
                    seconds: self.inner.config.pause_timeout.as_secs(),
                });
            }
        }
        let streams = self.take_streams(|entry| &entry.database == id);
        let pool = std::mem::take(&mut *database.pool.lock());
        let lock_path = lock_path(&database.path);
        let lock = database.lock.lock().take();
        let timeout = self.inner.config.pause_timeout;
        let exclusive = tokio::task::spawn_blocking(move || {
            drop(streams);
            drop(pool);
            drop(lock);
            exclusive_lock(&lock_path, timeout)
        })
        .await
        .map_err(|e| HostError::Engine(e.to_string()))?;
        match exclusive {
            Ok(file) => {
                *database.lock.lock() = Some(file);
                Ok(guard)
            }
            Err(error) => Err(error),
        }
    }

    /// Rolls back every stream and closes every connection, releasing file
    /// locks. Used when this process stops serving (drain or reload).
    pub async fn close_all(&self) {
        let streams = self.take_streams(|_| true);
        let databases: Vec<_> = self
            .inner
            .databases
            .write()
            .drain()
            .map(|(_, db)| db)
            .collect();
        // Requests that already looked a database up may still be waiting
        // for a slot; once closed, none of them can start.
        for database in &databases {
            database.state.lock().closed = true;
            database.permits.close();
        }
        // New requests no longer find these databases. Requests already
        // running keep a connection to the file, so its lock is held until
        // they finish: a successor that replaced the file meanwhile would
        // lose their writes. Closing waits for them up to the pause timeout;
        // a database still busy then keeps its lock in the background until
        // its last request ends (or this process exits, which ends both).
        let deadline = tokio::time::Instant::now() + self.inner.config.pause_timeout;
        let mut busy = Vec::new();
        let mut idle = Vec::new();
        for database in databases {
            let finished = loop {
                let notified = database.idle.notified();
                if database.state.lock().active == 0 {
                    break true;
                }
                if tokio::time::timeout_at(deadline, notified).await.is_err() {
                    break false;
                }
            };
            if finished {
                idle.push(database);
            } else {
                busy.push(database);
            }
        }
        for database in busy {
            tracing::warn!(
                database = %database.id,
                "closing a database with requests still running; its file stays locked until they finish"
            );
            tokio::spawn(async move {
                loop {
                    let notified = database.idle.notified();
                    if database.state.lock().active == 0 {
                        break;
                    }
                    notified.await;
                }
                let _ = tokio::task::spawn_blocking(move || release(&database)).await;
            });
        }
        tokio::task::spawn_blocking(move || {
            drop(streams);
            for database in idle {
                release(&database);
            }
        })
        .await
        .ok();
    }

    /// Closes streams idle for longer than the configured timeout.
    pub fn expire_idle_streams(&self) {
        let timeout = self.inner.config.stream_idle_timeout;
        let expired = self
            .take_streams(|entry| entry.stream.is_some() && entry.last_used.elapsed() > timeout);
        if !expired.is_empty() {
            tracing::debug!(count = expired.len(), "closing idle database streams");
            // Rolling back can wait on the disk; keep it off the async runtime.
            std::thread::spawn(move || drop(expired));
        }
    }

    /// Runs `expire_idle_streams` every second until the host is dropped.
    pub fn spawn_reaper(&self) -> tokio::task::JoinHandle<()> {
        let weak = Arc::downgrade(&self.inner);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(1));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                interval.tick().await;
                let Some(inner) = weak.upgrade() else { return };
                DatabaseHost { inner }.expire_idle_streams();
            }
        })
    }

    /// Runs one statement as an administrator, with the same restrictions as
    /// apps, returning at most `max_rows` rows.
    pub async fn admin_query(
        &self,
        id: &DatabaseId,
        sql: String,
        max_rows: usize,
    ) -> Result<AdminQuery, HostError> {
        let database = self.database(id)?;
        let active = DatabaseHost::enter(&database).map_err(|e| HostError::Engine(e.0))?;
        // Counts against the database's concurrency limit, like app requests.
        let permit = database
            .permits
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| HostError::Engine("database is closing".into()))?;
        // Results go back through the management relay, which carries at
        // most 16 MiB; leave room for its envelope.
        let budget = self
            .inner
            .config
            .max_response_bytes
            .min(MAX_ADMIN_QUERY_BYTES);
        tokio::task::spawn_blocking(move || {
            let _active = active;
            let _permit = permit;
            let mut conn = database
                .acquire()
                .map_err(|e| HostError::Engine(e.message))?;
            let mut rows = Vec::new();
            let mut columns = Vec::new();
            let mut truncated = false;
            let mut size = 0usize;
            let outcome = conn.execute_streaming(&sql, &engine::Args::Positional(&[]), |event| {
                match event {
                    engine::Event::Begin(cols) => {
                        columns = cols
                            .into_iter()
                            .map(|c| c.name.unwrap_or_default())
                            .collect();
                    }
                    engine::Event::Row(row) => {
                        if rows.len() >= max_rows {
                            // One row past the limit shows there are more.
                            truncated = true;
                            return Err(Error::new(engine::STOP, "row limit reached"));
                        } else {
                            size += row.encoded_size();
                            if size > budget {
                                return Err(Error::new(
                                    "RESPONSE_TOO_LARGE",
                                    "the result is larger than 8 MiB; select fewer rows or columns, or use `tako sqlite export`",
                                ));
                            }
                            rows.push(row.to_values());
                        }
                    }
                }
                Ok(())
            });
            if conn.take_wrote() {
                database.mark_changed();
            }
            let result = match outcome {
                // Each statement is its own transaction here: the connection
                // is released (and rolled back) before the next one runs.
                Ok(_) if !conn.is_autocommit() => Err(HostError::Engine(
                    "Transactions cannot span statements here; each statement commits on its own."
                        .into(),
                )),
                Ok((affected, _)) => Ok(AdminQuery {
                    columns,
                    rows,
                    affected_row_count: affected,
                    truncated,
                }),
                Err(error) => Err(HostError::Engine(match error.extended_code {
                    Some(code) => format!("{} ({code})", error.message),
                    None => format!("{} ({})", error.message, error.code),
                })),
            };
            database.release(conn);
            result
        })
        .await
        .map_err(|e| HostError::Engine(e.to_string()))?
    }

    /// Applies `app`'s pending migrations to a database, or reports them
    /// when `dry_run` is set. Runs under the database's request limit, with
    /// the same restrictions as apps.
    pub async fn migrate(
        &self,
        id: &DatabaseId,
        app: String,
        migrations: Vec<crate::Migration>,
        dry_run: bool,
    ) -> Result<crate::MigrationReport, HostError> {
        let database = self.database(id)?;
        let active = DatabaseHost::enter(&database).map_err(|e| HostError::Engine(e.0))?;
        let permit = database
            .permits
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| HostError::Engine("database is closing".into()))?;
        tokio::task::spawn_blocking(move || {
            let _active = active;
            let _permit = permit;
            let mut conn = database
                .acquire()
                .map_err(|e| HostError::Engine(e.message))?;
            let result = crate::migrate::run(&mut conn, &app, &migrations, dry_run);
            if conn.take_wrote() {
                database.mark_changed();
            }
            conn.rollback();
            database.release(conn);
            result.map_err(HostError::Engine)
        })
        .await
        .map_err(|e| HostError::Engine(e.to_string()))?
    }

    /// Size of a database's files (database and write-ahead log) in bytes.
    pub fn size_bytes(&self, id: &DatabaseId) -> Option<u64> {
        let path = self.path(id)?;
        let mut wal = path.clone().into_os_string();
        wal.push("-wal");
        let size = |p: &Path| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
        Some(size(&path) + size(Path::new(&wal)))
    }

    pub fn open_streams(&self) -> usize {
        self.inner.streams.lock().len()
    }

    /// Removes matching entries from the stream table and releases their
    /// slots. Entries of busy streams are removed too; their request finds the
    /// entry gone and drops the stream when it finishes.
    fn take_streams(
        &self,
        mut select: impl FnMut(&StreamEntry) -> bool,
    ) -> Vec<(DatabaseId, Stream)> {
        let removed: Vec<StreamEntry> = {
            let mut streams = self.inner.streams.lock();
            let keys: Vec<_> = streams
                .iter()
                .filter(|(_, entry)| select(entry))
                .map(|(key, _)| *key)
                .collect();
            keys.into_iter()
                .filter_map(|key| streams.remove(&key))
                .collect()
        };
        let databases = self.inner.databases.read();
        removed
            .into_iter()
            .filter_map(|entry| {
                if let Some(database) = databases.get(&entry.database) {
                    self.unreserve_stream(database);
                }
                entry.stream.map(|stream| (entry.database, stream))
            })
            .collect()
    }

    pub(crate) fn database(&self, id: &DatabaseId) -> Result<Arc<Database>, HostError> {
        self.inner
            .databases
            .read()
            .get(id)
            .cloned()
            .ok_or_else(|| HostError::UnknownDatabase(id.clone()))
    }

    pub(crate) fn resolve_token(&self, token: &str) -> Option<Resolved> {
        let hash = hash_token(token);
        let ScopedBinding { scope, binding } = self.inner.by_token.read().get(&hash).cloned()?;
        match binding.home {
            Home::Local => Some(Resolved::Serve {
                database: self.database(&binding.database).ok()?,
                principal: Principal::Local(hash),
            }),
            Home::Remote { upstream, key } => Some(Resolved::Forward {
                database: binding.database,
                upstream,
                scope: environment_scope(&scope).to_owned(),
                key,
            }),
            Home::Unavailable { reason } => Some(Resolved::Unavailable(reason)),
        }
    }

    /// Resolves a request forwarded by another server. Both the source
    /// address and the grant's key must match: any process on a server can
    /// reach this port from its address, but only that server knows the key.
    pub(crate) fn resolve_remote(
        &self,
        server: IpAddr,
        scope: &str,
        database: &DatabaseId,
        key: &str,
    ) -> Option<Resolved> {
        let presented = hash_token(key);
        let allowed = self
            .inner
            .remote_grants
            .read()
            .get(scope)
            .is_some_and(|grant| {
                constant_time_eq(&grant.key_hash, &presented) && grant.databases.contains(database)
            });
        if !allowed {
            return None;
        }
        Some(Resolved::Serve {
            database: self.database(database).ok()?,
            principal: Principal::Remote {
                server,
                scope: scope.to_owned(),
            },
        })
    }

    pub(crate) fn enter(database: &Arc<Database>) -> Result<ActiveGuard, Unavailable> {
        database.enter()
    }

    /// Retains a new stream, counting it against the server's and the
    /// database's limits. The check and the insertion happen under one
    /// lock, so concurrent requests cannot exceed the server's limit.
    pub(crate) fn insert_stream(
        &self,
        database: &Database,
        key: [u8; 16],
        entry: StreamEntry,
    ) -> Result<(), Error> {
        let config = &self.inner.config;
        let mut streams = self.inner.streams.lock();
        if streams.len() >= config.max_streams {
            return Err(Error::new(
                "STREAM_LIMIT",
                "too many open streams on this server",
            ));
        }
        {
            let mut state = database.state.lock();
            if state.streams >= config.max_streams_per_database {
                return Err(Error::new(
                    "STREAM_LIMIT",
                    "too many open streams on this database",
                ));
            }
            state.streams += 1;
        }
        streams.insert(key, entry);
        Ok(())
    }

    pub(crate) fn unreserve_stream(&self, database: &Database) {
        let mut state = database.state.lock();
        state.streams = state.streams.saturating_sub(1);
    }
}

/// Drops a closed database's connections and its file lock.
fn release(database: &Database) {
    database.pool.lock().clear();
    database.lock.lock().take();
}

/// The most an administrator query returns, so its result fits through the
/// 16 MiB management relay.
const MAX_ADMIN_QUERY_BYTES: usize = 8 * 1024 * 1024;

/// The result of [`DatabaseHost::admin_query`].
#[derive(Debug)]
pub struct AdminQuery {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<crate::proto::Value>>,
    pub affected_row_count: u64,
    pub truncated: bool,
}

/// Holds a database paused. Dropping it resumes service.
pub struct PauseGuard {
    database: Arc<Database>,
    resume: bool,
}

impl PauseGuard {
    pub fn path(&self) -> &Path {
        &self.database.path
    }

    /// The database is known to differ from the last snapshot (after a swap).
    pub fn mark_changed(&self) {
        self.database.mark_changed();
    }
}

impl Drop for PauseGuard {
    fn drop(&mut self) {
        if !self.resume {
            return;
        }
        let mut lock = self.database.lock.lock();
        if let Some(file) = lock.as_ref() {
            let _ = file.unlock();
            if file.lock_shared().is_err() {
                *lock = None;
            }
        } else if let Ok(file) = open_lock(&self.database.path)
            && file.lock_shared().is_ok()
        {
            *lock = Some(file);
        }
        drop(lock);
        self.database.state.lock().paused = None;
    }
}

fn lock_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".lock");
    path.with_file_name(name)
}

fn open_lock(path: &Path) -> std::io::Result<File> {
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path(path))
}

fn exclusive_lock(path: &Path, timeout: Duration) -> Result<File, HostError> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    let started = Instant::now();
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(file),
            Err(std::fs::TryLockError::WouldBlock) if started.elapsed() < timeout => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(std::fs::TryLockError::WouldBlock) => {
                return Err(HostError::Engine(format!(
                    "another server process still has {} open",
                    path.display()
                )));
            }
            Err(std::fs::TryLockError::Error(error)) => return Err(error.into()),
        }
    }
}
