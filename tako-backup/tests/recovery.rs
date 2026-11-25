mod support;

use std::{collections::BTreeSet, fs, path::Path, sync::Arc, sync::atomic::Ordering};

use anyhow::Result;
use object_store::{ObjectStore, ObjectStoreExt, PutPayload, path::Path as ObjectPath};
use rusqlite::Connection;
use tako_backup::{Point, RETENTION, Replica, Target, encryption::Key, windows};
use terse_litestream::{FileStore, ReplicaStore};

use support::{Process, Remote, capture::Capture};

const PREFIX: &str = "_tako/databases/app/generation";

fn key() -> Key {
    Key::new("database-generation".into(), [7; 32]).unwrap()
}

fn replica(remote: Arc<dyn ObjectStore>) -> Result<Replica> {
    Replica::new(remote, PREFIX, key())
}

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

async fn restore(replica: &Replica, target: Target, output: &Path) -> Result<u64> {
    let segments = replica.segments().await?;
    let txid = Replica::resolve(&segments, target)?;
    let scratch = tempfile::tempdir()?;
    replica
        .restore(&segments, txid, output, scratch.path())
        .await?;
    Ok(txid)
}

fn local_segments(spool: &Path) -> Result<usize> {
    let store = FileStore::new(spool);
    let mut count = 0;
    for level in 0..=9 {
        count += store.list(level)?.len();
    }
    Ok(count)
}

#[tokio::test]
async fn separate_writers_and_external_checkpoints_restore_latest_and_history() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let database = dir.path().join("app.db");
    let spool = dir.path().join("spool");
    let remote = Arc::new(Remote::new(&dir.path().join("remote"))?);
    let replica = replica(remote.clone())?;
    // The writer checkpoints after every page, as an app connection may.
    let mut writer = Process::start("writer", &database, &spool)?;
    let mut capture = Capture::open(&database, &spool, 0)?;
    let base = capture.snapshot()?;
    let mut history = Vec::new();
    for batch in 0..5 {
        writer.request(&inserts(batch * 10 + 1, batch * 10 + 10))?;
        history.push(((batch + 1) * 10, capture.sync()?));
        replica.upload(&spool).await?;
    }
    assert!(history.iter().all(|(_, txid)| *txid > base));
    // Uploaded segments leave the spool, except the newest.
    assert_eq!(local_segments(&spool)?, 1);
    drop(capture);
    drop(writer);

    let latest = dir.path().join("latest.db");
    restore(&replica, Target::Latest, &latest).await?;
    verify(&latest, 50)?;
    for (rows, txid) in history {
        let output = dir.path().join(format!("at-{txid}.db"));
        restore(&replica, Target::Txid(txid), &output).await?;
        verify(&output, rows)?;
    }
    Ok(())
}

#[tokio::test]
async fn restoring_on_the_same_spool_continues_above_the_existing_history() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let database = dir.path().join("app.db");
    let spool = dir.path().join("spool");
    let remote = Arc::new(Remote::new(&dir.path().join("remote"))?);
    let replica = replica(remote)?;
    let mut writer = Process::start("writer", &database, &spool)?;
    let mut capture = Capture::open(&database, &spool, 0)?;
    capture.snapshot()?;
    writer.request(&inserts(1, 5))?;
    let older = capture.sync()?;
    writer.request(&inserts(6, 10))?;
    let newer = capture.sync()?;
    replica.upload(&spool).await?;
    capture.close()?;
    drop(writer);

    // Replace the database with its older state, as a restore does while
    // the database is paused, then capture again with the same spool.
    let restored = dir.path().join("restored.db");
    restore(&replica, Target::Txid(older), &restored).await?;
    fs::remove_file(format!("{}-wal", database.display())).ok();
    fs::remove_file(format!("{}-shm", database.display())).ok();
    fs::rename(&restored, &database)?;
    let mut capture = Capture::open(&database, &spool, 0)?;
    let reopened = capture.snapshot()?;
    assert!(reopened > newer);
    replica.upload(&spool).await?;

    let latest = dir.path().join("latest.db");
    assert_eq!(restore(&replica, Target::Latest, &latest).await?, reopened);
    verify(&latest, 5)?;
    let before = dir.path().join("before.db");
    restore(&replica, Target::Txid(newer), &before).await?;
    verify(&before, 10)?;
    Ok(())
}

