use crate::instances::AppConfig;
use openssl::symm::{Cipher, decrypt_aead, encrypt_aead};
use rusqlite::OptionalExtension;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tako_core::UpgradeMode;

pub const STATE_SCHEMA_VERSION: i32 = 5;

#[derive(Debug, Clone)]
pub struct PersistedApp {
    pub config: AppConfig,
    pub routes: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum StateStoreError {
    #[error("sqlite error: {0}")]
    Sqlite(String),

    #[error("invalid data: {0}")]
    InvalidData(String),

    #[error("unsupported schema version: {found}")]
    UnsupportedSchemaVersion { found: i32 },
}

impl From<rusqlite::Error> for StateStoreError {
    fn from(e: rusqlite::Error) -> Self {
        StateStoreError::Sqlite(e.to_string())
    }
}

pub struct SqliteStateStore {
    path: PathBuf,
    encryption_key: [u8; 32],
}

impl SqliteStateStore {
    pub fn new(path: PathBuf, encryption_key: [u8; 32]) -> Self {
        Self {
            path,
            encryption_key,
        }
    }

    #[cfg(test)]
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn init(&self) -> Result<(), StateStoreError> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| StateStoreError::Sqlite(format!("create db parent: {e}")))?;
        }

        let conn = self.open_connection()?;
        let version: i32 = conn
            .query_row("PRAGMA user_version;", [], |row| row.get(0))
            .map_err(StateStoreError::from)?;

        if version > STATE_SCHEMA_VERSION {
            return Err(StateStoreError::UnsupportedSchemaVersion { found: version });
        }

        if version == 0 {
            self.initialize_schema(&conn)?;
        } else if version < STATE_SCHEMA_VERSION {
            self.migrate_schema(&conn, version)?;
        } else {
            self.ensure_schema_objects(&conn)?;
            self.ensure_default_rows(&conn)?;
        }

        Ok(())
    }

    pub fn upsert_app(&self, config: &AppConfig, routes: &[String]) -> Result<(), StateStoreError> {
        let conn = self.open_connection()?;
        let tx = conn
            .unchecked_transaction()
            .map_err(StateStoreError::from)?;
        upsert_app_on(&tx, config, routes)?;

        tx.commit().map_err(StateStoreError::from)?;
        Ok(())
    }

    pub fn delete_app(&self, name: &str, environment: &str) -> Result<(), StateStoreError> {
        let conn = self.open_connection()?;
        // Delete secrets for this app to prevent leaking to a future app with the same name.
        let secret_key = format!("{name}/{environment}");
        conn.execute("DELETE FROM app_secrets WHERE app = ?1;", [&secret_key])
            .map_err(StateStoreError::from)?;
        conn.execute("DELETE FROM app_storages WHERE app = ?1;", [&secret_key])
            .map_err(StateStoreError::from)?;
        conn.execute("DELETE FROM app_dns WHERE app = ?1;", [&secret_key])
            .map_err(StateStoreError::from)?;
        conn.execute(
            "DELETE FROM apps WHERE name = ?1 AND environment = ?2;",
            rusqlite::params![name, environment],
        )
        .map_err(StateStoreError::from)?;
        Ok(())
    }

    pub fn load_apps(&self) -> Result<Vec<PersistedApp>, StateStoreError> {
        let conn = self.open_connection()?;

        let mut stmt = conn
            .prepare(
                "SELECT
                    name, environment, version, min_instances, max_instances, source_ip
                 FROM apps
                 ORDER BY name, environment;",
            )
            .map_err(StateStoreError::from)?;

        let mut apps = Vec::new();
        let mut rows = stmt.query([]).map_err(StateStoreError::from)?;

        while let Some(row) = rows.next().map_err(StateStoreError::from)? {
            let name: String = row.get(0).map_err(StateStoreError::from)?;
            let environment: String = row.get(1).map_err(StateStoreError::from)?;
            let version: String = row.get(2).map_err(StateStoreError::from)?;
            let min_instances: i64 = row.get(3).map_err(StateStoreError::from)?;
            let max_instances: i64 = row.get(4).map_err(StateStoreError::from)?;
            let source_ip: String = row.get(5).map_err(StateStoreError::from)?;

            let mut routes_stmt = conn
                .prepare(
                    "SELECT route FROM app_routes
                     WHERE name = ?1 AND environment = ?2
                     ORDER BY route;",
                )
                .map_err(StateStoreError::from)?;
            let routes: Vec<String> = routes_stmt
                .query_map(rusqlite::params![&name, &environment], |r| r.get(0))
                .map_err(StateStoreError::from)?
                .collect::<Result<Vec<String>, _>>()
                .map_err(StateStoreError::from)?;

            let config = AppConfig {
                name,
                environment,
                version,
                min_instances: to_u32(min_instances, "min_instances")?,
                max_instances: to_u32(max_instances, "max_instances")?,
                source_ip: source_ip_from_str(&source_ip)?,
                ..Default::default()
            };

            apps.push(PersistedApp { config, routes });
        }

        Ok(apps)
    }

    pub fn set_server_mode(&self, mode: UpgradeMode) -> Result<(), StateStoreError> {
        let conn = self.open_connection()?;
        conn.execute(
            "UPDATE server_state SET server_mode = ?1 WHERE id = 1;",
            rusqlite::params![server_mode_to_str(mode)],
        )
        .map_err(StateStoreError::from)?;
        Ok(())
    }

    pub fn server_mode(&self) -> Result<UpgradeMode, StateStoreError> {
        let conn = self.open_connection()?;
        let mode_str: Option<String> = conn
            .query_row(
                "SELECT server_mode FROM server_state WHERE id = 1;",
                [],
                |row| row.get(0),
            )
            .optional()
            .map_err(StateStoreError::from)?;

        match mode_str {
            Some(s) => server_mode_from_str(&s),
            None => Ok(UpgradeMode::Normal),
        }
    }

    /// Stale lock threshold: locks older than this are force-acquired.
    const UPGRADE_LOCK_STALE_SECS: i64 = 600; // 10 minutes

    pub fn try_acquire_upgrade_lock(&self, owner: &str) -> Result<bool, StateStoreError> {
        let conn = self.open_connection()?;
        let tx = conn
            .unchecked_transaction()
            .map_err(StateStoreError::from)?;

        let existing: Option<(String, i64)> = tx
            .query_row(
                "SELECT owner, acquired_at_unix_secs FROM upgrade_lock WHERE id = 1;",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(StateStoreError::from)?;

        let now: i64 = tx
            .query_row("SELECT CAST(strftime('%s','now') AS INTEGER);", [], |row| {
                row.get(0)
            })
            .map_err(StateStoreError::from)?;

        let acquired = match existing {
            Some((ref existing_owner, _)) if existing_owner == owner => true,
            Some((_, acquired_at)) if now - acquired_at > Self::UPGRADE_LOCK_STALE_SECS => {
                // Stale lock — force-acquire by replacing it.
                tx.execute(
                    "UPDATE upgrade_lock SET owner = ?1, acquired_at_unix_secs = ?2 WHERE id = 1;",
                    rusqlite::params![owner, now],
                )
                .map_err(StateStoreError::from)?;
                true
            }
            Some(_) => false,
            None => {
                tx.execute(
                    "INSERT INTO upgrade_lock (id, owner, acquired_at_unix_secs)
                     VALUES (1, ?1, CAST(strftime('%s','now') AS INTEGER));",
                    rusqlite::params![owner],
                )
                .map_err(StateStoreError::from)?;
                true
            }
        };

        tx.commit().map_err(StateStoreError::from)?;
        Ok(acquired)
    }

    pub fn release_upgrade_lock(&self, owner: &str) -> Result<bool, StateStoreError> {
        let conn = self.open_connection()?;
        let tx = conn
            .unchecked_transaction()
            .map_err(StateStoreError::from)?;

        let existing: Option<String> = tx
            .query_row("SELECT owner FROM upgrade_lock WHERE id = 1;", [], |row| {
                row.get(0)
            })
            .optional()
            .map_err(StateStoreError::from)?;

        let released = match existing {
            Some(existing) if existing == owner => {
                tx.execute("DELETE FROM upgrade_lock WHERE id = 1;", [])
                    .map_err(StateStoreError::from)?;
                true
            }
            _ => false,
        };

        tx.commit().map_err(StateStoreError::from)?;
        Ok(released)
    }

    pub fn upgrade_lock_owner(&self) -> Result<Option<String>, StateStoreError> {
        let conn = self.open_connection()?;
        conn.query_row("SELECT owner FROM upgrade_lock WHERE id = 1;", [], |row| {
            row.get(0)
        })
        .optional()
        .map_err(StateStoreError::from)
    }

    pub fn set_secrets(
        &self,
        app: &str,
        secrets: &HashMap<String, String>,
    ) -> Result<(), StateStoreError> {
        let json = serde_json::to_vec(secrets)
            .map_err(|e| StateStoreError::InvalidData(format!("serialize secrets: {e}")))?;
        let encrypted = encrypt_blob(&self.encryption_key, &json)?;
        let conn = self.open_connection()?;
        conn.execute(
            "INSERT INTO app_secrets (app, encrypted_data)
             VALUES (?1, ?2)
             ON CONFLICT(app) DO UPDATE SET encrypted_data = excluded.encrypted_data;",
            rusqlite::params![app, encrypted],
        )
        .map_err(StateStoreError::from)?;
        Ok(())
    }

    pub fn get_secrets(&self, app: &str) -> Result<HashMap<String, String>, StateStoreError> {
        let conn = self.open_connection()?;
        let blob: Option<Vec<u8>> = conn
            .query_row(
                "SELECT encrypted_data FROM app_secrets WHERE app = ?1;",
                [app],
                |row| row.get(0),
            )
            .optional()
            .map_err(StateStoreError::from)?;

        match blob {
            Some(encrypted) => {
                let json = decrypt_blob(&self.encryption_key, &encrypted)?;
                serde_json::from_slice(&json)
                    .map_err(|e| StateStoreError::InvalidData(format!("deserialize secrets: {e}")))
            }
            None => Ok(HashMap::new()),
        }
    }

    pub fn set_storages(
        &self,
        app: &str,
        storages: &HashMap<String, tako_core::StorageBinding>,
    ) -> Result<(), StateStoreError> {
        let json = serde_json::to_vec(storages)
            .map_err(|e| StateStoreError::InvalidData(format!("serialize storages: {e}")))?;
        let encrypted = encrypt_blob(&self.encryption_key, &json)?;
        let conn = self.open_connection()?;
        conn.execute(
            "INSERT INTO app_storages (app, encrypted_data)
             VALUES (?1, ?2)
             ON CONFLICT(app) DO UPDATE SET encrypted_data = excluded.encrypted_data;",
            rusqlite::params![app, encrypted],
        )
        .map_err(StateStoreError::from)?;
        Ok(())
    }

    pub fn get_storages(
        &self,
        app: &str,
    ) -> Result<HashMap<String, tako_core::StorageBinding>, StateStoreError> {
        let conn = self.open_connection()?;
        let blob: Option<Vec<u8>> = conn
            .query_row(
                "SELECT encrypted_data FROM app_storages WHERE app = ?1;",
                [app],
                |row| row.get(0),
            )
            .optional()
            .map_err(StateStoreError::from)?;

        match blob {
            Some(encrypted) => {
                let json = decrypt_blob(&self.encryption_key, &encrypted)?;
                serde_json::from_slice(&json)
                    .map_err(|e| StateStoreError::InvalidData(format!("deserialize storages: {e}")))
            }
            None => Ok(HashMap::new()),
        }
    }

    pub fn set_dns(&self, app: &str, dns: &tako_core::DnsBinding) -> Result<(), StateStoreError> {
        let json = serde_json::to_vec(dns)
            .map_err(|e| StateStoreError::InvalidData(format!("serialize dns: {e}")))?;
        let encrypted = encrypt_blob(&self.encryption_key, &json)?;
        let conn = self.open_connection()?;
        conn.execute(
            "INSERT INTO app_dns (app, encrypted_data)
             VALUES (?1, ?2)
             ON CONFLICT(app) DO UPDATE SET encrypted_data = excluded.encrypted_data;",
            rusqlite::params![app, encrypted],
        )
        .map_err(StateStoreError::from)?;
        Ok(())
    }

    pub fn get_dns(&self, app: &str) -> Result<Option<tako_core::DnsBinding>, StateStoreError> {
        let conn = self.open_connection()?;
        let blob: Option<Vec<u8>> = conn
            .query_row(
                "SELECT encrypted_data FROM app_dns WHERE app = ?1;",
                [app],
                |row| row.get(0),
            )
            .optional()
            .map_err(StateStoreError::from)?;

        match blob {
            Some(encrypted) => {
                let json = decrypt_blob(&self.encryption_key, &encrypted)?;
                serde_json::from_slice(&json)
                    .map(Some)
                    .map_err(|e| StateStoreError::InvalidData(format!("deserialize dns: {e}")))
            }
            None => Ok(None),
        }
    }

    pub fn delete_dns(&self, app: &str) -> Result<(), StateStoreError> {
        let conn = self.open_connection()?;
        conn.execute("DELETE FROM app_dns WHERE app = ?1;", [app])
            .map_err(StateStoreError::from)?;
        Ok(())
    }

    #[cfg(test)]
    pub fn delete_secrets(&self, app: &str) -> Result<(), StateStoreError> {
        let conn = self.open_connection()?;
        conn.execute("DELETE FROM app_secrets WHERE app = ?1;", [app])
            .map_err(StateStoreError::from)?;
        Ok(())
    }

    fn open_connection(&self) -> Result<rusqlite::Connection, StateStoreError> {
        let conn = rusqlite::Connection::open(&self.path).map_err(StateStoreError::from)?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA foreign_keys = ON;
             PRAGMA busy_timeout = 5000;
             PRAGMA temp_store = MEMORY;
             PRAGMA wal_autocheckpoint = 1000;
             PRAGMA journal_size_limit = 67108864;
             PRAGMA trusted_schema = OFF;",
        )
        .map_err(StateStoreError::from)?;
        Ok(conn)
    }

    fn ensure_schema_objects(&self, conn: &rusqlite::Connection) -> Result<(), StateStoreError> {
        self.ensure_schema_objects_on(conn)
    }

    fn initialize_schema(&self, conn: &rusqlite::Connection) -> Result<(), StateStoreError> {
        let tx = conn
            .unchecked_transaction()
            .map_err(StateStoreError::from)?;
        self.ensure_schema_objects_on(&tx)?;
        self.ensure_default_rows_on(&tx)?;
        tx.execute_batch(&format!("PRAGMA user_version = {STATE_SCHEMA_VERSION};"))
            .map_err(StateStoreError::from)?;
        tx.commit().map_err(StateStoreError::from)?;
        Ok(())
    }

    fn migrate_schema(
        &self,
        conn: &rusqlite::Connection,
        from_version: i32,
    ) -> Result<(), StateStoreError> {
        let tx = conn
            .unchecked_transaction()
            .map_err(StateStoreError::from)?;

        if from_version < 2 {
            tx.execute_batch(
                "CREATE TABLE IF NOT EXISTS app_secrets (
                    app TEXT NOT NULL PRIMARY KEY,
                    encrypted_data BLOB NOT NULL
                );",
            )
            .map_err(StateStoreError::from)?;
        }

        if from_version < 3 {
            tx.execute_batch(
                "CREATE TABLE IF NOT EXISTS app_storages (
                    app TEXT NOT NULL PRIMARY KEY,
                    encrypted_data BLOB NOT NULL
                );",
            )
            .map_err(StateStoreError::from)?;
        }

        if from_version < 4 {
            tx.execute_batch(
                "CREATE TABLE IF NOT EXISTS app_dns (
                    app TEXT NOT NULL PRIMARY KEY,
                    encrypted_data BLOB NOT NULL
                );",
            )
            .map_err(StateStoreError::from)?;
        }

        if from_version < 5 {
            tx.execute_batch("ALTER TABLE apps ADD COLUMN source_ip TEXT NOT NULL DEFAULT 'auto';")
                .map_err(StateStoreError::from)?;
        }

        self.ensure_default_rows_on(&tx)?;
        tx.execute_batch(&format!("PRAGMA user_version = {STATE_SCHEMA_VERSION};"))
            .map_err(StateStoreError::from)?;
        tx.commit().map_err(StateStoreError::from)?;
        Ok(())
    }

    fn ensure_schema_objects_on(&self, conn: &rusqlite::Connection) -> Result<(), StateStoreError> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS apps (
                name TEXT NOT NULL,
                environment TEXT NOT NULL,
                version TEXT NOT NULL,
                min_instances INTEGER NOT NULL,
                max_instances INTEGER NOT NULL,
                source_ip TEXT NOT NULL DEFAULT 'auto',
                PRIMARY KEY (name, environment)
            );

            CREATE TABLE IF NOT EXISTS app_routes (
                name TEXT NOT NULL,
                environment TEXT NOT NULL,
                route TEXT NOT NULL,
                PRIMARY KEY (name, environment, route),
                FOREIGN KEY(name, environment) REFERENCES apps(name, environment) ON DELETE CASCADE
            );

            CREATE TABLE IF NOT EXISTS server_state (
                id INTEGER PRIMARY KEY CHECK(id = 1),
                server_mode TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS upgrade_lock (
                id INTEGER PRIMARY KEY CHECK(id = 1),
                owner TEXT NOT NULL,
                acquired_at_unix_secs INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS app_secrets (
                app TEXT NOT NULL PRIMARY KEY,
                encrypted_data BLOB NOT NULL
            );

            CREATE TABLE IF NOT EXISTS app_storages (
                app TEXT NOT NULL PRIMARY KEY,
                encrypted_data BLOB NOT NULL
            );

            CREATE TABLE IF NOT EXISTS app_dns (
                app TEXT NOT NULL PRIMARY KEY,
                encrypted_data BLOB NOT NULL
            );",
        )
        .map_err(StateStoreError::from)?;
        Ok(())
    }

    fn ensure_default_rows(&self, conn: &rusqlite::Connection) -> Result<(), StateStoreError> {
        self.ensure_default_rows_on(conn)
    }

    fn ensure_default_rows_on(&self, conn: &rusqlite::Connection) -> Result<(), StateStoreError> {
        conn.execute(
            "INSERT INTO server_state (id, server_mode)
             VALUES (1, 'normal')
             ON CONFLICT(id) DO NOTHING;",
            [],
        )
        .map_err(StateStoreError::from)?;

        Ok(())
    }
}

