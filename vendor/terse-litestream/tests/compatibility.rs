#[path = "fixtures/upstream.rs"]
mod upstream;

use anyhow::Result;
use rusqlite::Connection;
use std::{fs, path::Path};
use terse_litestream::{
    CheckpointMode, Database, FileStore, Options, ReplicaStore, compact, replicate, restore,
};

#[test]
fn rust_capture_restores_in_go_and_rust_at_every_position() -> Result<()> {
    upstream::verify_version()?;
    for page_size in [512, 1024, 2048, 4096, 8192, 16384, 32768, 65536] {
        let dir = tempfile::tempdir_in("/tmp")?;
        let path = dir.path().join("data.sqlite");
        let sql = schema(&path, page_size)?;
        let mut db = Database::open(&path, Options::default())?;
        let replica = FileStore::new(dir.path().join("replica"));
        for step in 0..7 {
            workload(&sql, step)?;
            db.sync()?;
            if step == 3 {
                db.checkpoint(CheckpointMode::Passive)?;
            }
            if step == 5 {
                db.checkpoint(CheckpointMode::Truncate)?;
            }
            let txid = replicate(db.store(), &replica)?;
            assert_restores_match(&replica, dir.path(), txid, &sql)?;
        }
        let segments = replica.list(0)?;
        compact(&replica, &segments, 9)?;
        for segment in segments {
            replica.remove(&segment)?;
        }
        assert_restores_match(&replica, dir.path(), db.position(), &sql)?;
    }
    Ok(())
}

#[test]
fn go_capture_restores_in_rust_and_compacts_back_to_go() -> Result<()> {
    upstream::verify_version()?;
    for page_size in [512, 4096, 65536] {
        let dir = tempfile::tempdir_in("/tmp")?;
        let path = dir.path().join("data.sqlite");
        let sql = schema(&path, page_size)?;
        let daemon = upstream::Upstream::start(&path)?;
        let replica = FileStore::new(&daemon.replica);
        let mut positions = Vec::new();
        for step in 0..7 {
            workload(&sql, step)?;
            let txid = daemon.sync()?;
            assert_restores_match(&replica, dir.path(), txid, &sql)?;
            positions.push((txid, contents(&sql)?));
        }
        for (i, (txid, expected)) in positions.iter().enumerate() {
            let restored = dir.path().join(format!("history-{i}.sqlite"));
            restore(&replica, &restored, Some(*txid))?;
            assert_eq!(contents(&Connection::open(restored)?)?, *expected);
        }
        let segments = replica.list(0)?;
        let end = positions.last().unwrap();
        let compacted = FileStore::new(dir.path().join("compacted"));
        for segment in &segments {
            let mut writer = compacted.create(segment)?;
            std::io::copy(&mut replica.open(segment)?, &mut writer)?;
            writer.commit()?;
        }
        let snapshot = compact(&compacted, &segments, 9)?;
        assert_eq!(snapshot.max_txid, end.0);
        for segment in segments {
            compacted.remove(&segment)?;
        }
        assert_restores_match(&compacted, dir.path(), snapshot.max_txid, &sql)?;
    }
    Ok(())
}

#[test]
fn rust_and_go_can_take_over_each_others_local_capture_metadata() -> Result<()> {
    upstream::verify_version()?;
    let dir = tempfile::tempdir_in("/tmp")?;
    let path = dir.path().join("data.sqlite");
    let sql = schema(&path, 4096)?;
    let mut db = Database::open(&path, Options::default())?;
    workload(&sql, 0)?;
    let first = db.sync()?.txid;
    drop(db);
    let daemon = upstream::Upstream::start(&path)?;
    workload(&sql, 1)?;
    let second = daemon.sync()?;
    assert!(second > first);
    assert_restores_match(&FileStore::new(&daemon.replica), dir.path(), second, &sql)?;
    drop(daemon);
    let mut db = Database::open(&path, Options::default())?;
    assert!(db.position() >= second);
    workload(&sql, 2)?;
    let third = db.sync()?.txid;
    let replica = FileStore::new(dir.path().join("final-replica"));
    replicate(db.store(), &replica)?;
    assert_restores_match(&replica, dir.path(), third, &sql)?;
    Ok(())
}

