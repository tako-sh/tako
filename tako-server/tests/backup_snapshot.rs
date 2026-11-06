use std::process::Command;
use std::time::{Duration, Instant};

#[test]
fn snapshot_waits_for_a_writer_and_includes_its_commit() {
    for journal in ["WAL", "DELETE"] {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("data");
        let dest = temp.path().join("snapshot");
        std::fs::create_dir_all(source.join("app")).unwrap();
        let db = rusqlite::Connection::open(source.join("app/database")).unwrap();
        db.pragma_update(None, "journal_mode", journal).unwrap();
        db.execute_batch(
            "PRAGMA secure_delete=OFF;
             CREATE TABLE discarded (value);
             INSERT INTO discarded VALUES ('erased-backup-marker');
             DELETE FROM discarded;
             CREATE TABLE items (id INTEGER);
             BEGIN IMMEDIATE;
             INSERT INTO items VALUES (42);",
        )
        .unwrap();
        let mut worker = Command::new(env!("CARGO_BIN_EXE_tako-server"))
            .arg("--snapshot-data")
            .arg(temp.path())
            .arg("data")
            .arg(&dest)
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !dest.is_dir() {
            assert!(worker.try_wait().unwrap().is_none());
            assert!(Instant::now() < deadline, "snapshot worker did not start");
            std::thread::sleep(Duration::from_millis(10));
        }
        std::thread::sleep(Duration::from_millis(100));
        assert!(
            worker.try_wait().unwrap().is_none(),
            "writer was still uncommitted in {journal} mode"
        );
        db.execute_batch("COMMIT").unwrap();
        assert!(worker.wait().unwrap().success());
        assert!(!dest.join("app/database-wal").exists());
        assert!(!dest.join("app/database-journal").exists());
        let path = dest.join("app/database");
        let marker = b"erased-backup-marker";
        assert!(
            !std::fs::read(&path)
                .unwrap()
                .windows(marker.len())
                .any(|bytes| bytes == marker),
            "{journal} snapshot retained deleted row bytes"
        );
        let restored = rusqlite::Connection::open(path).unwrap();
        let value: i64 = restored
            .query_row("SELECT id FROM items", [], |row| row.get(0))
            .unwrap();
        assert_eq!(value, 42, "{journal}");
    }
}
