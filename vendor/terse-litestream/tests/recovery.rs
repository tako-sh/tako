use anyhow::{Result, bail};
use rusqlite::Connection;
use std::{
    fs,
    io::{self, Read, Write},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
use terse_litestream::{
    CheckpointMode, Database, FileStore, Options, ReplicaStore, Segment, SegmentWriter, compact,
    replicate, restore,
};

#[test]
fn capture_failure_does_not_advance_position_and_retry_keeps_the_commit() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("data.sqlite");
    let sql = initialize(&path)?;
    let local = Arc::new(FaultStore::new(dir.path().join("local")));
    let mut db = Database::with_store(&path, Options::default(), local.clone())?;
    db.sync()?;
    let before = db.position();
    sql.execute("INSERT INTO data VALUES(2)", [])?;
    local.fail_write.store(true, Ordering::SeqCst);
    assert!(db.sync().is_err());
    assert_eq!(db.position(), before);
    assert_eq!(local.list(0)?.len(), 1);
    local.fail_write.store(false, Ordering::SeqCst);
    assert_eq!(db.sync()?.txid, before + 1);
    let output = dir.path().join("output.sqlite");
    restore(db.store(), &output, None)?;
    assert_eq!(count(&output)?, 2);
    Ok(())
}

#[test]
fn replication_failure_can_retry_without_skipping_transactions() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("data.sqlite");
    let sql = initialize(&path)?;
    let mut db = Database::open(&path, Options::default())?;
    db.sync()?;
    sql.execute("INSERT INTO data VALUES(2)", [])?;
    db.sync()?;
    let destination = FaultStore::new(dir.path().join("replica"));
    destination.fail_commit.store(true, Ordering::SeqCst);
    assert!(replicate(db.store(), &destination).is_err());
    assert!(destination.list(0)?.is_empty());
    destination.fail_commit.store(false, Ordering::SeqCst);
    assert_eq!(replicate(db.store(), &destination)?, db.position());
    assert_eq!(replicate(db.store(), &destination)?, db.position());
    Ok(())
}

#[test]
fn corrupt_segment_or_gap_never_publishes_a_restored_database() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("data.sqlite");
    let sql = initialize(&path)?;
    let mut db = Database::open(&path, Options::default())?;
    db.sync()?;
    sql.execute("INSERT INTO data VALUES(2)", [])?;
    db.sync()?;
    sql.execute("INSERT INTO data VALUES(3)", [])?;
    db.sync()?;
    let replica = FileStore::new(dir.path().join("replica"));
    replicate(db.store(), &replica)?;
    let segments = replica.list(0)?;
    let second = replica.path(&segments[1]);
    let original = fs::read(&second)?;
    for offset in [0, original.len() / 2, original.len() - 1] {
        let mut bytes = original.clone();
        bytes[offset] ^= 0x40;
        fs::write(&second, bytes)?;
        let output = dir.path().join("output.sqlite");
        assert!(restore(&replica, &output, None).is_err());
        assert!(!output.exists());
    }
    fs::write(&second, original)?;
    replica.remove(&segments[1])?;
    assert!(restore(&replica, dir.path().join("gap.sqlite"), None).is_err());
    let old = dir.path().join("old.sqlite");
    restore(&replica, &old, Some(segments[0].max_txid))?;
    assert_eq!(count(&old)?, 1);
    Ok(())
}

#[test]
fn restore_refuses_to_overwrite_an_existing_database() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("data.sqlite");
    let _sql = initialize(&path)?;
    let mut db = Database::open(&path, Options::default())?;
    db.sync()?;
    let output = dir.path().join("output.sqlite");
    fs::write(&output, b"keep me")?;
    assert!(restore(db.store(), &output, None).is_err());
    assert_eq!(fs::read(output)?, b"keep me");
    Ok(())
}

#[test]
fn restored_older_database_can_advance_past_the_existing_replica() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("data.sqlite");
    let sql = initialize(&path)?;
    let mut db = Database::open(&path, Options::default())?;
    let first = db.sync()?.txid;
    sql.execute("INSERT INTO data VALUES(2)", [])?;
    db.sync()?;
    let replica = FileStore::new(dir.path().join("replica"));
    let latest = replicate(db.store(), &replica)?;
    let old = dir.path().join("old.sqlite");
    restore(&replica, &old, Some(first))?;
    let mut replacement = Database::open(&old, Options::default())?;
    replacement.recover_position(&replica)?;
    assert_eq!(replacement.position(), latest);
    let old_sql = Connection::open(&old)?;
    old_sql.execute("INSERT INTO data VALUES(3)", [])?;
    assert_eq!(replacement.sync()?.txid, latest + 1);
    replicate(replacement.store(), &replica)?;
    let output = dir.path().join("output.sqlite");
    restore(&replica, &output, None)?;
    assert_eq!(
        Connection::open(output)?
            .query_row("SELECT group_concat(value) FROM data", [], |row| row
                .get::<_, String>(0))?,
        "1,3"
    );
    Ok(())
}

