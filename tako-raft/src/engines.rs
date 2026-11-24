use crate::{Writer, disk::error};
use parking_lot::Mutex;
use redb::{BackendError, StorageBackend, backends::FileBackend};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    io::{self, Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    sync::Arc,
};

/// Tracks the writer's revision across SQLite callbacks. A failed callback
/// fences the private file before the SQL error can reach the caller.
#[derive(Clone)]
pub struct SqliteWal {
    writer: Arc<Mutex<Writer>>,
    cursor: Arc<Mutex<terse_litestream::CommitCursor>>,
    gate: Arc<Mutex<()>>,
    path: PathBuf,
}

impl SqliteWal {
    pub fn new(writer: Writer) -> Self {
        let path = writer.path().to_owned();
        Self {
            writer: Arc::new(Mutex::new(writer)),
            cursor: Default::default(),
            gate: Default::default(),
            path,
        }
    }
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Every connection to the private writer takes this guard for the full
    /// statement, including the WAL callback. Other connections cannot read
    /// a locally committed write while its quorum result is still unknown.
    pub fn enter(&self) -> io::Result<parking_lot::MutexGuard<'_, ()>> {
        let guard = self.gate.lock();
        self.writer.lock().check_current()?;
        Ok(guard)
    }

    pub fn usable(&self) -> bool {
        self.writer.lock().revision_hint().is_ok()
    }

    /// Buffered SQL can wait to validate its result until SQLite finishes.
    /// Its own Raft commit is already a quorum barrier for a mutation.
    pub fn begin_buffered(&self) -> io::Result<(parking_lot::MutexGuard<'_, ()>, u64)> {
        let guard = self.gate.lock();
        let before = self.writer.lock().revision_hint()?;
        Ok((guard, before))
    }
    pub fn confirm_buffered(&self, before: u64) -> io::Result<()> {
        let writer = self.writer.lock();
        if writer.revision_hint()? == before {
            writer.check_current()?;
        }
        Ok(())
    }

    pub fn install(&self, connection: &mut rusqlite::Connection) -> io::Result<()> {
        let actual = connection
            .path()
            .ok_or_else(|| error("expected a file-backed SQLite connection"))?;
        if Path::new(actual) != self.path {
            return Err(error("SQLite connection does not use the private checkout"));
        }
        let mode: String = connection
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .map_err(error)?;
        if mode != "wal" {
            return Err(error("replicated SQLite requires WAL mode"));
        }
        let mut wal_path = self.path.as_os_str().to_owned();
        wal_path.push("-wal");
        let wal_path = PathBuf::from(wal_path);
        let state = self.clone();
        connection
            .wal_hook(Some(move |_: &mut rusqlite::hooks::Wal, frames: i32| {
                let mut lease = state.writer.lock();
                // rusqlite's native hook treats an unwind as success. Convert
                // both I/O errors and unwinds to a fenced writer and a SQL error.
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let mut wal = File::open(&wal_path)?;
                    let (size, len, pages) = state
                        .cursor
                        .lock()
                        .capture(&mut wal, u32::try_from(frames).map_err(error)?)
                        .map_err(error)?;
                    lease.publish(size, len, pages)
                }))
                .unwrap_or_else(|_| Err(error("replication callback interrupted")));
                result.map_err(|e| {
                    lease.fence();
                    rusqlite::Error::SqliteFailure(
                        rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_IOERR),
                        Some(e.to_string()),
                    )
                })
            }))
            .map_err(error)
    }
}

const BLOCK: u64 = 4096;

/// Private redb backend. Cache flushes and internal fsyncs only mark changed
/// blocks. The caller publishes once, after redb's entire transaction commits.
#[derive(Debug, Clone)]
pub struct RedbFile {
    backend: Arc<FileBackend>,
    dirty: Arc<Mutex<BTreeSet<u64>>>,
}

impl RedbFile {
    pub fn open(writer: &Writer) -> io::Result<Self> {
        let file = File::options().read(true).write(true).open(writer.path())?;
        Ok(Self {
            backend: Arc::new(FileBackend::new(file)),
            dirty: Default::default(),
        })
    }

    pub fn publish(&self, writer: &mut Writer) -> io::Result<()> {
        let result = self.publish_inner(writer);
        if result.is_err() {
            writer.fence();
        }
        result
    }
    fn publish_inner(&self, writer: &mut Writer) -> io::Result<()> {
        let mut file = File::open(writer.path())?;
        let len = file.metadata()?.len();
        if !len.is_multiple_of(BLOCK) {
            return Err(error("redb file size is not page aligned"));
        }
        let mut pages = BTreeMap::new();
        let mut dirty = self.dirty.lock();
        for &page in dirty.range(..len / BLOCK) {
            let mut bytes = vec![0; BLOCK as usize];
            file.seek(SeekFrom::Start(page * BLOCK))?;
            file.read_exact(&mut bytes)?;
            pages.insert(page, bytes);
        }
        writer.publish(BLOCK as u32, len, pages)?;
        dirty.clear();
        Ok(())
    }
}

impl StorageBackend for RedbFile {
    fn len(&self) -> Result<u64, BackendError> {
        self.backend.len()
    }
    fn read(&self, offset: u64, out: &mut [u8]) -> Result<(), BackendError> {
        self.backend.read(offset, out)
    }
    fn set_len(&self, len: u64) -> Result<(), BackendError> {
        // redb can shrink then grow inside a transaction. Publish zeroes for
        // truncated blocks that become addressable again before commit.
        let old = self.backend.len()?;
        self.backend.set_len(len)?;
        if len < old {
            self.dirty.lock().extend(len / BLOCK..old.div_ceil(BLOCK));
        }
        Ok(())
    }
    fn sync_data(&self) -> Result<(), BackendError> {
        self.backend.sync_data()
    }
    fn write(&self, offset: u64, data: &[u8]) -> Result<(), BackendError> {
        self.backend.write(offset, data)?;
        if !data.is_empty() {
            let end = offset + data.len() as u64;
            self.dirty
                .lock()
                .extend(offset / BLOCK..end.div_ceil(BLOCK));
        }
        Ok(())
    }
}
