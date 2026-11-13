use anyhow::Result;
use rusqlite::Connection;
use terse_litestream::{
    CheckpointMode, Database, Durability, FileStore, Options, ReplicaStore, compact, replicate,
    restore, restore_with,
};

#[test]
fn capture_restore_and_idle_sync_preserve_database() -> Result<()> {
    for page_size in [512, 1024, 4096, 8192, 65536] {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("data.sqlite");
        let sql = Connection::open(&path)?;
        sql.execute_batch(&format!("PRAGMA page_size={page_size}; PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0; CREATE TABLE data(id INTEGER PRIMARY KEY, value BLOB);"))?;
        let mut db = Database::open(&path, Options::default())?;
        let first = db.sync()?;
        assert!(first.changed && first.snapshot);
        let idle = db.sync()?;
        assert!(!idle.changed);
        assert_eq!(idle.txid, first.txid);
        sql.execute("INSERT INTO data VALUES(1, ?)", [vec![7u8; 20000]])?;
        let second = db.sync()?;
        assert!(second.changed && !second.snapshot);
        let replica = FileStore::new(dir.path().join("replica"));
        assert_eq!(replicate(db.store(), &replica)?, second.txid);
        let restored = dir.path().join("restored.sqlite");
        assert_eq!(restore(&replica, &restored, None)?, second.txid);
        let recovered = Connection::open(restored)?;
        let blob: Vec<u8> = recovered.query_row("SELECT value FROM data", [], |r| r.get(0))?;
        assert_eq!(blob, vec![7; 20000]);
        assert_eq!(
            recovered.query_row("PRAGMA integrity_check", [], |r| r.get::<_, String>(0))?,
            "ok"
        );
        let old = dir.path().join("old.sqlite");
        restore(&replica, &old, Some(first.txid))?;
        assert_eq!(
            Connection::open(old)?
                .query_row("SELECT count(*) FROM data", [], |r| r.get::<_, i64>(0))?,
            0
        );
    }
    Ok(())
}

#[test]
fn checkpoints_reopen_vacuum_and_compaction_keep_all_rows() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("data.sqlite");
    let sql = Connection::open(&path)?;
    sql.execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0; CREATE TABLE data(id INTEGER PRIMARY KEY, value BLOB)")?;
    let mut db = Database::open(&path, Options::default())?;
    db.sync()?;
    for (i, mode) in [
        CheckpointMode::Passive,
        CheckpointMode::Full,
        CheckpointMode::Restart,
        CheckpointMode::Truncate,
    ]
    .into_iter()
    .enumerate()
    {
        sql.execute("INSERT INTO data VALUES (?, zeroblob(20000))", [i as i64])?;
        db.sync()?;
        db.checkpoint(mode)?;
        sql.execute(
            "UPDATE data SET value = randomblob(11000) WHERE id = ?",
            [i as i64],
        )?;
        db.sync()?;
    }
    sql.execute_batch("DELETE FROM data WHERE id % 2 = 0; VACUUM")?;
    db.sync()?;
    let last = db.position();
    drop(db);
    let mut db = Database::open(&path, Options::default())?;
    assert_eq!(db.position(), last);
    sql.execute("INSERT INTO data VALUES (100, x'1234')", [])?;
    db.sync()?;
    let replica = FileStore::new(dir.path().join("replica"));
    replicate(db.store(), &replica)?;
    let segments = replica.list(0)?;
    compact(&replica, &segments, 9)?;
    for segment in segments {
        replica.remove(&segment)?;
    }
    let output = dir.path().join("output.sqlite");
    restore(&replica, &output, None)?;
    let recovered = Connection::open(output)?;
    assert_eq!(
        recovered.query_row("SELECT group_concat(id) FROM data", [], |r| r
            .get::<_, String>(0))?,
        "1,3,100"
    );
    assert_eq!(
        recovered.query_row("PRAGMA integrity_check", [], |r| r.get::<_, String>(0))?,
        "ok"
    );
    Ok(())
}