fn encrypt_blob(key: &[u8; 32], plaintext: &[u8]) -> Result<Vec<u8>, StateStoreError> {
    let cipher = Cipher::aes_256_gcm();
    let mut nonce = [0u8; 12];
    openssl::rand::rand_bytes(&mut nonce)
        .map_err(|e| StateStoreError::Sqlite(format!("generate nonce: {e}")))?;
    let mut tag = [0u8; 16];
    let ciphertext = encrypt_aead(cipher, key, Some(&nonce), &[], plaintext, &mut tag)
        .map_err(|e| StateStoreError::Sqlite(format!("encrypt: {e}")))?;
    let mut blob = Vec::with_capacity(12 + 16 + ciphertext.len());
    blob.extend_from_slice(&nonce);
    blob.extend_from_slice(&tag);
    blob.extend_from_slice(&ciphertext);
    Ok(blob)
}

fn decrypt_blob(key: &[u8; 32], blob: &[u8]) -> Result<Vec<u8>, StateStoreError> {
    if blob.len() < 28 {
        return Err(StateStoreError::InvalidData(
            "encrypted blob too short".to_string(),
        ));
    }
    let cipher = Cipher::aes_256_gcm();
    let nonce = &blob[..12];
    let tag = &blob[12..28];
    let ciphertext = &blob[28..];
    decrypt_aead(cipher, key, Some(nonce), &[], ciphertext, tag)
        .map_err(|e| StateStoreError::InvalidData(format!("decrypt secrets: {e}")))
}

