use super::*;
use crate::operations::backup::journal::RollbackDir;
use tempfile::TempDir;

fn database(path: &Path, value: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let db = rusqlite::Connection::open(path).unwrap();
    db.execute_batch("CREATE TABLE IF NOT EXISTS items(value TEXT); DELETE FROM items;")
        .unwrap();
    db.execute("INSERT INTO items VALUES (?1)", [value])
        .unwrap();
}

fn value(path: &Path) -> String {
    rusqlite::Connection::open(path)
        .unwrap()
        .query_row("SELECT value FROM items", [], |row| row.get(0))
        .unwrap()
}

/// A database snapshot archive made by the creation path from `data`.
fn snapshot_archive(temp: &Path, data: &Path) -> PathBuf {
    let snapshot = temp.join("snapshot");
    super::super::snapshot_database_tree(data, Path::new(""), &snapshot).unwrap();
    let archive = temp.join("databases.tar.zst");
    super::super::create_database_snapshot_archive(&snapshot, &archive).unwrap();
    archive
}

enum Entry<'a> {
    Dir(&'a [u8]),
    File(&'a [u8], &'a [u8]),
    Link(&'a [u8], tar::EntryType, &'a str),
}

/// A tar.zst whose entry names are written verbatim, as a hostile archive
/// could, bypassing the path checks of `tar::Builder`.
fn raw_archive(path: &Path, entries: &[Entry<'_>]) {
    let file = File::create(path).unwrap();
    let mut builder = tar::Builder::new(zstd::stream::write::Encoder::new(file, 3).unwrap());
    for entry in entries {
        let mut header = tar::Header::new_gnu();
        let (name, data): (&[u8], &[u8]) = match entry {
            Entry::Dir(name) => {
                header.set_entry_type(tar::EntryType::Directory);
                (name, b"")
            }
            Entry::File(name, data) => {
                header.set_entry_type(tar::EntryType::Regular);
                (name, data)
            }
            Entry::Link(name, kind, target) => {
                header.set_entry_type(*kind);
                if !target.is_empty() {
                    header.set_link_name(target).unwrap();
                }
                (name, b"")
            }
        };
        header.as_old_mut().name[..name.len()].copy_from_slice(name);
        header.set_mode(0o644);
        header.set_size(data.len() as u64);
        header.set_cksum();
        builder.append(&header, data).unwrap();
    }
    builder.into_inner().unwrap().finish().unwrap();
}

fn sqlite_bytes() -> Vec<u8> {
    let temp = TempDir::new().unwrap();
    let path = temp.path().join("db");
    database(&path, "archived");
    std::fs::read(path).unwrap()
}

#[test]
fn extraction_accepts_a_database_snapshot() {
    let temp = TempDir::new().unwrap();
    let data = temp.path().join("data");
    database(&data.join("app/nested/data.sqlite"), "one");
    database(&data.join("tako/workflows.sqlite"), "runs");
    std::fs::create_dir_all(data.join("app/empty")).unwrap();
    std::fs::write(data.join("app/upload.bin"), b"plain").unwrap();
    let archive = snapshot_archive(temp.path(), &data);

    let dest = temp.path().join("extracted");
    let databases = extract_database_snapshot(&archive, &dest, u64::MAX).unwrap();
    assert_eq!(
        databases,
        [
            Path::new("app/nested/data.sqlite"),
            Path::new("tako/workflows.sqlite")
        ]
    );
    assert_eq!(value(&dest.join("databases/app/nested/data.sqlite")), "one");
    assert_eq!(
        std::fs::metadata(dest.join("databases/app/nested/data.sqlite"))
            .unwrap()
            .mode()
            & 0o777,
        0o600
    );
}

#[test]
fn extraction_rejects_malformed_and_malicious_archives() {
    let db = sqlite_bytes();
    let base = || {
        vec![
            Entry::Dir(b"databases/"),
            Entry::Dir(b"databases/app/"),
            Entry::Dir(b"databases/tako/"),
        ]
    };
    let cases: Vec<(&str, Vec<Entry<'_>>, &str)> = vec![
        (
            "symlink",
            vec![Entry::Link(
                b"databases/app/link.sqlite",
                tar::EntryType::Symlink,
                "/etc/passwd",
            )],
            "not a regular file or directory",
        ),
        (
            "hard link",
            vec![Entry::Link(
                b"databases/app/link.sqlite",
                tar::EntryType::Link,
                "databases/app/other",
            )],
            "not a regular file or directory",
        ),
        (
            "parent path",
            vec![Entry::File(b"databases/app/../../escape.sqlite", &db)],
            "unsafe path",
        ),
        (
            "absolute path",
            vec![Entry::File(b"/databases/app/abs.sqlite", &db)],
            "outside databases/",
        ),
        (
            "non-SQLite file",
            vec![Entry::File(b"databases/app/notes.txt", b"plain text")],
            "not a SQLite database",
        ),
        (
            "wrong root",
            vec![Entry::File(b"app/data.sqlite", &db)],
            "outside databases/",
        ),
        (
            "unknown data directory",
            vec![Entry::File(b"databases/releases/data.sqlite", &db)],
            "outside databases/app/ and databases/tako/",
        ),
        (
            "file beside the data directories",
            vec![Entry::File(b"databases/app", &db)],
            "not inside app/ or tako/",
        ),
        (
            "sidecar",
            vec![
                Entry::File(b"databases/app/data.sqlite", &db),
                Entry::File(b"databases/app/data.sqlite-wal", &db),
            ],
            "SQLite sidecar of app/data.sqlite",
        ),
        (
            "transient store",
            vec![Entry::File(b"databases/tako/channels.sqlite", &db)],
            "transient Tako store",
        ),
        (
            "duplicate",
            vec![
                Entry::File(b"databases/app/data.sqlite", &db),
                Entry::File(b"databases/app/data.sqlite", &db),
            ],
            "more than once",
        ),
        (
            "fifo",
            vec![Entry::Link(b"databases/app/pipe", tar::EntryType::Fifo, "")],
            "not a regular file or directory",
        ),
    ];
    for (label, entries, expected) in cases {
        let temp = TempDir::new().unwrap();
        let archive = temp.path().join("archive.tar.zst");
        let mut all = base();
        all.extend(entries);
        raw_archive(&archive, &all);
        let dest = temp.path().join("work/extracted");
        std::fs::create_dir(temp.path().join("work")).unwrap();
        let error = extract_database_snapshot(&archive, &dest, u64::MAX).unwrap_err();
        assert!(error.contains(expected), "{label}: {error}");
        assert!(!temp.path().join("escape.sqlite").exists(), "{label}");
        assert!(!Path::new("/databases").exists(), "{label}");
    }

    let temp = TempDir::new().unwrap();
    let archive = temp.path().join("archive.tar.zst");
    raw_archive(
        &archive,
        &[
            Entry::Dir(b"databases/app/"),
            Entry::File(b"databases/app/data.sqlite", &db),
        ],
    );
    let error = extract_database_snapshot(&archive, &temp.path().join("a"), u64::MAX).unwrap_err();
    assert!(error.contains("missing databases/tako/"), "{error}");

    raw_archive(&archive, &base());
    let error = extract_database_snapshot(&archive, &temp.path().join("b"), u64::MAX).unwrap_err();
    assert!(error.contains("contains no databases"), "{error}");

    let mut over_budget = base();
    over_budget.push(Entry::File(b"databases/app/data.sqlite", &db));
    raw_archive(&archive, &over_budget);
    let error = extract_database_snapshot(&archive, &temp.path().join("c"), db.len() as u64 - 1)
        .unwrap_err();
    assert!(error.contains("Not enough free disk space"), "{error}");
    assert!(!temp.path().join("c/databases/app/data.sqlite").exists());
}

struct Fixture {
    temp: TempDir,
    data: PathBuf,
    staged: PathBuf,
    databases: Vec<PathBuf>,
}

/// Live data and a staged snapshot of a subset of it, taken earlier.
fn fixture() -> Fixture {
    let temp = TempDir::new().unwrap();
    let data = temp.path().join("data");
    database(&data.join("app/data.sqlite"), "snapshot");
    database(&data.join("tako/workflows.sqlite"), "snapshot runs");
    database(&data.join("app/new/created.sqlite"), "created");
    let archive = snapshot_archive(temp.path(), &data);
    let extracted = temp.path().join("extracted");
    let databases = extract_database_snapshot(&archive, &extracted, u64::MAX).unwrap();

    // Newer live state: later writes, sidecars, files the snapshot lacks.
    std::fs::remove_dir_all(data.join("app/new")).unwrap();
    database(&data.join("app/data.sqlite"), "live");
    database(&data.join("tako/workflows.sqlite"), "live runs");
    database(&data.join("app/other.sqlite"), "other");
    std::fs::write(data.join("app/data.sqlite-wal"), b"newer wal").unwrap();
    std::fs::write(data.join("app/data.sqlite-shm"), b"newer shm").unwrap();
    std::fs::write(data.join("tako/workflows.sqlite-journal"), b"newer journal").unwrap();
    std::fs::write(data.join("app/upload.bin"), b"upload").unwrap();
    // Modes the app controls: never carried over to the restored copy.
    std::fs::set_permissions(
        data.join("app/data.sqlite"),
        std::fs::Permissions::from_mode(0o777),
    )
    .unwrap();
    std::fs::set_permissions(
        data.join("tako/workflows.sqlite"),
        std::fs::Permissions::from_mode(0o666),
    )
    .unwrap();
    Fixture {
        staged: extracted.join("databases"),
        temp,
        data,
        databases,
    }
}

fn previous_dirs(temp: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(temp)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with(".data-restore-prev-")
        })
        .map(|entry| entry.path())
        .collect()
}

