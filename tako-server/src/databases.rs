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
    DatabaseAccess, DatabaseBinding, DatabaseConnection, DatabaseInfo, DatabaseQueryResult,
    Response, StorageBinding, StorageProvider,
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
    native: RwLock<Option<NativeContext>>,
    pub(crate) backups: Mutex<HashMap<String, snapshots::BackupState>>,
    transfers: Mutex<transfer::Transfers>,
    locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

#[derive(Clone)]
struct NativeContext {
    server_id: String,
    proxy: Arc<crate::native_proxy::NativeProxy>,
}

impl Databases {
    pub(crate) fn new(data_dir: &Path) -> Self {
        Self {
            host: DatabaseHost::new(HostConfig::default()),
            root: data_dir.join("databases"),
            app_port: app_port(),
            peer_port: env_port("TAKO_DATABASE_PEER_PORT", tako_core::DATABASE_PEER_PORT),
            native: RwLock::new(None),
            backups: Mutex::new(HashMap::new()),
            transfers: Mutex::new(transfer::Transfers::default()),
            locks: Mutex::new(HashMap::new()),
        }
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
        self.native.read().as_ref().map(|n| n.server_id.clone())
    }

    /// The connections app processes receive for their bindings.
    pub(crate) fn connections(
        &self,
        bindings: &HashMap<String, AppDatabase>,
    ) -> HashMap<String, DatabaseConnection> {
        connections(bindings)
    }

