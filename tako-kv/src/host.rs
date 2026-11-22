//! The KV host: stores on this server, who may use them, and where the
//! stores of other servers' apps live.

use std::collections::{BTreeSet, HashMap};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize};
use std::time::{Duration, Instant};

use bytes::Bytes;
use parking_lot::RwLock;
use sha2::{Digest, Sha256};
use tokio::sync::Semaphore;

use crate::command::{self, Class, Limits};
use crate::resp::Reply;
use crate::store::{Clock, Stats, Store, StoreConfig, StoreError, system_clock};

/// A store's identifier: its name, unique within a network.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct StoreId(String);

impl StoreId {
    /// Names start with a lowercase letter and contain only lowercase
    /// letters, digits, `-` and `_`, at most 63 characters.
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
                "KV store name `{value}` must start with a lowercase letter and contain only lowercase letters, digits, `-` and `_` (at most 63 characters)"
            ))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for StoreId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

pub type TokenHash = [u8; 32];

pub fn hash_token(token: &str) -> TokenHash {
    Sha256::digest(token.as_bytes()).into()
}

/// Generates a binding token: 32 random bytes, hex encoded, so it can sit in
/// a `redis://` URL unescaped.
pub fn generate_token() -> String {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).expect("operating system random source");
    hex::encode(bytes)
}

#[derive(Clone)]
pub struct HostConfig {
    /// Bytes of data one store may hold; see [`StoreConfig::max_bytes`].
    pub max_store_bytes: u64,
    /// Longest key or hash field.
    pub max_key_bytes: usize,
    /// Longest string or hash field value.
    pub max_value_bytes: usize,
    /// Largest single command.
    pub max_request_bytes: usize,
    /// Largest single reply.
    pub max_reply_bytes: usize,
    /// Request bytes buffered across all connections beyond each one's
    /// first 64 KiB.
    pub max_buffered_bytes: usize,
    /// Reply bytes buffered across all connections beyond each one's first
    /// 64 KiB, while they wait to be sent.
    pub max_buffered_reply_bytes: usize,
    /// Open connections per listener.
    pub max_connections: usize,
    /// How long a request waits for buffer space, and a reply for the
    /// client to read it, before the connection closes.
    pub stall_timeout: Duration,
    /// redb page cache per store.
    pub cache_bytes: usize,
    /// How long forwarding waits to reach another server.
    pub connect_timeout: Duration,
    pub clock: Clock,
}

impl Default for HostConfig {
    fn default() -> Self {
        Self {
            max_store_bytes: 1024 * 1024 * 1024,
            max_key_bytes: 64 * 1024,
            max_value_bytes: 16 * 1024 * 1024,
            max_request_bytes: 32 * 1024 * 1024,
            max_reply_bytes: 64 * 1024 * 1024,
            max_buffered_bytes: 256 * 1024 * 1024,
            max_buffered_reply_bytes: 256 * 1024 * 1024,
            max_connections: 4096,
            stall_timeout: Duration::from_secs(30),
            cache_bytes: 16 * 1024 * 1024,
            connect_timeout: Duration::from_secs(5),
            clock: system_clock(),
        }
    }
}

impl HostConfig {
    pub(crate) fn limits(&self) -> Limits {
        Limits {
            max_key_bytes: self.max_key_bytes,
            max_value_bytes: self.max_value_bytes,
        }
    }

    fn store_config(&self) -> StoreConfig {
        StoreConfig {
            max_bytes: self.max_store_bytes,
            limits: self.limits(),
            cache_bytes: self.cache_bytes,
            max_reply_bytes: self.max_reply_bytes,
            clock: self.clock.clone(),
        }
    }
}

/// Where a binding's store lives.
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

/// A binding of one app environment on this server.
#[derive(Clone, Debug)]
pub struct Binding {
    pub token_hash: TokenHash,
    pub store: StoreId,
    pub home: Home,
}

#[derive(Clone)]
struct Grant {
    stores: BTreeSet<StoreId>,
    key_hash: TokenHash,
}

