#[path = "cases/defaults.rs"]
mod defaults;
#[path = "cases/retention.rs"]
mod retention;
mod support;

use std::{fs, path::Path, sync::atomic::Ordering};

use anyhow::Result;
use object_store::path::Path as ObjectPath;
use rusqlite::Connection;
use tako_terse_prototype::{Capture, restore_remote, upload_pending};
use terse_litestream::{FileStore, ReplicaStore};

use support::{Process, Remote};

fn inserts(first: i64, last: i64) -> String {
    let mut sql = "BEGIN IMMEDIATE;".to_string();
    for id in first..=last {
        sql.push_str(&format!("INSERT INTO items VALUES({id},'value-{id}');"));
    }
    sql.push_str("COMMIT;");
    sql
}

fn verify(path: &Path, expected: i64) -> Result<()> {
    let connection = Connection::open(path)?;
    let integrity: String = connection.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
    assert_eq!(integrity, "ok");
    let mut query = connection.prepare("SELECT id, value FROM items ORDER BY id")?;
    let rows: Vec<(i64, String)> = query
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let expected: Vec<_> = (1..=expected)
        .map(|id| (id, format!("value-{id}")))
        .collect();
    assert_eq!(rows, expected);
    Ok(())
}

#[tokio::test]
async fn separate_writers_and_external_checkpoints_restore_latest_and_history() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let database = dir.path().join("app.sqlite");
    let spool = dir.path().join("spool");
    let remote = Remote::new(&dir.path().join("remote"))?;
    let prefix = ObjectPath::from("app");
    let mut first = Process::start("writer", &database, &spool)?;
    let mut second = Process::start("writer", &database, &spool)?;
    let mut capture = Capture::open(&database, &spool)?;
    let mut historical = 0;
    for round in 0..4 {
        let start = round * 50 + 1;
        first.send(&inserts(start, start + 24))?;
        second.send(&inserts(start + 25, start + 49))?;
        capture.sync()?;
        assert_eq!(first.receive()?, "ok");
        assert_eq!(second.receive()?, "ok");
        let txid = capture.sync()?;
        upload_pending(&capture.spool(), &remote, &prefix).await?;
        if round == 1 {
            historical = txid;
        }
    }
    first.request("PRAGMA wal_checkpoint(PASSIVE);")?;
    capture.sync()?;
    upload_pending(&capture.spool(), &remote, &prefix).await?;
    assert_eq!(remote.calls.lists.load(Ordering::SeqCst), 0);
    drop(capture);
    drop(first);
    drop(second);
    fs::remove_dir_all(&spool)?;
    fs::remove_file(&database)?;
    let latest = dir.path().join("latest.sqlite");
    restore_remote(&remote, &prefix, &latest, None).await?;
    verify(&latest, 200)?;
    let old = dir.path().join("history.sqlite");
    assert_eq!(
        restore_remote(&remote, &prefix, &old, Some(historical)).await?,
        historical
    );
    verify(&old, 100)?;
    Ok(())
}

#[tokio::test]
async fn capture_process_crash_preserves_pending_segments_and_writer_can_continue() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let database = dir.path().join("app.sqlite");
    let spool = dir.path().join("spool");
    let prefix = ObjectPath::from("app");
    let remote = Remote::new(&dir.path().join("remote"))?;
    let mut writer = Process::start("writer", &database, &spool)?;
    writer.request(&inserts(1, 12))?;
    let mut process = Process::start("capture", &database, &spool)?;
    let historical: u64 = process.request("sync")?.parse()?;
    process.crash()?;
    writer.request(&inserts(13, 24))?;
    writer.request("PRAGMA wal_checkpoint(TRUNCATE);")?;
    let mut resumed = Capture::open(&database, &spool)?;
    resumed.sync()?;
    upload_pending(&resumed.spool(), &remote, &prefix).await?;
    drop(resumed);
    drop(writer);
    fs::remove_dir_all(&spool)?;
    fs::remove_file(&database)?;
    let latest = dir.path().join("latest.sqlite");
    restore_remote(&remote, &prefix, &latest, None).await?;
    verify(&latest, 24)?;
    let old = dir.path().join("history.sqlite");
    restore_remote(&remote, &prefix, &old, Some(historical)).await?;
    verify(&old, 12)?;
    Ok(())
}

#[tokio::test]
async fn writer_crash_does_not_publish_uncommitted_rows() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let database = dir.path().join("app.sqlite");
    let spool = dir.path().join("spool");
    let remote = Remote::new(&dir.path().join("remote"))?;
    let prefix = ObjectPath::from("app");
    let mut writer = Process::start("writer", &database, &spool)?;
    let mut capture = Capture::open(&database, &spool)?;
    writer.request(&inserts(1, 5))?;
    capture.sync()?;
    writer.request("BEGIN IMMEDIATE; INSERT INTO items VALUES(999,'uncommitted');")?;
    writer.crash()?;
    capture.sync()?;
    upload_pending(&capture.spool(), &remote, &prefix).await?;
    let restored = dir.path().join("restored.sqlite");
    restore_remote(&remote, &prefix, &restored, None).await?;
    verify(&restored, 5)?;
    Ok(())
}

