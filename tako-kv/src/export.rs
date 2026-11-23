//! KV export files: what `tako kv export` downloads, `tako kv import`
//! loads, and a snapshot holds before it is compressed and encrypted.
//!
//! An export is a redb file of its own, written from one consistent read
//! of a store. Its layout is independent of the store's internal tables,
//! and versioned, so a newer Tako still reads older exports:
//!
//! - `entries`: key → `(expires_at_ms, json)`, with 0 for no expiry;
//! - `meta`: `format` (1), `created_at_ms` and `keys`.

use std::path::{Path, PathBuf};

use redb::{Database, ReadableDatabase, ReadableTable, ReadableTableMetadata, TableDefinition};

use crate::store::{Stats, StoreError, storage, weight};

pub const FORMAT_VERSION: u64 = 1;

const ENTRIES: TableDefinition<&str, (u64, &[u8])> = TableDefinition::new("entries");
const META: TableDefinition<&str, u64> = TableDefinition::new("meta");

type Result<T, E = StoreError> = std::result::Result<T, E>;

/// Writes an export file. Nothing appears at the destination until
/// [`Writer::finish`] commits it.
pub struct Writer {
    temp: PathBuf,
    path: PathBuf,
    db: Option<Database>,
    txn: Option<redb::WriteTransaction>,
    stats: Stats,
}

impl Writer {
    pub fn create(path: &Path) -> Result<Self> {
        let temp = path.with_extension("partial");
        let _ = std::fs::remove_file(&temp);
        let db = Database::create(&temp).map_err(storage)?;
        let txn = db.begin_write().map_err(storage)?;
        Ok(Self {
            temp,
            path: path.to_owned(),
            db: Some(db),
            txn: Some(txn),
            stats: Stats::default(),
        })
    }

    pub fn insert(&mut self, key: &str, expires_at: u64, json: &[u8]) -> Result<()> {
        let txn = self.txn.as_ref().expect("open until finished");
        txn.open_table(ENTRIES)
            .map_err(storage)?
            .insert(key, (expires_at, json))
            .map_err(storage)?;
        self.stats.keys += 1;
        self.stats.bytes += weight(key, json.len());
        if expires_at != 0 {
            self.stats.expires += 1;
        }
        Ok(())
    }

    /// Commits the file and moves it into place.
    pub fn finish(mut self, now: u64) -> Result<Stats> {
        let txn = self.txn.take().expect("open until finished");
        {
            // Created even when empty, so an empty export still reads.
            txn.open_table(ENTRIES).map_err(storage)?;
            let mut meta = txn.open_table(META).map_err(storage)?;
            meta.insert("format", FORMAT_VERSION).map_err(storage)?;
            meta.insert("created_at_ms", now).map_err(storage)?;
            meta.insert("keys", self.stats.keys).map_err(storage)?;
        }
        txn.commit().map_err(storage)?;
        // Closed before it is moved into place.
        drop(self.db.take());
        std::fs::rename(&self.temp, &self.path).map_err(storage)?;
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&self.path, std::fs::Permissions::from_mode(0o600));
        }
        Ok(self.stats)
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        if self.txn.is_some() {
            let _ = std::fs::remove_file(&self.temp);
        }
    }
}

/// Reads an export file.
pub struct Reader {
    _db: Database,
    entries: redb::ReadOnlyTable<&'static str, (u64, &'static [u8])>,
    keys: u64,
}

fn invalid(message: impl Into<String>) -> StoreError {
    StoreError::Invalid(message.into())
}

impl Reader {
    pub fn open(path: &Path) -> Result<Self> {
        let not_export = || invalid("The file is not a Tako KV export.");
        let db = Database::open(path).map_err(|_| not_export())?;
        let txn = db.begin_read().map_err(storage)?;
        let meta = txn.open_table(META).map_err(|_| not_export())?;
        let format = meta
            .get("format")
            .map_err(storage)?
            .map(|v| v.value())
            .ok_or_else(not_export)?;
        if format != FORMAT_VERSION {
            return Err(invalid(format!(
                "The export uses format {format}; this Tako reads format {FORMAT_VERSION}. Upgrade Tako to import it."
            )));
        }
        let keys = meta.get("keys").map_err(storage)?.map_or(0, |v| v.value());
        let entries = txn.open_table(ENTRIES).map_err(|_| not_export())?;
        Ok(Self {
            _db: db,
            entries,
            keys,
        })
    }

    /// Keys the export recorded.
    pub fn keys(&self) -> u64 {
        self.keys
    }

    /// Checks every entry against the key and value limits, and that each
    /// value is JSON, before anything is replaced.
    pub fn check(&self) -> Result<()> {
        let mut count = 0;
        for entry in self.entries()? {
            let (key, _, json) = entry?;
            let request = tako_core::KvRequest::Put {
                key,
                json: String::from_utf8(json).map_err(|_| invalid("An export value is not UTF-8 JSON."))?,
                ttl_ms: None,
            };
            request
                .validate()
                .map_err(|error| invalid(format!("Key '{}': {error}", request.key())))?;
            count += 1;
        }
        if count != self.entries.len().map_err(storage)? {
            return Err(invalid("The export is truncated."));
        }
        Ok(())
    }

    /// Every entry: key, absolute expiry (0 for none) and value.
    pub fn entries(&self) -> Result<impl Iterator<Item = Result<(String, u64, Vec<u8>)>> + '_> {
        let iter = self.entries.iter().map_err(storage)?;
        Ok(iter.map(|item| {
            let (key, value) = item.map_err(storage)?;
            let (expires_at, json) = value.value();
            Ok((key.value().to_owned(), expires_at, json.to_vec()))
        }))
    }
}
