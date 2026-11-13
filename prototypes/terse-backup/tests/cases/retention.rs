use std::{
    fs,
    io::Write,
    sync::atomic::Ordering,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::Result;
use object_store::path::Path as ObjectPath;
use tako_terse_prototype::{Capture, RETENTION, prune_expired, restore_remote, upload_pending};
use terse_litestream::{FileStore, ReplicaStore};
use terse_ltx::{Decoder, Encoder};

use crate::{
    support::{Process, Remote},
    verify,
};

const DAY: Duration = Duration::from_secs(24 * 60 * 60);

fn at(day: u32) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(1_700_000_000) + DAY * day
}

// Advance capture ages without sleeping or changing the machine clock. Re-encoding
// updates the file checksum as well as the real LTX header timestamp.
fn date_capture(spool: &FileStore, after: u64, date: SystemTime) -> Result<()> {
    for segment in spool.list(0)?.into_iter().filter(|s| s.max_txid > after) {
        let mut decoder = Decoder::new(spool.open(&segment)?)?;
        let mut header = *decoder.header();
        header.timestamp = date.duration_since(UNIX_EPOCH)?.as_millis().try_into()?;
        let mut bytes = Vec::new();
        let mut encoder = Encoder::new(&mut bytes, header)?;
        while let Some(page) = decoder.next_page()? {
            encoder.write_page(page.number, &page.data)?;
        }
        encoder.finish(decoder.trailer().unwrap().post_apply_checksum)?;
        let path = spool.path(&segment);
        let mut replacement = tempfile::NamedTempFile::new_in(path.parent().unwrap())?;
        replacement.write_all(&bytes)?;
        replacement.as_file().sync_all()?;
        replacement.persist(&path)?;
    }
    Ok(())
}

async fn restore_point(remote: &Remote, prefix: &ObjectPath, txid: u64, rows: i64) -> Result<()> {
    let dir = tempfile::tempdir()?;
    let output = dir.path().join("restore.sqlite");
    assert_eq!(
        restore_remote(remote, prefix, &output, Some(txid)).await?,
        txid
    );
    verify(&output, rows)
}

#[tokio::test]
async fn seven_day_pruning_keeps_every_retained_point_and_rolls_the_anchor_forward() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let database = dir.path().join("app.sqlite");
    let spool = dir.path().join("spool");
    let remote = Remote::new(&dir.path().join("remote"))?;
    let prefix = ObjectPath::from("app");
    let mut writer = Process::start("writer", &database, &spool)?;
    let mut capture = Capture::open(&database, &spool)?;
    let mut previous = 0;
    let mut points = Vec::new();
    for day in 0..=11 {
        let rows = i64::from(day + 1);
        writer.request(&format!("INSERT INTO items VALUES({rows},'value-{rows}');"))?;
        let txid = capture.sync()?;
        date_capture(&capture.spool(), previous, at(day))?;
        previous = txid;
        upload_pending(&capture.spool(), &remote, &prefix).await?;
        points.push((day, txid, rows));
        prune_expired(&capture.spool(), &remote, &prefix, at(day)).await?;
    }
    assert_eq!(RETENTION, DAY * 7);
    assert_eq!(remote.calls.lists.load(Ordering::SeqCst), 0);
    assert_eq!(remote.calls.gets.load(Ordering::SeqCst), 0);
    assert_eq!(capture.spool().list(9)?.len(), 1);
    assert_eq!(capture.spool().list(0)?.len(), 8);
    let oldest = points[0].1;
    drop(capture);
    drop(writer);
    fs::remove_dir_all(&spool)?;
    fs::remove_file(database)?;
    for &(day, txid, rows) in &points {
        if day >= 4 {
            restore_point(&remote, &prefix, txid, rows).await?;
        }
    }
    let latest = dir.path().join("latest.sqlite");
    restore_remote(&remote, &prefix, &latest, None).await?;
    verify(&latest, 12)?;
    let expired = dir.path().join("expired.sqlite");
    assert!(
        restore_remote(&remote, &prefix, &expired, Some(oldest))
            .await
            .is_err()
    );
    assert!(!expired.exists());
    Ok(())
}

#[tokio::test]
async fn failed_anchor_upload_and_lost_ack_delete_nothing_and_resume_after_restart() -> Result<()> {
    for lost_ack in [false, true] {
        let dir = tempfile::tempdir()?;
        let database = dir.path().join("app.sqlite");
        let spool = dir.path().join("spool");
        let remote = Remote::new(&dir.path().join("remote"))?;
        let prefix = ObjectPath::from("app");
        let mut writer = Process::start("writer", &database, &spool)?;
        let mut capture = Capture::open(&database, &spool)?;
        let mut previous = 0;
        let mut points = Vec::new();
        for day in [0, 1, 2, 8] {
            let rows = points.len() as i64 + 1;
            writer.request(&format!("INSERT INTO items VALUES({rows},'value-{rows}');"))?;
            let txid = capture.sync()?;
            date_capture(&capture.spool(), previous, at(day))?;
            previous = txid;
            upload_pending(&capture.spool(), &remote, &prefix).await?;
            points.push((txid, rows));
        }
        if lost_ack {
            remote.calls.lose_ack.store(true, Ordering::SeqCst);
        } else {
            remote.calls.fail_before.store(true, Ordering::SeqCst);
        }
        assert!(
            prune_expired(&capture.spool(), &remote, &prefix, at(9))
                .await
                .is_err()
        );
        assert_eq!(remote.calls.deletes.load(Ordering::SeqCst), 0);
        restore_point(&remote, &prefix, points[0].0, 1).await?;
        drop(capture);
        let mut resumed = Capture::open(&database, &spool)?;
        assert!(prune_expired(&resumed.spool(), &remote, &prefix, at(9)).await? > 0);
        writer.request("INSERT INTO items VALUES(5,'value-5');")?;
        resumed.sync()?;
        upload_pending(&resumed.spool(), &remote, &prefix).await?;
        restore_point(&remote, &prefix, points[2].0, 3).await?;
        let output = dir.path().join("latest.sqlite");
        restore_remote(&remote, &prefix, &output, None).await?;
        verify(&output, 5)?;
    }
    Ok(())
}

