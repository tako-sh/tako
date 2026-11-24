//! Databases hosted by this server, and the database bindings of its apps.
//!
//! Each database is a file under `<data_dir>/databases/<id>/`, owned by the
//! server process: a SQLite database served over the libSQL HTTP protocol on
//! a loopback port, or a KV store (redb) served through the internal socket,
//! both with the app's binding token. Apps on other servers of the network
//! reach it through their own server, which forwards requests to this
//! server's private address.

use std::collections::{BTreeSet, HashMap};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use parking_lot::{Mutex, RwLock};
use tako_core::{
    CHANNEL_REPLAY_BINDING, DatabaseBinding, DatabaseConnection, DatabaseEngine, DatabaseInfo,
    DatabaseQueryResult, KvConnection, Response, StorageBinding, StorageProvider,
    WORKFLOWS_BINDING, channel_replay_grant_scope,
};
use tako_db::{Binding, DatabaseHost, DatabaseId, Home, HostConfig};

use crate::ServerState;
use crate::instances::{AppConfig, AppLaunch};
use crate::state_store::{AppDatabase, DatabaseSecrets, StoredDatabase};

pub(crate) mod backups;
mod encryption;
mod objects;
pub(crate) mod replication;
pub(crate) mod snapshots;
mod transfer;

/// Scope suffix of the temporary bindings a release command receives.
const RELEASE_SCOPE_SUFFIX: &str = "#release";

pub(crate) struct Databases {
    pub(crate) host: DatabaseHost,
    /// KV stores, which share the databases' names, bindings and grants.
    pub(crate) kv: tako_kv::KvHost,
    root: PathBuf,
    app_port: u16,
    peer_port: u16,
    raft: replication::Storage,
    kv_peer_port: u16,
    /// This server's device ID on a native network.
    server_id: RwLock<Option<String>>,
    /// Resolves other servers' private addresses, once the network is up.
    proxy: RwLock<Option<Arc<crate::native_proxy::NativeProxy>>>,
    opened: std::sync::atomic::AtomicBool,
    started: std::sync::atomic::AtomicBool,
    /// Listener and scheduler tasks, stopped when this process hands over.
    tasks: Mutex<Vec<tokio::task::AbortHandle>>,
    pub(crate) backups: Mutex<HashMap<String, backups::BackupState>>,
    /// Each database's continuous capture.
    pub(crate) slots: backups::Slots,
    /// Disk space held for restores, resets, and transfers in progress.
    pub(crate) snapshot_space: backups::SnapshotSpace,
    /// The binding sets last applied to each scope, so a periodic refresh
    /// re-resolves homes without dropping a rollout's second set.
    applied: Mutex<HashMap<String, Vec<HashMap<String, AppDatabase>>>>,
    transfers: Mutex<transfer::Transfers>,
    locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// Databases deploys are binding, and databases being deleted, so one
    /// cannot delete a database another is about to bind.
    pins: Arc<Mutex<HashMap<String, Pin>>>,
    /// Held while grants are read and rewritten, so a grant and a delete's
    /// revocation do not overwrite each other's changes.
    grants: Mutex<()>,
    /// Databases release commands are using, by app, until each ends.
    release_pins: Mutex<HashMap<String, DatabasePins>>,
}

#[derive(Default)]
struct Pin {
    deploys: usize,
    deleting: bool,
}

/// Databases a deploy binds; they cannot be deleted until it is dropped.
pub(crate) struct DatabasePins {
    pins: Arc<Mutex<HashMap<String, Pin>>>,
    names: Vec<String>,
}

impl Drop for DatabasePins {
    fn drop(&mut self) {
        let mut pins = self.pins.lock();
        for name in &self.names {
            if let Some(pin) = pins.get_mut(name) {
                pin.deploys = pin.deploys.saturating_sub(1);
                if pin.deploys == 0 && !pin.deleting {
                    pins.remove(name);
                }
            }
        }
    }
}

/// Marks a database as being deleted until dropped.
pub(crate) struct Deleting {
    pins: Arc<Mutex<HashMap<String, Pin>>>,
    name: String,
}

impl Drop for Deleting {
    fn drop(&mut self) {
        let mut pins = self.pins.lock();
        if let Some(pin) = pins.get_mut(&self.name) {
            pin.deleting = false;
            if pin.deploys == 0 {
                pins.remove(&self.name);
            }
        }
    }
}

/// A release command's database and KV store connections, by binding.
pub(crate) type ReleaseConnections = (
    HashMap<String, DatabaseConnection>,
    HashMap<String, KvConnection>,
);

impl Databases {
    pub(crate) fn new(data_dir: &Path) -> Self {
        Self {
            host: DatabaseHost::new(HostConfig::default()),
            kv: tako_kv::KvHost::new(tako_kv::HostConfig::default()),
            root: data_dir.join("databases"),
            app_port: app_port(),
            raft: Default::default(),
            peer_port: env_port("TAKO_DATABASE_PEER_PORT", tako_core::DATABASE_PEER_PORT),
            kv_peer_port: env_port("TAKO_KV_PEER_PORT", tako_core::KV_PEER_PORT),
            server_id: RwLock::new(None),
            proxy: RwLock::new(None),
            opened: std::sync::atomic::AtomicBool::new(false),
            started: std::sync::atomic::AtomicBool::new(false),
            tasks: Mutex::new(Vec::new()),
            backups: Mutex::new(HashMap::new()),
            slots: Default::default(),
            snapshot_space: backups::SnapshotSpace::default(),
            applied: Mutex::new(HashMap::new()),
            transfers: Mutex::new(transfer::Transfers::default()),
            locks: Mutex::new(HashMap::new()),
            pins: Arc::new(Mutex::new(HashMap::new())),
            grants: Mutex::new(()),
            release_pins: Mutex::new(HashMap::new()),
        }
    }