#[test]
fn uncommitted_writes_never_reach_replica() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("data.sqlite");
    let sql = Connection::open(&path)?;
    sql.execute_batch(
        "PRAGMA journal_mode=WAL; CREATE TABLE data(value); INSERT INTO data VALUES (1)",
    )?;
    let mut db = Database::open(&path, Options::default())?;
    db.sync()?;
    sql.execute_batch("BEGIN; INSERT INTO data VALUES (2)")?;
    let before = db.position();
    let result = db.sync();
    assert!(result.is_err() || !result?.changed);
    sql.execute_batch("ROLLBACK")?;
    assert_eq!(db.position(), before);
    let output = dir.path().join("output.sqlite");
    restore(db.store(), &output, None)?;
    assert_eq!(
        Connection::open(output)?
            .query_row("SELECT count(*) FROM data", [], |r| r.get::<_, i64>(0))?,
        1
    );
    Ok(())
}

#[test]
fn concurrent_writers_survive_checkpoint_boundaries() -> Result<()> {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("data.sqlite");
    let sql = Connection::open(&path)?;
    sql.execute_batch(
        "PRAGMA journal_mode=WAL; CREATE TABLE data(id INTEGER PRIMARY KEY, value BLOB)",
    )?;
    let options = Options {
        min_checkpoint_pages: 16,
        truncate_pages: 128,
        max_sync_wal_bytes: 4096,
        ..Options::default()
    };
    let mut db = Database::open(&path, options)?;
    db.sync()?;
    let done = Arc::new(AtomicBool::new(false));
    let writing = done.clone();
    let writer_path = path.clone();
    let writer = std::thread::spawn(move || -> Result<()> {
        let sql = Connection::open(writer_path)?;
        sql.busy_timeout(std::time::Duration::from_secs(10))?;
        sql.execute_batch("PRAGMA wal_autocheckpoint=0")?;
        for id in 0..200 {
            sql.execute(
                "INSERT INTO data VALUES (?, ?)",
                rusqlite::params![id, vec![id as u8; 4000 + id as usize * 13]],
            )?;
            if id % 3 == 0 {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        }
        writing.store(true, Ordering::SeqCst);
        Ok(())
    });
    let mut rounds = 0;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while !done.load(Ordering::SeqCst) && !writer.is_finished() {
        anyhow::ensure!(
            std::time::Instant::now() < deadline,
            "writer made no progress"
        );
        db.sync()?;
        rounds += 1;
        if rounds % 5 == 0 {
            db.checkpoint(CheckpointMode::Truncate)?;
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    writer.join().unwrap()?;
    db.sync()?;
    let output = dir.path().join("output.sqlite");
    restore(db.store(), &output, None)?;
    let recovered = Connection::open(output)?;
    assert_eq!(
        recovered.query_row("SELECT count(*) FROM data", [], |r| r.get::<_, i64>(0))?,
        200
    );
    for id in 0..200 {
        assert_eq!(
            recovered.query_row("SELECT value FROM data WHERE id=?", [id], |r| r
                .get::<_, Vec<u8>>(0))?,
            vec![id as u8; 4000 + id as usize * 13]
        );
    }
    assert_eq!(
        recovered.query_row("PRAGMA integrity_check", [], |r| r.get::<_, String>(0))?,
        "ok"
    );
    Ok(())
}

#[test]
fn blocked_checkpoint_preserves_read_lock_and_recovers_after_reader_releases() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("data.sqlite");
    let sql = Connection::open(&path)?;
    sql.execute_batch(
        "PRAGMA journal_mode=WAL; CREATE TABLE data(value); INSERT INTO data VALUES(1)",
    )?;
    let mut db = Database::open(
        &path,
        Options {
            busy_timeout: std::time::Duration::from_millis(10),
            ..Options::default()
        },
    )?;
    db.sync()?;
    let reader = Connection::open(&path)?;
    reader.execute_batch("BEGIN; SELECT * FROM data")?;
    sql.execute("INSERT INTO data VALUES(2)", [])?;
    assert!(db.checkpoint(CheckpointMode::Truncate).is_err());
    reader.execute_batch("ROLLBACK")?;
    db.checkpoint(CheckpointMode::Truncate)?;
    sql.execute("INSERT INTO data VALUES(3)", [])?;
    db.sync()?;
    let output = dir.path().join("output.sqlite");
    restore(db.store(), &output, None)?;
    assert_eq!(
        Connection::open(output)?
            .query_row("SELECT count(*) FROM data", [], |r| r.get::<_, i64>(0))?,
        3
    );
    Ok(())
}

#[test]
fn tiny_capture_limit_never_splits_a_transaction() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("data.sqlite");
    let sql = Connection::open(&path)?;
    sql.execute_batch(
        "PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0; CREATE TABLE data(value)",
    )?;
    let mut db = Database::open(
        &path,
        Options {
            max_sync_wal_bytes: 1,
            ..Options::default()
        },
    )?;
    db.sync()?;
    for value in 1..=10 {
        sql.execute("INSERT INTO data VALUES(?)", [value])?;
    }
    let first = db.position();
    assert_eq!(db.sync()?.txid, first + 10);
    for number in 1..=10 {
        let output = dir.path().join(format!("step-{number}.sqlite"));
        restore(db.store(), &output, Some(first + number))?;
        assert_eq!(
            Connection::open(output)?
                .query_row("SELECT count(*) FROM data", [], |r| r.get::<_, i64>(0))?,
            number as i64
        );
    }
    Ok(())
}

#[test]
fn dropping_control_tables_does_not_break_future_capture() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("data.sqlite");
    let sql = Connection::open(&path)?;
    sql.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE data(value)")?;
    let mut db = Database::open(&path, Options::default())?;
    db.sync()?;
    sql.execute_batch(
        "DROP TABLE _litestream_lock; DROP TABLE _litestream_seq; INSERT INTO data VALUES(1)",
    )?;
    db.sync()?;
    db.checkpoint(CheckpointMode::Truncate)?;
    let output = dir.path().join("output.sqlite");
    restore(db.store(), &output, None)?;
    assert_eq!(
        Connection::open(output)?
            .query_row("SELECT count(*) FROM data", [], |r| r.get::<_, i64>(0))?,
        1
    );
    Ok(())
}