/// Load or generate a 256-bit device encryption key.
///
/// On first call, generates a random key and writes it to `path` with 0600
/// permissions. On subsequent calls, reads the existing key from disk.
pub fn load_or_create_device_key(path: &Path) -> Result<[u8; 32], StateStoreError> {
    if path.exists() {
        let key_bytes = std::fs::read(path)
            .map_err(|e| StateStoreError::Sqlite(format!("read device key: {e}")))?;
        if key_bytes.len() != 32 {
            return Err(StateStoreError::InvalidData(format!(
                "device key must be 32 bytes, got {}",
                key_bytes.len()
            )));
        }
        let mut key = [0u8; 32];
        key.copy_from_slice(&key_bytes);
        Ok(key)
    } else {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| StateStoreError::Sqlite(format!("create key dir: {e}")))?;
        }
        let mut key = [0u8; 32];
        openssl::rand::rand_bytes(&mut key)
            .map_err(|e| StateStoreError::Sqlite(format!("generate device key: {e}")))?;
        #[cfg(unix)]
        {
            use std::io::Write;
            use std::os::unix::fs::OpenOptionsExt;
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(path)
                .map_err(|e| StateStoreError::Sqlite(format!("write device key: {e}")))?;
            f.write_all(&key)
                .map_err(|e| StateStoreError::Sqlite(format!("write device key: {e}")))?;
        }
        #[cfg(not(unix))]
        {
            std::fs::write(path, &key)
                .map_err(|e| StateStoreError::Sqlite(format!("write device key: {e}")))?;
        }
        Ok(key)
    }
}