    /// Pins the databases a deploy binds until the returned guard drops.
    /// Fails for a database being deleted.
    pub(crate) fn pin<'a>(
        &self,
        names: impl IntoIterator<Item = &'a str>,
    ) -> Result<DatabasePins, String> {
        let mut names: Vec<String> = names.into_iter().map(str::to_owned).collect();
        names.sort();
        names.dedup();
        let mut pins = self.pins.lock();
        if let Some(name) = names
            .iter()
            .find(|name| pins.get(*name).is_some_and(|pin| pin.deleting))
        {
            return Err(format!("Database {name} is being deleted."));
        }
        for name in &names {
            pins.entry(name.clone()).or_default().deploys += 1;
        }
        Ok(DatabasePins {
            pins: self.pins.clone(),
            names,
        })
    }

    /// Marks a database as being deleted, unless a deploy is binding it.
    pub(crate) fn start_deleting(&self, name: &str) -> Result<Deleting, String> {
        let mut pins = self.pins.lock();
        let pin = pins.entry(name.to_owned()).or_default();
        if pin.deploys > 0 {
            return Err(format!(
                "A deploy that binds database {name} is in progress; delete it once that deploy finishes."
            ));
        }
        pin.deleting = true;
        Ok(Deleting {
            pins: self.pins.clone(),
            name: name.to_owned(),
        })
    }

    pub(crate) fn database_dir(&self, id: &str) -> PathBuf {
        self.root.join(id)
    }

    pub(crate) fn database_path(&self, id: &str) -> PathBuf {
        self.database_dir(id).join("database.sqlite")
    }

    pub(crate) fn kv_path(&self, id: &str) -> PathBuf {
        self.database_dir(id).join("store.redb")
    }

    /// Opens a stored database's file and serves it. Blocks.
    fn register(&self, name: &str, engine: DatabaseEngine) -> Result<(), String> {
        match engine {
            DatabaseEngine::Sqlite => {
                let id = DatabaseId::parse(name)?;
                self.host
                    .register(id, self.database_path(name))
                    .map_err(|e| e.to_string())
            }
            DatabaseEngine::Kv => {
                let id = tako_kv::StoreId::parse(name)?;
                self.kv
                    .register(id, self.kv_path(name))
                    .map_err(|e| e.to_string())
            }
        }
    }

    fn is_registered(&self, name: &str, engine: DatabaseEngine) -> bool {
        match engine {
            DatabaseEngine::Sqlite => {
                DatabaseId::parse(name).is_ok_and(|id| self.host.is_registered(&id))
            }
            DatabaseEngine::Kv => {
                tako_kv::StoreId::parse(name).is_ok_and(|id| self.kv.is_registered(&id))
            }
        }
    }

    /// Replaces an app environment's grants on both hosts; each serves only
    /// the names of its own engine.
    fn set_remote_grants(&self, scope: &str, names: &BTreeSet<String>, key: &str) {
        self.host.set_remote_grants(
            scope,
            names
                .iter()
                .filter_map(|id| DatabaseId::parse(id).ok())
                .collect(),
            key,
        );
        self.kv.set_remote_grants(
            scope,
            names
                .iter()
                .filter_map(|id| tako_kv::StoreId::parse(id).ok())
                .collect(),
            key,
        );
    }

    pub(crate) fn work_dir(&self, id: &str) -> PathBuf {
        self.database_dir(id).join("work")
    }

    /// Serializes administrative operations (snapshot, restore, import,
    /// export) on one database.
    pub(crate) fn lock(&self, id: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.locks
            .lock()
            .entry(id.to_owned())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }

    pub(crate) fn server_id(&self) -> Option<String> {
        self.server_id.read().clone()
    }

    /// Whether `binding` reaches this server's copy of database `name`. A
    /// binding to the same name on another server (where a move took the
    /// database) does not: that copy is someone else's.
    pub(crate) fn binds_here(&self, binding: &DatabaseBinding, name: &str) -> bool {
        binding.name == name
            && match &binding.generation {
                Some(id) => self.raft.hosts(id, name),
                None => self.server_id().is_none_or(|own| own == binding.server),
            }
    }

    /// The connections app processes receive for their bindings.
    pub(crate) fn connections(
        &self,
        bindings: &HashMap<String, AppDatabase>,
    ) -> HashMap<String, DatabaseConnection> {
        connections(bindings)
    }

    fn home(&self, binding: &DatabaseBinding) -> Home {
        let server_id = self.server_id();
        let peer_port = match binding.engine {
            DatabaseEngine::Sqlite => self.peer_port,
            DatabaseEngine::Kv => self.kv_peer_port,
        };
        if let Some(id) = &binding.generation {
            let unavailable = || Home::Unavailable {
                reason: format!("waiting for the leader of {}", binding.name),
            };
            if !self.raft.hosts(id, &binding.name) {
                return unavailable();
            }
            let Some(leader) = self.raft.leader(id) else {
                return unavailable();
            };
            let proxy = self.proxy.read();
            let Some(proxy) = proxy.as_ref() else {
                return unavailable();
            };
            if proxy.address == leader {
                return Home::Local;
            }
            let address = IpAddr::V6(leader);
            if proxy.is_server_address(address)
                && let Some(key) = &binding.peer_key
            {
                return Home::Remote {
                    upstream: SocketAddr::new(address, peer_port),
                    key: key.clone(),
                };
            }
            return unavailable();
        }
        match server_id {
            Some(own) if own == binding.server => Home::Local,
            Some(_) => match (
                self.proxy
                    .read()
                    .as_ref()
                    .and_then(|proxy| proxy.server_address(&binding.server)),
                &binding.peer_key,
            ) {
                (Some(address), Some(key)) => Home::Remote {
                    upstream: SocketAddr::new(address, peer_port),
                    key: key.clone(),
                },
                (_, None) => Home::Unavailable {
                    reason: format!(
                        "the {}'s server has not granted this app access; deploy it again from a full client",
                        binding.engine.noun()
                    ),
                },
                (None, _) => Home::Unavailable {
                    reason: format!(
                        "the server hosting this {} is not reachable right now",
                        binding.engine.noun()
                    ),
                },
            },
            // Outside a native network every database is local.
            None => Home::Local,
        }
    }

    /// Points the tokens of an app environment at their databases. Several
    /// binding sets can be active at once during a rolling update.
    pub(crate) fn apply_bindings(&self, scope: &str, sets: &[&HashMap<String, AppDatabase>]) {
        let owned: Vec<HashMap<String, AppDatabase>> =
            sets.iter().map(|set| (*set).clone()).collect();
        // Held while resolving, so a concurrent refresh cannot restore
        // older sets.
        let mut applied = self.applied.lock();
        self.resolve_bindings(scope, &owned);
        if owned.iter().all(HashMap::is_empty) {
            applied.remove(scope);
        } else {
            applied.insert(scope.to_owned(), owned);
        }
    }

    /// Re-resolves where the databases of every applied binding live.
    pub(crate) fn refresh_homes(&self) {
        let applied = self.applied.lock();
        for (scope, sets) in applied.iter() {
            self.resolve_bindings(scope, sets);
        }
    }

    fn resolve_bindings(&self, scope: &str, sets: &[HashMap<String, AppDatabase>]) {
        let mut seen = BTreeSet::new();
        let mut local_replicas = BTreeSet::new();
        let mut bindings = Vec::new();
        let mut replay = Vec::new();
        let mut workflows = Vec::new();
        let mut kv = Vec::new();
        for set in sets {
            for (alias, binding) in set {
                if !seen.insert(binding.token.clone()) {
                    continue;
                }
                let home = self.home(&binding.binding);
                if binding
                    .binding
                    .generation
                    .as_ref()
                    .is_some_and(|id| self.raft.hosts(id, &binding.binding.name))
                {
                    local_replicas
                        .insert(tako_core::BindingTokenHashes::new(&binding.token).replica);
                }
                match binding.binding.engine {
                    DatabaseEngine::Sqlite => {
                        let Ok(database) = DatabaseId::parse(&binding.binding.name) else {
                            continue;
                        };
                        let target = match alias.as_str() {
                            CHANNEL_REPLAY_BINDING => &mut replay,
                            WORKFLOWS_BINDING => &mut workflows,
                            _ => &mut bindings,
                        };
                        target.push(Binding {
                            tokens: tako_core::BindingTokenHashes::new(&binding.token),
                            database,
                            home,
                        });
                    }
                    DatabaseEngine::Kv => {
                        let Ok(store) = tako_kv::StoreId::parse(&binding.binding.name) else {
                            continue;
                        };
                        kv.push(tako_kv::Binding {
                            tokens: tako_core::BindingTokenHashes::new(&binding.token),
                            store,
                            home: match home {
                                Home::Local => tako_kv::Home::Local,
                                Home::Remote { upstream, key } => {
                                    tako_kv::Home::Remote { upstream, key }
                                }
                                Home::Unavailable { reason } => {
                                    tako_kv::Home::Unavailable { reason }
                                }
                            },
                        });
                    }
                }
            }
        }
        self.host
            .set_bindings_with_replicas(scope, bindings, &local_replicas);
        self.kv
            .set_bindings_with_replicas(scope, kv, &local_replicas);
        if !scope.ends_with(RELEASE_SCOPE_SUFFIX) {
            self.host.set_bindings_with_replicas(
                &channel_replay_grant_scope(scope),
                replay,
                &local_replicas,
            );
            self.host.set_bindings_with_replicas(
                &tako_core::workflows_grant_scope(scope),
                workflows,
                &local_replicas,
            );
        }
    }

    pub(crate) fn remove_bindings(&self, scope: &str) {
        let mut applied = self.applied.lock();
        applied.remove(scope);
        self.resolve_bindings(scope, &[]);
    }

    /// Validates requested bindings and assigns tokens. A binding keeps its
    /// token while it names the same database.
    pub(crate) fn prepare_bindings(
        &self,
        previous: &HashMap<String, AppDatabase>,
        requested: HashMap<String, DatabaseBinding>,
        config: &AppConfig,
    ) -> Result<HashMap<String, AppDatabase>, String> {
        if requested.is_empty() {
            return Ok(HashMap::new());
        }
        let own = self.server_id();
        let mut prepared = HashMap::new();
        for (name, binding) in requested {
            if tako_core::is_runtime_binding(&name) {
                let needs = if name == CHANNEL_REPLAY_BINDING {
                    "Channel replay"
                } else {
                    "Workflow storage"
                };
                // App code reaches channels through its SDK. Workflow runs
                // are stored by tako-server itself, so a container release
                // without the SDK can still run a workflow command.
                let sdk = match (&config.compose, &config.launch) {
                    (Some(compose), _) => compose.has_sdk_services(),
                    (None, AppLaunch::Container { .. }) => config.container_options.sdk,
                    (None, AppLaunch::Native) => true,
                    (None, AppLaunch::Compose) => false,
                };
                if name == CHANNEL_REPLAY_BINDING && !sdk {
                    return Err("Hosted channel replay requires an SDK-enabled release.".into());
                }
                if binding.engine != DatabaseEngine::Sqlite {
                    return Err(format!(
                        "{needs} needs a SQLite database; {} is a KV store.",
                        binding.name
                    ));
                }
            } else {
                tako_core::validate_database_name(&name)?;
                if config.compose.is_some() || !matches!(config.launch, AppLaunch::Native) {
                    return Err(
                        "Database and KV store bindings are available to native releases only; container and Compose releases cannot use them yet."
                            .into(),
                    );
                }
            }
            tako_core::validate_database_name(&binding.name)?;
            if let Some(id) = &binding.generation {
                tako_core::validate_database_generation(id)?;
                if !self.raft.hosts(id, &binding.name) {
                    return Err(format!(
                        "Replica of {} is not ready on this server.",
                        binding.name
                    ));
                }
            }
            let local = binding.generation.is_some()
                || match &own {
                    Some(own) => own == &binding.server,
                    None => true,
                };
            if local && !self.is_registered(&binding.name, binding.engine) {
                return Err(format!(
                    "Database binding `{name}` names {} {} that is not hosted on this server.",
                    binding.engine.noun(),
                    binding.name
                ));
            }
            // A binding keeps its token only while it reaches the same copy:
            // after a database moves to another server, old and new
            // instances must route to their own homes during the rollout.
            let existing = previous
                .get(&name)
                .filter(|existing| existing.binding.same_target(&binding));
            let token = existing
                .map(|existing| existing.token.clone())
                .unwrap_or_else(tako_db::generate_token);
            let mut binding = binding;
            if binding.peer_key.is_none() {
                binding.peer_key = existing.and_then(|existing| existing.binding.peer_key.clone());
            }
            if tako_core::is_runtime_binding(&name)
                && !local
                && binding.peer_key.as_deref().is_none_or(str::is_empty)
            {
                return Err(if name == CHANNEL_REPLAY_BINDING {
                    "Remote channel replay requires a database grant from a full client.".into()
                } else {
                    "Remote workflow storage requires a database grant from a full client.".into()
                });
            }
            prepared.insert(name, AppDatabase { binding, token });
        }
        Ok(prepared)
    }

    /// Registers temporary bindings for a release command and returns their
    /// connections. They are removed with [`Self::end_release`].
    pub(crate) fn begin_release(
        &self,
        app: &str,
        config: &AppConfig,
        requested: HashMap<String, DatabaseBinding>,
    ) -> Result<ReleaseConnections, String> {
        // Release commands get their own tokens, and the environment's peer
        // keys, which deployers without administration do not send.
        let mut requested = requested;
        if requested.contains_key(CHANNEL_REPLAY_BINDING) {
            return Err("Release commands cannot use the channel replay binding.".into());
        }
        if requested.contains_key(WORKFLOWS_BINDING) {
            return Err("Release commands cannot use the workflow storage binding.".into());
        }
        for (name, binding) in &mut requested {
            if binding.peer_key.is_none()
                && let Some(current) = config
                    .databases
                    .get(name)
                    .filter(|current| current.binding.same_target(binding))
            {
                binding.peer_key = current.binding.peer_key.clone();
            }
        }
        // The app's committed configuration may not bind these yet (a first
        // deploy runs its release command before the bindings are stored),
        // so the command pins them itself until it ends.
        let pins = self.pin(requested.values().map(|binding| binding.name.as_str()))?;
        let bindings = self.prepare_bindings(&HashMap::new(), requested, config)?;
        self.apply_bindings(&format!("{app}{RELEASE_SCOPE_SUFFIX}"), &[&bindings]);
        self.release_pins.lock().insert(app.to_owned(), pins);
        Ok((self.connections(&bindings), kv_connections(&bindings)))
    }

    pub(crate) fn end_release(&self, app: &str) {
        self.remove_bindings(&format!("{app}{RELEASE_SCOPE_SUFFIX}"));
        self.release_pins.lock().remove(app);
    }

    pub(crate) fn transfers(&self) -> parking_lot::MutexGuard<'_, transfer::Transfers> {
        self.transfers.lock()
    }
}

