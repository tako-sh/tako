//! Bounded, disposable redb stores. The host resolves the private app or shared
//! environment directory; requests never select a filesystem path.
use redb::{ConcurrencyMode, Database, ReadableDatabase, ReadableTable, TableDefinition};
use sha2::{Digest, Sha256};
use std::collections::VecDeque;
use std::fs;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use tako_core::{CacheRequest, ErrorCode, SocketError};

pub const FILE_NAME: &str = "cache.redb";
const CAPACITY: u64 = 64 * 1024 * 1024;
const MAX_JSON: usize = 256 * 1024;
const HOT_STORES: usize = 8;
const ENTRIES: TableDefinition<&str, (u64, &str)> = TableDefinition::new("entries");
const EXPIRY: TableDefinition<(u64, &str), ()> = TableDefinition::new("expiry");
const USAGE: TableDefinition<(), u64> = TableDefinition::new("usage");

type DbError = Box<dyn std::error::Error + Send + Sync>;
type FileId = (u64, u64);

/// Keep only the most recent handles. Restores replace whole directories:
/// never serve the old inode after the data tree changes.
#[derive(Default)]
pub struct Caches(Mutex<VecDeque<(PathBuf, FileId, Arc<Database>)>>);

impl Caches {
    fn database(&self, dir: &Path, create: bool) -> Result<Option<Arc<Database>>, DbError> {
        let path = dir.join(FILE_NAME);
        let file_id = || fs::metadata(&path).map(|m| (m.dev(), m.ino()));
        let mut recent = self.0.lock().map_err(|_| "cache registry poisoned")?;
        if let Some(i) = recent.iter().position(|(p, _, _)| p == &path)
            && let Some(entry) = recent.remove(i)
            && file_id().ok() == Some(entry.1)
        {
            let db = Arc::clone(&entry.2);
            recent.push_front(entry);
            return Ok(Some(db));
        }
        // A miss in a never-written store must not create a file or evict a hot handle.
        if recent.len() == HOT_STORES {
            if !create && !path.try_exists()? {
                return Ok(None);
            }
            let idle = recent
                .iter()
                .rposition(|(_, _, db)| Arc::strong_count(db) == 1)
                .ok_or("all cache handles are busy")?;
            recent.remove(idle);
        }
        let file = match fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(create)
            .truncate(false)
            .mode(0o600)
            .open(&path)
        {
            Err(e) if !create && e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            result => result?,
        };
        let meta = file.metadata()?;
        // Rolling upgrades and a concurrent `tako run` may overlap writers.
        let db = Database::builder()
            .set_concurrency_mode(ConcurrencyMode::MultiWriter)
            .set_cache_size(1024 * 1024)
            .create_with_backend(redb::backends::FileBackend::new(file))?;
        // Reopening an evicted handle on a read should not fsync a write.
        match db.begin_read()?.open_table(USAGE) {
            Ok(_) => {}
            Err(redb::TableError::TableDoesNotExist(_)) => {
                let txn = db.begin_write()?;
                txn.open_table(ENTRIES)?;
                txn.open_table(EXPIRY)?;
                txn.open_table(USAGE)?;
                txn.commit()?;
            }
            Err(error) => return Err(error.into()),
        }
        let db = Arc::new(db);
        recent.push_front((path, (meta.dev(), meta.ino()), Arc::clone(&db)));
        Ok(Some(db))
    }

    pub fn reply(&self, dir: &Path, request: CacheRequest) -> tako_core::Response {
        self.reply_at(dir, request, false)
    }

    /// Kept outside app data trees (including shared-data mounts). Only the
    /// host supplies `environment`; hashing also supports long local env names.
    pub fn reply_shared(
        &self,
        host: &Path,
        environment: &str,
        request: CacheRequest,
    ) -> tako_core::Response {
        let dir = host
            .join("shared-cache")
            .join(hex::encode(Sha256::digest(environment.as_bytes())));
        self.reply_at(&dir, request, true)
    }