fn upsert_app_on(
    conn: &rusqlite::Connection,
    config: &AppConfig,
    routes: &[String],
) -> Result<(), StateStoreError> {
    conn.execute(
        "INSERT INTO apps (
            name, environment, version, min_instances, max_instances, source_ip
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(name, environment) DO UPDATE SET
            version = excluded.version,
            min_instances = excluded.min_instances,
            max_instances = excluded.max_instances,
            source_ip = excluded.source_ip;",
        rusqlite::params![
            &config.name,
            &config.environment,
            &config.version,
            config.min_instances as i64,
            config.max_instances as i64,
            source_ip_to_str(config.source_ip),
        ],
    )
    .map_err(StateStoreError::from)?;

    conn.execute(
        "DELETE FROM app_routes WHERE name = ?1 AND environment = ?2;",
        rusqlite::params![&config.name, &config.environment],
    )
    .map_err(StateStoreError::from)?;

    for route in routes {
        conn.execute(
            "INSERT INTO app_routes (name, environment, route) VALUES (?1, ?2, ?3);",
            rusqlite::params![&config.name, &config.environment, route],
        )
        .map_err(StateStoreError::from)?;
    }

    Ok(())
}

fn to_u32(value: i64, field: &str) -> Result<u32, StateStoreError> {
    u32::try_from(value).map_err(|_| {
        StateStoreError::InvalidData(format!("field '{field}' out of range for u32: {value}"))
    })
}