fn report(report: tako_db::MigrationReport) -> tako_core::DatabaseMigrationReport {
    tako_core::DatabaseMigrationReport {
        applied: report.applied,
        pending: report.pending,
        warnings: report.warnings,
    }
}

/// The loopback port apps reach databases on.
pub(crate) fn app_port() -> u16 {
    static PORT: std::sync::OnceLock<u16> = std::sync::OnceLock::new();
    *PORT.get_or_init(|| env_port("TAKO_DATABASE_PORT", tako_core::DATABASE_APP_PORT))
}

/// The connections app processes receive for their SQLite bindings.
pub(crate) fn connections(
    bindings: &HashMap<String, AppDatabase>,
) -> HashMap<String, DatabaseConnection> {
    bindings
        .iter()
        .filter(|(name, binding)| {
            !tako_core::is_runtime_binding(name) && binding.binding.engine == DatabaseEngine::Sqlite
        })
        .map(|(name, binding)| (name.clone(), connection(binding)))
        .collect()
}

/// The hosted store of an app's workflow runs, from its `@workflows`
/// binding. tako-server is its only client.
pub(crate) fn workflow_store(
    app: &str,
    bindings: &HashMap<String, AppDatabase>,
) -> Option<tako_workflows::WorkflowStoreConfig> {
    bindings.get(WORKFLOWS_BINDING).map(|binding| {
        tako_workflows::WorkflowStoreConfig::hosted(
            connection(binding).url,
            binding.token.clone(),
            app,
        )
    })
}

pub(crate) fn connection(binding: &AppDatabase) -> DatabaseConnection {
    DatabaseConnection {
        url: format!("http://127.0.0.1:{}", app_port()),
        replica_auth_token: Some(tako_core::replica_token(&binding.token)),
        auth_token: binding.token.clone(),
    }
}

/// The connections app processes receive for their KV store bindings.
pub(crate) fn kv_connections(
    bindings: &HashMap<String, AppDatabase>,
) -> HashMap<String, KvConnection> {
    bindings
        .iter()
        .filter(|(_, binding)| binding.binding.engine == DatabaseEngine::Kv)
        .map(|(name, binding)| (name.clone(), KvConnection::new(&binding.token)))
        .collect()
}

fn env_port(name: &str, default: u16) -> u16 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

impl ServerState {
    /// The server's committed hosted replay connection for an app environment.
    pub fn channel_replay_database(&self, app: &str) -> Result<Option<DatabaseConnection>, String> {
        self.state_store
            .channel_credentials(app)
            .map_err(|_| "channel database binding unavailable".into())
    }

    /// Opens the hosted databases and serves them to this server's apps.
    /// Runs before apps are restored, so they reach their databases at boot;
    /// their bindings are applied as each app is restored. Only a primary
    /// process opens databases: a standby would hold their file locks.
    pub(crate) async fn open_databases(
        self: &Arc<Self>,
        server_id: Option<String>,
    ) -> Result<(), String> {
        use std::sync::atomic::Ordering;
        let databases = &self.databases;
        if databases.opened.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        if let Some(server_id) = server_id {
            *databases.server_id.write() = Some(server_id);
        }
        let prepared = create_private_dir(&databases.root).and_then(|()| {
            self.state_store
                .list_databases()
                .map_err(|e| format!("read databases: {e}"))
        });
        let stored = match prepared {
            Ok(stored) => stored,
            Err(error) => {
                // Nothing was opened, so a later call (such as
                // `start_databases`) tries again.
                databases.opened.store(false, Ordering::Release);
                return Err(error);
            }
        };
        let groups = self
            .state_store
            .replication_groups()
            .map_err(|e| e.to_string())?;
        for database in stored {
            if groups.contains_key(&database.generation) {
                continue;
            }
            let state = self.clone();
            let registered = tokio::task::spawn_blocking(move || {
                state
                    .databases
                    .register(&database.name, database.engine)
                    .map_err(|error| (database.name, error))
            })
            .await;
            match registered {
                Ok(Ok(())) => {}
                Ok(Err((name, error))) => {
                    tracing::error!(database = %name, %error, "Failed to open database");
                }
                Err(error) => tracing::error!(%error, "Failed to open database"),
            }
        }
        self.spawn_database_app_listener();
        Ok(())
    }

