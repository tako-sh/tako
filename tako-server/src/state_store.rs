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

impl From<turso::Error> for StateStoreError {
    fn from(e: turso::Error) -> Self {
        StateStoreError::Sqlite(e.to_string())
    }
}

pub(crate) use tako_sqlite::block_on;

pub struct SqliteStateStore {
    path: PathBuf,
    encryption_key: [u8; 32],
    conn: parking_lot::Mutex<Option<turso::Connection>>,
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
    ) -> Result<parking_lot::MappedMutexGuard<'_, turso::Connection>, StateStoreError> {
        let mut guard = self.conn.lock();
        if guard.is_none() {
            let path = self
                .path
                .to_str()
                .ok_or_else(|| StateStoreError::Sqlite("non-UTF-8 state db path".into()))?;
            *guard = Some(block_on(tako_sqlite::open_local(path))?);
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
        block_on(conn.execute(
            &format!(
                "INSERT INTO {table} (app, encrypted_data)
                 VALUES (?1, ?2)
                 ON CONFLICT(app) DO UPDATE SET encrypted_data = excluded.encrypted_data;"
            ),
            (app, encrypted),
        ))?;
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
        let blob: Option<Vec<u8>> = block_on(async {
            let mut rows = conn
                .query(
                    &format!("SELECT encrypted_data FROM {table} WHERE app = ?1;"),
                    (app,),
                )
                .await?;
            match rows.next().await? {
                Some(row) => Ok::<_, StateStoreError>(Some(row.get::<Vec<u8>>(0)?)),
                None => Ok(None),
            }
        })?;
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
        block_on(conn.execute(&format!("DELETE FROM {table} WHERE app = ?1;"), (app,)))?;
        Ok(())
    }

    #[cfg(test)]
    pub fn delete_secrets(&self, app: &str) -> Result<(), StateStoreError> {
        let conn = self.lock_conn()?;
        block_on(conn.execute("DELETE FROM app_secrets WHERE app = ?1;", (app,)))
            .map_err(StateStoreError::from)?;
        Ok(())
    }

    /// Test-only raw SQL escape hatches.
    #[cfg(test)]
    pub fn raw_execute(&self, sql: &str, params: impl turso::IntoParams) {
        let conn = self.lock_conn().expect("open connection");
        block_on(conn.execute(sql, params)).expect("raw execute");
    }

    #[cfg(test)]
    pub fn raw_execute_batch(&self, sql: &str) {
        let conn = self.lock_conn().expect("open connection");
        block_on(conn.execute_batch(sql)).expect("raw execute batch");
    }

    #[cfg(test)]
    pub fn raw_query_i64(&self, sql: &str, params: impl turso::IntoParams) -> i64 {
        let conn = self.lock_conn().expect("open connection");
        block_on(async {
            let mut rows = conn.query(sql, params).await.expect("raw query");
            let row = rows
                .next()
                .await
                .expect("raw row")
                .expect("no row returned");
            row.get::<i64>(0).expect("i64 column")
        })
    }

    #[cfg(test)]
    pub fn raw_query_blob(&self, sql: &str, params: impl turso::IntoParams) -> Vec<u8> {
        let conn = self.lock_conn().expect("open connection");
        block_on(async {
            let mut rows = conn.query(sql, params).await.expect("raw query");
            let row = rows
                .next()
                .await
                .expect("raw row")
                .expect("no row returned");
            row.get::<Vec<u8>>(0).expect("blob column")
        })
    }

    /// Collect one string column (by index) across all rows.
    #[cfg(test)]
    pub fn raw_query_strings(&self, sql: &str, column: usize) -> Vec<String> {
        let conn = self.lock_conn().expect("open connection");
        block_on(async {
            let mut rows = conn.query(sql, ()).await.expect("raw query");
            let mut out = Vec::new();
            while let Some(row) = rows.next().await.expect("raw row") {
                out.push(row.get::<String>(column).expect("string column"));
            }
            out
        })
    }
}

#[cfg(test)]
mod tests;
