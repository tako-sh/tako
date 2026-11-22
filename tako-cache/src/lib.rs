//! Bounded, disposable redb stores. The host resolves the private app or shared
//! environment directory; requests never select a filesystem path.
mod pool;

use pool::{Access, Pool};
use redb::{
    ConcurrencyMode, Database, DatabaseError, ReadOnlyDatabase, ReadableDatabase, ReadableTable,
    TableDefinition, backends::FileBackend,
};
use sha2::{Digest, Sha256};
use std::fs;
use std::os::unix::fs::DirBuilderExt;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};
use tako_core::{CacheRequest, ErrorCode, SocketError};

pub const FILE_NAME: &str = "cache.redb";
const CAPACITY: u64 = 64 * 1024 * 1024;
const MAX_JSON: usize = 256 * 1024;
const WRITERS: usize = 8;
const READERS: usize = 64;
const WRITER_CACHE: usize = 512 * 1024;
const READER_CACHE: usize = 64 * 1024;
const _: () = assert!(WRITERS * WRITER_CACHE + READERS * READER_CACHE <= 8 * 1024 * 1024);
const ENTRIES: TableDefinition<&str, (u64, &str)> = TableDefinition::new("entries");
const EXPIRY: TableDefinition<(u64, &str), ()> = TableDefinition::new("expiry");
const USAGE: TableDefinition<(), u64> = TableDefinition::new("usage");

type DbError = Box<dyn std::error::Error + Send + Sync>;
type Reply = Result<Option<String>, DbError>;

/// Readers follow peer commits without paying the allocator/flush cost of a writer open/close.
#[derive(Default)]
pub struct Caches {
    readers: Pool<ReadOnlyDatabase, READERS>,
    writers: Pool<Database, WRITERS>,
}

fn open_writer(backend: FileBackend) -> Result<Database, DatabaseError> {
    Database::builder()
        .set_concurrency_mode(ConcurrencyMode::MultiWriter)
        .set_cache_size(WRITER_CACHE)
        .create_with_backend(backend)
}

fn open_reader(backend: FileBackend) -> Result<ReadOnlyDatabase, DatabaseError> {
    Database::builder()
        .set_concurrency_mode(ConcurrencyMode::MultiWriter)
        .set_cache_size(READER_CACHE)
        .open_read_only_with_backend(backend)
}

impl Caches {
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
            let path = dir.join(FILE_NAME);
            if let CacheRequest::Get { key } = &request {
                let result = self.readers.with(&path, Access::Read, open_reader, |db| read(db, key, now));
                // An unclean shutdown needs a writer to repair on first open.
                if !matches!(&result, Err(e) if matches!(e.downcast_ref(), Some(DatabaseError::RepairAborted))) {
                    return result;
                }
            }
            let access = if create { Access::Create } else { Access::Update };
            self.writers.with(&path, access, open_writer, |db| execute(db, request, now, CAPACITY))
        })()
        .map_err(|e| SocketError::new(ErrorCode::Unavailable, format!("cache unavailable: {e}")))
    }
}

// Charge both copies of the key and a fixed overhead so tiny entries are bounded too.
fn weight(key: &str, json: &str) -> u64 {
    (64 + 2 * key.len() + json.len()) as u64
}

fn read(db: &impl ReadableDatabase, key: &str, now: impl FnOnce() -> u64) -> Reply {
    // A transaction is a snapshot. Only the handle can be reused across requests.
    let txn = db.begin_read()?;
    let entries = match txn.open_table(ENTRIES) {
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
        result => result?,
    };
    Ok(entries.get(key)?.and_then(|entry| {
        let (expiry, json) = entry.value();
        (expiry > now()).then(|| json.to_owned())
    }))
}

fn execute(
    db: &Database,
    request: CacheRequest,
    now: impl FnOnce() -> u64,
    capacity: u64,
) -> Result<Option<String>, DbError> {
    let (key, put) = match &request {
        CacheRequest::Get { key } => return read(db, key, now),
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