    /// Registers a stored database that is not being served, as after a
    /// failed registration. Holds the database's lock, so it cannot race a
    /// delete, which unregisters it under that lock before forgetting it.
    pub(crate) async fn retry_database_registration(&self, name: &str) {
        let databases = &self.databases;
        if !databases.opened.load(std::sync::atomic::Ordering::Acquire) {
            return;
        }
        let Ok(Some(stored)) = self.state_store.get_database(name) else {
            return;
        };
        // A replica must never fall back to its original bootstrap file.
        if self
            .state_store
            .replication_groups()
            .map_or(true, |groups| groups.contains_key(&stored.generation))
        {
            return;
        }
        if databases.is_registered(name, stored.engine) {
            return;
        }
        let lock = databases.lock(name);
        let _guard = lock.lock().await;
        let Ok(Some(stored)) = self.state_store.get_database(name) else {
            return;
        };
        if databases.is_registered(name, stored.engine) {
            return;
        }
        let state = self.databases.clone();
        let owned = name.to_owned();
        let registered =
            tokio::task::spawn_blocking(move || state.register(&owned, stored.engine)).await;
        match registered {
            Ok(Ok(())) => {
                tracing::info!(database = %name, "Opened database after an earlier failure");
                self.refresh_database_routes();
            }
            Ok(Err(error)) => {
                tracing::warn!(database = %name, %error, "Database still cannot be opened")
            }
            Err(error) => {
                tracing::warn!(database = %name, %error, "Database still cannot be opened")
            }
        }
    }

    /// Starts what needs the network: other servers' addresses, grants for
    /// apps on other servers, the peer listener, and snapshots. A promoted
    /// standby runs this, with [`Self::open_databases`], on promotion.
    pub(crate) async fn start_databases(
        self: &Arc<Self>,
        native: Option<(String, Arc<crate::native_proxy::NativeProxy>)>,
    ) -> Result<(), String> {
        use std::sync::atomic::Ordering;
        let server_id = native.as_ref().map(|(id, _)| id.clone());
        self.open_databases(server_id).await?;
        let databases = &self.databases;
        if databases.started.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        if let Some((_, proxy)) = native {
            *databases.proxy.write() = Some(proxy);
        }
        self.start_storage_replication().await?;
        self.refresh_database_routes();
        match self.state_store.all_database_grants() {
            Ok(grants) => {
                for (scope, grant) in grants {
                    databases.set_remote_grants(&scope, &grant.databases, &grant.key);
                }
            }
            Err(error) => tracing::error!(%error, "Failed to read database grants"),
        }
        let reaper = databases.host.spawn_reaper();
        databases.tasks.lock().push(reaper.abort_handle());
        self.spawn_database_peer_listener();
        let scheduler = backups::spawn_scheduler(self.clone());
        databases.tasks.lock().push(scheduler.abort_handle());
        let kv_snapshots = snapshots::spawn_scheduler(self.clone());
        databases.tasks.lock().push(kv_snapshots.abort_handle());
        Ok(())
    }

    /// Stops serving databases before this process exits, for example when
    /// a reloaded process has taken over: closes the listeners, so new
    /// connections reach the successor (which knows every current token),
    /// uploads what capture still has and stops it, and releases streams and
    /// file locks.
    pub(crate) async fn stop_databases(&self) {
        for task in self.databases.tasks.lock().drain(..) {
            task.abort();
        }
        backups::stop_all(self).await;
        self.databases.host.close_all().await;
        let kv = self.databases.kv.clone();
        let _ = tokio::task::spawn_blocking(move || kv.close_all()).await;
        self.databases.raft.shutdown().await;
    }

    /// Re-resolves where each applied binding's database lives, since other
    /// servers' addresses come from network state. Keeps a rollout's old and
    /// new binding sets.
    pub(crate) fn refresh_database_routes(&self) {
        self.databases.refresh_homes();
    }

