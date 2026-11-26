use crate::{Writer, disk::error};
use parking_lot::Mutex;
use redb::{BackendError, StorageBackend, backends::FileBackend};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    io::{self, Read, Seek, SeekFrom},
    sync::Arc,
};

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