/// Every directory and file below `root` with its link count, mode, and
/// bytes, so a comparison also catches extra names and stray sidecars.
fn tree_state(root: &Path) -> std::collections::BTreeMap<PathBuf, String> {
    let mut state = std::collections::BTreeMap::new();
    let mut dirs = vec![root.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            let meta = std::fs::symlink_metadata(&path).unwrap();
            let relative = path.strip_prefix(root).unwrap().to_path_buf();
            if meta.is_dir() {
                state.insert(relative, "dir".to_string());
                dirs.push(path);
            } else {
                let bytes = std::fs::read(&path).unwrap();
                state.insert(
                    relative,
                    format!(
                        "links={} mode={:o} {}",
                        meta.nlink(),
                        meta.mode() & 0o7777,
                        hex::encode(sha2::Digest::finalize(
                            <sha2::Sha256 as sha2::Digest>::new_with_prefix(bytes)
                        ))
                    ),
                );
            }
        }
    }
    state
}

/// Checks sidecars before opening databases: SQLite may discard a log it
/// does not recognize when it opens the database.
fn assert_live_state_is_original(fixture: &Fixture) {
    let data = &fixture.data;
    assert_eq!(
        std::fs::read(data.join("app/data.sqlite-wal")).unwrap(),
        b"newer wal"
    );
    assert_eq!(
        std::fs::read(data.join("app/data.sqlite-shm")).unwrap(),
        b"newer shm"
    );
    assert_eq!(
        std::fs::read(data.join("tako/workflows.sqlite-journal")).unwrap(),
        b"newer journal"
    );
    assert_eq!(value(&data.join("app/data.sqlite")), "live");
    assert_eq!(value(&data.join("tako/workflows.sqlite")), "live runs");
    assert!(!data.join("app/new").exists());
    assert_eq!(value(&data.join("app/other.sqlite")), "other");
    assert_eq!(
        std::fs::read(data.join("app/upload.bin")).unwrap(),
        b"upload"
    );
    assert!(previous_dirs(fixture.temp.path()).is_empty());
}