    fn spawn_database_app_listener(self: &Arc<Self>) {
        let state = self.clone();
        let task = tokio::spawn(async move {
            let address =
                SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), state.databases.app_port);
            loop {
                match bind_reuseport(address) {
                    Ok(listener) => {
                        tracing::info!(%address, "Serving databases to apps");
                        tako_db::http::serve(
                            state.databases.host.clone(),
                            listener,
                            tako_db::http::Access::Apps,
                        )
                        .await;
                    }
                    Err(error) => {
                        tracing::warn!(%address, %error, "Database listener unavailable; retrying");
                        tokio::time::sleep(Duration::from_secs(5)).await;
                    }
                }
            }
        });
        self.databases.tasks.lock().push(task.abort_handle());
    }

    fn spawn_database_peer_listener(self: &Arc<Self>) {
        let Some(proxy) = self.databases.proxy.read().clone() else {
            return;
        };
        let port = self.databases.peer_port;
        let host = self.databases.host.clone();
        let kv_port = self.databases.kv_peer_port;
        let kv = self.databases.kv.clone();
        let kv_proxy = proxy.clone();
        let task = tokio::spawn(async move {
            let address = SocketAddr::new(IpAddr::V6(kv_proxy.address), kv_port);
            loop {
                match bind_reuseport(address) {
                    Ok(listener) => {
                        tracing::info!(%address, "Serving KV stores to network servers");
                        let proxy = kv_proxy.clone();
                        tako_kv::serve_peers(
                            kv.clone(),
                            listener,
                            Arc::new(move |peer| proxy.is_server_address(peer)),
                        )
                        .await;
                    }
                    Err(error) => {
                        // The WireGuard interface may not be up yet.
                        tracing::debug!(%address, %error, "KV peer listener unavailable; retrying");
                        tokio::time::sleep(Duration::from_secs(5)).await;
                    }
                }
            }
        });
        self.databases.tasks.lock().push(task.abort_handle());
        let task = tokio::spawn(async move {
            let address = SocketAddr::new(IpAddr::V6(proxy.address), port);
            loop {
                match bind_reuseport(address) {
                    Ok(listener) => {
                        tracing::info!(%address, "Serving databases to network servers");
                        let proxy = proxy.clone();
                        tako_db::http::serve(
                            host.clone(),
                            listener,
                            tako_db::http::Access::Peers {
                                allow: Arc::new(move |peer| proxy.is_server_address(peer)),
                            },
                        )
                        .await;
                    }
                    Err(error) => {
                        // The WireGuard interface may not be up yet.
                        tracing::debug!(%address, %error, "Database peer listener unavailable; retrying");
                        tokio::time::sleep(Duration::from_secs(5)).await;
                    }
                }
            }
        });
        self.databases.tasks.lock().push(task.abort_handle());
    }

    pub(crate) async fn handle_database_command(&self, command: tako_core::Command) -> Response {
        use tako_core::Command;
        // SQL and migrations are SQLite's; backups and file transfers work
        // on both engines.
        let sqlite_only = match &command {
            Command::QueryDatabase { name, .. }
            | Command::MigrateDatabase { name, .. }
            | Command::ResetDatabase { name, .. } => Some(name.as_str()),
            _ => None,
        };
        if let Some(name) = sqlite_only
            && let Ok(Some(stored)) = self.state_store.get_database(name)
            && stored.engine == DatabaseEngine::Kv
        {
            return Response::error(format!(
                "{name} is a KV store; this works only on SQLite databases."
            ));
        }
        let result = match command {
            Command::CoordinateStorage { request } => self.coordinate_storage(request).await,
            Command::CreateDatabase {
                name,
                engine,
                generation,
                backup_key_base64,
                backup,
            } => {
                self.create_database(name, engine, generation, backup_key_base64, backup)
                    .await
            }
            Command::DeleteDatabase {
                name,
                generation,
                keep_snapshots,
            } => {
                self.delete_database(&name, generation.as_deref(), keep_snapshots)
                    .await
            }
            Command::DatabaseStatus { name } => self.database_status(name.as_deref()),
            Command::ConfigureDatabaseBackup { name, backup } => {
                self.configure_database_backup(&name, backup).await
            }
            Command::GrantDatabases {
                app,
                databases,
                keep_existing,
            } => self.grant_databases(&app, databases, keep_existing),
            Command::SnapshotDatabase { name, reason, app } => {
                self.snapshot_database_command(&name, reason, app.as_deref())
                    .await
            }
            Command::ListDatabaseSnapshots { name } => match self.engine_of(&name) {
                Some(DatabaseEngine::Kv) => snapshots::list_command(self, &name).await,
                _ => backups::list_command(self, &name).await,
            },
            Command::RestoreDatabase {
                name,
                snapshot,
                at_unix_ms,
                backup_key_base64,
            } => match (self.engine_of(&name), at_unix_ms) {
                // KV stores keep snapshot backups, without a history to
                // restore a moment of.
                (Some(DatabaseEngine::Kv), Some(_)) => Err(format!(
                    "{name} is a KV store, which restores from its snapshots; a time works only on SQLite databases."
                )),
                (Some(DatabaseEngine::Kv), None) => {
                    snapshots::restore(self, &name, snapshot.as_deref(), backup_key_base64).await
                }
                _ => match restore_target(snapshot.as_deref(), at_unix_ms) {
                    Ok(target) => backups::restore(self, &name, target, backup_key_base64).await,
                    Err(error) => Err(error),
                },
            },
            Command::QueryDatabase { name, sql } => self.query_database(&name, sql).await,
            // KV errors keep their codes (a full store, an invalid request).
            Command::QueryKv { name, request } => return self.query_kv(&name, request).await,
            Command::MigrateDatabase {
                name,
                owner,
                migrations,
                dry_run,
                app,
            } => {
                self.migrate_database(&name, owner, migrations, dry_run, app.as_deref())
                    .await
            }
            Command::ResetDatabase {
                name,
                owner,
                migrations,
            } => self.reset_database(&name, owner, migrations).await,
            Command::BeginDatabaseImport {
                name,
                size_bytes,
                sha256,
            } => transfer::begin_import(self, &name, size_bytes, &sha256).await,
            Command::DatabaseImportChunk {
                name,
                upload_id,
                offset,
                data_base64,
            } => transfer::import_chunk(self, &name, &upload_id, offset, &data_base64).await,
            Command::FinishDatabaseImport { name, upload_id } => {
                transfer::finish_import(self, &name, &upload_id).await
            }
            Command::BeginDatabaseExport { name } => transfer::begin_export(self, &name).await,
            Command::DatabaseExportChunk {
                name,
                export_id,
                offset,
            } => transfer::export_chunk(self, &name, &export_id, offset).await,
            Command::FinishDatabaseExport { name, export_id } => {
                transfer::finish_export(self, &name, &export_id)
            }
            _ => Err("not a database command".to_string()),
        };
        match result {
            Ok(data) => Response::Ok { data },
            Err(message) => Response::error(message),
        }
    }

    /// The engine of a hosted database, when there is one by that name.
    fn engine_of(&self, id: &str) -> Option<DatabaseEngine> {
        self.state_store
            .get_database(id)
            .ok()
            .flatten()
            .map(|stored| stored.engine)
    }

    pub(crate) fn hosted_database(&self, id: &str) -> Result<StoredDatabase, String> {
        tako_core::validate_database_name(id)?;
        self.state_store
            .get_database(id)
            .map_err(|e| format!("read database: {e}"))?
            .ok_or_else(|| format!("Database {id} is not hosted on this server."))
    }

    async fn create_database(
        &self,
        id: String,
        engine: DatabaseEngine,
        generation: String,
        backup_key_base64: String,
        backup: Option<StorageBinding>,
    ) -> Result<serde_json::Value, String> {
        tako_core::validate_database_name(&id)?;
        tako_core::validate_database_generation(&generation)?;
        decode_backup_key(&backup_key_base64)?;
        if let Some(backup) = &backup {
            validate_backup_storage(backup)?;
        }
        // Held until the database is recorded and registered, so a concurrent
        // create of the same name waits and then finds it, rather than
        // sharing (and on failure removing) its directory.
        let lock = self.databases.lock(&id);
        let _guard = lock.lock().await;
        if self
            .state_store
            .get_database(&id)
            .map_err(|e| e.to_string())?
            .is_some()
        {
            return Err(format!("Database {id} already exists on this server."));
        }
        create_private_dir(&self.databases.root)?;
        create_private_dir(&self.databases.database_dir(&id))?;
        // A create that fails part way leaves nothing behind, so it can be
        // retried under the same name.
        let undo = |recorded: bool| {
            if recorded && let Err(error) = self.state_store.delete_database(&id) {
                tracing::warn!(database = %id, %error, "Could not remove the record of a failed create");
            }
            let _ = std::fs::remove_dir_all(self.databases.database_dir(&id));
        };
        let create_path = match engine {
            DatabaseEngine::Sqlite => self.databases.database_path(&id),
            DatabaseEngine::Kv => self.databases.kv_path(&id),
        };
        let created = tokio::task::spawn_blocking(move || match engine {
            DatabaseEngine::Sqlite => {
                DatabaseHost::create_file(&create_path).map_err(|e| e.to_string())
            }
            DatabaseEngine::Kv => {
                tako_kv::KvHost::create_file(&create_path).map_err(|e| e.to_string())
            }
        })
        .await
        .map_err(|e| e.to_string())
        .and_then(|created| created.map_err(|e| format!("create database file: {e}")));
        if let Err(error) = created {
            undo(false);
            return Err(error);
        }
        let stored = StoredDatabase {
            name: id.clone(),
            engine,
            generation,
            created_at_unix_secs: now_unix_secs(),
            secrets: DatabaseSecrets {
                backup_key_base64,
                backup,
            },
        };
        if let Err(error) = self.state_store.create_database(&stored) {
            undo(false);
            return Err(format!("record database: {error}"));
        }
        // A copy created for a move backs up only once it is restored; a new
        // database clears this on its first backup. Marked before the
        // database is registered, so no scheduler tick finds it unmarked.
        if engine == DatabaseEngine::Sqlite
            && let Err(error) = backups::mark_unrestored(self, &id)
        {
            undo(true);
            return Err(error);
        }
        let databases = self.databases.clone();
        let register_id = id.clone();
        let registered =
            tokio::task::spawn_blocking(move || databases.register(&register_id, engine))
                .await
                .map_err(|e| e.to_string())
                .and_then(|registered| registered);
        if let Err(error) = registered {
            undo(true);
            return Err(format!("open database: {error}"));
        }
        match engine {
            DatabaseEngine::Sqlite => {
                // Nothing to capture until something is written.
                if let Ok(database) = DatabaseId::parse(&id) {
                    self.databases.host.clear_changed(&database);
                }
            }
            DatabaseEngine::Kv => {
                // Nothing to snapshot until something is written.
                if let Ok(store) = tako_kv::StoreId::parse(&id) {
                    self.databases.kv.clear_changed(&store);
                }
            }
        }
        tracing::info!(database = %id, engine = ?engine, "Created database");
        Ok(serde_json::to_value(self.database_info(&stored)?).expect("info serializes"))
    }

    async fn delete_database(
        &self,
        id: &str,
        generation: Option<&str>,
        keep_snapshots: bool,
    ) -> Result<serde_json::Value, String> {
        let requested = self.hosted_database(id)?;
        let lock = self.databases.lock(id);
        let _guard = lock.lock().await;
        // Another delete and create of the same name may have run while this
        // waited for the lock, or before the caller sent it: act only on the
        // database that was asked for.
        let stored = self.hosted_database(id)?;
        if stored.generation != generation.unwrap_or(&requested.generation) {
            return Err(format!(
                "Database {id} on this server is not the one asked for: a database of the same name was created again meanwhile. Nothing was deleted."
            ));
        }
        // From here, deploys cannot start binding the database; one already
        // binding it makes the delete wait for it to finish.
        let _deleting = self.databases.start_deleting(id)?;
        let info = self.database_info(&stored)?;
        // Bindings of apps on this server are known to be current. Grants for
        // other servers can outlive the deployments that used them (deleting
        // an app does not reach this server), so they are revoked instead;
        // the CLI names them before asking to delete.
        if !info.bindings.is_empty() {
            return Err(format!(
                "Database {} is still bound by {} on this server. Remove the bindings and deploy those apps first.",
                stored.name,
                info.bindings.join(", ")
            ));
        }
        // Unregistering can time out on a request in flight; the database is
        // then still hosted, so its grants stay until this succeeds.
        match stored.engine {
            DatabaseEngine::Sqlite => {
                let database = DatabaseId::parse(id)?;
                if self.databases.host.is_registered(&database) {
                    self.databases
                        .host
                        .unregister(&database)
                        .await
                        .map_err(|e| e.to_string())?;
                }
            }
            DatabaseEngine::Kv => {
                let store = tako_kv::StoreId::parse(id)?;
                if self.databases.kv.is_registered(&store) {
                    let kv = self.databases.kv.clone();
                    tokio::task::spawn_blocking(move || kv.unregister(&store))
                        .await
                        .map_err(|e| e.to_string())?
                        .map_err(|e| e.to_string())?;
                }
            }
        }
        let grants = self.databases.grants.lock();
        for (scope, grant) in self
            .state_store
            .all_database_grants()
            .map_err(|e| format!("read grants: {e}"))?
        {
            if !grant.databases.contains(id) {
                continue;
            }
            let remaining: BTreeSet<String> = grant
                .databases
                .iter()
                .filter(|other| other.as_str() != id)
                .cloned()
                .collect();
            let key = self
                .state_store
                .set_database_grants(&scope, &remaining)
                .map_err(|e| format!("revoke grants: {e}"))?;
            self.databases.set_remote_grants(
                &scope,
                &remaining,
                key.as_deref().unwrap_or_default(),
            );
        }
        drop(grants);
        // An open import file would otherwise keep its disk space, and take
        // more chunks, after its directory is removed.
        transfer::cancel(self, id);
        // Capture holds the file and its spool, which go with the directory.
        backups::forget(self, id, false).await?;
        // The files go before the record, so a delete that fails here can be
        // run again: it still finds the database, and its snapshots.
        let dir = self.databases.database_dir(id);
        tokio::task::spawn_blocking(move || std::fs::remove_dir_all(dir))
            .await
            .map_err(|e| e.to_string())?
            .or_else(|e| {
                if e.kind() == std::io::ErrorKind::NotFound {
                    Ok(())
                } else {
                    Err(format!(
                        "remove database files: {e}. The database is no longer served; run the delete again to finish it."
                    ))
                }
            })?;
        self.state_store
            .delete_database(id)
            .map_err(|e| format!("forget database: {e}"))?;
        self.databases.backups.lock().remove(id);
        tracing::info!(database = %stored.name, "Deleted database");
        // Without the database's records its snapshots cannot be decrypted;
        // they go with it, unless another copy lives on and shares them.
        let mut warnings = Vec::new();
        if keep_snapshots {
            // A copy left by an unfinished move may still hold the
            // snapshots, which would keep the database's own copy from
            // taking any.
            let released = match stored.engine {
                DatabaseEngine::Sqlite => backups::release_holder(self, &stored).await,
                DatabaseEngine::Kv => snapshots::release_holder(self, &stored).await,
            };
            if let Err(error) = released {
                warnings.push(error);
            }
            return Ok(serde_json::json!({"deleted": id, "warnings": warnings}));
        }
        // Deleting the snapshots deletes them for every copy of this
        // generation, also one that a move left holding them, and fences the
        // prefix so none publishes again; removing only a copy keeps them.
        let deleted = match stored.engine {
            DatabaseEngine::Sqlite => backups::delete_all(&stored).await,
            DatabaseEngine::Kv => snapshots::delete_all(&stored).await,
        };
        match deleted {
            Ok(0) => {}
            Ok(count) => {
                tracing::info!(database = %stored.name, objects = count, "Deleted database snapshots");
            }
            Err(error) => warnings.push(format!(
                "Could not delete every snapshot of {}: {error}. Remove `_tako/databases/{}/{}/` from the backup storage.",
                stored.name, stored.name, stored.generation
            )),
        }
        Ok(serde_json::json!({"deleted": id, "warnings": warnings}))
    }

    fn database_status(&self, id: Option<&str>) -> Result<serde_json::Value, String> {
        let stored = match id {
            Some(id) => vec![self.hosted_database(id)?],
            None => self
                .state_store
                .list_databases()
                .map_err(|e| format!("read databases: {e}"))?,
        };
        let infos = stored
            .iter()
            .map(|db| self.database_info(db))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(serde_json::json!({ "databases": infos }))
    }

    fn database_info(&self, stored: &StoredDatabase) -> Result<DatabaseInfo, String> {
        let mut bindings = BTreeSet::new();
        let mut record = |scope: &str, set: &HashMap<String, AppDatabase>| {
            for (alias, binding) in set {
                if self.databases.binds_here(&binding.binding, &stored.name) {
                    bindings.insert(
                        tako_core::runtime_grant_scope(alias, scope)
                            .unwrap_or_else(|| scope.to_owned()),
                    );
                }
            }
        };
        for (scope, set) in self
            .state_store
            .all_app_databases()
            .map_err(|e| format!("read database bindings: {e}"))?
        {
            record(&scope, &set);
        }
        // A compacted release can still roll back from its archive. Only an
        // actual archive makes a historical binding reachable. An I/O error
        // must never be interpreted as permission to drop the resource.
        for (scope, version) in self
            .state_store
            .releases_with_databases()
            .map_err(|e| e.to_string())?
        {
            let archive = crate::release::app_root(&self.runtime_config().data_dir, &scope)
                .join("archives")
                .join(&version)
                .join("release.tar.zst");
            if archive
                .try_exists()
                .map_err(|e| format!("inspect release archive: {e}"))?
                && let Some(bindings) = self
                    .state_store
                    .release_databases(&scope, &version)
                    .map_err(|e| e.to_string())?
            {
                record(&scope, &bindings);
            }
        }
        // The binding sets in use, also the previous one a rollout keeps
        // until old instances drain, and a release command's.
        for (scope, sets) in self.databases.applied.lock().iter() {
            for set in sets {
                record(scope, set);
            }
        }
        let remote_grants = self
            .state_store
            .all_database_grants()
            .map_err(|e| format!("read database grants: {e}"))?
            .into_iter()
            .filter(|(_, grant)| grant.databases.contains(&stored.name))
            .map(|(scope, _)| scope)
            .collect();
        let size_bytes = match stored.engine {
            DatabaseEngine::Sqlite => DatabaseId::parse(&stored.name)
                .ok()
                .and_then(|id| self.databases.host.size_bytes(&id)),
            DatabaseEngine::Kv => tako_kv::StoreId::parse(&stored.name)
                .ok()
                .and_then(|id| self.databases.kv.size_bytes(&id)),
        }
        .unwrap_or(0);
        Ok(DatabaseInfo {
            name: stored.name.clone(),
            engine: stored.engine,
            generation: stored.generation.clone(),
            created_at_unix_secs: stored.created_at_unix_secs,
            size_bytes,
            bindings: bindings.into_iter().collect(),
            remote_grants,
            backup: match stored.engine {
                DatabaseEngine::Sqlite => backups::status(self, stored),
                DatabaseEngine::Kv => snapshots::status(self, stored),
            },
        })
    }

    async fn configure_database_backup(
        &self,
        id: &str,
        backup: Option<StorageBinding>,
    ) -> Result<serde_json::Value, String> {
        self.hosted_database(id)?;
        if let Some(backup) = &backup {
            validate_backup_storage(backup)?;
        }
        // Waits for a restore or a transfer in progress.
        let lock = self.databases.lock(id);
        let _guard = lock.lock().await;
        let stored = self.hosted_database(id)?;
        let moved = match (&stored.secrets.backup, &backup) {
            (Some(old), Some(new)) => !tako_core::same_backup_destination(old, new),
            (None, None) => false,
            _ => true,
        };
        let secrets = DatabaseSecrets {
            backup_key_base64: stored.secrets.backup_key_base64.clone(),
            backup,
        };
        self.state_store
            .set_database_secrets(id, &secrets)
            .map_err(|e| format!("store backup configuration: {e}"))?;
        if moved {
            match stored.engine {
                DatabaseEngine::Sqlite => {
                    // The history starts over at the new destination, with
                    // a full image on the next tick; the previous one keeps
                    // what it has.
                    backups::forget(self, id, true).await?;
                }
                DatabaseEngine::Kv => {
                    // The cached status describes the previous destination,
                    // and the new one needs a snapshot even if the store is
                    // idle.
                    self.databases.backups.lock().remove(id);
                    if secrets.backup.is_some() {
                        snapshots::mark_changed(self, &stored);
                    }
                }
            }
        }
        let updated = self.hosted_database(id)?;
        Ok(serde_json::to_value(self.database_info(&updated)?).expect("info serializes"))
    }

    fn grant_databases(
        &self,
        app: &str,
        ids: Vec<String>,
        keep_existing: bool,
    ) -> Result<serde_json::Value, String> {
        let base = app
            .strip_suffix("#channels")
            .or_else(|| app.strip_suffix("#workflows"))
            .unwrap_or(app);
        base.parse::<tako_network::AppEnv>()
            .map_err(|_| "Grants name an app environment as `<app>/<env>`.".to_owned())?;
        crate::release::validate_app_name(base)?;
        // Pinned while the grant is stored, so a delete cannot pass its
        // revocation in between: one already under way makes this fail, and
        // one that starts meanwhile is refused. Grants kept from before are
        // covered by the grants lock, which a delete's revocation takes.
        let _pins = self.databases.pin(ids.iter().map(String::as_str))?;
        let _grants = self.databases.grants.lock();
        let mut granted = BTreeSet::new();
        for id in ids {
            self.hosted_database(&id)?;
            granted.insert(id);
        }
        if keep_existing
            && let Some(existing) = self
                .state_store
                .all_database_grants()
                .map_err(|e| format!("read grants: {e}"))?
                .remove(app)
        {
            granted.extend(existing.databases);
        }
        let key = self
            .state_store
            .set_database_grants(app, &granted)
            .map_err(|e| format!("store grants: {e}"))?;
        self.databases
            .set_remote_grants(app, &granted, key.as_deref().unwrap_or_default());
        // The deploying administrator hands the key to the app's servers.
        Ok(serde_json::json!({"app": app, "databases": granted, "peer_key": key}))
    }

    /// Only application SQL bindings and grants authorize deploy-time SQL.
    fn app_uses_database(&self, app: &str, id: &str) -> bool {
        if app.parse::<tako_network::AppEnv>().is_err() {
            return false;
        }
        let bound = self
            .state_store
            .get_app_databases(app)
            .is_ok_and(|bindings| {
                bindings.iter().any(|(name, binding)| {
                    !tako_core::is_runtime_binding(name)
                        && self.databases.binds_here(&binding.binding, id)
                })
            });
        bound
            || self
                .state_store
                .all_database_grants()
                .unwrap_or_default()
                .get(app)
                .is_some_and(|grant| grant.databases.contains(id))
    }

    async fn snapshot_database_command(
        &self,
        id: &str,
        reason: tako_core::SnapshotReason,
        app: Option<&str>,
    ) -> Result<serde_json::Value, String> {
        use tako_core::SnapshotReason;
        let stored = self.hosted_database(id)?;
        // App deployers may record restore points only for databases their
        // app already uses; administrators send no app.
        if let Some(app) = app {
            if !self.app_uses_database(app, id) {
                return Err(format!("{app} does not use database {}.", stored.name));
            }
            // Deploys record only the point before a release command; full
            // images need administration.
            if reason != SnapshotReason::Release {
                return Err(
                    "App deployers may record only the restore point before a release command."
                        .into(),
                );
            }
        }
        // A database without backups is not an error before a release.
        if reason == SnapshotReason::Release && stored.secrets.backup.is_none() {
            return Ok(serde_json::json!({"skipped": "backups are not configured"}));
        }
        if stored.engine == DatabaseEngine::Kv {
            // A KV store's backups are snapshots; before a release, an
            // unchanged store needs no new one.
            let only_if_changed = reason == SnapshotReason::Release;
            return match snapshots::take(self, &stored, reason, only_if_changed).await? {
                Some(snapshot) => Ok(serde_json::json!({ "snapshot": snapshot })),
                None => Ok(serde_json::json!({"skipped": "no changes since the last snapshot"})),
            };
        }
        let snapshot = match reason {
            SnapshotReason::Manual | SnapshotReason::Scheduled => {
                backups::full_snapshot(self, &stored, reason).await?
            }
            SnapshotReason::Release | SnapshotReason::Migration | SnapshotReason::Restore => {
                backups::mark(self, &stored, reason).await?
            }
        };
        Ok(serde_json::json!({ "snapshot": snapshot }))
    }

    async fn migrate_database(
        &self,
        id: &str,
        owner: String,
        migrations: Vec<tako_core::DatabaseMigration>,
        dry_run: bool,
        app: Option<&str>,
    ) -> Result<serde_json::Value, String> {
        let stored = self.hosted_database(id)?;
        // A deployer applies only its own app's migrations, and only to a
        // database its app environment uses.
        if let Some(app) = app {
            if !self.app_uses_database(app, id) {
                return Err(format!("{app} does not use database {}.", stored.name));
            }
            if app.split('/').next() != Some(owner.as_str()) {
                return Err(format!("{app} cannot apply migrations of {owner}."));
            }
        }
        let database = DatabaseId::parse(id)?;
        let migrations: Vec<tako_db::Migration> = migrations
            .into_iter()
            .map(|m| tako_db::Migration {
                name: m.name,
                sql: m.sql,
            })
            .collect();
        let plan = self
            .databases
            .host
            .migrate(&database, owner.clone(), migrations.clone(), true)
            .await
            .map_err(|e| e.to_string())?;
        if dry_run || plan.pending.is_empty() {
            return Ok(serde_json::to_value(report(plan)).expect("report serializes"));
        }
        // Names what the migrations change, when backups are configured.
        if stored.secrets.backup.is_some() {
            backups::mark(self, &stored, tako_core::SnapshotReason::Migration)
                .await
                .map_err(|e| format!("Back up {} before migrations: {e}", stored.name))?;
        }
        let lock = self.databases.lock(id);
        let _guard = lock.lock().await;
        // The database was read before the lock; a delete and create of the
        // same name meanwhile leaves one these migrations were not planned
        // and snapshotted for.
        if self.hosted_database(id)?.generation != stored.generation {
            return Err(format!(
                "Database {id} was replaced by a new database of the same name meanwhile; no migrations were applied."
            ));
        }
        let applied = self
            .databases
            .host
            .migrate(&database, owner, migrations, false)
            .await
            .map_err(|e| e.to_string())?;
        if !applied.applied.is_empty() {
            tracing::info!(database = %id, migrations = ?applied.applied, "Applied migrations");
        }
        Ok(serde_json::to_value(report(applied)).expect("report serializes"))
    }

    async fn reset_database(
        &self,
        id: &str,
        owner: Option<String>,
        migrations: Vec<tako_core::DatabaseMigration>,
    ) -> Result<serde_json::Value, String> {
        let stored = self.hosted_database(id)?;
        if !migrations.is_empty() && owner.as_deref().is_none_or(str::is_empty) {
            return Err("A reset with migrations must name the app they belong to.".into());
        }
        // The new file, with whatever the migrations write into it, is held
        // in the ledger snapshots, restores and transfers share. What they
        // write is not known ahead; twice their text is the estimate.
        let estimate = migrations
            .iter()
            .map(|migration| migration.sql.len() as u64)
            .sum::<u64>()
            .saturating_mul(2)
            .saturating_add(1024 * 1024);
        let available = crate::operations::available_bytes(&self.databases.database_dir(id))?;
        // Declared before the work directory, so it is released after it.
        let _space = self
            .databases
            .snapshot_space
            .reserve(estimate, available, "the reset")
            .map_err(|error| format!("Cannot reset {id}: {error}."))?;
        let work = backups::fresh_work_dir(self, id, "reset")?;
        let empty = work.path.join("empty.sqlite");
        let create = empty.clone();
        let migrations: Vec<tako_db::Migration> = migrations
            .into_iter()
            .map(|m| tako_db::Migration {
                name: m.name,
                sql: m.sql,
            })
            .collect();
        // The schema goes into the new file before it replaces the old one.
        let name = id.to_owned();
        let report = tokio::task::spawn_blocking(move || {
            DatabaseHost::create_file(&create)
                .map_err(|e| format!("create an empty database: {e}"))?;
            match owner {
                Some(owner) if !migrations.is_empty() => {
                    tako_db::migrate::migrate_file(&create, &owner, &migrations, false)
                        .map(Some)
                        .map_err(|e| format!("The migrations failed; {name} was not reset: {e}"))
                }
                _ => Ok(None),
            }
        })
        .await
        .map_err(|e| e.to_string())??;
        let mut warnings =
            backups::replace(self, &stored, empty, "database is being reset").await?;
        drop(work);
        let applied = match report {
            Some(report) => {
                warnings.extend(report.warnings);
                report.applied
            }
            None => Vec::new(),
        };
        tracing::info!(database = %id, ?applied, "Reset database");
        Ok(serde_json::json!({"database": id, "warnings": warnings, "applied": applied}))
    }

    async fn query_database(&self, id: &str, sql: String) -> Result<serde_json::Value, String> {
        self.hosted_database(id)?;
        let database = DatabaseId::parse(id)?;
        let result = self
            .databases
            .host
            .admin_query(&database, sql, tako_core::DATABASE_QUERY_MAX_ROWS)
            .await
            .map_err(|e| e.to_string())?;
        let rows = result
            .rows
            .into_iter()
            .map(|row| row.into_iter().map(json_value).collect())
            .collect();
        Ok(serde_json::to_value(DatabaseQueryResult {
            columns: result.columns,
            rows,
            affected_row_count: result.affected_row_count,
            truncated: result.truncated,
        })
        .expect("query result serializes"))
    }

    async fn query_kv(&self, id: &str, request: tako_core::KvRequest) -> Response {
        let stored = match self.hosted_database(id) {
            Ok(stored) => stored,
            Err(message) => return Response::error(message),
        };
        if stored.engine != DatabaseEngine::Kv {
            return Response::error(format!("{id} is a SQLite database; use `tako db shell`."));
        }
        let store = match tako_kv::StoreId::parse(id) {
            Ok(store) => store,
            Err(message) => return Response::error(message),
        };
        let kv = self.databases.kv.clone();
        match tokio::task::spawn_blocking(move || kv.admin(&store, request)).await {
            Ok(Ok(value)) => Response::ok(value),
            Ok(Err(error)) => error.into_response(),
            Err(error) => Response::error(error.to_string()),
        }
    }
}

