use std::path::PathBuf;

mod app_registry;
mod bindings;
mod credentials;
mod device_key;
mod encryption;
mod schema;
mod upgrade;

pub use device_key::load_or_create_device_key;

pub const STATE_SCHEMA_VERSION: i32 = 8;

#[derive(Debug, Clone)]
pub struct PersistedApp {
    pub config: crate::instances::AppConfig,
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
    pub fn path(&self) -> &std::path::Path {
        &self.path
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

    #[cfg(test)]
    pub fn delete_secrets(&self, app: &str) -> Result<(), StateStoreError> {
        let conn = self.open_connection()?;
        conn.execute("DELETE FROM app_secrets WHERE app = ?1;", [app])
            .map_err(StateStoreError::from)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests;