fn source_ip_to_str(mode: tako_core::SourceIpMode) -> &'static str {
    match mode {
        tako_core::SourceIpMode::Auto => "auto",
        tako_core::SourceIpMode::Direct => "direct",
        tako_core::SourceIpMode::CloudflareProxy => "cloudflare-proxy",
    }
}

fn source_ip_from_str(value: &str) -> Result<tako_core::SourceIpMode, StateStoreError> {
    match value {
        "auto" => Ok(tako_core::SourceIpMode::Auto),
        "direct" => Ok(tako_core::SourceIpMode::Direct),
        "cloudflare-proxy" => Ok(tako_core::SourceIpMode::CloudflareProxy),
        other => Err(StateStoreError::InvalidData(format!(
            "unsupported source_ip mode '{other}'"
        ))),
    }
}

fn server_mode_to_str(mode: UpgradeMode) -> &'static str {
    match mode {
        UpgradeMode::Normal => "normal",
        UpgradeMode::Upgrading => "upgrading",
    }
}

fn server_mode_from_str(value: &str) -> Result<UpgradeMode, StateStoreError> {
    match value {
        "normal" => Ok(UpgradeMode::Normal),
        "upgrading" => Ok(UpgradeMode::Upgrading),
        other => Err(StateStoreError::InvalidData(format!(
            "unknown server_mode value: {}",
            other
        ))),
    }
}

#[cfg(test)]
mod tests;
