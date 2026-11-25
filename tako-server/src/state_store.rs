use std::path::PathBuf;

mod app_registry;
mod bindings;
mod compose;
mod databases;
pub(crate) use databases::{AppDatabase, DatabaseSecrets, StoredDatabase};
mod credentials;
mod device_key;
mod drain;
pub(crate) use drain::{DrainLease, wait_for_lease};
pub(crate) mod encryption;
mod release_routing;

pub(crate) use release_routing::ReleaseRouting;
mod upgrade;
pub(crate) use upgrade::promotion_ready;

pub(crate) use app_registry::load_persisted_releases_read_only;
pub use device_key::load_or_create_device_key;

const STATE_FORMAT: i32 = 1;

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
    #[error("state store: {0}")]
    Storage(String),

    #[error("invalid data: {0}")]
    InvalidData(String),

    #[error("unsupported schema version: {found}")]
    UnsupportedSchemaVersion { found: i32 },

    /// An app's workflow database did not answer; a retry can succeed.
    #[error("workflow database unavailable: {0}")]
    WorkflowStoreUnavailable(String),
}

impl From<serde_json::Error> for StateStoreError {
    fn from(e: serde_json::Error) -> Self {
        Self::InvalidData(e.to_string())
    }
}
fn storage(e: impl std::fmt::Display) -> StateStoreError {
    StateStoreError::Storage(e.to_string())
}

use redb::{Database, ReadOnlyTable, ReadableDatabase, ReadableTable, Table, TableDefinition};
use std::{
    fs::OpenOptions,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::Path,
    sync::Arc,
};
type Key = (&'static str, &'static str, &'static str);
type Rows<'a> = Table<'a, Key, &'static [u8]>;
const RECORDS: TableDefinition<Key, &[u8]> = TableDefinition::new("state");

fn builder() -> redb::Builder {
    let mut b = Database::builder();
    b.set_cache_size(8 << 20)
        .set_concurrency_mode(redb::ConcurrencyMode::MultiWriter);
    b
}

fn read_records<T>(
    db: &impl ReadableDatabase,
    f: impl FnOnce(&ReadOnlyTable<Key, &[u8]>) -> Result<T, StateStoreError>,
) -> Result<T, StateStoreError> {
    let tx = db.begin_read().map_err(storage)?;
    let rows = tx.open_table(RECORDS).map_err(storage)?;
    f(&rows)
}

fn inspect<T>(
    path: &Path,
    f: impl FnOnce(&ReadOnlyTable<Key, &[u8]>) -> Result<T, StateStoreError>,
) -> Result<T, StateStoreError> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(storage)?;
    let db = builder()
        .open_read_only_with_backend(redb::backends::FileBackend::new(file))
        .map_err(storage)?;
    read_records(&db, f)
}
fn bytes(
    rows: &impl ReadableTable<Key, &'static [u8]>,
    key: (&str, &str, &str),
) -> Result<Option<Vec<u8>>, StateStoreError> {
    Ok(rows.get(key).map_err(storage)?.map(|v| v.value().to_vec()))
}
fn get<T: serde::de::DeserializeOwned>(
    rows: &impl ReadableTable<Key, &'static [u8]>,
    key: (&str, &str, &str),
) -> Result<Option<T>, StateStoreError> {
    rows.get(key)
        .map_err(storage)?
        .map(|v| serde_json::from_slice(v.value()))
        .transpose()
        .map_err(Into::into)
}
fn put(
    rows: &mut Rows<'_>,
    key: (&str, &str, &str),
    value: &impl serde::Serialize,
) -> Result<(), StateStoreError> {
    rows.insert(key, serde_json::to_vec(value)?.as_slice())
        .map_err(storage)?;
    Ok(())
}
fn remove(rows: &mut Rows<'_>, key: (&str, &str, &str)) -> Result<(), StateStoreError> {
    rows.remove(key).map_err(storage)?;
    Ok(())
}
fn scan<T>(
    rows: &impl ReadableTable<Key, &'static [u8]>,
    bucket: &str,
    mut read: impl FnMut(&str, &str, &[u8]) -> Result<T, StateStoreError>,
) -> Result<Vec<T>, StateStoreError> {
    let end = format!("{bucket}\0");
    rows.range((bucket, "", "")..(end.as_str(), "", ""))
        .map_err(storage)?
        .map(|item| {
            let (key, value) = item.map_err(storage)?;
            let (_, id, version) = key.value();
            read(id, version, value.value())
        })
        .collect()
}