    fn reply_at(&self, dir: &Path, request: CacheRequest, shared: bool) -> tako_core::Response {
        match self.execute(dir, request, shared) {
            Ok(json) => tako_core::Response::ok(json),
            Err(error) => error.into(),
        }
    }

    fn execute(
        &self,
        dir: &Path,
        request: CacheRequest,
        shared: bool,
    ) -> Result<Option<String>, SocketError> {
        let key = match &request {
            CacheRequest::Get { key }
            | CacheRequest::Delete { key }
            | CacheRequest::Put { key, .. } => key,
        };
        let invalid = |msg| SocketError::new(ErrorCode::InvalidRequest, msg);
        if key.trim().is_empty() || key.len() > 1024 {
            return Err(invalid("cache key must be nonempty and at most 1024 bytes"));
        }
        if let CacheRequest::Put { json, ttl_ms, .. } = &request
            && (*ttl_ms == 0
                || json.len() > MAX_JSON
                || serde_json::from_str::<&serde_json::value::RawValue>(json).is_err())
        {
            return Err(invalid(
                "cache put requires a positive TTL and up to 256 KiB of JSON",
            ));
        }
        let now = || {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64
        };
        let create = matches!(request, CacheRequest::Put { .. });
        (|| -> Result<_, DbError> {
            if shared && create && !dir.is_dir() {
                fs::DirBuilder::new()
                    .recursive(true)
                    .mode(0o700)
                    .create(dir)?;
            }
            match self.database(dir, create)? {
                Some(db) => execute(&db, request, now, CAPACITY),
                None => Ok(None),
            }
        })()
        .map_err(|e| SocketError::new(ErrorCode::Unavailable, format!("cache unavailable: {e}")))
    }
}

// Charge both copies of the key and a fixed overhead so tiny entries are bounded too.
fn weight(key: &str, json: &str) -> u64 {
    (64 + 2 * key.len() + json.len()) as u64
}

fn execute(
    db: &Database,
    request: CacheRequest,
    now: impl FnOnce() -> u64,
    capacity: u64,
) -> Result<Option<String>, DbError> {
    let (key, put) = match &request {
        CacheRequest::Get { key } => {
            let txn = db.begin_read()?;
            let entries = txn.open_table(ENTRIES)?;
            return Ok(entries.get(key.as_str())?.and_then(|entry| {
                let (expiry, json) = entry.value();
                (expiry > now()).then(|| json.to_owned())
            }));
        }
        CacheRequest::Put { key, json, ttl_ms } => (key.as_str(), Some((json.as_str(), *ttl_ms))),
        CacheRequest::Delete { key } => (key.as_str(), None),
    };
    let txn = db.begin_write()?;
    let now = now();
    {
        let mut entries = txn.open_table(ENTRIES)?;
        let mut expiry = txn.open_table(EXPIRY)?;
        let mut usage = txn.open_table(USAGE)?;
        let mut bytes = usage.get(())?.map_or(0, |v| v.value());
        if let Some(old) = entries.remove(key)? {
            let (when, json) = old.value();
            expiry.remove((when, key))?;
            bytes -= weight(key, json);
        }
        if let Some((json, ttl_ms)) = put {
            let when = now.saturating_add(ttl_ms);
            entries.insert(key, (when, json))?;
            expiry.insert((when, key), ())?;
            bytes += weight(key, json);
        }
        // Amortize expired-key cleanup. Under pressure keep evicting the
        // earliest expiry until this transaction fits the budget.
        for n in 0.. {
            let Some((first, _)) = expiry.first()? else {
                break;
            };
            let (when, key) = first.value();
            if bytes <= capacity && (when > now || n >= 64) {
                break;
            }
            if let Some(old) = entries.remove(key)? {
                bytes -= weight(key, old.value().1);
            }
            drop(first);
            expiry.pop_first()?;
        }
        usage.insert((), bytes)?;
    }
    txn.commit()?;
    Ok(None)
}

#[cfg(test)]
mod tests;
