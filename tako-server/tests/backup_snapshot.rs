use std::process::Command;
use std::time::{Duration, Instant};

fn can_snapshot_during_wal_write() -> bool {
    #[cfg(target_os = "linux")]
    {
        use landlock::{
            ABI, Access, AccessFs, CompatLevel, Compatible, Ruleset, RulesetAttr, RulesetStatus,
        };
        // Some container policies allow creating rules but deny enforcing them.
        std::thread::spawn(|| {
            Ruleset::default()
                .set_compatibility(CompatLevel::HardRequirement)
                .handle_access(AccessFs::from_all(ABI::V3))
                .and_then(|rules| rules.create())
                .and_then(|rules| rules.restrict_self())
                .is_ok_and(|status| status.ruleset == RulesetStatus::FullyEnforced)
        })
        .join()
        .unwrap()
    }
    #[cfg(not(target_os = "linux"))]
    {
        false
    }
}

#[test]
fn snapshot_captures_committed_rows_with_an_open_writer() {
    for journal in ["WAL", "DELETE"] {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("data");
        let dest = temp.path().join("snapshot");
        std::fs::create_dir_all(source.join("app")).unwrap();
        let db = rusqlite::Connection::open(source.join("app/database")).unwrap();
        db.pragma_update(None, "journal_mode", journal).unwrap();
        if journal == "DELETE" {
            db.pragma_update(None, "synchronous", "OFF").unwrap();
        }
        db.execute_batch(
            "PRAGMA secure_delete=OFF;
             CREATE TABLE discarded (value);
             INSERT INTO discarded VALUES ('erased-backup-marker');
             DELETE FROM discarded;
             CREATE TABLE items (id INTEGER);
             INSERT INTO items VALUES (7);
             BEGIN IMMEDIATE;
             INSERT INTO items VALUES (42);",
        )
        .unwrap();
        if journal == "DELETE" {
            // synchronous=OFF writes the header while RESERVED is still held.
            assert_ne!(
                std::fs::read(source.join("app/database-journal")).unwrap()[0],
                0
            );
        }
        let mut worker = Command::new(env!("CARGO_BIN_EXE_tako-server"))
            .arg("--snapshot-data")
            .arg(temp.path())
            .arg("data")
            .arg(&dest)
            .spawn()
            .unwrap();
        let expected = if journal == "DELETE" || can_snapshot_during_wal_write() {
            assert!(
                worker.wait().unwrap().success(),
                "{journal}: capture should finish while the transaction is open"
            );
            db.execute_batch("ROLLBACK").unwrap();
            7
        } else {
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
            42
        };
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
            .query_row("SELECT max(id) FROM items", [], |row| row.get(0))
            .unwrap();
        assert_eq!(value, expected, "{journal}");
    }
}

#[test]
fn database_snapshot_worker_captures_only_sqlite_databases() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("data");
    std::fs::create_dir_all(source.join("app/media")).unwrap();
    std::fs::create_dir_all(source.join("tako")).unwrap();
    std::fs::write(source.join("app/media/photo.jpg"), b"jpeg").unwrap();
    let db = rusqlite::Connection::open(source.join("app/database")).unwrap();
    db.execute_batch(
        "PRAGMA journal_mode=WAL; CREATE TABLE items (id INTEGER); INSERT INTO items VALUES (3);",
    )
    .unwrap();

    let measure = Command::new(env!("CARGO_BIN_EXE_tako-server"))
        .arg("--measure-databases")
        .arg(temp.path())
        .arg("data")
        .output()
        .unwrap();
    assert!(measure.status.success());
    let measured: u64 = String::from_utf8(measure.stdout)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let on_disk = ["app/database", "app/database-wal"]
        .iter()
        .map(|path| std::fs::metadata(source.join(path)).unwrap().len())
        .sum::<u64>();
    assert_eq!(measured, on_disk);

    let dest = temp.path().join("snapshot");
    let status = Command::new(env!("CARGO_BIN_EXE_tako-server"))
        .arg("--snapshot-databases")
        .arg(temp.path())
        .arg("data")
        .arg(&dest)
        .status()
        .unwrap();
    assert!(status.success());
    assert!(!dest.join("app/media").exists());
    assert!(!dest.join("app/database-wal").exists());
    assert!(dest.join("tako").is_dir());
    let value: i64 = rusqlite::Connection::open(dest.join("app/database"))
        .unwrap()
        .query_row("SELECT id FROM items", [], |row| row.get(0))
        .unwrap();
    assert_eq!(value, 3);
}