fn swap(
    fixture: &Fixture,
    databases: &[PathBuf],
) -> (RollbackDir, Result<RestoredDatabases, SwapError>) {
    let dir = RollbackDir::create(&fixture.data).unwrap();
    let result = swap_databases(&fixture.staged, databases, &fixture.data, dir.path());
    (dir, result)
}

#[test]
fn swap_replaces_only_snapshot_databases_and_drops_their_sidecars() {
    let fixture = fixture();
    let data = &fixture.data;
    let other_before = std::fs::read(data.join("app/other.sqlite")).unwrap();
    let (dir, summary) = swap(&fixture, &fixture.databases);
    assert_eq!(
        summary.unwrap(),
        RestoredDatabases {
            replaced: vec!["app/data.sqlite".into(), "tako/workflows.sqlite".into()],
            created: vec!["app/new/created.sqlite".into()],
            untouched: vec!["app/other.sqlite".into()],
        }
    );

    assert_eq!(value(&data.join("app/data.sqlite")), "snapshot");
    assert_eq!(value(&data.join("tako/workflows.sqlite")), "snapshot runs");
    assert_eq!(value(&data.join("app/new/created.sqlite")), "created");
    for sidecar in [
        "app/data.sqlite-wal",
        "app/data.sqlite-shm",
        "tako/workflows.sqlite-journal",
    ] {
        assert!(!data.join(sidecar).exists(), "{sidecar}");
    }
    // The modes app data provisioning sets: group read/write in app/,
    // service-only in tako/.
    let mode = |path: &str| std::fs::metadata(data.join(path)).unwrap().mode() & 0o7777;
    assert_eq!(mode("app/data.sqlite"), 0o660);
    assert_eq!(mode("app/new/created.sqlite"), 0o660);
    assert_eq!(mode("tako/workflows.sqlite"), 0o600);
    let meta = std::fs::metadata(data.join("app/data.sqlite")).unwrap();
    assert_eq!(meta.nlink(), 1);
    assert_eq!(
        std::fs::read(data.join("app/other.sqlite")).unwrap(),
        other_before
    );
    assert_eq!(
        std::fs::read(data.join("app/upload.bin")).unwrap(),
        b"upload"
    );

    // The originals are the rollback copy, outside the data tree.
    let [previous] = previous_dirs(fixture.temp.path()).try_into().unwrap();
    assert_eq!(previous, dir.path());
    assert_eq!(
        std::fs::read(previous.join("app/data.sqlite-wal")).unwrap(),
        b"newer wal"
    );
    assert_eq!(value(&previous.join("app/data.sqlite")), "live");
    journal::commit(dir.path()).unwrap();
    assert!(!previous.exists());
    assert_eq!(value(&data.join("app/data.sqlite")), "snapshot");
}

