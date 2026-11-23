//! The KV host: stores on this server, which app environments bind them,
//! and where the stores of this server's apps live.
//!
//! Apps call through the server's internal socket with their binding's
//! token. The host serves a store it holds, or forwards the request to the
//! store's server over the private network ([`crate::peer`]), which checks
//! the environment's grant there.

use std::collections::{BTreeSet, HashMap};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use parking_lot::{Mutex, RwLock};
use sha2::{Digest, Sha256};
use tako_core::{ErrorCode, KvRequest, Response};

use crate::peer::{PeerPool, PeerRequest};
use crate::store::{Clock, Replaced, Stats, Store, StoreConfig, StoreError, system_clock};

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

/// Generates a binding token: 32 random bytes, hex encoded.
pub fn generate_token() -> String {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).expect("operating system random source");
    hex::encode(bytes)
}

#[derive(Clone)]
pub struct HostConfig {
    /// redb page cache per store, while [`Self::total_cache_bytes`] has room.
    pub cache_bytes: usize,
    /// redb page cache shared by every store of the host. A store opened
    /// once it is used up gets [`MIN_CACHE_BYTES`], so memory stays bounded
    /// however many stores a server hosts.
    pub total_cache_bytes: usize,
    /// How long forwarding waits to reach another server.
    pub connect_timeout: Duration,
    /// How long a forwarded request waits for its reply.
    pub request_timeout: Duration,
    pub clock: Clock,
}

impl Default for HostConfig {
    fn default() -> Self {
        Self {
            cache_bytes: 16 * 1024 * 1024,
            total_cache_bytes: 256 * 1024 * 1024,
            connect_timeout: Duration::from_secs(5),
            request_timeout: Duration::from_secs(30),
            clock: system_clock(),
        }
    }
}

/// The page cache a store gets once the host's shared budget is used up.
pub const MIN_CACHE_BYTES: usize = 1024 * 1024;