    fn home(&self, binding: &DatabaseBinding) -> Home {
        let native = self.native.read().clone();
        match native {
            Some(native) if native.server_id == binding.server => Home::Local,
            Some(native) => match native.proxy.server_address(&binding.server) {
                Some(address) => Home::Remote {
                    upstream: SocketAddr::new(address, self.peer_port),
                },
                None => Home::Unavailable {
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
        let mut seen = BTreeSet::new();
        let mut bindings = Vec::new();
        for set in sets {
            for binding in set.values() {
                if seen.insert(binding.token.clone()) {
                    bindings.push(Binding {
                        token_hash: tako_database::hash_token(&binding.token),
                        database: match DatabaseId::parse(&binding.binding.id) {
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
            tako_core::validate_database_id(&binding.id)?;
            let local = match &own {
                Some(own) => own == &binding.server,
                None => true,
            };
            if local {
                let id = DatabaseId::parse(&binding.id)?;
                if !self.host.is_registered(&id) {
                    return Err(format!(
                        "Database binding `{name}` names database {} that is not hosted on this server.",
                        binding.id
                    ));
                }
            } else if binding.access != DatabaseAccess::Network {
                return Err(format!(
                    "Database binding `{name}` names a database on another server; set `access = \"network\"` on the database to allow that."
                ));
            }
            let token = previous
                .get(&name)
                .filter(|existing| existing.binding.id == binding.id)
                .map(|existing| existing.token.clone())
                .unwrap_or_else(tako_database::generate_token);
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
    /// Registers hosted databases and bindings and starts serving them. Only
    /// the primary process runs this; a promoted standby runs it on promotion.
    pub(crate) async fn start_databases(
        self: &Arc<Self>,
        native: Option<(String, Arc<crate::native_proxy::NativeProxy>)>,
    ) -> Result<(), String> {
        let databases = &self.databases;
        if let Some((server_id, proxy)) = native {
            *databases.native.write() = Some(NativeContext { server_id, proxy });
        }
        create_private_dir(&databases.root)?;
        let stored = self
            .state_store
            .list_databases()
            .map_err(|e| format!("read databases: {e}"))?;
        for database in stored {
            let id = DatabaseId::parse(&database.id)?;
            let path = databases.database_path(&database.id);
            let host = databases.host.clone();
            let registered = tokio::task::spawn_blocking(move || host.register(id, path)).await;
            match registered {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    tracing::error!(database = %database.id, %error, "Failed to open database");
                }
                Err(error) => {
                    tracing::error!(database = %database.id, %error, "Failed to open database")
                }
            }
        }
        self.refresh_database_routes();
        match self.state_store.all_database_grants() {
            Ok(grants) => {
                for (scope, ids) in grants {
                    databases.host.set_remote_grants(
                        &scope,
                        ids.iter()
                            .filter_map(|id| DatabaseId::parse(id).ok())
                            .collect(),
                    );
                }
            }
            Err(error) => tracing::error!(%error, "Failed to read database grants"),
        }
        // Detached; the reaper stops once the host is dropped.
        drop(databases.host.spawn_reaper());
        self.spawn_database_app_listener();
        self.spawn_database_peer_listener();
        snapshots::spawn_scheduler(self.clone());
        Ok(())
    }

    /// Re-resolves where each binding's database lives. Runs at startup and
    /// periodically, since other servers' addresses come from network state.
    pub(crate) fn refresh_database_routes(&self) {
        for app in self.app_manager.list_apps() {
            if let Some(app) = self.app_manager.get_app(&app) {
                let config = app.config.read().clone();
                self.databases
                    .apply_bindings(&config.deployment_id(), &[&config.databases]);
            }
        }
    }

    fn spawn_database_app_listener(self: &Arc<Self>) {
        let state = self.clone();
        tokio::spawn(async move {
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
    }

    fn spawn_database_peer_listener(self: &Arc<Self>) {
        let Some(native) = self.databases.native.read().clone() else {
            return;
        };
        let port = self.databases.peer_port;
        let host = self.databases.host.clone();
        tokio::spawn(async move {
            let address = SocketAddr::new(IpAddr::V6(native.proxy.address), port);
            loop {
                match bind_reuseport(address) {
                    Ok(listener) => {
                        tracing::info!(%address, "Serving databases to network servers");
                        let proxy = native.proxy.clone();
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
    }

    pub(crate) async fn handle_database_command(&self, command: tako_core::Command) -> Response {
        use tako_core::Command;
        let result = match command {
            Command::CreateDatabase {
                id,
                name,
                backup_key_base64,
                backup,
            } => {
                self.create_database(id, name, backup_key_base64, backup)
                    .await
            }
            Command::DeleteDatabase { id } => self.delete_database(&id).await,
            Command::DatabaseStatus { id } => self.database_status(id.as_deref()),
            Command::ConfigureDatabaseBackup { id, backup } => {
                self.configure_database_backup(&id, backup)
            }
            Command::GrantDatabases { app, databases } => self.grant_databases(&app, databases),
            Command::SnapshotDatabase { id, reason, app } => {
                self.snapshot_database_command(&id, reason, app.as_deref())
                    .await
            }
            Command::ListDatabaseSnapshots { id } => snapshots::list_command(self, &id).await,
            Command::RestoreDatabase {
                id,
                snapshot,
                backup_key_base64,
            } => snapshots::restore(self, &id, snapshot.as_deref(), backup_key_base64).await,
            Command::QueryDatabase { id, sql } => self.query_database(&id, sql).await,
            Command::BeginDatabaseImport {
                id,
                size_bytes,
                sha256,
            } => transfer::begin_import(self, &id, size_bytes, &sha256).await,
            Command::DatabaseImportChunk {
                id,
                upload_id,
                offset,
                data_base64,
            } => transfer::import_chunk(self, &id, &upload_id, offset, &data_base64).await,
            Command::FinishDatabaseImport { id, upload_id } => {
                transfer::finish_import(self, &id, &upload_id).await
            }
            Command::BeginDatabaseExport { id } => transfer::begin_export(self, &id).await,
            Command::DatabaseExportChunk {
                id,
                export_id,
                offset,
            } => transfer::export_chunk(self, &id, &export_id, offset).await,
            Command::FinishDatabaseExport { id, export_id } => {
                transfer::finish_export(self, &id, &export_id)
            }
            _ => Err("not a database command".to_string()),
        };
        match result {
            Ok(data) => Response::Ok { data },
            Err(message) => Response::error(message),
        }
    }

    pub(crate) fn hosted_database(&self, id: &str) -> Result<StoredDatabase, String> {
        tako_core::validate_database_id(id)?;
        self.state_store
            .get_database(id)
            .map_err(|e| format!("read database: {e}"))?
            .ok_or_else(|| format!("Database {id} is not hosted on this server."))
    }

    async fn create_database(
        &self,
        id: String,
        name: String,
        backup_key_base64: String,
        backup: Option<StorageBinding>,
    ) -> Result<serde_json::Value, String> {
        tako_core::validate_database_id(&id)?;
        tako_core::validate_database_name(&name)?;
        decode_backup_key(&backup_key_base64)?;
        if let Some(backup) = &backup {
            validate_backup_storage(backup)?;
        }
        if self
            .state_store
            .get_database(&id)
            .map_err(|e| e.to_string())?
            .is_some()
        {
            return Err(format!("Database {id} already exists on this server."));
        }
        let path = self.databases.database_path(&id);
        create_private_dir(&self.databases.root)?;
        create_private_dir(&self.databases.database_dir(&id))?;
        let create_path = path.clone();
        tokio::task::spawn_blocking(move || DatabaseHost::create_file(&create_path))
            .await
            .map_err(|e| e.to_string())?
            .map_err(|e| format!("create database file: {e}"))?;
        let stored = StoredDatabase {
            id: id.clone(),
            name,
            created_at_unix_secs: now_unix_secs(),
            secrets: DatabaseSecrets {
                backup_key_base64,
                backup,
            },
        };
        if let Err(error) = self.state_store.create_database(&stored) {
            let _ = std::fs::remove_dir_all(self.databases.database_dir(&id));
            return Err(format!("record database: {error}"));
        }
        let host = self.databases.host.clone();
        let register_id = DatabaseId::parse(&id)?;
        tokio::task::spawn_blocking(move || host.register(register_id, path))
            .await
            .map_err(|e| e.to_string())?
            .map_err(|e| e.to_string())?;
        tracing::info!(database = %id, name = %stored.name, "Created database");
        Ok(serde_json::to_value(self.database_info(&stored)).expect("info serializes"))
    }

    async fn delete_database(&self, id: &str) -> Result<serde_json::Value, String> {
        let stored = self.hosted_database(id)?;
        let info = self.database_info(&stored);
        if !info.bindings.is_empty() || !info.remote_grants.is_empty() {
            let users: Vec<_> = info
                .bindings
                .iter()
                .chain(info.remote_grants.iter())
                .cloned()
                .collect();
            return Err(format!(
                "Database {} is still used by {}. Remove its bindings and deploy those apps first.",
                stored.name,
                users.join(", ")
            ));
        }
        let lock = self.databases.lock(id);
        let _guard = lock.lock().await;
        let database = DatabaseId::parse(id)?;
        if self.databases.host.is_registered(&database) {
            self.databases
                .host
                .unregister(&database)
                .await
                .map_err(|e| e.to_string())?;
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
        tracing::info!(database = %id, name = %stored.name, "Deleted database");
        Ok(serde_json::json!({"deleted": id}))
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
                    .any(|binding| binding.binding.id == stored.id)
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
            .filter(|(_, ids)| ids.contains(&stored.id))
            .map(|(scope, _)| scope)
            .collect();
        let size_bytes = DatabaseId::parse(&stored.id)
            .ok()
            .and_then(|id| self.databases.host.size_bytes(&id))
            .unwrap_or(0);
        DatabaseInfo {
            id: stored.id.clone(),
            name: stored.name.clone(),
            created_at_unix_secs: stored.created_at_unix_secs,
            size_bytes,
            bindings: bindings.into_iter().collect(),
            remote_grants,
            backup: snapshots::status(self, stored),
        }
    }

    fn configure_database_backup(
        &self,
        id: &str,
        backup: Option<StorageBinding>,
    ) -> Result<serde_json::Value, String> {
        let stored = self.hosted_database(id)?;
        if let Some(backup) = &backup {
            validate_backup_storage(backup)?;
        }
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
            // The cached status describes the previous destination.
            self.databases.backups.lock().remove(id);
        }
        let updated = self.hosted_database(id)?;
        Ok(serde_json::to_value(self.database_info(&updated)).expect("info serializes"))
    }

    fn grant_databases(&self, app: &str, ids: Vec<String>) -> Result<serde_json::Value, String> {
        crate::release::validate_app_name(app)?;
        if !app.contains('/') {
            return Err("Grants name an app environment as `<app>/<env>`.".into());
        }
        let mut granted = BTreeSet::new();
        for id in ids {
            self.hosted_database(&id)?;
            granted.insert(id);
        }
        self.state_store
            .set_database_grants(app, &granted)
            .map_err(|e| format!("store grants: {e}"))?;
        self.databases.host.set_remote_grants(
            app,
            granted
                .iter()
                .filter_map(|id| DatabaseId::parse(id).ok())
                .collect(),
        );
        Ok(serde_json::json!({"app": app, "databases": granted}))
    }

    /// Whether an app environment uses a database hosted here, through a
    /// binding on this server or a grant for other servers.
    pub(crate) fn app_uses_database(&self, app: &str, id: &str) -> bool {
        let bound = self.app_manager.get_app(app).is_some_and(|app| {
            app.config
                .read()
                .databases
                .values()
                .any(|binding| binding.binding.id == id)
        });
        bound
            || self
                .state_store
                .all_database_grants()
                .unwrap_or_default()
                .get(app)
                .is_some_and(|ids| ids.contains(id))
    }

    async fn snapshot_database_command(
        &self,
        id: &str,
        reason: tako_core::SnapshotReason,
        app: Option<&str>,
    ) -> Result<serde_json::Value, String> {
        let stored = self.hosted_database(id)?;
        if let Some(app) = app {
            if !self.app_uses_database(app, id) {
                return Err(format!("{app} does not use database {}.", stored.name));
            }
            // Before a release, an unchanged database needs no new snapshot,
            // and a database without backups is not an error.
            if stored.secrets.backup.is_none() {
                return Ok(serde_json::json!({"skipped": "backups are not configured"}));
            }
        }
        let only_if_changed = app.is_some();
        match snapshots::take(self, &stored, reason, only_if_changed).await? {
            Some(snapshot) => Ok(serde_json::json!({ "snapshot": snapshot })),
            None => Ok(serde_json::json!({"skipped": "no changes since the last snapshot"})),
        }
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