fn json_value(value: tako_db::Value) -> serde_json::Value {
    match value {
        tako_db::Value::Null => serde_json::Value::Null,
        tako_db::Value::Integer(value) => value.into(),
        tako_db::Value::Float(value) => serde_json::Number::from_f64(value)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null),
        tako_db::Value::Text(value) => value.into(),
        tako_db::Value::Blob(value) => serde_json::json!({"base64": BASE64.encode(value)}),
    }
}

/// What a `RestoreDatabase` command selects.
fn restore_target(
    snapshot: Option<&str>,
    at_unix_ms: Option<i64>,
) -> Result<tako_backup::Target, String> {
    match (snapshot, at_unix_ms) {
        (Some(_), Some(_)) => Err("Restore either a restore point or a time, not both.".into()),
        (Some(snapshot), None) => snapshot
            .parse::<u64>()
            .ok()
            .filter(|txid| *txid > 0)
            .map(tako_backup::Target::Txid)
            .ok_or_else(|| format!("Restore point {snapshot} is not one `tako db backups` lists.")),
        (None, Some(at)) => Ok(tako_backup::Target::Time(at)),
        (None, None) => Ok(tako_backup::Target::Latest),
    }
}

pub(crate) fn decode_backup_key(value: &str) -> Result<[u8; 32], String> {
    let bytes = BASE64
        .decode(value)
        .map_err(|_| "database backup key is not valid base64".to_string())?;
    bytes
        .try_into()
        .map_err(|_| "database backup key must be 32 bytes".to_string())
}

