//! Databases hosted by this server, and the database bindings of its apps.
//!
//! Each database is a SQLite file under `<data_dir>/databases/<id>/`, owned by
//! the server process. Apps on this server reach it on a loopback port with
//! their binding token. Apps on other servers of the network reach it through
//! their own server, which forwards requests to this server's private address.

use std::collections::{BTreeSet, HashMap};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use parking_lot::{Mutex, RwLock};
use tako_core::{
    DatabaseBinding, DatabaseConnection, DatabaseInfo, DatabaseQueryResult, Response,
    StorageBinding, StorageProvider,
};
use tako_database::{Binding, DatabaseHost, DatabaseId, Home, HostConfig};

use crate::ServerState;
use crate::instances::{AppConfig, AppLaunch};
use crate::state_store::{AppDatabase, DatabaseSecrets, StoredDatabase};

pub(crate) mod snapshots;
mod transfer;

/// Scope suffix of the temporary bindings a release command receives.
const RELEASE_SCOPE_SUFFIX: &str = "#release";

pub(crate) struct Databases {
    pub(crate) host: DatabaseHost,
    root: PathBuf,
    app_port: u16,
    peer_port: u16,
    /// This server's device ID on a native network.
    server_id: RwLock<Option<String>>,
    /// Resolves other servers' private addresses, once the network is up.
    proxy: RwLock<Option<Arc<crate::native_proxy::NativeProxy>>>,
    opened: std::sync::atomic::AtomicBool,
    started: std::sync::atomic::AtomicBool,
    /// Listener and scheduler tasks, stopped when this process hands over.
    tasks: Mutex<Vec<tokio::task::AbortHandle>>,
    pub(crate) backups: Mutex<HashMap<String, snapshots::BackupState>>,
    /// Disk space held for snapshot copies in progress.
    pub(crate) snapshot_space: snapshots::SnapshotSpace,
    /// The binding sets last applied to each scope, so a periodic refresh
    /// re-resolves homes without dropping a rollout's second set.
    applied: Mutex<HashMap<String, Vec<HashMap<String, AppDatabase>>>>,
    transfers: Mutex<transfer::Transfers>,
    locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// Databases deploys are binding, and databases being deleted, so one
    /// cannot delete a database another is about to bind.
    pins: Arc<Mutex<HashMap<String, Pin>>>,
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
struct Deleting {
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

impl Databases {
    pub(crate) fn new(data_dir: &Path) -> Self {
        Self {
            host: DatabaseHost::new(HostConfig::default()),
            root: data_dir.join("databases"),
            app_port: app_port(),
            peer_port: env_port("TAKO_DATABASE_PEER_PORT", tako_core::DATABASE_PEER_PORT),
            server_id: RwLock::new(None),
            proxy: RwLock::new(None),
            opened: std::sync::atomic::AtomicBool::new(false),
            started: std::sync::atomic::AtomicBool::new(false),
            tasks: Mutex::new(Vec::new()),
            backups: Mutex::new(HashMap::new()),
            snapshot_space: snapshots::SnapshotSpace::default(),
            applied: Mutex::new(HashMap::new()),
            transfers: Mutex::new(transfer::Transfers::default()),
            locks: Mutex::new(HashMap::new()),
            pins: Arc::new(Mutex::new(HashMap::new())),
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
    fn start_deleting(&self, name: &str) -> Result<Deleting, String> {
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

    fn server_id(&self) -> Option<String> {
        self.server_id.read().clone()
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
                    upstream: SocketAddr::new(address, self.peer_port),
                    key: key.clone(),
                },
                (_, None) => Home::Unavailable {
                    reason: "the database's server has not granted this app access; deploy it again as an administrator".into(),
                },
                (None, _) => Home::Unavailable {
                    reason: "the server hosting this database is not reachable right now".into(),
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
        let mut bindings = Vec::new();
        for set in sets {
            for binding in set.values() {
                if seen.insert(binding.token.clone()) {
                    bindings.push(Binding {
                        token_hash: tako_database::hash_token(&binding.token),
                        database: match DatabaseId::parse(&binding.binding.name) {
                            Ok(id) => id,
                            Err(_) => continue,
                        },
                        home: self.home(&binding.binding),
                    });
                }
            }
        }
        self.host.set_bindings(scope, bindings);
    }

    pub(crate) fn remove_bindings(&self, scope: &str) {
        let mut applied = self.applied.lock();
        applied.remove(scope);
        self.host.set_bindings(scope, Vec::new());
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
        if config.compose.is_some() || !matches!(config.launch, AppLaunch::Native) {
            return Err(
                "Database bindings are available to native releases only; container and Compose releases cannot use them yet."
                    .into(),
            );
        }
        let own = self.server_id();
        let mut prepared = HashMap::new();
        for (name, binding) in requested {
            tako_core::validate_database_name(&name)?;
            tako_core::validate_database_name(&binding.name)?;
            let local = match &own {
                Some(own) => own == &binding.server,
                None => true,
            };
            if local {
                let id = DatabaseId::parse(&binding.name)?;
                if !self.host.is_registered(&id) {
                    return Err(format!(
                        "Database binding `{name}` names database {} that is not hosted on this server.",
                        binding.name
                    ));
                }
            }
            // A binding keeps its token only while it reaches the same copy:
            // after a database moves to another server, old and new
            // instances must route to their own homes during the rollout.
            let existing = previous
                .get(&name)
                .filter(|existing| existing.binding.same_target(&binding));
            let token = existing
                .map(|existing| existing.token.clone())
                .unwrap_or_else(tako_database::generate_token);
            let mut binding = binding;
            if binding.peer_key.is_none() {
                binding.peer_key = existing.and_then(|existing| existing.binding.peer_key.clone());
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
    ) -> Result<HashMap<String, DatabaseConnection>, String> {
        // Release commands get their own tokens, and the environment's peer
        // keys, which deployers without administration do not send.
        let mut requested = requested;
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
        let bindings = self.prepare_bindings(&HashMap::new(), requested, config)?;
        self.apply_bindings(&format!("{app}{RELEASE_SCOPE_SUFFIX}"), &[&bindings]);
        Ok(self.connections(&bindings))
    }

    pub(crate) fn end_release(&self, app: &str) {
        self.remove_bindings(&format!("{app}{RELEASE_SCOPE_SUFFIX}"));
    }

    pub(crate) fn transfers(&self) -> parking_lot::MutexGuard<'_, transfer::Transfers> {
        self.transfers.lock()
    }
}

fn report(report: tako_database::MigrationReport) -> tako_core::DatabaseMigrationReport {
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

/// The connections app processes receive for their bindings.
pub(crate) fn connections(
    bindings: &HashMap<String, AppDatabase>,
) -> HashMap<String, DatabaseConnection> {
    bindings
        .iter()
        .map(|(name, binding)| {
            (
                name.clone(),
                DatabaseConnection {
                    url: format!("http://127.0.0.1:{}", app_port()),
                    auth_token: binding.token.clone(),
                },
            )
        })
        .collect()
}

fn env_port(name: &str, default: u16) -> u16 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

impl ServerState {
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
        for database in stored {
            let id = match DatabaseId::parse(&database.name) {
                Ok(id) => id,
                Err(error) => {
                    tracing::error!(database = %database.name, %error, "Failed to open database");
                    continue;
                }
            };
            let path = databases.database_path(&database.name);
            let host = databases.host.clone();
            let registered = tokio::task::spawn_blocking(move || host.register(id, path)).await;
            match registered {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    tracing::error!(database = %database.name, %error, "Failed to open database");
                }
                Err(error) => {
                    tracing::error!(database = %database.name, %error, "Failed to open database")
                }
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
        let Ok(id) = DatabaseId::parse(name) else {
            return;
        };
        if databases.host.is_registered(&id) {
            return;
        }
        let lock = databases.lock(name);
        let _guard = lock.lock().await;
        if databases.host.is_registered(&id)
            || !matches!(self.state_store.get_database(name), Ok(Some(_)))
        {
            return;
        }
        let path = databases.database_path(name);
        let host = databases.host.clone();
        let registered = tokio::task::spawn_blocking(move || host.register(id, path)).await;
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
        self.refresh_database_routes();
        match self.state_store.all_database_grants() {
            Ok(grants) => {
                for (scope, grant) in grants {
                    databases.host.set_remote_grants(
                        &scope,
                        grant
                            .databases
                            .iter()
                            .filter_map(|id| DatabaseId::parse(id).ok())
                            .collect(),
                        &grant.key,
                    );
                }
            }
            Err(error) => tracing::error!(%error, "Failed to read database grants"),
        }
        let reaper = databases.host.spawn_reaper();
        databases.tasks.lock().push(reaper.abort_handle());
        self.spawn_database_peer_listener();
        let scheduler = snapshots::spawn_scheduler(self.clone());
        databases.tasks.lock().push(scheduler.abort_handle());
        Ok(())
    }

    /// Stops serving databases before this process exits, for example when
    /// a reloaded process has taken over: closes the listeners, so new
    /// connections reach the successor (which knows every current token),
    /// stops snapshots, and releases streams and file locks.
    pub(crate) async fn stop_databases(&self) {
        for task in self.databases.tasks.lock().drain(..) {
            task.abort();
        }
        self.databases.host.close_all().await;
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
                        tako_database::http::serve(
                            state.databases.host.clone(),
                            listener,
                            tako_database::http::Access::Apps,
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
        let task = tokio::spawn(async move {
            let address = SocketAddr::new(IpAddr::V6(proxy.address), port);
            loop {
                match bind_reuseport(address) {
                    Ok(listener) => {
                        tracing::info!(%address, "Serving databases to network servers");
                        let proxy = proxy.clone();
                        tako_database::http::serve(
                            host.clone(),
                            listener,
                            tako_database::http::Access::Peers {
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
        let result = match command {
            Command::CreateDatabase {
                name,
                generation,
                backup_key_base64,
                backup,
            } => {
                self.create_database(name, generation, backup_key_base64, backup)
                    .await
            }
            Command::DeleteDatabase {
                name,
                keep_snapshots,
            } => self.delete_database(&name, keep_snapshots).await,
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
            Command::ListDatabaseSnapshots { name } => snapshots::list_command(self, &name).await,
            Command::RestoreDatabase {
                name,
                snapshot,
                backup_key_base64,
            } => snapshots::restore(self, &name, snapshot.as_deref(), backup_key_base64).await,
            Command::QueryDatabase { name, sql } => self.query_database(&name, sql).await,
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
        let database = DatabaseId::parse(&id)?;
        let path = self.databases.database_path(&id);
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
        let create_path = path.clone();
        let created = tokio::task::spawn_blocking(move || DatabaseHost::create_file(&create_path))
            .await
            .map_err(|e| e.to_string())
            .and_then(|created| created.map_err(|e| format!("create database file: {e}")));
        if let Err(error) = created {
            undo(false);
            return Err(error);
        }
        let stored = StoredDatabase {
            name: id.clone(),
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
        let host = self.databases.host.clone();
        let register_id = database.clone();
        let registered = tokio::task::spawn_blocking(move || host.register(register_id, path))
            .await
            .map_err(|e| e.to_string())
            .and_then(|registered| registered.map_err(|e| e.to_string()));
        if let Err(error) = registered {
            undo(true);
            return Err(format!("open database: {error}"));
        }
        // Nothing to snapshot until something is written.
        self.databases.host.clear_changed(&database);
        tracing::info!(database = %id, "Created database");
        Ok(serde_json::to_value(self.database_info(&stored)).expect("info serializes"))
    }

    async fn delete_database(
        &self,
        id: &str,
        keep_snapshots: bool,
    ) -> Result<serde_json::Value, String> {
        let requested = self.hosted_database(id)?;
        let lock = self.databases.lock(id);
        let _guard = lock.lock().await;
        // Another delete and create of the same name may have run while this
        // waited for the lock: act only on the database that was asked for.
        let stored = self.hosted_database(id)?;
        if stored.generation != requested.generation {
            return Err(format!(
                "Database {id} was replaced by a new database of the same name while this delete waited; nothing was deleted."
            ));
        }
        // From here, deploys cannot start binding the database; one already
        // binding it makes the delete wait for it to finish.
        let _deleting = self.databases.start_deleting(id)?;
        let info = self.database_info(&stored);
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
        let database = DatabaseId::parse(id)?;
        // Unregistering can time out on a request in flight; the database is
        // then still hosted, so its grants stay until this succeeds.
        if self.databases.host.is_registered(&database) {
            self.databases
                .host
                .unregister(&database)
                .await
                .map_err(|e| e.to_string())?;
        }
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
            self.databases.host.set_remote_grants(
                &scope,
                remaining
                    .iter()
                    .filter_map(|other| DatabaseId::parse(other).ok())
                    .collect(),
                key.as_deref().unwrap_or_default(),
            );
        }
        self.state_store
            .delete_database(id)
            .map_err(|e| format!("forget database: {e}"))?;
        let dir = self.databases.database_dir(id);
        tokio::task::spawn_blocking(move || std::fs::remove_dir_all(dir))
            .await
            .map_err(|e| e.to_string())?
            .or_else(|e| {
                if e.kind() == std::io::ErrorKind::NotFound {
                    Ok(())
                } else {
                    Err(format!("remove database files: {e}"))
                }
            })?;
        self.databases.backups.lock().remove(id);
        tracing::info!(database = %stored.name, "Deleted database");
        // Without the database's records its snapshots cannot be decrypted;
        // they go with it, unless another copy lives on and shares them.
        let mut warnings = Vec::new();
        if keep_snapshots {
            // A copy left by an unfinished move may still hold the
            // snapshots, which would keep the database's own copy from
            // taking any.
            if let Err(error) = snapshots::release_holder(self, &stored).await {
                warnings.push(error);
            }
            return Ok(serde_json::json!({"deleted": id, "warnings": warnings}));
        }
        match snapshots::delete_all(&stored).await {
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
        let infos: Vec<DatabaseInfo> = stored.iter().map(|db| self.database_info(db)).collect();
        Ok(serde_json::json!({ "databases": infos }))
    }

    pub(crate) fn database_info(&self, stored: &StoredDatabase) -> DatabaseInfo {
        let mut bindings = BTreeSet::new();
        for app in self.app_manager.list_apps() {
            if let Some(app) = self.app_manager.get_app(&app) {
                let config = app.config.read();
                if config
                    .databases
                    .values()
                    .any(|binding| binding.binding.name == stored.name)
                {
                    bindings.insert(config.deployment_id());
                }
            }
        }
        let remote_grants = self
            .state_store
            .all_database_grants()
            .unwrap_or_default()
            .into_iter()
            .filter(|(_, grant)| grant.databases.contains(&stored.name))
            .map(|(scope, _)| scope)
            .collect();
        let size_bytes = DatabaseId::parse(&stored.name)
            .ok()
            .and_then(|id| self.databases.host.size_bytes(&id))
            .unwrap_or(0);
        DatabaseInfo {
            name: stored.name.clone(),
            generation: stored.generation.clone(),
            created_at_unix_secs: stored.created_at_unix_secs,
            size_bytes,
            bindings: bindings.into_iter().collect(),
            remote_grants,
            backup: snapshots::status(self, stored),
        }
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
        // Waits for a snapshot in flight, which uploads to the destination
        // it started with.
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
            // The cached status describes the previous destination, and the
            // new one needs a snapshot even if the database is idle.
            self.databases.backups.lock().remove(id);
            if secrets.backup.is_some()
                && let Ok(database) = DatabaseId::parse(id)
            {
                self.databases.host.mark_changed(&database);
            }
        }
        let updated = self.hosted_database(id)?;
        Ok(serde_json::to_value(self.database_info(&updated)).expect("info serializes"))
    }

    fn grant_databases(
        &self,
        app: &str,
        ids: Vec<String>,
        keep_existing: bool,
    ) -> Result<serde_json::Value, String> {
        crate::release::validate_app_name(app)?;
        if !app.contains('/') {
            return Err("Grants name an app environment as `<app>/<env>`.".into());
        }
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
        self.databases.host.set_remote_grants(
            app,
            granted
                .iter()
                .filter_map(|id| DatabaseId::parse(id).ok())
                .collect(),
            key.as_deref().unwrap_or_default(),
        );
        // The deploying administrator hands the key to the app's servers.
        Ok(serde_json::json!({"app": app, "databases": granted, "peer_key": key}))
    }

    /// Whether an app environment uses a database hosted here, through a
    /// binding on this server or a grant for other servers.
    pub(crate) fn app_uses_database(&self, app: &str, id: &str) -> bool {
        let bound = self.app_manager.get_app(app).is_some_and(|app| {
            app.config
                .read()
                .databases
                .values()
                .any(|binding| binding.binding.name == id)
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
        let stored = self.hosted_database(id)?;
        // App deployers may snapshot only databases their app already uses;
        // administrators send no app.
        if let Some(app) = app {
            if !self.app_uses_database(app, id) {
                return Err(format!("{app} does not use database {}.", stored.name));
            }
            // Deploys take only the release snapshot, which is skipped for an
            // unchanged database; other snapshots need administration.
            if reason != tako_core::SnapshotReason::Release {
                return Err(
                    "App deployers may take only the snapshot before a release command.".into(),
                );
            }
        }
        // Before a release, an unchanged database needs no new snapshot, and
        // a database without backups is not an error.
        let release = reason == tako_core::SnapshotReason::Release;
        if release && stored.secrets.backup.is_none() {
            return Ok(serde_json::json!({"skipped": "backups are not configured"}));
        }
        let only_if_changed = release;
        match snapshots::take(self, &stored, reason, only_if_changed).await? {
            Some(snapshot) => Ok(serde_json::json!({ "snapshot": snapshot })),
            None => Ok(serde_json::json!({"skipped": "no changes since the last snapshot"})),
        }
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
        let migrations: Vec<tako_database::Migration> = migrations
            .into_iter()
            .map(|m| tako_database::Migration {
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
        // A copy of what the migrations change, when backups are configured.
        if stored.secrets.backup.is_some() {
            snapshots::take(self, &stored, tako_core::SnapshotReason::Migration, true)
                .await
                .map_err(|e| format!("Snapshot {} before migrations: {e}", stored.name))?;
        }
        let lock = self.databases.lock(id);
        let _guard = lock.lock().await;
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
        let work = snapshots::fresh_work_dir(self, id, "reset")?;
        let empty = work.path.join("empty.sqlite");
        let create = empty.clone();
        let migrations: Vec<tako_database::Migration> = migrations
            .into_iter()
            .map(|m| tako_database::Migration {
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
                    tako_database::migrate::migrate_file(&create, &owner, &migrations, false)
                        .map(Some)
                        .map_err(|e| format!("The migrations failed; {name} was not reset: {e}"))
                }
                _ => Ok(None),
            }
        })
        .await
        .map_err(|e| e.to_string())??;
        let mut warnings =
            snapshots::replace(self, &stored, empty, "database is being reset").await?;
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
}

fn json_value(value: tako_database::Value) -> serde_json::Value {
    match value {
        tako_database::Value::Null => serde_json::Value::Null,
        tako_database::Value::Integer(value) => value.into(),
        tako_database::Value::Float(value) => serde_json::Number::from_f64(value)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null),
        tako_database::Value::Text(value) => value.into(),
        tako_database::Value::Blob(value) => serde_json::json!({"base64": BASE64.encode(value)}),
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
            server: server.into(),
            peer_key: key.map(str::to_owned),
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
        assert!(reason.contains("administrator"));
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
        let connections = databases
            .begin_release("demo/production", &config, requested)
            .unwrap();
        assert_ne!(
            connections["main"].auth_token, "app-token",
            "release commands get their own token"
        );
        let applied = databases.applied.lock();
        let release = &applied["demo/production#release"][0]["main"];
        assert_eq!(release.binding.peer_key.as_deref(), Some("issued"));
    }
}