#[test]
fn position_recovery_and_replication_work_after_l0_retention() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("data.sqlite");
    let sql = initialize(&path)?;
    let mut db = Database::open(&path, Options::default())?;
    db.sync()?;
    sql.execute("INSERT INTO data VALUES(2)", [])?;
    db.sync()?;
    let replica = FileStore::new(dir.path().join("replica"));
    let latest = replicate(db.store(), &replica)?;
    let segments = replica.list(0)?;
    compact(&replica, &segments, 9)?;
    for segment in segments {
        replica.remove(&segment)?;
    }
    let old = dir.path().join("old.sqlite");
    restore(&replica, &old, None)?;
    let mut replacement = Database::open(&old, Options::default())?;
    replacement.recover_position(&replica)?;
    assert_eq!(replacement.position(), latest);
    replacement.sync()?;
    assert_eq!(replicate(replacement.store(), &replica)?, latest + 1);
    Ok(())
}

#[test]
fn resumed_restore_captures_only_changed_pages_at_the_next_transaction() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("data.sqlite");
    let sql = initialize(&path)?;
    sql.execute("WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 20) INSERT INTO data SELECT zeroblob(3000) FROM n", [])?;
    let mut db = Database::open(&path, Options::default())?;
    let txid = db.sync()?.txid;
    let replica = FileStore::new(dir.path().join("replica"));
    replicate(db.store(), &replica)?;
    let restored = dir.path().join("restored.sqlite");
    restore(&replica, &restored, Some(txid))?;
    let before = fs::read(&restored)?;
    let local = Arc::new(FileStore::new(dir.path().join("local")));
    let mut resumed = Database::resume(&restored, Options::default(), local.clone(), txid)?;
    assert!(!resumed.sync()?.changed);
    Connection::open(&restored)?.execute("INSERT INTO data VALUES(2)", [])?;
    let capture = resumed.sync()?;
    assert!(capture.changed && !capture.snapshot);
    let delta = Segment::new(0, txid + 1, txid + 1)?;
    assert_eq!(local.list(0)?, vec![delta]);
    assert_eq!(replicate(resumed.store(), &replica)?, txid + 1);
    let output = dir.path().join("output.sqlite");
    restore(&replica, &output, None)?;
    resumed.checkpoint(CheckpointMode::Passive)?;
    let after = fs::read(&output)?;
    assert_eq!(after, fs::read(&restored)?);
    let changed = changed_pages(&before, &after);
    assert_eq!(changed.len(), 1);
    assert_eq!(ltx_pages(&local, &delta)?, changed);
    Ok(())
}

#[test]
fn malformed_unicode_filenames_are_ignored() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let store = FileStore::new(dir.path());
    fs::create_dir_all(dir.path().join("ltx/0"))?;
    let name = format!("{}é{}.ltx", "x".repeat(15), "x".repeat(16));
    fs::write(dir.path().join("ltx/0").join(name), b"invalid")?;
    assert!(store.list(0)?.is_empty());
    Ok(())
}

#[test]
fn checksummed_restore_rejects_a_false_database_checksum() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let store = FileStore::new(dir.path().join("replica"));
    let page = vec![7u8; 512];
    let crc = crc::Crc::<u64>::new(&crc::CRC_64_GO_ISO);
    let mut hash = crc.digest();
    hash.update(&1u32.to_be_bytes());
    hash.update(&page);
    let sum = hash.finalize() | terse_ltx::CHECKSUM_FLAG;
    let first = Segment::new(0, 1, 1)?;
    let mut encoder = terse_ltx::Encoder::new(
        store.create(&first)?,
        terse_ltx::Header {
            page_size: 512,
            commit: 1,
            min_txid: 1,
            max_txid: 1,
            ..Default::default()
        },
    )?;
    encoder.write_page(1, &page)?;
    encoder.finish(sum)?.commit()?;
    let valid = dir.path().join("valid.sqlite");
    restore(&store, &valid, Some(1))?;
    assert_eq!(fs::read(valid)?, page);
    let second = Segment::new(0, 2, 2)?;
    let mut encoder = terse_ltx::Encoder::new(
        store.create(&second)?,
        terse_ltx::Header {
            page_size: 512,
            commit: 1,
            min_txid: 2,
            max_txid: 2,
            pre_apply_checksum: sum,
            ..Default::default()
        },
    )?;
    encoder.write_page(1, &vec![8u8; 512])?;
    encoder.finish(sum)?.commit()?;
    let output = dir.path().join("output.sqlite");
    assert!(restore(&store, &output, None).is_err());
    assert!(!output.exists());
    Ok(())
}