pub(crate) fn validate_backup_storage(storage: &StorageBinding) -> Result<(), String> {
    if storage.provider != StorageProvider::S3 {
        return Err("Database backup storage must be S3-compatible.".into());
    }
    if storage.public_base_url.is_some() {
        return Err("Database backup storage must be private (no public_base_url).".into());
    }
    for (value, field) in [
        (&storage.bucket, "bucket"),
        (&storage.endpoint, "endpoint"),
        (&storage.region, "region"),
        (&storage.access_key_id, "access_key_id"),
        (&storage.secret_access_key, "secret_access_key"),
    ] {
        if value.as_deref().is_none_or(str::is_empty) {
            return Err(format!("Database backup storage is missing `{field}`."));
        }
    }
    Ok(())
}

fn create_private_dir(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
        .map_err(|e| format!("create {}: {e}", path.display()))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
        .map_err(|e| format!("secure {}: {e}", path.display()))
}

fn bind_reuseport(address: SocketAddr) -> std::io::Result<tokio::net::TcpListener> {
    let socket = match address {
        SocketAddr::V4(_) => tokio::net::TcpSocket::new_v4()?,
        SocketAddr::V6(_) => tokio::net::TcpSocket::new_v6()?,
    };
    socket.set_reuseaddr(true)?;
    // A reloading server's old and new processes serve the port together.
    socket.set_reuseport(true)?;
    socket.bind(address)?;
    socket.listen(1024)
}

