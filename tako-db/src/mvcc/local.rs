//! Local commands can share a database in one process. Between processes an
//! exclusive lease covers opening, recovery, and the last connection closing.
use super::*;
use parking_lot::Mutex;
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{LazyLock, Weak},
    time::{Duration, Instant},
};

static OWNERS: LazyLock<Mutex<HashMap<PathBuf, Weak<Owner>>>> = LazyLock::new(Default::default);

fn path_and_lock(path: &Path) -> io::Result<(PathBuf, File)> {
    let filename = path
        .file_name()
        .ok_or_else(|| other("database has no name"))?;
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(parent)?;
    let path = match fs::symlink_metadata(path) {
        Ok(_) => path.canonicalize()?,
        Err(e) if e.kind() == io::ErrorKind::NotFound => parent.canonicalize()?.join(filename),
        Err(e) => return Err(e),
    };
    let name = directory_io::with_suffix(&path, ".owner.lock");
    let lock = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(name)?;
    Ok((path, lock))
}

fn try_lock(file: &File) -> io::Result<bool> {
    match file.try_lock() {
        Ok(()) => Ok(true),
        Err(fs::TryLockError::WouldBlock) => Ok(false),
        Err(fs::TryLockError::Error(error)) => Err(error),
    }
}

fn wait(start: Instant, timeout: Duration) -> io::Result<()> {
    let remaining = timeout.saturating_sub(start.elapsed());
    if remaining.is_zero() {
        return Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "another local process is using the database",
        ));
    }
    std::thread::sleep(remaining.min(Duration::from_millis(10)));
    Ok(())
}

impl Owner {
    pub(crate) fn local(path: &Path, timeout: Duration) -> io::Result<Arc<Self>> {
        let (path, lock) = path_and_lock(path)?;
        let start = Instant::now();
        loop {
            if let Some(owner) = OWNERS.lock().get(&path).and_then(Weak::upgrade) {
                return Ok(owner);
            }
            if try_lock(&lock)? {
                let owner = Arc::new(Self::open_database(&path, None, Some(lock), None, None)?);
                let mut owners = OWNERS.lock();
                owners.retain(|_, previous| previous.strong_count() != 0);
                owners.insert(path, Arc::downgrade(&owner));
                return Ok(owner);
            }
            wait(start, timeout)?;
        }
    }

    pub(crate) fn snapshot_local(path: &Path, dest: &Path, timeout: Duration) -> io::Result<()> {
        let (path, lock) = path_and_lock(path)?;
        let start = Instant::now();
        while !try_lock(&lock)? {
            wait(start, timeout)?;
        }
        let directory = tempfile::Builder::new()
            .prefix("turso-export-")
            .tempdir_in(path.parent().unwrap())?;
        let copy = directory.path().join("data.db");
        fs::copy(&path, &copy)?;
        for (sidecar, name) in [
            (path.with_extension("db-log"), "data.db-log"),
            (directory_io::with_suffix(&path, "-wal"), "data.db-wal"),
        ] {
            match fs::copy(sidecar, directory.path().join(name)) {
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
        }
        drop(lock);
        let owner = Self::open_database(&copy, Some(directory), None, None, None)?;
        export_copy(&owner, dest)
    }
}

/// Reset local application data in one native transaction, including the migration ledger.
pub fn reset_database_file(path: &Path) -> Result<(), String> {
    let owner = Owner::local(path, Duration::from_secs(5)).map_err(|e| e.to_string())?;
    let conn = owner.connect().map_err(|e| e.to_string())?;
    conn.execute("PRAGMA foreign_keys=OFF; BEGIN IMMEDIATE")
        .map_err(|e| e.to_string())?;
    let reset = || -> turso_core::Result<()> {
        let rows = conn
            .prepare(
                "SELECT type, name FROM sqlite_schema
             WHERE type IN ('view', 'trigger', 'table')
             ORDER BY CASE type WHEN 'trigger' THEN 0 WHEN 'view' THEN 1 ELSE 2 END",
            )?
            .run_collect_rows()?;
        for row in rows {
            let [turso_core::Value::Text(kind), turso_core::Value::Text(name)] = row.as_slice()
            else {
                return Err(turso_core::LimboError::InternalError(
                    "invalid schema row".into(),
                ));
            };
            if turso_core::schema::is_system_table(name.as_str()) {
                continue;
            }
            conn.execute(format!(
                "DROP {} IF EXISTS \"{}\"",
                kind.as_str(),
                name.as_str().replace('"', "\"\"")
            ))?;
        }
        conn.execute("PRAGMA user_version=0; COMMIT")
    };
    reset().map_err(|e| {
        let _ = conn.execute("ROLLBACK");
        e.to_string()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_owner_handoff_recovers_only_committed_transactions() {
        const TEST: &str =
            "mvcc::local::tests::local_owner_handoff_recovers_only_committed_transactions";
        const PATH: &str = "TAKO_TEST_LOCAL_DATABASE_HANDOFF";
        if let Some(path) = std::env::var_os(PATH) {
            let owner = Owner::local(Path::new(&path), Duration::ZERO);
            if std::env::var_os("TAKO_TEST_EXPECT_BUSY").is_some() {
                assert_eq!(
                    owner.err().expect("must have one owner").kind(),
                    io::ErrorKind::WouldBlock
                );
                return;
            }
            let db = owner.unwrap();
            let conn = db.connect().unwrap();
            conn.execute("BEGIN CONCURRENT; INSERT INTO items VALUES (1); COMMIT; BEGIN CONCURRENT; INSERT INTO items VALUES (2)").unwrap();
            // Simulate process loss before Turso can close or checkpoint.
            std::process::exit(0);
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sessions.sqlite");
        let owner = Owner::local(&path, Duration::ZERO).unwrap();
        owner
            .connect()
            .unwrap()
            .execute("CREATE TABLE items(id INTEGER PRIMARY KEY)")
            .unwrap();
        let binary = std::env::current_exe().unwrap();
        let mut child = std::process::Command::new(binary);
        child.args(["--exact", TEST]).env(PATH, &path);
        assert!(
            child
                .env("TAKO_TEST_EXPECT_BUSY", "1")
                .status()
                .unwrap()
                .success()
        );
        drop(owner);
        assert!(
            child
                .env_remove("TAKO_TEST_EXPECT_BUSY")
                .status()
                .unwrap()
                .success()
        );

        let dest = dir.path().join("export.sqlite");
        Owner::snapshot_local(&path, &dest, Duration::ZERO).unwrap();
        for file in [path, dest] {
            let db = Owner::local(&file, Duration::ZERO).unwrap();
            let rows = db
                .connect()
                .unwrap()
                .prepare("SELECT id FROM items")
                .unwrap()
                .run_collect_rows()
                .unwrap();
            assert_eq!(rows, [[turso_core::Value::from_i64(1)]]);
        }
    }
}