#[tokio::test]
async fn restores_each_history_when_independent_ltx_sources_leave_a_gap() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let remote = Arc::new(Remote::new(&dir.path().join("remote"))?);
    let replica = replica(remote)?;
    let old = dir.path().join("old.db");
    let old_spool = dir.path().join("old-spool");
    let mut old_writer = Process::start("writer", &old, &old_spool)?;
    let mut old_capture = Capture::open(&old, &old_spool, 0)?;
    old_capture.snapshot()?;
    old_writer.request(&inserts(1, 5))?;
    let moved_at = old_capture.sync()?;
    replica.upload(&old_spool).await?;

    // The new server restores the latest state into its own file and
    // captures it with an empty spool.
    let new = dir.path().join("new.db");
    restore(&replica, Target::Latest, &new).await?;
    let new_spool = dir.path().join("new-spool");
    let floor = moved_at + 100;
    let mut new_capture = Capture::open(&new, &new_spool, floor)?;
    let base = new_capture.snapshot()?;
    assert_eq!(base, floor + 1);
    replica.upload(&new_spool).await?;

    // The old server has not noticed yet and uploads more of its own.
    old_writer.request(&inserts(6, 8))?;
    let stale = old_capture.sync()?;
    replica.upload(&old_spool).await?;
    let mut new_writer = Process::start("writer", &new, &new_spool)?;
    new_writer.request(&inserts(6, 20))?;
    new_capture.sync()?;
    replica.upload(&new_spool).await?;

    let segments = replica.segments().await?;
    assert_eq!(windows(&segments).len(), 2);
    let latest = dir.path().join("latest.db");
    restore(&replica, Target::Latest, &latest).await?;
    verify(&latest, 20)?;
    let old_timeline = dir.path().join("old-timeline.db");
    restore(&replica, Target::Txid(stale), &old_timeline).await?;
    verify(&old_timeline, 8)?;
    Ok(())
}

#[tokio::test]
async fn a_time_restores_the_newest_capture_at_or_before_it() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let database = dir.path().join("app.db");
    let spool = dir.path().join("spool");
    let remote = Arc::new(Remote::new(&dir.path().join("remote"))?);
    let replica = replica(remote)?;
    let mut writer = Process::start("writer", &database, &spool)?;
    let mut capture = Capture::open(&database, &spool, 0)?;
    capture.snapshot()?;
    replica.upload(&spool).await?;
    let mut times = Vec::new();
    for batch in 0..4 {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        writer.request(&inserts(batch * 5 + 1, batch * 5 + 5))?;
        capture.sync()?;
        times.push(((batch + 1) * 5, chrono_millis()));
        replica.upload(&spool).await?;
    }
    for (rows, at) in times {
        let output = dir.path().join(format!("{rows}.db"));
        restore(&replica, Target::Time(at), &output).await?;
        verify(&output, rows)?;
    }
    let segments = replica.segments().await?;
    let error = Replica::resolve(&segments, Target::Time(0)).unwrap_err();
    assert!(
        error.downcast_ref::<tako_backup::TooEarly>().is_some(),
        "{error}"
    );
    Ok(())
}