impl HostConfig {
    fn store_config(&self, cache_bytes: usize) -> StoreConfig {
        StoreConfig {
            cache_bytes,
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

#[derive(Debug, thiserror::Error)]
pub enum HostError {
    #[error("KV store {0} is not hosted on this server")]
    UnknownStore(StoreId),
    #[error("KV store {0} is already registered")]
    AlreadyRegistered(StoreId),
    #[error("{0}")]
    Store(#[from] StoreError),
}

/// Why a KV call failed, as the internal socket reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KvError {
    pub code: Option<ErrorCode>,
    pub message: String,
}

pub(crate) const NOT_SERVED: &str = "This server is not serving the KV store right now; retry.";
const REBOUND: &str =
    "This app's KV binding changed or was removed; the app's processes restart with the new one.";
pub(crate) const REFUSED: &str =
    "The KV store's server refused this app. Deploy the environment with full access to grant it.";

impl KvError {
    pub fn new(code: Option<ErrorCode>, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new(Some(ErrorCode::InvalidRequest), message)
    }

    pub fn unavailable(message: impl Into<String>) -> Self {
        Self::new(Some(ErrorCode::Unavailable), message)
    }

    pub fn into_response(self) -> Response {
        match self.code {
            Some(code) => Response::coded_error(code, self.message),
            None => Response::error(self.message),
        }
    }
}

impl From<StoreError> for KvError {
    fn from(error: StoreError) -> Self {
        match error {
            StoreError::Closed => Self::unavailable(NOT_SERVED),
            StoreError::Invalid(message) => Self::invalid(message),
            StoreError::Storage(message) => Self::new(None, format!("KV storage error: {message}")),
        }
    }
}

struct Inner {
    config: HostConfig,
    stores: RwLock<HashMap<StoreId, Arc<Store>>>,
    /// Page cache held by each open store, from the shared budget.
    caches: Mutex<HashMap<StoreId, usize>>,
    bindings: RwLock<HashMap<String, Vec<Binding>>>,
    by_token: RwLock<HashMap<TokenHash, (String, Binding)>>,
    remote_grants: RwLock<HashMap<String, Grant>>,
    peers: PeerPool,
}

/// The KV host. Cheap to clone.
#[derive(Clone)]
pub struct KvHost {
    inner: Arc<Inner>,
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

/// A value reply: the JSON text, or null.
fn reply(value: Option<Vec<u8>>) -> serde_json::Value {
    match value {
        Some(json) => serde_json::Value::String(String::from_utf8_lossy(&json).into_owned()),
        None => serde_json::Value::Null,
    }
}

impl KvHost {
    pub fn new(config: HostConfig) -> Self {
        let peers = PeerPool::new(config.connect_timeout, config.request_timeout);
        Self {
            inner: Arc::new(Inner {
                config,
                stores: RwLock::default(),
                caches: Mutex::default(),
                bindings: RwLock::default(),
                by_token: RwLock::default(),
                remote_grants: RwLock::default(),
                peers,
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
        let cache = {
            let mut caches = self.inner.caches.lock();
            if caches.contains_key(&id) {
                return Err(HostError::AlreadyRegistered(id));
            }
            let used: usize = caches.values().sum();
            let config = &self.inner.config;
            let cache = config
                .cache_bytes
                .min(config.total_cache_bytes.saturating_sub(used))
                .max(MIN_CACHE_BYTES.min(config.cache_bytes));
            caches.insert(id.clone(), cache);
            cache
        };
        let opened = Store::open(&path, self.inner.config.store_config(cache));
        let mut stores = self.inner.stores.write();
        match opened {
            Ok(store) if !stores.contains_key(&id) => {
                stores.insert(id, Arc::new(store));
                Ok(())
            }
            Ok(_) => Err(HostError::AlreadyRegistered(id)),
            Err(error) => {
                self.inner.caches.lock().remove(&id);
                Err(error.into())
            }
        }
    }

    /// Page cache the host's open stores hold together.
    pub fn cache_bytes(&self) -> usize {
        self.inner.caches.lock().values().sum()
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

    fn store(&self, id: &StoreId) -> Option<Arc<Store>> {
        self.inner.stores.read().get(id).cloned()
    }

    /// Stops serving a store and closes its file once queued writes finish.
    /// Calls to it get errors until it is registered again. Blocks.
    pub fn unregister(&self, id: &StoreId) -> Result<(), HostError> {
        let store = self
            .inner
            .stores
            .write()
            .remove(id)
            .ok_or_else(|| HostError::UnknownStore(id.clone()))?;
        store.close();
        self.inner.caches.lock().remove(id);
        Ok(())
    }

    /// Stops serving every store, as when a reloaded process takes over.
    /// Blocks.
    pub fn close_all(&self) {
        let stores: Vec<Arc<Store>> = self.inner.stores.write().drain().map(|(_, s)| s).collect();
        for store in stores {
            store.close();
        }
        self.inner.caches.lock().clear();
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

    /// Whether another server may act for `scope` on `store`. Both the
    /// source address (checked by the peer listener) and the grant's key
    /// must match: any process on a server can reach the peer port from its
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

    /// An app's call through the internal socket. `app` is the caller the
    /// socket authenticated; `token` is one of its bindings. Blocks, and
    /// may wait on another server.
    pub fn call(
        &self,
        app: &str,
        token: &str,
        request: KvRequest,
    ) -> Result<serde_json::Value, KvError> {
        request.validate().map_err(KvError::invalid)?;
        let (scope, binding) = self
            .inner
            .by_token
            .read()
            .get(&hash_token(token))
            .cloned()
            .ok_or_else(|| KvError::unavailable(REBOUND))?;
        // A token is only good for the app environment it was issued to.
        if environment_scope(&scope) != app {
            return Err(KvError::unavailable(REBOUND));
        }
        match binding.home {
            Home::Local => self.serve(&binding.store, request),
            Home::Remote { upstream, key } => self.inner.peers.request(
                upstream,
                &PeerRequest {
                    scope: environment_scope(&scope).to_owned(),
                    grant: key,
                    store: binding.store.as_str().to_owned(),
                    request,
                },
            ),
            Home::Unavailable { reason } => Err(KvError::unavailable(reason)),
        }
    }

    /// A request another server forwarded for one of its app environments.
    pub(crate) fn serve_peer(&self, peer: PeerRequest) -> Result<serde_json::Value, KvError> {
        peer.request.validate().map_err(KvError::invalid)?;
        let store = StoreId::parse(&peer.store).map_err(KvError::invalid)?;
        if !self.granted(&peer.scope, &store, &peer.grant) {
            return Err(KvError::unavailable(REFUSED));
        }
        self.serve(&store, peer.request)
    }

    /// An administrator's call on a store this server hosts, for
    /// `tako kv get|put|delete`. Blocks.
    pub fn admin(&self, id: &StoreId, request: KvRequest) -> Result<serde_json::Value, KvError> {
        request.validate().map_err(KvError::invalid)?;
        self.serve(id, request)
    }

    fn serve(&self, id: &StoreId, request: KvRequest) -> Result<serde_json::Value, KvError> {
        let store = self
            .store(id)
            .ok_or_else(|| KvError::unavailable(NOT_SERVED))?;
        match request {
            KvRequest::Get { key } => Ok(reply(store.get(&key)?)),
            KvRequest::Put { key, json, ttl_ms } => {
                store.put(&key, json.into_bytes(), ttl_ms)?;
                Ok(serde_json::Value::Null)
            }
            KvRequest::Delete { key } => {
                store.delete(&key)?;
                Ok(serde_json::Value::Null)
            }
        }
    }

    /// The size of a store's file.
    pub fn size_bytes(&self, id: &StoreId) -> Option<u64> {
        let path = self.path(id)?;
        std::fs::metadata(path).ok().map(|meta| meta.len())
    }

    /// Key counts and charged size of a store.
    pub fn stats(&self, id: &StoreId) -> Result<Stats, HostError> {
        let store = self
            .store(id)
            .ok_or_else(|| HostError::UnknownStore(id.clone()))?;
        Ok(store.stats()?)
    }

    /// Writes an export of a store this server hosts to `path`. Blocks.
    pub fn export(&self, id: &StoreId, path: &Path) -> Result<Stats, HostError> {
        let store = self
            .store(id)
            .ok_or_else(|| HostError::UnknownStore(id.clone()))?;
        Ok(store.export_to(path)?)
    }

    /// Writes an export of a store for a backup, counting the store as
    /// unchanged until its next write. Blocks.
    pub fn snapshot(&self, id: &StoreId, path: &Path) -> Result<Stats, HostError> {
        let store = self
            .store(id)
            .ok_or_else(|| HostError::UnknownStore(id.clone()))?;
        Ok(store.snapshot_to(path)?)
    }

    /// Whether a store may differ from its last snapshot. False for a store
    /// this server does not host.
    pub fn has_changes(&self, id: &StoreId) -> bool {
        self.store(id).is_some_and(|store| store.has_changes())
    }

    /// Records that a store differs from its last snapshot.
    pub fn mark_changed(&self, id: &StoreId) {
        if let Some(store) = self.store(id) {
            store.mark_changed();
        }
    }

    /// Records that a store matches its last snapshot, as a new, empty one
    /// does.
    pub fn clear_changed(&self, id: &StoreId) {
        if let Some(store) = self.store(id) {
            store.clear_changed();
        }
    }

    /// Replaces a store's contents with the export at `path`, exporting the
    /// contents it replaces to `previous` first when given. Blocks.
    pub fn import(
        &self,
        id: &StoreId,
        path: &Path,
        previous: Option<&Path>,
    ) -> Result<Replaced, HostError> {
        let store = self
            .store(id)
            .ok_or_else(|| HostError::UnknownStore(id.clone()))?;
        Ok(store.replace_from(path, previous)?)
    }

    /// Creates an empty store file at `path`, closed again before this
    /// returns.
    pub fn create_file(path: &Path) -> Result<(), HostError> {
        Ok(crate::store::create_file(path)?)
    }
}

#[cfg(test)]
mod tests;