#[test]
fn rollback_restores_original_databases_and_sidecars() {
    let fixture = fixture();
    let data = &fixture.data;
    let (dir, summary) = swap(&fixture, &fixture.databases);
    summary.unwrap();
    // A failed restart opened the restored databases and left sidecars.
    std::fs::write(data.join("app/data.sqlite-wal"), b"restored wal").unwrap();
    std::fs::write(data.join("app/new/created.sqlite-shm"), b"restored shm").unwrap();
    journal::roll_back(dir.path()).unwrap();
    assert_live_state_is_original(&fixture);
}

#[test]
fn a_failed_replacement_rolls_back_the_databases_already_replaced() {
    let fixture = fixture();
    let mut databases = fixture.databases.clone();
    // Sorted after the real entries; its staged copy does not exist.
    databases.push("tako/zz-missing.sqlite".into());
    let (_dir, result) = swap(&fixture, &databases);
    let error = result.unwrap_err();
    assert!(error.originals_intact);
    let error = error.message;
    assert!(error.contains("restore tako/zz-missing.sqlite"), "{error}");
    assert_live_state_is_original(&fixture);
}

/// Stop the swap before each of its mutating steps, as if the server died
/// there, then run the startup recovery: the tree is the pre-restore tree.
#[test]
fn recovery_after_a_crash_at_any_step_restores_the_pre_restore_tree() {
    let mut stop_at = 0;
    loop {
        let fixture = fixture();
        let before = tree_state(&fixture.data);
        let dir = RollbackDir::create(&fixture.data).unwrap();
        let path = dir.path().to_path_buf();
        let result = swap_databases_until(
            &fixture.staged,
            &fixture.databases,
            &fixture.data,
            &path,
            Some(stop_at),
        );
        if result.is_ok() {
            // Past the last step; every boundary was covered.
            assert!(stop_at > 10, "only {stop_at} steps");
            break;
        }
        assert_eq!(result.unwrap_err().message, "stopped");
        // A running restore holds its directory.
        assert_eq!(
            journal::recover_rollback_dir(&path).unwrap(),
            journal::Recovery::InUse
        );
        dir.leave_for_recovery();
        let recovered = journal::recover_rollback_dir(&path).unwrap();
        assert_eq!(
            recovered,
            if stop_at == 0 {
                journal::Recovery::Cleaned
            } else {
                journal::Recovery::RolledBack
            },
            "step {stop_at}"
        );
        assert_eq!(tree_state(&fixture.data), before, "step {stop_at}");
        assert_live_state_is_original(&fixture);
        stop_at += 1;
    }
}