#[tokio::test]
async fn a_time_restores_from_a_base_uploaded_long_after_its_capture() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let database = dir.path().join("app.db");
    let spool = dir.path().join("spool");
    let remote = Arc::new(Remote::new(&dir.path().join("remote"))?);
    let replica = replica(remote)?;
    let mut writer = Process::start("writer", &database, &spool)?;
    writer.request(&inserts(1, 3))?;
    // Storage is unavailable: the base and a capture wait in the spool.
    let mut capture = Capture::open(&database, &spool, 0)?;
    let base = capture.snapshot()?;
    let base_at = chrono_millis();
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    writer.request(&inserts(4, 6))?;
    capture.sync()?;
    let between = chrono_millis();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    // Storage is back. No restore point names the base, as after a crash
    // before it was recorded: the base's name carries its capture time.
    replica.upload(&spool).await?;
    assert!(replica.points().await?.is_empty());
    let segments = replica.segments().await?;
    let stored = segments
        .iter()
        .find(|stored| stored.txid() == base)
        .unwrap();
    assert!(stored.captured_at_ms <= base_at && stored.uploaded_at_ms > between);
    let output = dir.path().join("between.db");
    restore(&replica, Target::Time(between), &output).await?;
    verify(&output, 6)?;
    let early = dir.path().join("early.db");
    restore(&replica, Target::Time(base_at), &early).await?;
    verify(&early, 3)?;
    drop(capture);
    drop(writer);
    Ok(())
}

#[tokio::test]
async fn large_segments_upload_in_parts_and_restore() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let database = dir.path().join("app.db");
    let spool = dir.path().join("spool");
    let remote = Arc::new(Remote::new(&dir.path().join("remote"))?);
    // Every segment counts as large here, in parts of 1 KiB.
    let replica = replica(remote.clone())?.with_multipart(0, 1024);
    let mut writer = Process::start("writer", &database, &spool)?;
    writer.request(&inserts(1, 200))?;
    let mut capture = Capture::open(&database, &spool, 0)?;
    capture.snapshot()?;
    writer.request(&inserts(201, 210))?;
    capture.sync()?;
    remote.calls.lose_ack.store(true, Ordering::SeqCst);
    assert!(replica.upload(&spool).await.is_err());
    // The retry finds the first segment written and compares it.
    replica.upload(&spool).await?;
    assert!(Replica::uploaded(&spool)?);
    assert!(
        remote.calls.puts.load(Ordering::SeqCst) > 2,
        "the segments went up in parts"
    );
    drop(capture);
    drop(writer);
    let output = dir.path().join("out.db");
    restore(&replica, Target::Latest, &output).await?;
    verify(&output, 210)?;
    Ok(())
}

fn chrono_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

#[tokio::test]
async fn failed_uploads_keep_segments_and_idle_uploads_make_no_requests() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let database = dir.path().join("app.db");
    let spool = dir.path().join("spool");
    let remote = Arc::new(Remote::new(&dir.path().join("remote"))?);
    let replica = replica(remote.clone())?;
    let mut writer = Process::start("writer", &database, &spool)?;
    let mut capture = Capture::open(&database, &spool, 0)?;
    capture.snapshot()?;
    writer.request(&inserts(1, 3))?;
    capture.sync()?;
    remote.calls.fail_before.store(true, Ordering::SeqCst);
    assert!(replica.upload(&spool).await.is_err());
    assert!(!Replica::uploaded(&spool)?);
    // The base and the capture.
    assert_eq!(replica.upload(&spool).await?.len(), 2);
    assert!(Replica::uploaded(&spool)?);
    let calls =
        remote.calls.puts.load(Ordering::SeqCst) + remote.calls.lists.load(Ordering::SeqCst);
    assert!(replica.upload(&spool).await?.is_empty());
    assert_eq!(
        remote.calls.puts.load(Ordering::SeqCst) + remote.calls.lists.load(Ordering::SeqCst),
        calls,
        "an idle upload makes no requests"
    );
    drop(capture);
    drop(writer);
    let output = dir.path().join("out.db");
    restore(&replica, Target::Latest, &output).await?;
    verify(&output, 3)?;
    Ok(())
}

#[tokio::test]
async fn a_lost_upload_acknowledgement_is_verified_before_the_receipt() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let database = dir.path().join("app.db");
    let spool = dir.path().join("spool");
    let remote = Arc::new(Remote::new(&dir.path().join("remote"))?);
    let replica = replica(remote.clone())?;
    let mut writer = Process::start("writer", &database, &spool)?;
    let mut capture = Capture::open(&database, &spool, 0)?;
    capture.snapshot()?;
    writer.request(&inserts(1, 3))?;
    capture.sync()?;
    remote.calls.lose_ack.store(true, Ordering::SeqCst);
    assert!(replica.upload(&spool).await.is_err());
    // The retry finds the object written and compares it.
    replica.upload(&spool).await?;
    assert!(Replica::uploaded(&spool)?);
    Ok(())
}