#[test]
fn ambiguous_publication_fences_capture_until_reopened() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("data.sqlite");
    let sql = initialize(&path)?;
    let local = Arc::new(FaultStore::new(dir.path().join("local")));
    let mut db = Database::with_store(&path, Options::default(), local.clone())?;
    db.sync()?;
    sql.execute("INSERT INTO data VALUES(2)", [])?;
    local.fail_after_commit.store(true, Ordering::SeqCst);
    assert!(db.sync().is_err());
    let published = local.list(0)?.pop().unwrap();
    let original = fs::read(local.inner.path(&published))?;
    local.fail_after_commit.store(false, Ordering::SeqCst);
    sql.execute("INSERT INTO data VALUES(3)", [])?;
    assert!(
        db.sync().is_err(),
        "an ambiguous publish requires reopening"
    );
    assert_eq!(fs::read(local.inner.path(&published))?, original);
    drop(db);
    let mut db = Database::with_store(&path, Options::default(), local)?;
    assert_eq!(db.sync()?.txid, published.max_txid + 1);
    let output = dir.path().join("output.sqlite");
    restore(db.store(), &output, None)?;
    assert_eq!(count(&output)?, 3);
    Ok(())
}

#[test]
fn published_segments_cannot_be_replaced() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let store = FileStore::new(dir.path());
    let segment = Segment::new(0, 1, 1)?;
    let mut first = store.create(&segment)?;
    first.write_all(b"first")?;
    first.commit()?;
    let mut second = store.create(&segment)?;
    second.write_all(b"second")?;
    assert!(second.commit().is_err());
    assert_eq!(fs::read(store.path(&segment))?, b"first");
    Ok(())
}

fn initialize(path: &std::path::Path) -> Result<Connection> {
    let connection = Connection::open(path)?;
    connection.execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0; CREATE TABLE data(value); INSERT INTO data VALUES(1)")?;
    Ok(connection)
}

fn count(path: &std::path::Path) -> Result<i64> {
    Ok(Connection::open(path)?.query_row("SELECT count(*) FROM data", [], |r| r.get(0))?)
}

fn changed_pages(before: &[u8], after: &[u8]) -> Vec<u32> {
    after
        .chunks(4096)
        .enumerate()
        .filter(|(index, page)| before.chunks(4096).nth(*index) != Some(*page))
        .map(|(index, _)| index as u32 + 1)
        .collect()
}

fn ltx_pages(store: &FileStore, segment: &Segment) -> Result<Vec<u32>> {
    let mut decoder = terse_ltx::Decoder::new(store.open(segment)?)?;
    let mut pages = Vec::new();
    while let Some(page) = decoder.next_page()? {
        pages.push(page.number);
    }
    Ok(pages)
}

struct FaultStore {
    inner: FileStore,
    fail_write: Arc<AtomicBool>,
    fail_commit: Arc<AtomicBool>,
    fail_after_commit: Arc<AtomicBool>,
}
impl FaultStore {
    fn new(path: std::path::PathBuf) -> Self {
        Self {
            inner: FileStore::new(path),
            fail_write: Arc::new(AtomicBool::new(false)),
            fail_commit: Arc::new(AtomicBool::new(false)),
            fail_after_commit: Arc::new(AtomicBool::new(false)),
        }
    }
}
impl ReplicaStore for FaultStore {
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
        Ok(Box::new(FaultWriter {
            inner: self.inner.create(segment)?,
            fail_write: self.fail_write.clone(),
            fail_commit: self.fail_commit.clone(),
            fail_after_commit: self.fail_after_commit.clone(),
        }))
    }
}
struct FaultWriter {
    inner: Box<dyn SegmentWriter>,
    fail_write: Arc<AtomicBool>,
    fail_commit: Arc<AtomicBool>,
    fail_after_commit: Arc<AtomicBool>,
}
impl Write for FaultWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.fail_write.load(Ordering::SeqCst) {
            return Err(io::Error::other("injected write failure"));
        }
        self.inner.write(bytes)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}
impl SegmentWriter for FaultWriter {
    fn commit(self: Box<Self>) -> Result<()> {
        if self.fail_commit.load(Ordering::SeqCst) {
            bail!("injected commit failure");
        }
        self.inner.commit()?;
        if self.fail_after_commit.load(Ordering::SeqCst) {
            bail!("injected ambiguous publication");
        }
        Ok(())
    }
}

#[test]
fn restore_accepts_a_filename_in_the_current_directory() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("data.sqlite");
    let _sql = initialize(&path)?;
    let mut db = Database::open(&path, Options::default())?;
    db.sync()?;
    let output = tempfile::NamedTempFile::new_in(".")?.into_temp_path();
    fs::remove_file(&output)?;
    restore(db.store(), output.file_name().unwrap(), None)?;
    assert_eq!(count(&output)?, 1);
    Ok(())
}
