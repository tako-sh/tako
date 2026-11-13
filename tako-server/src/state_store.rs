use std::path::PathBuf;

mod app_registry;
mod bindings;
mod compose;
mod credentials;
mod device_key;
mod drain;
pub(crate) use drain::{DrainLease, wait_for_lease};
mod encryption;
mod schema;
mod upgrade;

pub(crate) use app_registry::load_persisted_releases_read_only;
pub use device_key::load_or_create_device_key;

pub const STATE_SCHEMA_VERSION: i32 = 12;

#[derive(Debug, Clone)]
pub struct PersistedApp {
    pub config: crate::instances::AppConfig,
    pub routes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PersistedRelease {
    pub(crate) app_id: String,
    pub(crate) version: String,
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
    conn: parking_lot::Mutex<Option<rusqlite::Connection>>,
}

impl SqliteStateStore {
    pub fn new(path: PathBuf, encryption_key: [u8; 32]) -> Self {
        Self {
            path,
            encryption_key,
            conn: parking_lot::Mutex::new(None),
        }
    }

    #[cfg(test)]
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    /// Lock the store's cached connection, opening it on first use.
    fn lock_conn(
        &self,
    ) -> Result<parking_lot::MappedMutexGuard<'_, rusqlite::Connection>, StateStoreError> {
        let mut guard = self.conn.lock();
        if guard.is_none() {
            use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
            let file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW)
                .open(&self.path)
                .map_err(|e| StateStoreError::Sqlite(format!("secure state database: {e}")))?;
            file.set_permissions(std::fs::Permissions::from_mode(0o600))
                .map_err(|e| StateStoreError::Sqlite(format!("secure state database: {e}")))?;
            for suffix in ["-wal", "-shm"] {
                let sidecar = PathBuf::from(format!("{}{suffix}", self.path.display()));
                match std::fs::OpenOptions::new()
                    .read(true)
                    .custom_flags(libc::O_NOFOLLOW)
                    .open(sidecar)
                {
                    Ok(file) => file
                        .set_permissions(std::fs::Permissions::from_mode(0o600))
                        .map_err(|e| StateStoreError::Sqlite(e.to_string()))?,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
                    Err(e) => return Err(StateStoreError::Sqlite(e.to_string())),
                }
            }
            *guard = Some(tako_sqlite::open_local(&self.path)?);
        }
        Ok(parking_lot::MutexGuard::map(guard, |conn| {
            conn.as_mut().expect("connection opened above")
        }))
    }

    /// Upsert an encrypted per-app blob row into one of the
    /// `(app, encrypted_data)` tables.
    fn set_encrypted_row(
        &self,
        table: &str,
        app: &str,
        plaintext: &[u8],
    ) -> Result<(), StateStoreError> {
        let encrypted = encryption::encrypt_blob(&self.encryption_key, plaintext)?;
        let conn = self.lock_conn()?;
        conn.execute(
            &format!(
                "INSERT INTO {table} (app, encrypted_data)
                 VALUES (?1, ?2)
                 ON CONFLICT(app) DO UPDATE SET encrypted_data = excluded.encrypted_data;"
            ),
            (app, encrypted),
        )?;
        Ok(())
    }

    /// Read and decrypt a per-app blob row from one of the
    /// `(app, encrypted_data)` tables. Returns `None` when absent.
    fn get_encrypted_row(
        &self,
        table: &str,
        app: &str,
    ) -> Result<Option<Vec<u8>>, StateStoreError> {
        let conn = self.lock_conn()?;
        let blob = match conn.query_row(
            &format!("SELECT encrypted_data FROM {table} WHERE app = ?1;"),
            (app,),
            |row| row.get::<_, Vec<u8>>(0),
        ) {
            Ok(blob) => Some(blob),
            Err(rusqlite::Error::QueryReturnedNoRows) => None,
            Err(e) => return Err(e.into()),
        };
        match blob {
            Some(encrypted) => Ok(Some(encryption::decrypt_blob(
                &self.encryption_key,
                &encrypted,
            )?)),
            None => Ok(None),
        }
    }

    fn delete_row(&self, table: &str, app: &str) -> Result<(), StateStoreError> {
        let conn = self.lock_conn()?;
        conn.execute(&format!("DELETE FROM {table} WHERE app = ?1;"), (app,))?;
        Ok(())
    }

    #[cfg(test)]
    pub fn delete_secrets(&self, app: &str) -> Result<(), StateStoreError> {
        let conn = self.lock_conn()?;
        conn.execute("DELETE FROM app_secrets WHERE app = ?1;", (app,))?;
        Ok(())
    }

    /// Test-only raw SQL escape hatches.
    #[cfg(test)]
    pub fn raw_execute(&self, sql: &str, params: impl rusqlite::Params) {
        let conn = self.lock_conn().expect("open connection");
        conn.execute(sql, params).expect("raw execute");
    }

    #[cfg(test)]
    pub fn raw_execute_batch(&self, sql: &str) {
        let conn = self.lock_conn().expect("open connection");
        conn.execute_batch(sql).expect("raw execute batch");
    }

    #[cfg(test)]
    pub fn raw_query_i64(&self, sql: &str, params: impl rusqlite::Params) -> i64 {
        let conn = self.lock_conn().expect("open connection");
        conn.query_row(sql, params, |row| row.get(0))
            .expect("raw query i64")
    }

    #[cfg(test)]
    pub fn raw_query_blob(&self, sql: &str, params: impl rusqlite::Params) -> Vec<u8> {
        let conn = self.lock_conn().expect("open connection");
        conn.query_row(sql, params, |row| row.get(0))
            .expect("raw query blob")
    }

    /// Collect one string column (by index) across all rows.
    #[cfg(test)]
    pub fn raw_query_strings(&self, sql: &str, column: usize) -> Vec<String> {
        let conn = self.lock_conn().expect("open connection");
        let mut stmt = conn.prepare(sql).expect("raw prepare");
        let rows = stmt
            .query_map([], |row| row.get(column))
            .expect("raw query strings");
        rows.collect::<Result<Vec<_>, _>>()
            .expect("raw query strings")
    }
}

#[cfg(test)]
mod tests;