pub(crate) fn now_unix_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn binding(server: &str, key: Option<&str>) -> DatabaseBinding {
        DatabaseBinding {
            name: "shop".into(),
            generation: None,
            server: server.into(),
            peer_key: key.map(str::to_owned),
            engine: tako_core::DatabaseEngine::Sqlite,
        }
    }

    #[test]
    fn remote_bindings_need_their_grant_key() {
        let dir = tempfile::tempdir().unwrap();
        let databases = Databases::new(dir.path());
        *databases.server_id.write() = Some("this".into());
        assert_eq!(databases.home(&binding("this", None)), Home::Local);
        let Home::Unavailable { reason } = databases.home(&binding("other", None)) else {
            panic!("a remote binding without a key is unavailable");
        };
        assert!(reason.contains("full client"));
        // Without the network, other servers' addresses are unknown.
        let Home::Unavailable { reason } = databases.home(&binding("other", Some("key"))) else {
            panic!("an unknown address is unavailable");
        };
        assert!(reason.contains("not reachable"));
    }

    #[test]
    fn release_bindings_take_the_environment_peer_key() {
        let dir = tempfile::tempdir().unwrap();
        let databases = Databases::new(dir.path());
        *databases.server_id.write() = Some("this".into());
        let mut config = AppConfig::default();
        config.databases.insert(
            "main".into(),
            AppDatabase {
                binding: binding("other", Some("issued")),
                token: "app-token".into(),
            },
        );
        let requested = HashMap::from([("main".to_owned(), binding("other", None))]);
        let (connections, _) = databases
            .begin_release("demo/production", &config, requested)
            .unwrap();
        assert_ne!(
            connections["main"].auth_token, "app-token",
            "release commands get their own token"
        );
        {
            let applied = databases.applied.lock();
            let release = &applied["demo/production#release"][0]["main"];
            assert_eq!(release.binding.peer_key.as_deref(), Some("issued"));
        }
        // The database cannot be deleted while the release command runs.
        assert!(databases.start_deleting("shop").is_err());
        databases.end_release("demo/production");
        assert!(databases.start_deleting("shop").is_ok());
    }

    #[test]
    fn only_sdk_releases_can_bind_hosted_replay_and_only_native_releases_can_bind_app_sql() {
        let dir = tempfile::tempdir().unwrap();
        let databases = Databases::new(dir.path());
        *databases.server_id.write() = Some("edge".into());
        let prepare = |alias: &str, config: &AppConfig| {
            databases.prepare_bindings(
                &HashMap::new(),
                HashMap::from([(alias.to_owned(), binding("other", Some("issued")))]),
                config,
            )
        };
        let mut config = AppConfig::default();
        assert!(prepare(CHANNEL_REPLAY_BINDING, &config).is_ok());
        assert!(prepare("main", &config).is_ok());
        config.launch = AppLaunch::Container {
            image: "app".into(),
            port: 3000,
        };
        assert!(prepare(CHANNEL_REPLAY_BINDING, &config).is_ok());
        assert!(prepare("main", &config).is_err());
        config.container_options.sdk = false;
        assert!(prepare(CHANNEL_REPLAY_BINDING, &config).is_err());
        config.launch = AppLaunch::Compose;
        config.compose = Some(Default::default());
        assert!(prepare(CHANNEL_REPLAY_BINDING, &config).is_err());
        config.compose.as_mut().unwrap().services.insert(
            "worker".into(),
            tako_core::compose::ComposeServiceConfig {
                sdk: true,
                ..Default::default()
            },
        );
        assert!(prepare(CHANNEL_REPLAY_BINDING, &config).is_ok());
        assert!(prepare("main", &config).is_err());
    }
}