#[test]
fn randomized_transactions_and_checkpoints_restore_identically_in_go() -> Result<()> {
    upstream::verify_version()?;
    for seed in 1..=6u64 {
        let dir = tempfile::tempdir_in("/tmp")?;
        let path = dir.path().join("data.sqlite");
        let sql = schema(&path, if seed % 2 == 0 { 512 } else { 4096 })?;
        let mut db = Database::open(
            &path,
            Options {
                max_sync_wal_bytes: 1024,
                min_checkpoint_pages: 64,
                truncate_pages: 256,
                ..Options::default()
            },
        )?;
        let replica = FileStore::new(dir.path().join("replica"));
        let mut random = seed;
        for step in 0..80 {
            random ^= random << 13;
            random ^= random >> 7;
            random ^= random << 17;
            let id = (random % 23) as i64;
            match random % 7 {
                0..=2 => {
                    sql.execute("INSERT INTO data VALUES(?1,?2,?3) ON CONFLICT(id) DO UPDATE SET text=excluded.text,value=excluded.value", rusqlite::params![id, format!("seed-{seed}-step-{step}"), vec![random as u8; (random % 15000) as usize]])?;
                }
                3 => {
                    sql.execute("DELETE FROM data WHERE id=?", [id])?;
                }
                4 => {
                    sql.execute_batch("BEGIN; DELETE FROM data; ROLLBACK")?;
                }
                5 => {
                    sql.execute_batch("VACUUM")?;
                }
                6 => {
                    db.checkpoint(CheckpointMode::Truncate)?;
                }
                _ => unreachable!(),
            }
            db.sync()?;
            if step % 10 == 0 || step == 79 {
                let txid = replicate(db.store(), &replica)?;
                assert_restores_match(&replica, dir.path(), txid, &sql)?;
            }
        }
    }
    Ok(())
}

fn schema(path: &Path, page_size: u32) -> Result<Connection> {
    let sql = Connection::open(path)?;
    sql.execute_batch(&format!("PRAGMA page_size={page_size}; PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0; CREATE TABLE data(id INTEGER PRIMARY KEY, text TEXT, value BLOB); CREATE INDEX data_text ON data(text); PRAGMA user_version=17;"))?;
    Ok(sql)
}

fn workload(sql: &Connection, step: usize) -> Result<()> {
    match step {
        0 => {
            sql.execute("INSERT INTO data VALUES (1,'first',zeroblob(90000))", [])?;
        }
        1 => {
            sql.execute(
                "INSERT INTO data VALUES (2,'second',?1)",
                [vec![0xacu8; 25000]],
            )?;
        }
        2 => {
            sql.execute_batch(
                "BEGIN; INSERT INTO data VALUES(3,'rollback',randomblob(5000)); ROLLBACK;",
            )?;
        }
        3 => {
            sql.execute_batch("UPDATE data SET text='updated',value=x'010203' WHERE id=1; CREATE TABLE extra(k TEXT PRIMARY KEY, v) WITHOUT ROWID; INSERT INTO extra VALUES('key','value');")?;
        }
        4 => {
            sql.execute_batch("DELETE FROM data WHERE id=2; VACUUM;")?;
        }
        5 => {
            sql.execute_batch("INSERT INTO data VALUES(4,'after-vacuum',zeroblob(300000)); PRAGMA user_version=22;")?;
        }
        6 => {
            sql.execute_batch("UPDATE data SET value=x'fe' WHERE id=4; DROP TABLE extra; VACUUM;")?;
        }
        _ => unreachable!(),
    }
    Ok(())
}

fn assert_restores_match(
    replica: &FileStore,
    dir: &Path,
    txid: u64,
    source: &Connection,
) -> Result<()> {
    let rust = dir.join("rust.sqlite");
    let go = dir.join("go.sqlite");
    for path in [&rust, &go] {
        if path.exists() {
            fs::remove_file(path)?;
        }
    }
    restore(replica, &rust, Some(txid))?;
    upstream::restore(replica.root(), &go, Some(txid))?;
    assert!(
        fs::read(&rust)? == fs::read(&go)?,
        "Rust and upstream restore differ at txid {txid}"
    );
    let recovered = Connection::open(&rust)?;
    assert!(
        contents(source)? == contents(&recovered)?,
        "restored SQL differs at txid {txid}"
    );
    assert_eq!(
        recovered.query_row("PRAGMA integrity_check", [], |row| row.get::<_, String>(0))?,
        "ok"
    );
    assert_eq!(
        source.query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))?,
        recovered.query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))?
    );
    Ok(())
}

fn contents(sql: &Connection) -> Result<Vec<(i64, String, Vec<u8>)>> {
    Ok(sql
        .prepare("SELECT id,text,value FROM data ORDER BY id")?
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?)
}

#[test]
fn explicit_snapshots_restore_in_go_after_all_earlier_files_are_removed() -> Result<()> {
    upstream::verify_version()?;
    for page_size in [512, 4096, 65536] {
        let dir = tempfile::tempdir_in("/tmp")?;
        let path = dir.path().join("data.sqlite");
        let sql = schema(&path, page_size)?;
        let mut db = Database::open(&path, Options::default())?;
        let replica = FileStore::new(dir.path().join("replica"));
        for step in 0..=3 {
            workload(&sql, step)?;
            let snapshot = db.snapshot()?;
            replicate(db.store(), &replica)?;
            for segment in replica.list(9)? {
                if segment.max_txid < snapshot.txid {
                    replica.remove(&segment)?;
                }
            }
            assert_restores_match(&replica, dir.path(), snapshot.txid, &sql)?;
        }
    }
    Ok(())
}