#[tokio::test]
async fn a_conflicting_remote_object_is_not_overwritten_or_acknowledged() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let database = dir.path().join("app.db");
    let spool = dir.path().join("spool");
    let remote = Arc::new(Remote::new(&dir.path().join("remote"))?);
    let replica = replica(remote.clone())?;
    let writer = Process::start("writer", &database, &spool)?;
    let mut capture = Capture::open(&database, &spool, 0)?;
    let txid = capture.snapshot()?;
    // Another writer took this transaction's key.
    let key = ObjectPath::from(format!(
        "{PREFIX}/ltx/9/{:016x}-{:016x}-{}.ltx",
        1,
        txid,
        capture.captured_at(txid)?
    ));
    let sealed = {
        let inner = Arc::new(object_store::memory::InMemory::new());
        let store = tako_backup::encryption::EncryptedStore::new(
            inner.clone(),
            vec![self::key()],
            PREFIX.into(),
        )?;
        store
            .put_opts(&key, PutPayload::from_static(b"other"), Default::default())
            .await?;
        inner.get(&key).await?.bytes().await?
    };
    remote
        .put_opts(&key, sealed.into(), Default::default())
        .await?;
    let error = replica.upload(&spool).await.unwrap_err();
    assert!(error.to_string().contains("conflicts"), "{error}");
    assert!(!Replica::uploaded(&spool)?);
    drop(capture);
    drop(writer);
    Ok(())
}

#[tokio::test]
async fn a_missing_or_corrupt_segment_fails_the_restore_without_output() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let database = dir.path().join("app.db");
    let spool = dir.path().join("spool");
    let remote_dir = dir.path().join("remote");
    let remote = Arc::new(Remote::new(&remote_dir)?);
    let replica = replica(remote.clone())?;
    let mut writer = Process::start("writer", &database, &spool)?;
    let mut capture = Capture::open(&database, &spool, 0)?;
    capture.snapshot()?;
    writer.request(&inserts(1, 3))?;
    let first = capture.sync()?;
    writer.request(&inserts(4, 6))?;
    capture.sync()?;
    replica.upload(&spool).await?;
    let segments = replica.segments().await?;
    let middle = segments
        .iter()
        .find(|stored| stored.txid() == first)
        .unwrap();
    let path = ObjectPath::from(format!(
        "{PREFIX}/ltx/0/{:016x}-{:016x}-{}.ltx",
        middle.segment.min_txid, middle.segment.max_txid, middle.captured_at_ms
    ));

    // Corrupt: authentication fails.
    remote
        .put_opts(
            &path,
            PutPayload::from_static(b"garbage"),
            Default::default(),
        )
        .await?;
    let output = dir.path().join("corrupt.db");
    assert!(restore(&replica, Target::Latest, &output).await.is_err());
    assert!(!output.exists());

    // Missing: the chain has a gap, so the latest is no longer restorable.
    remote.delete(&path).await?;
    let segments = replica.segments().await?;
    let latest = Replica::resolve(&segments, Target::Latest)?;
    assert!(latest < first, "only the base before the gap restores");
    Ok(())
}

#[tokio::test]
async fn a_capture_process_crash_keeps_pending_segments_and_writes_continue() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let database = dir.path().join("app.db");
    let spool = dir.path().join("spool");
    let remote = Arc::new(Remote::new(&dir.path().join("remote"))?);
    let replica = replica(remote)?;
    let mut writer = Process::start("writer", &database, &spool)?;
    writer.request(&inserts(1, 4))?;
    let mut capture = Process::start("capture", &database, &spool)?;
    capture.request("sync")?;
    capture.crash()?;
    writer.request(&inserts(5, 9))?;
    let mut capture = Capture::open(&database, &spool, 0)?;
    capture.sync()?;
    replica.upload(&spool).await?;
    drop(capture);
    drop(writer);
    let output = dir.path().join("out.db");
    restore(&replica, Target::Latest, &output).await?;
    verify(&output, 9)?;
    Ok(())
}