#[test]
fn recovery_removes_the_originals_of_a_committed_restore() {
    let fixture = fixture();
    let (dir, summary) = swap(&fixture, &fixture.databases);
    summary.unwrap();
    let path = dir.path().to_path_buf();
    // The server died right after the commit point.
    journal::retire(&path).unwrap();
    dir.leave_for_recovery();
    assert_eq!(
        journal::recover_rollback_dir(&path).unwrap(),
        journal::Recovery::Cleaned
    );
    assert!(!path.exists());
    assert_eq!(value(&fixture.data.join("app/data.sqlite")), "snapshot");
    assert_eq!(
        std::fs::metadata(fixture.data.join("app/data.sqlite"))
            .unwrap()
            .nlink(),
        1
    );
}

#[test]
fn swap_refuses_unsafe_live_paths_before_changing_anything() {
    let fixture = fixture();
    let data = &fixture.data;
    let outside = fixture.temp.path().join("outside");
    std::fs::create_dir(&outside).unwrap();
    std::os::unix::fs::symlink(&outside, data.join("app/new")).unwrap();
    let error = swap(&fixture, &fixture.databases).1.unwrap_err().message;
    assert!(
        error.contains("Cannot restore app/new/created.sqlite: new is not a directory"),
        "{error}"
    );
    assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 0);
    std::fs::remove_file(data.join("app/new")).unwrap();
    for dir in previous_dirs(fixture.temp.path()) {
        std::fs::remove_dir_all(dir).unwrap();
    }
    assert_live_state_is_original(&fixture);

    std::fs::remove_file(data.join("app/data.sqlite")).unwrap();
    std::fs::write(data.join("app/data.sqlite"), b"not a database").unwrap();
    let error = swap(&fixture, &fixture.databases).1.unwrap_err().message;
    assert!(error.contains("is not a SQLite database"), "{error}");
    assert_eq!(
        std::fs::read(data.join("app/data.sqlite")).unwrap(),
        b"not a database"
    );

    std::fs::remove_file(data.join("app/data.sqlite")).unwrap();
    std::os::unix::fs::symlink(outside.join("target"), data.join("app/data.sqlite")).unwrap();
    let error = swap(&fixture, &fixture.databases).1.unwrap_err().message;
    assert!(error.contains("not a regular file"), "{error}");
    assert!(!outside.join("target").exists());
}

#[test]
fn the_server_swaps_and_rolls_back_in_a_worker_process() {
    let fixture = fixture();
    let swap =
        journal::swap_databases_in_worker(&fixture.staged, &fixture.databases, &fixture.data)
            .unwrap();
    assert_ne!(swap.worker_pid, std::process::id());
    assert_eq!(value(&fixture.data.join("app/data.sqlite")), "snapshot");
    swap.rollback().unwrap();
    assert_live_state_is_original(&fixture);
}

