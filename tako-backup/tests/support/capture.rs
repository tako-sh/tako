//! Stock SQLite fixture for checking LTX interoperability with an independent WAL producer.
use anyhow::{Context, Result, bail, ensure};
use std::{fs, fs::File, io::Write, path::Path, sync::Arc, time::Duration};
use terse_litestream::{Database, FileStore, Options, ReplicaStore};

/// One capture owner for an existing SQLite database in WAL mode. Run its
/// methods on a blocking worker.
pub struct Capture {
    database: Database,
    spool: FileStore,
    _lock: File,
}

impl Capture {
    /// Opens capture with its spool in `directory`, at a position of at
    /// least `floor`. Keep one spool and one remote
    /// prefix per database. The first capture after opening is a full image.
    pub fn open(database: &Path, directory: &Path, floor: u64) -> Result<Self> {
        let source = fs::canonicalize(database)?;
        fs::create_dir_all(directory)?;
        let lock = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(directory.join("capture.lock"))?;
        lock.try_lock()
            .context("spool already has a capture owner")?;
        let identity = directory.join("source");
        let source = source.to_str().context("source path must be UTF-8")?;
        if identity.exists() {
            ensure!(
                fs::read_to_string(&identity)? == source,
                "spool belongs to another database"
            );
        } else {
            let mut marker = tempfile::NamedTempFile::new_in(directory)?;
            marker.write_all(source.as_bytes())?;
            marker.persist(&identity)?;
        }
        let spool = FileStore::new(directory);
        let options = Options {
            busy_timeout: Duration::from_secs(5),
            ..Options::default()
        };
        let database =
            Database::with_store_from(database, options, Arc::new(spool.clone()), floor)?;
        Ok(Self {
            database,
            spool,
            _lock: lock,
        })
    }

    /// Captures committed changes durably on this machine and returns the
    /// latest local transaction.
    pub fn sync(&mut self) -> Result<u64> {
        self.database.sync()?;
        Ok(self.database.position())
    }

    /// Captures a full image of the database at a new transaction, which
    /// restores without any earlier segment.
    pub fn snapshot(&mut self) -> Result<u64> {
        self.database.snapshot()?;
        Ok(self.database.position())
    }

    /// When a segment still in the spool was captured, from its header.
    pub fn captured_at(&self, txid: u64) -> Result<i64> {
        for level in [9, 0] {
            if let Some(segment) = self
                .spool
                .list(level)?
                .into_iter()
                .find(|segment| segment.max_txid == txid)
            {
                let decoder = terse_ltx::Decoder::new(self.spool.open(&segment)?)?;
                return Ok(decoder.header().timestamp);
            }
        }
        bail!("transaction {txid} is not in the spool")
    }

    pub fn position(&self) -> u64 {
        self.database.position()
    }

    /// Captures what is left and releases the database and the spool.
    pub fn close(self) -> Result<u64> {
        let position = self.database.close()?;
        Ok(position)
    }

    /// The spool, for uploads; cloning does not borrow the capture.
    pub fn spool(&self) -> FileStore {
        self.spool.clone()
    }
}