pub struct StateStore {
    path: PathBuf,
    encryption_key: [u8; 32],
    db: parking_lot::Mutex<Option<Arc<Database>>>,
    #[cfg(test)]
    pub(crate) reject_reads: std::sync::atomic::AtomicBool,
    #[cfg(test)]
    pub(crate) reject_writes: std::sync::atomic::AtomicBool,
}

impl StateStore {
    pub fn new(path: PathBuf, encryption_key: [u8; 32]) -> Self {
        Self {
            path,
            encryption_key,
            db: Default::default(),
            #[cfg(test)]
            reject_writes: Default::default(),
            #[cfg(test)]
            reject_reads: Default::default(),
        }
    }
    #[cfg(test)]
    pub fn path(&self) -> &Path {
        &self.path
    }
    fn database(&self) -> Result<Arc<Database>, StateStoreError> {
        let mut db = self.db.lock();
        if let Some(db) = db.as_ref() {
            return Ok(db.clone());
        }
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(storage)?
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .custom_flags(libc::O_NOFOLLOW)
            .mode(0o600)
            .open(&self.path)
            .map_err(storage)?;
        if !file.metadata().map_err(storage)?.is_file() {
            return Err(storage("state must be a regular file"));
        }
        file.set_permissions(std::fs::Permissions::from_mode(0o600))
            .map_err(storage)?;
        let opened = Arc::new(
            builder()
                .create_with_backend(redb::backends::FileBackend::new(file))
                .map_err(storage)?,
        );
        *db = Some(opened.clone());
        Ok(opened)
    }
    fn read<T>(
        &self,
        f: impl FnOnce(&ReadOnlyTable<Key, &[u8]>) -> Result<T, StateStoreError>,
    ) -> Result<T, StateStoreError> {
        #[cfg(test)]
        if self.reject_reads.load(std::sync::atomic::Ordering::Relaxed) {
            return Err(storage("injected read failure"));
        }
        read_records(self.database()?.as_ref(), f)
    }
    fn write<T>(
        &self,
        f: impl FnOnce(&mut Rows<'_>) -> Result<T, StateStoreError>,
    ) -> Result<T, StateStoreError> {
        let db = self.database()?;
        let tx = db.begin_write().map_err(storage)?;
        let result = {
            let mut rows = tx.open_table(RECORDS).map_err(storage)?;
            f(&mut rows)?
        };
        #[cfg(test)]
        if self
            .reject_writes
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            return Err(storage("injected commit failure"));
        }
        tx.commit().map_err(storage)?;
        Ok(result)
    }
    pub fn init(&self) -> Result<(), StateStoreError> {
        self.write(|rows| match get::<i32>(rows, ("format", "", ""))? {
            Some(STATE_FORMAT) => Ok(()),
            Some(found) => Err(StateStoreError::UnsupportedSchemaVersion { found }),
            None => {
                put(rows, ("format", "", ""), &STATE_FORMAT)?;
                put(rows, ("mode", "", ""), &tako_core::UpgradeMode::Normal)
            }
        })
    }
    #[cfg(test)]
    fn set_encrypted_row(
        &self,
        bucket: &str,
        id: &str,
        data: &[u8],
    ) -> Result<(), StateStoreError> {
        let blob = encryption::encrypt_blob(&self.encryption_key, data)?;
        self.write(|r| {
            r.insert((bucket, id, ""), blob.as_slice())
                .map_err(storage)?;
            Ok(())
        })
    }
    fn get_encrypted_row(
        &self,
        bucket: &str,
        id: &str,
    ) -> Result<Option<Vec<u8>>, StateStoreError> {
        self.read(|r| bytes(r, (bucket, id, "")))?
            .map(|v| encryption::decrypt_blob(&self.encryption_key, &v))
            .transpose()
    }
    #[cfg(test)]
    pub fn delete_secrets(&self, app: &str) -> Result<(), StateStoreError> {
        self.write(|r| remove(r, ("app_secrets", app, "")))
    }
}

#[cfg(test)]
mod tests;
