use anyhow::{Result, ensure};
use rusqlite::Connection;
use std::{
    process::{Child, Command, Stdio},
    sync::Arc,
    time::{Duration, Instant},
};
use terse_litestream::{Database, FileStore, Options, ReplicaStore, restore};

#[test]
fn process_death_before_and_after_publication_preserves_committed_prefix() -> Result<()> {
    for mode in ["before", "after"] {
        for _ in 0..3 {
            let dir = tempfile::tempdir()?;
            let mut child = KillOnDrop(
                Command::new(env!("CARGO_BIN_EXE_crash-fixture"))
                    .arg(dir.path())
                    .arg(mode)
                    .stdin(Stdio::piped())
                    .spawn()?,
            );
            let deadline = Instant::now() + Duration::from_secs(15);
            while !dir.path().join("ready").exists() {
                ensure!(child.0.try_wait()?.is_none(), "fixture exited early");
                ensure!(Instant::now() < deadline, "fixture timed out");
                std::thread::sleep(Duration::from_millis(5));
            }
            child.0.kill()?;
            child.0.wait()?;
            let store = Arc::new(FileStore::new(dir.path().join("replica")));
            let expected = if mode == "before" { 1 } else { 2 };
            assert_eq!(store.list(0)?.len(), expected);
            let output = dir.path().join("recovered.sqlite");
            assert_eq!(restore(store.as_ref(), &output, None)?, expected as u64);
            assert_eq!(row_count(&output)?, expected as i64);
            let mut db =
                Database::with_store(dir.path().join("data.sqlite"), Options::default(), store)?;
            assert_eq!(db.sync()?.txid, expected as u64 + 1);
            let output = dir.path().join("latest.sqlite");
            restore(db.store(), &output, None)?;
            assert_eq!(row_count(&output)?, 2);
        }
    }
    Ok(())
}
fn row_count(path: &std::path::Path) -> Result<i64> {
    let sql = Connection::open(path)?;
    assert_eq!(
        sql.query_row("PRAGMA integrity_check", [], |r| r.get::<_, String>(0))?,
        "ok"
    );
    Ok(sql.query_row("SELECT count(*) FROM data", [], |r| r.get(0))?)
}
struct KillOnDrop(Child);
impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