#[test]
fn automatic_checkpoints_reuse_wal_without_repeated_full_snapshots() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("data.sqlite");
    let sql = Connection::open(&path)?;
    sql.execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0; CREATE TABLE data(value); INSERT INTO data VALUES(1)")?;
    let mut db = Database::open(
        &path,
        Options {
            min_checkpoint_pages: 1,
            truncate_pages: 2,
            ..Default::default()
        },
    )?;
    let first = db.sync()?;
    assert_eq!(
        first.txid, 1,
        "successful passive checkpoint needs no second snapshot"
    );
    for value in 2..=12 {
        sql.execute("INSERT INTO data VALUES(?)", [value])?;
        let result = db.sync()?;
        assert!(
            !result.snapshot,
            "automatic WAL reuse should remain incremental"
        );
        let txid = db.position();
        for _ in 0..3 {
            assert_eq!(db.sync()?.txid, txid);
        }
    }
    let output = dir.path().join("output.sqlite");
    restore(db.store(), &output, None)?;
    assert_eq!(
        Connection::open(output)?
            .query_row("SELECT count(*) FROM data", [], |r| r.get::<_, i64>(0))?,
        12
    );
    Ok(())
}

#[test]
fn checkpoint_results_report_new_capture_and_snapshot() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("data.sqlite");
    let sql = Connection::open(&path)?;
    sql.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE data(value)")?;
    let mut db = Database::open(&path, Options::default())?;
    db.sync()?;
    for mode in [
        CheckpointMode::Full,
        CheckpointMode::Restart,
        CheckpointMode::Truncate,
    ] {
        let before = db.position();
        let result = db.checkpoint(mode)?;
        assert!(
            result.changed && result.snapshot,
            "checkpoint must report its published snapshot"
        );
        assert!(result.txid > before);
    }
    sql.execute("INSERT INTO data VALUES(1)", [])?;
    let result = db.checkpoint(CheckpointMode::Passive)?;
    assert!(result.changed);
    assert!(!result.snapshot);
    assert!(result.wal_bytes > 0);
    Ok(())
}

