use std::{
    sync::atomic::Ordering,
    time::{Duration, Instant},
};

use anyhow::Result;
use object_store::{memory::InMemory, path::Path as ObjectPath};
use tako_terse_prototype::{
    BACKUP_INTERVAL, BackupLoop, Capture, MAX_RETRY_INTERVAL, restore_remote,
};

use crate::support::{Process, Remote};

#[tokio::test]
async fn default_batches_writes_instead_of_uploading_a_file_for_every_tick() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let database = dir.path().join("app.sqlite");
    let spool = dir.path().join("spool");
    let remote = Remote::new(&dir.path().join("remote"))?;
    let prefix = ObjectPath::from("app");
    let mut writer = Process::start("writer", &database, &spool)?;
    let mut capture = Capture::open(&database, &spool)?;
    let now = Instant::now();
    let mut backup = BackupLoop::new(now);
    assert_eq!(backup.tick(now, &mut capture, &remote, &prefix).await?, 1);
    let puts = remote.calls.puts.load(Ordering::SeqCst);
    for second in 1..60 {
        writer.request(&format!(
            "INSERT INTO items VALUES({second},'value-{second}');"
        ))?;
        assert_eq!(
            backup
                .tick(
                    now + Duration::from_secs(second),
                    &mut capture,
                    &remote,
                    &prefix
                )
                .await?,
            0
        );
    }
    assert_eq!(remote.calls.puts.load(Ordering::SeqCst), puts);
    assert_eq!(
        backup
            .tick(now + BACKUP_INTERVAL, &mut capture, &remote, &prefix)
            .await?,
        1
    );
    assert_eq!(remote.calls.puts.load(Ordering::SeqCst), puts + 1);
    for minute in 2..=100 {
        assert_eq!(
            backup
                .tick(
                    now + BACKUP_INTERVAL * minute,
                    &mut capture,
                    &remote,
                    &prefix
                )
                .await?,
            0
        );
    }
    assert_eq!(remote.calls.puts.load(Ordering::SeqCst), puts + 1);
    assert_eq!(remote.calls.gets.load(Ordering::SeqCst), 0);
    assert_eq!(remote.calls.lists.load(Ordering::SeqCst), 0);
    Ok(())
}

#[tokio::test]
async fn upload_backoff_keeps_capturing_and_caps_retry_frequency() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let database = dir.path().join("app.sqlite");
    let spool = dir.path().join("spool");
    let remote = Remote::from_store(std::sync::Arc::new(InMemory::new()));
    let prefix = ObjectPath::from("app");
    let mut writer = Process::start("writer", &database, &spool)?;
    let mut capture = Capture::open(&database, &spool)?;
    let mut now = Instant::now();
    let mut backup = BackupLoop::new(now);
    let mut delay = BACKUP_INTERVAL;
    for id in 1..=8 {
        writer.request(&format!("INSERT INTO items VALUES({id},'value-{id}');"))?;
        remote.calls.fail_before.store(true, Ordering::SeqCst);
        assert!(
            backup
                .tick(now, &mut capture, &remote, &prefix)
                .await
                .is_err()
        );
        let puts = remote.calls.puts.load(Ordering::SeqCst);
        assert_eq!(
            backup
                .tick(
                    now + delay - Duration::from_millis(1),
                    &mut capture,
                    &remote,
                    &prefix
                )
                .await?,
            0
        );
        assert_eq!(remote.calls.puts.load(Ordering::SeqCst), puts);
        now += delay;
        delay = (delay * 2).min(MAX_RETRY_INTERVAL);
    }
    assert!(backup.tick(now, &mut capture, &remote, &prefix).await? > 0);
    backup
        .tick(now + BACKUP_INTERVAL, &mut capture, &remote, &prefix)
        .await?;
    assert_eq!(remote.calls.gets.load(Ordering::SeqCst), 0);
    assert_eq!(remote.calls.lists.load(Ordering::SeqCst), 0);
    let output = dir.path().join("restored.sqlite");
    restore_remote(&remote, &prefix, &output, None).await?;
    crate::verify(&output, 8)?;
    Ok(())
}