/// A file is a sidecar only when the database it is named after exists
/// beside it. Capture and restore apply the same rule, so a database named
/// like a sidecar round-trips.
#[test]
fn a_database_named_like_a_sidecar_round_trips() {
    let temp = TempDir::new().unwrap();
    let data = temp.path().join("data");
    database(&data.join("app/audit-journal"), "audit snapshot");
    database(&data.join("app/events-wal"), "events snapshot");
    std::fs::create_dir_all(data.join("tako")).unwrap();
    let archive = snapshot_archive(temp.path(), &data);
    let extracted = temp.path().join("extracted");
    let databases = extract_database_snapshot(&archive, &extracted, u64::MAX).unwrap();
    assert_eq!(
        databases,
        [Path::new("app/audit-journal"), Path::new("app/events-wal")]
    );

    database(&data.join("app/audit-journal"), "audit live");
    let dir = RollbackDir::create(&data).unwrap();
    let summary =
        swap_databases(&extracted.join("databases"), &databases, &data, dir.path()).unwrap();
    assert_eq!(summary.replaced, ["app/audit-journal", "app/events-wal"]);
    assert_eq!(value(&data.join("app/audit-journal")), "audit snapshot");
    journal::commit(dir.path()).unwrap();
}

#[test]
fn a_restore_that_would_turn_a_database_into_a_sidecar_is_refused() {
    let db = sqlite_bytes();
    // Both names in one archive: capture never produces it.
    let temp = TempDir::new().unwrap();
    let archive = temp.path().join("archive.tar.zst");
    raw_archive(
        &archive,
        &[
            Entry::Dir(b"databases/"),
            Entry::Dir(b"databases/app/"),
            Entry::Dir(b"databases/tako/"),
            Entry::File(b"databases/app/audit", &db),
            Entry::File(b"databases/app/audit-journal", &db),
        ],
    );
    let error =
        extract_database_snapshot(&archive, &temp.path().join("extracted"), u64::MAX).unwrap_err();
    assert!(
        error.contains("app/audit-journal is a SQLite sidecar of app/audit"),
        "{error}"
    );

    // Live data where the restored name would pair with another database.
    let temp = TempDir::new().unwrap();
    let data = temp.path().join("data");
    database(&data.join("app/audit-journal"), "snapshot");
    std::fs::create_dir_all(data.join("tako")).unwrap();
    let archive = snapshot_archive(temp.path(), &data);
    let extracted = temp.path().join("extracted");
    let databases = extract_database_snapshot(&archive, &extracted, u64::MAX).unwrap();
    std::fs::remove_file(data.join("app/audit-journal")).unwrap();
    database(&data.join("app/audit"), "live");
    let dir = RollbackDir::create(&data).unwrap();
    let error =
        swap_databases(&extracted.join("databases"), &databases, &data, dir.path()).unwrap_err();
    assert!(error.originals_intact);
    assert!(
        error.message.contains("app/audit is a live database"),
        "{}",
        error.message
    );
    assert!(!data.join("app/audit-journal").exists());
    assert_eq!(value(&data.join("app/audit")), "live");
}

/// A restore moves files with rename and link, which cannot cross
/// filesystems. It refuses up front instead of failing part way.
#[test]
fn a_restore_across_filesystems_is_refused_before_changing_anything() {
    let shm = Path::new("/dev/shm");
    let other = match std::fs::metadata(shm) {
        Ok(meta) if meta.dev() != std::fs::metadata(std::env::temp_dir()).unwrap().dev() => {
            TempDir::new_in(shm).unwrap()
        }
        _ => return,
    };
    let fixture = fixture();
    let before = tree_state(&fixture.data);
    let staged = other.path().join("databases");
    let status = std::process::Command::new("cp")
        .arg("-a")
        .arg(&fixture.staged)
        .arg(&staged)
        .status()
        .unwrap();
    assert!(status.success());
    let dir = RollbackDir::create(&fixture.data).unwrap();
    let error = swap_databases(&staged, &fixture.databases, &fixture.data, dir.path()).unwrap_err();
    assert!(error.originals_intact);
    assert!(
        error.message.contains("is on a different filesystem"),
        "{}",
        error.message
    );
    assert_eq!(tree_state(&fixture.data), before);
}