#[test]
fn automatic_checkpoint_contention_reports_error_and_preserves_captured_writes() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("data.sqlite");
    let sql = Connection::open(&path)?;
    sql.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE data(value)")?;
    let mut db = Database::open(
        &path,
        Options {
            min_checkpoint_pages: 1,
            busy_timeout: std::time::Duration::from_millis(5),
            ..Default::default()
        },
    )?;
    db.sync()?;
    sql.execute_batch("INSERT INTO data VALUES(1); BEGIN IMMEDIATE; INSERT INTO data VALUES(2)")?;
    assert!(
        db.sync().is_err(),
        "checkpoint contention must not report a stale successful position"
    );
    let captured = db.position();
    sql.execute_batch("ROLLBACK")?;
    assert_eq!(db.sync()?.txid, captured);
    let output = dir.path().join("output.sqlite");
    restore(db.store(), &output, None)?;
    assert_eq!(
        Connection::open(output)?
            .query_row("SELECT count(*) FROM data", [], |r| r.get::<_, i64>(0))?,
        1
    );
    Ok(())
}

#[test]
fn explicit_snapshots_allow_retention_and_incremental_capture_after_reopen() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("data.sqlite");
    let sql = Connection::open(&path)?;
    sql.execute_batch(
        "PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0; CREATE TABLE data(value)",
    )?;
    let mut db = Database::open(&path, Options::default())?;
    db.sync()?;
    sql.execute("INSERT INTO data VALUES(1)", [])?;
    let snapshot = db.snapshot()?;
    assert!(snapshot.snapshot && snapshot.changed);
    assert_eq!(db.store().list(9)?.len(), 1);
    let replica = FileStore::new(dir.path().join("replica"));
    assert_eq!(replicate(db.store(), &replica)?, snapshot.txid);
    for level in [0, 9] {
        for segment in db.store().list(level)? {
            if segment.level == 0 || segment.max_txid < snapshot.txid {
                db.store().remove(&segment)?;
            }
        }
    }
    assert!(!db.sync()?.changed);
    drop(db);
    let mut db = Database::open(&path, Options::default())?;
    sql.execute("INSERT INTO data VALUES(2)", [])?;
    assert!(db.sync()?.txid > snapshot.txid);
    replicate(db.store(), &replica)?;
    let output = dir.path().join("output.sqlite");
    restore(&replica, &output, None)?;
    assert_eq!(
        Connection::open(output)?
            .query_row("SELECT count(*) FROM data", [], |r| r.get::<_, i64>(0))?,
        2
    );
    Ok(())
}

#[test]
fn scratch_replicas_take_the_deferred_base_snapshot_at_the_first_write() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("data.sqlite");
    let sql = Connection::open(&path)?;
    sql.execute_batch(
        "PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0; CREATE TABLE data(value);",
    )?;
    let options = Options {
        defer_base_snapshot: true,
        ..Options::default()
    };
    let store = std::sync::Arc::new(FileStore::scratch(dir.path().join("local")));
    let mut db = Database::with_store(&path, options, store.clone())?;
    assert_eq!(db.sync()?.txid, 0);
    sql.execute("INSERT INTO data VALUES(1)", [])?;
    let first = db.sync()?;
    assert!(first.changed && first.snapshot);
    let restored = dir.path().join("restored.sqlite");
    restore_with(store.as_ref(), &restored, None, Durability::Scratch)?;
    assert_eq!(
        Connection::open(restored)?
            .query_row("SELECT count(*) FROM data", [], |r| r.get::<_, i64>(0))?,
        1
    );
    Ok(())
}
