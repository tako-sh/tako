use parking_lot::Mutex;
use rusqlite::{Connection, OptionalExtension};
use serde::{Serialize, de::DeserializeOwned};
use std::{io, path::Path, sync::Arc};

pub(crate) fn error(e: impl std::fmt::Display) -> io::Error {
    io::Error::other(e.to_string())
}
pub(crate) fn encode<T: Serialize>(value: &T) -> io::Result<Vec<u8>> {
    postcard::to_allocvec(value).map_err(error)
}
pub(crate) fn decode<T: DeserializeOwned>(value: &[u8]) -> io::Result<T> {
    let (value, tail) = postcard::take_from_bytes(value).map_err(error)?;
    if !tail.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "trailing data"));
    }
    Ok(value)
}

#[derive(Clone)]
pub(crate) struct Disk(Arc<Mutex<Connection>>);

impl Disk {
    pub(crate) fn open(path: &Path, schema: &str) -> io::Result<Self> {
        let conn = Connection::open(path).map_err(error)?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;
            CREATE TABLE IF NOT EXISTS meta(key TEXT PRIMARY KEY, value BLOB NOT NULL) WITHOUT ROWID;")
            .map_err(error)?;
        conn.execute_batch(schema).map_err(error)?;
        Ok(Self(Arc::new(Mutex::new(conn))))
    }

    pub(crate) async fn run<T: Send + 'static>(
        &self,
        f: impl FnOnce(&mut Connection) -> io::Result<T> + Send + 'static,
    ) -> io::Result<T> {
        let disk = self.clone();
        tokio::task::spawn_blocking(move || f(&mut disk.0.lock()))
            .await
            .map_err(error)?
    }
}

pub(crate) fn get<T: DeserializeOwned>(conn: &Connection, key: &str) -> io::Result<Option<T>> {
    let bytes: Option<Vec<u8>> = conn
        .query_row("SELECT value FROM meta WHERE key=?1", [key], |row| {
            row.get(0)
        })
        .optional()
        .map_err(error)?;
    bytes.as_deref().map(decode).transpose()
}

pub(crate) fn set<T: Serialize>(conn: &Connection, key: &str, value: &T) -> io::Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO meta VALUES (?1,?2)",
        (key, encode(value)?),
    )
    .map_err(error)?;
    Ok(())
}
