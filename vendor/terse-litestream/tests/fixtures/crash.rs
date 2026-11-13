use anyhow::{Context, Result};
use rusqlite::Connection;
use std::{
    io::{self, Read, Write},
    path::PathBuf,
    sync::Arc,
};
use terse_litestream::{Database, FileStore, Options, ReplicaStore, Segment, SegmentWriter};

fn main() -> Result<()> {
    let mut args = std::env::args_os().skip(1);
    let root = PathBuf::from(args.next().context("root missing")?);
    let mode = args.next().context("mode missing")?;
    let path = root.join("data.sqlite");
    let sql = Connection::open(&path)?;
    sql.execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0; CREATE TABLE data(value); INSERT INTO data VALUES(1)")?;
    let store = Arc::new(PausingStore {
        inner: FileStore::new(root.join("replica")),
        marker: root.join("ready"),
        after: mode == "after",
    });
    let mut db = Database::with_store(&path, Options::default(), store)?;
    db.sync()?;
    sql.execute("INSERT INTO data VALUES(2)", [])?;
    db.sync()?;
    anyhow::bail!("parent should kill this process during publication")
}

struct PausingStore {
    inner: FileStore,
    marker: PathBuf,
    after: bool,
}
impl ReplicaStore for PausingStore {
    fn list(&self, level: u8) -> Result<Vec<Segment>> {
        self.inner.list(level)
    }
    fn open(&self, segment: &Segment) -> Result<Box<dyn Read + Send>> {
        self.inner.open(segment)
    }
    fn remove(&self, segment: &Segment) -> Result<()> {
        self.inner.remove(segment)
    }
    fn create(&self, segment: &Segment) -> Result<Box<dyn SegmentWriter>> {
        Ok(Box::new(PausingWriter {
            inner: self.inner.create(segment)?,
            marker: self.marker.clone(),
            pause: segment.max_txid == 2,
            after: self.after,
        }))
    }
}
struct PausingWriter {
    inner: Box<dyn SegmentWriter>,
    marker: PathBuf,
    pause: bool,
    after: bool,
}
impl Write for PausingWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.inner.write(bytes)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}
impl SegmentWriter for PausingWriter {
    fn commit(mut self: Box<Self>) -> Result<()> {
        self.inner.flush()?;
        if self.pause && !self.after {
            pause(&self.marker)?;
        }
        self.inner.commit()?;
        if self.pause && self.after {
            pause(&self.marker)?;
        }
        Ok(())
    }
}
fn pause(marker: &std::path::Path) -> Result<()> {
    std::fs::write(marker, b"ready")?;
    io::stdin().read_exact(&mut [0])?;
    anyhow::bail!("expected parent to kill the child")
}