#[tokio::test]
async fn interrupted_deletion_keeps_restores_working_and_retries_missing_objects() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let database = dir.path().join("app.sqlite");
    let spool = dir.path().join("spool");
    let remote = Remote::new(&dir.path().join("remote"))?;
    let prefix = ObjectPath::from("app");
    let mut writer = Process::start("writer", &database, &spool)?;
    let mut capture = Capture::open(&database, &spool)?;
    let mut previous = 0;
    let mut retained = 0;
    for day in 0..=10 {
        let rows = day + 1;
        writer.request(&format!("INSERT INTO items VALUES({rows},'value-{rows}');"))?;
        let txid = capture.sync()?;
        date_capture(&capture.spool(), previous, at(day))?;
        previous = txid;
        if day == 3 {
            retained = txid;
        }
    }
    upload_pending(&capture.spool(), &remote, &prefix).await?;
    remote.calls.fail_delete_at.store(2, Ordering::SeqCst);
    assert!(
        prune_expired(&capture.spool(), &remote, &prefix, at(10))
            .await
            .is_err()
    );
    drop(capture);
    restore_point(&remote, &prefix, retained, 4).await?;
    restore_point(&remote, &prefix, previous, 11).await?;
    let reopened = FileStore::new(&spool);
    prune_expired(&reopened, &remote, &prefix, at(10)).await?;
    assert_eq!(prune_expired(&reopened, &remote, &prefix, at(10)).await?, 0);
    restore_point(&remote, &prefix, retained, 4).await?;
    assert_eq!(reopened.list(9)?.len(), 1);
    assert_eq!(reopened.list(0)?.len(), 8);
    Ok(())
}

#[tokio::test]
async fn idle_database_keeps_one_base_without_remote_maintenance_requests() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let database = dir.path().join("app.sqlite");
    let spool = dir.path().join("spool");
    let remote = Remote::new(&dir.path().join("remote"))?;
    let prefix = ObjectPath::from("app");
    let _writer = Process::start("writer", &database, &spool)?;
    let mut capture = Capture::open(&database, &spool)?;
    let txid = capture.sync()?;
    date_capture(&capture.spool(), 0, at(0))?;
    upload_pending(&capture.spool(), &remote, &prefix).await?;
    let puts = remote.calls.puts.load(Ordering::SeqCst);
    for day in 8..=100 {
        assert_eq!(
            prune_expired(&capture.spool(), &remote, &prefix, at(day)).await?,
            0
        );
    }
    assert_eq!(remote.calls.puts.load(Ordering::SeqCst), puts);
    assert_eq!(remote.calls.gets.load(Ordering::SeqCst), 0);
    assert_eq!(remote.calls.lists.load(Ordering::SeqCst), 0);
    assert_eq!(remote.calls.deletes.load(Ordering::SeqCst), 0);
    restore_point(&remote, &prefix, txid, 0).await
}

#[tokio::test]
async fn backwards_capture_clock_never_prunes_across_a_retained_point() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let database = dir.path().join("app.sqlite");
    let spool = dir.path().join("spool");
    let remote = Remote::new(&dir.path().join("remote"))?;
    let prefix = ObjectPath::from("app");
    let mut writer = Process::start("writer", &database, &spool)?;
    let mut capture = Capture::open(&database, &spool)?;
    let mut previous = 0;
    let mut points = Vec::new();
    for day in [0, 1, 10, 2] {
        let rows = points.len() as i64 + 1;
        writer.request(&format!("INSERT INTO items VALUES({rows},'value-{rows}');"))?;
        let txid = capture.sync()?;
        date_capture(&capture.spool(), previous, at(day))?;
        previous = txid;
        points.push((txid, rows));
    }
    upload_pending(&capture.spool(), &remote, &prefix).await?;
    prune_expired(&capture.spool(), &remote, &prefix, at(14)).await?;
    for &(txid, rows) in &points[2..] {
        restore_point(&remote, &prefix, txid, rows).await?;
    }
    Ok(())
}

#[tokio::test]
async fn missing_local_compaction_input_leaves_remote_history_untouched() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let database = dir.path().join("app.sqlite");
    let spool = dir.path().join("spool");
    let remote = Remote::new(&dir.path().join("remote"))?;
    let prefix = ObjectPath::from("app");
    let mut writer = Process::start("writer", &database, &spool)?;
    let mut capture = Capture::open(&database, &spool)?;
    let mut previous = 0;
    for day in [0, 1, 2, 8] {
        let rows = capture.spool().list(0)?.len() + 1;
        writer.request(&format!("INSERT INTO items VALUES({rows},'value-{rows}');"))?;
        let txid = capture.sync()?;
        date_capture(&capture.spool(), previous, at(day))?;
        previous = txid;
    }
    upload_pending(&capture.spool(), &remote, &prefix).await?;
    let middle = capture.spool().list(0)?[1];
    capture.spool().remove(&middle)?;
    assert!(
        prune_expired(&capture.spool(), &remote, &prefix, at(10))
            .await
            .is_err()
    );
    assert_eq!(remote.calls.deletes.load(Ordering::SeqCst), 0);
    restore_point(&remote, &prefix, previous, 4).await
}