#[tokio::test]
async fn failed_upload_retries_and_idle_ticks_make_no_remote_requests() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let database = dir.path().join("app.sqlite");
    let spool = dir.path().join("spool");
    let remote = Remote::new(&dir.path().join("remote"))?;
    let prefix = ObjectPath::from("app");
    let mut writer = Process::start("writer", &database, &spool)?;
    writer.request(&inserts(1, 10))?;
    let mut capture = Capture::open(&database, &spool)?;
    capture.sync()?;
    remote.calls.fail_before.store(true, Ordering::SeqCst);
    assert!(
        upload_pending(&capture.spool(), &remote, &prefix)
            .await
            .is_err()
    );
    writer.request(&inserts(11, 20))?;
    capture.sync()?;
    let count = upload_pending(&capture.spool(), &remote, &prefix).await?;
    assert!(count > 0);
    let puts = remote.calls.puts.load(Ordering::SeqCst);
    for _ in 0..100 {
        capture.sync()?;
        assert_eq!(upload_pending(&capture.spool(), &remote, &prefix).await?, 0);
    }
    // A fresh uploader uses durable local receipts, without querying the bucket.
    assert_eq!(
        upload_pending(&FileStore::new(&spool), &remote, &prefix).await?,
        0
    );
    assert_eq!(remote.calls.puts.load(Ordering::SeqCst), puts);
    assert_eq!(remote.calls.gets.load(Ordering::SeqCst), 0);
    assert_eq!(remote.calls.lists.load(Ordering::SeqCst), 0);
    let restored = dir.path().join("restored.sqlite");
    restore_remote(&remote, &prefix, &restored, None).await?;
    verify(&restored, 20)?;
    Ok(())
}

#[tokio::test]
async fn conflicting_remote_object_is_not_overwritten_or_acknowledged() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let database = dir.path().join("app.sqlite");
    let spool = dir.path().join("spool");
    let remote_dir = dir.path().join("remote");
    let remote = Remote::new(&remote_dir)?;
    let prefix = ObjectPath::from("app");
    let mut writer = Process::start("writer", &database, &spool)?;
    writer.request(&inserts(1, 10))?;
    let mut capture = Capture::open(&database, &spool)?;
    capture.sync()?;
    let segment = capture.spool().list(0)?[0];
    let path = remote_dir.join("app/ltx/0").join(segment.filename());
    fs::create_dir_all(path.parent().unwrap())?;
    fs::write(&path, b"another database")?;
    assert!(
        upload_pending(&capture.spool(), &remote, &prefix)
            .await
            .is_err()
    );
    assert_eq!(fs::read(&path)?, b"another database");
    assert!(
        !spool
            .join("uploaded")
            .join(format!("0-{}", segment.filename()))
            .exists()
    );
    assert_eq!(remote.calls.lists.load(Ordering::SeqCst), 0);
    Ok(())
}

#[tokio::test]
async fn lost_upload_ack_is_verified_before_persisting_receipt() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let database = dir.path().join("app.sqlite");
    let spool = dir.path().join("spool");
    let remote = Remote::new(&dir.path().join("remote"))?;
    let prefix = ObjectPath::from("app");
    let mut writer = Process::start("writer", &database, &spool)?;
    writer.request(&inserts(1, 10))?;
    let mut capture = Capture::open(&database, &spool)?;
    capture.sync()?;
    remote.calls.lose_ack.store(true, Ordering::SeqCst);
    assert!(
        upload_pending(&capture.spool(), &remote, &prefix)
            .await
            .is_err()
    );
    assert!(upload_pending(&FileStore::new(&spool), &remote, &prefix).await? > 0);
    assert_eq!(remote.calls.gets.load(Ordering::SeqCst), 1);
    assert_eq!(remote.calls.lists.load(Ordering::SeqCst), 0);
    let puts = remote.calls.puts.load(Ordering::SeqCst);
    assert_eq!(upload_pending(&capture.spool(), &remote, &prefix).await?, 0);
    assert_eq!(remote.calls.puts.load(Ordering::SeqCst), puts);
    let restored = dir.path().join("restored.sqlite");
    restore_remote(&remote, &prefix, &restored, None).await?;
    verify(&restored, 10)?;
    Ok(())
}

#[tokio::test]
async fn missing_or_corrupt_remote_segment_fails_without_publishing_restore() -> Result<()> {
    for corrupt in [false, true] {
        let dir = tempfile::tempdir()?;
        let database = dir.path().join("app.sqlite");
        let spool = dir.path().join("spool");
        let remote_dir = dir.path().join("remote");
        let remote = Remote::new(&remote_dir)?;
        let prefix = ObjectPath::from("app");
        let mut writer = Process::start("writer", &database, &spool)?;
        let mut capture = Capture::open(&database, &spool)?;
        for id in 1..=3 {
            writer.request(&inserts(id, id))?;
            capture.sync()?;
        }
        let segments = capture.spool().list(0)?;
        assert!(segments.len() >= 3);
        upload_pending(&capture.spool(), &remote, &prefix).await?;
        let damaged = remote_dir.join("app/ltx/0").join(segments[1].filename());
        if corrupt {
            fs::write(&damaged, b"corrupt")?;
        } else {
            fs::remove_file(&damaged)?;
        }
        let output = dir.path().join("restored.sqlite");
        assert!(
            restore_remote(&remote, &prefix, &output, None)
                .await
                .is_err()
        );
        assert!(!output.exists());
    }
    Ok(())
}

#[test]
fn capture_spool_rejects_another_owner_or_database() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let database = dir.path().join("app.sqlite");
    let other = dir.path().join("other.sqlite");
    Connection::open(&database)?;
    Connection::open(&other)?;
    let spool = dir.path().join("spool");
    let capture = Capture::open(&database, &spool)?;
    assert!(Capture::open(&database, &spool).is_err());
    drop(capture);
    assert!(Capture::open(&other, &spool).is_err());
    Ok(())
}