/// What a connection's credentials resolve to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Resolved {
    Serve(StoreId),
    Forward(Forward),
    Unavailable(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Forward {
    pub store: StoreId,
    pub upstream: SocketAddr,
    /// The app environment, without a temporary scope's suffix.
    pub scope: String,
    pub key: String,
}

#[derive(Debug, thiserror::Error)]
pub enum HostError {
    #[error("KV store {0} is not hosted on this server")]
    UnknownStore(StoreId),
    #[error("KV store {0} is already registered")]
    AlreadyRegistered(StoreId),
    #[error("{0}")]
    Store(#[from] StoreError),
    #[error("{0}")]
    Io(#[from] std::io::Error),
}

pub(crate) struct Inner {
    pub(crate) config: HostConfig,
    stores: RwLock<HashMap<StoreId, Arc<Store>>>,
    bindings: RwLock<HashMap<String, Vec<Binding>>>,
    by_token: RwLock<HashMap<TokenHash, (String, Binding)>>,
    remote_grants: RwLock<HashMap<String, Grant>>,
    /// Request bytes buffered beyond each connection's base buffer.
    pub(crate) buffered: Arc<Semaphore>,
    /// Reply bytes buffered beyond each connection's base allowance.
    pub(crate) replies: Arc<Semaphore>,
    /// Counts `close_all` calls; connections close when it changes.
    pub(crate) closings: tokio::sync::watch::Sender<u64>,
    pub(crate) next_client: AtomicU64,
    pub(crate) connected: AtomicUsize,
    pub(crate) started: Instant,
}

/// The KV host. Cheap to clone.
#[derive(Clone)]
pub struct KvHost {
    pub(crate) inner: Arc<Inner>,
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

impl KvHost {
    pub fn new(config: HostConfig) -> Self {
        let buffered = Arc::new(Semaphore::new(config.max_buffered_bytes));
        let replies = Arc::new(Semaphore::new(config.max_buffered_reply_bytes));
        Self {
            inner: Arc::new(Inner {
                config,
                stores: RwLock::new(HashMap::new()),
                bindings: RwLock::new(HashMap::new()),
                by_token: RwLock::new(HashMap::new()),
                remote_grants: RwLock::new(HashMap::new()),
                buffered,
                replies,
                closings: tokio::sync::watch::Sender::new(0),
                next_client: AtomicU64::new(1),
                connected: AtomicUsize::new(0),
                started: Instant::now(),
            }),
        }
    }

    pub fn config(&self) -> &HostConfig {
        &self.inner.config
    }

    /// Opens a store file, creating it if needed, and serves it. Blocks.
    pub fn register(&self, id: StoreId, path: PathBuf) -> Result<(), HostError> {
        if self.is_registered(&id) {
            return Err(HostError::AlreadyRegistered(id));
        }
        let store = Arc::new(Store::open(&path, self.inner.config.store_config())?);
        let mut stores = self.inner.stores.write();
        if stores.contains_key(&id) {
            return Err(HostError::AlreadyRegistered(id));
        }
        stores.insert(id, store);
        Ok(())
    }

    pub fn is_registered(&self, id: &StoreId) -> bool {
        self.inner.stores.read().contains_key(id)
    }

    pub fn registered(&self) -> Vec<StoreId> {
        let mut ids: Vec<StoreId> = self.inner.stores.read().keys().cloned().collect();
        ids.sort();
        ids
    }

    pub fn path(&self, id: &StoreId) -> Option<PathBuf> {
        self.store(id).map(|store| store.path().to_owned())
    }

    pub(crate) fn store(&self, id: &StoreId) -> Option<Arc<Store>> {
        self.inner.stores.read().get(id).cloned()
    }

    /// Stops serving a store and closes its file once queued writes finish.
    /// Connections using it get errors until they reconnect.
    pub async fn unregister(&self, id: &StoreId) -> Result<(), HostError> {
        let store = self
            .inner
            .stores
            .write()
            .remove(id)
            .ok_or_else(|| HostError::UnknownStore(id.clone()))?;
        store.close().await;
        Ok(())
    }

    /// Stops serving every store, as when a reloaded process takes over.
    /// Connections close after their current command, so clients reconnect
    /// to the process that serves the stores now.
    pub async fn close_all(&self) {
        self.inner.closings.send_modify(|count| *count += 1);
        let stores: Vec<Arc<Store>> = self.inner.stores.write().drain().map(|(_, s)| s).collect();
        for store in stores {
            store.close().await;
        }
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
                by_token.insert(binding.token_hash, (scope.to_owned(), binding.clone()));
            }
            all.insert(scope.to_owned(), bindings);
        }
    }

    /// Replaces which stores on this server an app environment running on
    /// other servers may use. Their servers present `key` when forwarding.
    pub fn set_remote_grants(&self, scope: &str, stores: BTreeSet<StoreId>, key: &str) {
        let mut grants = self.inner.remote_grants.write();
        if stores.is_empty() {
            grants.remove(scope);
        } else {
            grants.insert(
                scope.to_owned(),
                Grant {
                    stores,
                    key_hash: hash_token(key),
                },
            );
        }
    }

    pub(crate) fn resolve_token(&self, hash: &TokenHash) -> Option<Resolved> {
        let (scope, binding) = self.inner.by_token.read().get(hash).cloned()?;
        Some(match binding.home {
            Home::Local => Resolved::Serve(binding.store),
            Home::Remote { upstream, key } => Resolved::Forward(Forward {
                store: binding.store,
                upstream,
                scope: environment_scope(&scope).to_owned(),
                key,
            }),
            Home::Unavailable { reason } => Resolved::Unavailable(reason),
        })
    }

    /// Whether another server may act for `scope` on `store`. Both the
    /// source address (checked by the listener) and the grant's key must
    /// match: any process on a server can reach the peer port from its
    /// address, but only that server knows the key.
    pub(crate) fn granted(&self, scope: &str, store: &StoreId, key: &str) -> bool {
        let presented = hash_token(key);
        self.inner
            .remote_grants
            .read()
            .get(scope)
            .is_some_and(|grant| {
                constant_time_eq(&grant.key_hash, &presented) && grant.stores.contains(store)
            })
    }

    /// The size of a store's file.
    pub fn size_bytes(&self, id: &StoreId) -> Option<u64> {
        let path = self.path(id)?;
        std::fs::metadata(path).ok().map(|meta| meta.len())
    }

    /// Key counts and logical size of a store.
    pub async fn stats(&self, id: &StoreId) -> Result<Stats, HostError> {
        let store = self
            .store(id)
            .ok_or_else(|| HostError::UnknownStore(id.clone()))?;
        Ok(store.stats().await?)
    }

    /// Runs one command with full access, for administrators. Transactions
    /// and connection commands other than `PING`, `ECHO` and `TIME` are
    /// refused.
    pub async fn admin(&self, id: &StoreId, args: Vec<Bytes>) -> Result<Reply, HostError> {
        let store = self
            .store(id)
            .ok_or_else(|| HostError::UnknownStore(id.clone()))?;
        if args.is_empty() {
            return Ok(Reply::error("ERR empty command"));
        }
        let spec = match command::lookup(&args) {
            Ok(spec) => spec,
            Err(reply) => return Ok(reply),
        };
        if spec.class == Class::Connection {
            return Ok(match spec.name {
                "ping" => args
                    .get(1)
                    .cloned()
                    .map_or(Reply::Simple("PONG".into()), Reply::Bulk),
                "echo" => Reply::Bulk(args[1].clone()),
                "time" => crate::server::time(&self.inner.config),
                _ => Reply::Error(format!(
                    "ERR {} is not available here; connect with a Redis client instead",
                    spec.name.to_uppercase()
                )),
            });
        }
        let cmd = match command::parse(spec, &args, self.inner.config.limits()) {
            Ok(cmd) => cmd,
            Err(reply) => return Ok(reply),
        };
        let max_reply = self.inner.config.max_reply_bytes;
        let mut ran = store.run(vec![cmd], max_reply).await?;
        Ok(ran.replies.pop().unwrap_or(Reply::Nil))
    }

    /// Creates an empty store file.
    pub fn create_file(path: &Path) -> Result<(), HostError> {
        Ok(crate::store::create_file(path)?)
    }
}