#[tokio::test]
async fn a_writer_crash_publishes_no_uncommitted_rows() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let database = dir.path().join("app.db");
    let spool = dir.path().join("spool");
    let remote = Arc::new(Remote::new(&dir.path().join("remote"))?);
    let replica = replica(remote)?;
    let mut writer = Process::start("writer", &database, &spool)?;
    writer.request(&inserts(1, 3))?;
    writer.send("BEGIN IMMEDIATE; INSERT INTO items VALUES(99, 'uncommitted');")?;
    writer.receive()?;
    writer.crash()?;
    let mut capture = Capture::open(&database, &spool, 0)?;
    capture.snapshot()?;
    capture.sync()?;
    replica.upload(&spool).await?;
    drop(capture);
    let output = dir.path().join("out.db");
    restore(&replica, Target::Latest, &output).await?;
    verify(&output, 3)?;
    Ok(())
}

#[test]
fn a_spool_rejects_another_owner_or_database() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let first = dir.path().join("first.db");
    let second = dir.path().join("second.db");
    for path in [&first, &second] {
        Connection::open(path)?.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE t(x)")?;
    }
    let spool = dir.path().join("spool");
    let capture = Capture::open(&first, &spool, 0)?;
    assert!(Capture::open(&first, &spool, 0).is_err());
    drop(capture);
    assert!(Capture::open(&second, &spool, 0).is_err());
    Ok(())
}

#[tokio::test]
async fn retention_keeps_the_last_week_and_the_kept_bases() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let database = dir.path().join("app.db");
    let spool = dir.path().join("spool");
    let remote = Arc::new(Remote::new(&dir.path().join("remote"))?);
    let replica = replica(remote)?;
    let mut writer = Process::start("writer", &database, &spool)?;
    let mut capture = Capture::open(&database, &spool, 0)?;
    // Old history: a base kept as a long-term snapshot, one that is not,
    // and captures between them.
    let kept = capture.snapshot()?;
    writer.request(&inserts(1, 2))?;
    let old_capture = capture.sync()?;
    let dropped = capture.snapshot()?;
    writer.request(&inserts(3, 4))?;
    let anchor_capture = capture.sync()?;
    let anchor = capture.snapshot()?;
    replica.upload(&spool).await?;
    for (txid, reason) in [
        (old_capture, "release"),
        (anchor, "scheduled"),
        (kept, "manual"),
    ] {
        replica
            .put_point(&Point {
                created_at_ms: chrono_millis(),
                txid,
                reason: reason.into(),
            })
            .await?;
    }
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let cutoff = chrono_millis();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    // Recent history.
    writer.request(&inserts(5, 6))?;
    let recent = capture.sync()?;
    replica.upload(&spool).await?;

    let now = cutoff + RETENTION.as_millis() as i64;
    let segments = replica.segments().await?;
    let points = replica.points().await?;
    let pruned = replica
        .prune(&segments, &points, now, &BTreeSet::from([kept]))
        .await?;
    // The capture and the base before the anchor, and the release point.
    assert_eq!(pruned.segments, 3, "{pruned:?}");
    assert_eq!(pruned.points, 1);
    let remaining: BTreeSet<u64> = replica
        .segments()
        .await?
        .iter()
        .map(|stored| stored.txid())
        .collect();
    assert_eq!(remaining, BTreeSet::from([kept, anchor, recent]));
    assert!(!remaining.contains(&dropped) && !remaining.contains(&anchor_capture));
    let reasons: BTreeSet<String> = replica
        .points()
        .await?
        .into_iter()
        .map(|point| point.reason)
        .collect();
    assert_eq!(
        reasons,
        BTreeSet::from(["manual".into(), "scheduled".into()])
    );

    for (txid, rows) in [(kept, 0), (anchor, 4), (recent, 6)] {
        let output = dir.path().join(format!("{txid}.db"));
        restore(&replica, Target::Txid(txid), &output).await?;
        verify(&output, rows)?;
    }
    drop(capture);
    drop(writer);
    Ok(())
}
