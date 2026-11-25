use redb::{
    Database, ReadTransaction, ReadableDatabase, ReadableTable, TableDefinition, WriteTransaction,
};
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

pub(crate) const CACHE_BYTES: usize = 8 * 1024 * 1024;

pub(crate) type Records = TableDefinition<'static, &'static str, &'static [u8]>;
pub(crate) const META: Records = TableDefinition::new("meta");

pub(crate) trait RecordReader {
    fn records(
        &self,
        table: Records,
    ) -> io::Result<impl ReadableTable<&'static str, &'static [u8]> + '_>;
}
impl RecordReader for ReadTransaction {
    fn records(
        &self,
        table: Records,
    ) -> io::Result<impl ReadableTable<&'static str, &'static [u8]> + '_> {
        self.open_table(table).map_err(error)
    }
}
impl RecordReader for WriteTransaction {
    fn records(
        &self,
        table: Records,
    ) -> io::Result<impl ReadableTable<&'static str, &'static [u8]> + '_> {
        self.open_table(table).map_err(error)
    }
}

pub(crate) fn get<T: DeserializeOwned>(tx: &impl RecordReader, key: &str) -> io::Result<Option<T>> {
    lookup(tx, META, key)
}

pub(crate) fn lookup<T: DeserializeOwned>(
    tx: &impl RecordReader,
    table: Records,
    key: &str,
) -> io::Result<Option<T>> {
    tx.records(table)?
        .get(key)
        .map_err(error)?
        .map(|data| decode(data.value()))
        .transpose()
}

pub(crate) fn put<T: Serialize>(
    tx: &WriteTransaction,
    table: Records,
    key: &str,
    value: &T,
) -> io::Result<()> {
    tx.open_table(table)
        .map_err(error)?
        .insert(key, encode(value)?.as_slice())
        .map_err(error)?;
    Ok(())
}
pub(crate) fn set<T: Serialize>(tx: &WriteTransaction, key: &str, value: &T) -> io::Result<()> {
    put(tx, META, key, value)
}

#[derive(Clone)]
pub(crate) struct Disk(Arc<Database>);

impl Disk {
    pub(crate) fn open(
        path: &Path,
        initialize: impl FnOnce(&WriteTransaction) -> io::Result<()>,
    ) -> io::Result<Self> {
        let db = Database::builder()
            .set_cache_size(CACHE_BYTES)
            .create(path)
            .map_err(error)?;
        let disk = Self(Arc::new(db));
        disk.write(initialize)?;
        Ok(disk)
    }

    pub(crate) fn read<T>(
        &self,
        f: impl FnOnce(&ReadTransaction) -> io::Result<T>,
    ) -> io::Result<T> {
        let tx = self.0.begin_read().map_err(error)?;
        f(&tx)
    }

    pub(crate) fn write<T>(
        &self,
        f: impl FnOnce(&WriteTransaction) -> io::Result<T>,
    ) -> io::Result<T> {
        let tx = self.0.begin_write().map_err(error)?;
        let result = f(&tx)?;
        tx.commit().map_err(error)?;
        Ok(result)
    }

    pub(crate) async fn run<T: Send + 'static>(
        &self,
        f: impl FnOnce(&Disk) -> io::Result<T> + Send + 'static,
    ) -> io::Result<T> {
        let disk = self.clone();
        tokio::task::spawn_blocking(move || f(&disk))
            .await
            .map_err(error)?
    }

    pub(crate) async fn update<T: Send + 'static>(
        &self,
        f: impl FnOnce(&WriteTransaction) -> io::Result<T> + Send + 'static,
    ) -> io::Result<T> {
        self.run(move |disk| disk.write(f)).await
    }

    pub(crate) async fn query<T: Send + 'static>(
        &self,
        f: impl FnOnce(&ReadTransaction) -> io::Result<T> + Send + 'static,
    ) -> io::Result<T> {
        self.run(move |disk| disk.read(f)).await
    }
}
