//! SQLite must never resolve a pathname in the app's writable tree. Capture
//! the main database and WAL under SQLite-compatible POSIX locks and let
//! SQLite read/recover the private copy (including committed WAL frames).
use super::*;
use std::ffi::OsString;
use std::os::fd::AsRawFd;
use std::time::{Duration, Instant};

mod copy;

// SQLite Unix VFS locks (https://www.sqlite.org/walformat.html). The main
// database uses 512 bytes beginning at PENDING_BYTE. SHM bytes 120 and 121
// belong to WAL writers and checkpointers. Byte 128 is the dead-man switch:
// claiming it would identify this worker as an initialized SQLite connection.
const DB_PENDING_BYTE: libc::off_t = 0x4000_0000;
const WAL_WRITE_LOCK: libc::off_t = 120;
const WAL_CHECKPOINT_LOCK: libc::off_t = 121;

pub(super) fn snapshot(parent: &File, name: &OsStr, file: File, dest: &Path) -> io::Result<()> {
    let work = tempfile::tempdir_in(dest.parent().unwrap())?;
    let staged = work.path().join("database");
    stage(parent, name, file, &staged, copy::copy)?;
    let db =
        rusqlite::Connection::open_with_flags(&staged, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(io::Error::other)?;
    db.set_db_config(
        rusqlite::config::DbConfig::SQLITE_DBCONFIG_TRUSTED_SCHEMA,
        false,
    )
    .map_err(io::Error::other)?;
    let compact = work.path().join("compacted");
    // VACUUM copies live records only. Both SQLite filenames are private so
    // the VFS cannot resolve a symlink or sidecar supplied by the app.
    db.execute(
        "VACUUM main INTO CAST(?1 AS TEXT)",
        [compact.as_os_str().as_bytes()],
    )
    .map_err(io::Error::other)?;
    std::fs::rename(compact, dest)
}

fn stage(
    parent: &File,
    name: &OsStr,
    mut db: File,
    staged: &Path,
    copy_main: impl FnOnce(&mut File, &Path) -> io::Result<()>,
) -> io::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(10);
    // The main-file lock excludes rollback-mode writers and the EXCLUSIVE
    // lock SQLite needs to switch journal mode or remove the last WAL/SHM.
    read_lock(&db, DB_PENDING_BYTE, 512, deadline)?;
    let sidecar = |suffix: &str| {
        let mut path = OsString::from(name);
        path.push(suffix);
        path
    };
    let shm_name = sidecar("-shm");
    let wal_name = sidecar("-wal");
    let journal_name = sidecar("-journal");
    let shm = open_optional(parent, &shm_name)?;
    if let Some(shm) = &shm {
        // Acquire both bytes atomically. Recovery also requires exclusive
        // access to the checkpoint byte.
        read_lock(
            shm,
            WAL_WRITE_LOCK,
            WAL_CHECKPOINT_LOCK - WAL_WRITE_LOCK + 1,
            deadline,
        )?;
    }
    let journal = open_optional(parent, &journal_name)?;
    if let Some(journal) = &journal {
        let mut first = [0];
        journal.read_at(&mut first, 0)?;
        if first != [0] {
            return Err(io::Error::other("database has a hot rollback journal"));
        }
    }
    let mut wal = open_optional(parent, &wal_name)?;
    if let Some(wal) = &mut wal {
        copy::copy(wal, &staged.with_file_name("database-wal"))?;
        if let Some(shm) = &shm {
            let mut journal_versions = [0; 2];
            db.read_exact_at(&mut journal_versions, 18)?;
            if journal_versions == [2, 2] {
                // The staged WAL contains the snapshot's commits. Writers may
                // append again while the checkpoint lock freezes the main file.
                set_lock(shm, WAL_WRITE_LOCK, 1, libc::F_UNLCK as _)?;
            }
        }
    }
    copy_main(&mut db, staged)?;

    // In particular, a previously absent SHM may have been created by the
    // first WAL connection during the copy. Its locks weren't held.
    for (name, file) in [
        (name, Some(&db)),
        (&shm_name, shm.as_ref()),
        (&wal_name, wal.as_ref()),
        (&journal_name, journal.as_ref()),
    ] {
        let current = match fs::statat(parent, name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(stat) => Some((stat.st_dev, stat.st_ino)),
            Err(rustix::io::Errno::NOENT) => None,
            Err(error) => return Err(error.into()),
        };
        let pinned = file
            .map(fs::fstat)
            .transpose()?
            .map(|stat| (stat.st_dev, stat.st_ino));
        if current != pinned {
            return Err(io::Error::other(format!(
                "database entry {name:?} changed during snapshot"
            )));
        }
    }
    Ok(())
}

fn open_optional(parent: &File, name: &OsStr) -> io::Result<Option<File>> {
    match open_regular(parent, name) {
        Ok(file) => Ok(Some(file)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn set_lock(
    file: &File,
    start: libc::off_t,
    len: libc::off_t,
    kind: libc::c_short,
) -> io::Result<()> {
    // All fields of flock are integers. Zero any platform-specific fields.
    let mut lock: libc::flock = unsafe { std::mem::zeroed() };
    lock.l_type = kind;
    lock.l_whence = libc::SEEK_SET as _;
    lock.l_start = start;
    lock.l_len = len;
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETLK, &lock) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn read_lock(
    file: &File,
    start: libc::off_t,
    len: libc::off_t,
    deadline: Instant,
) -> io::Result<()> {
    loop {
        match set_lock(file, start, len, libc::F_RDLCK as _) {
            Ok(()) => return Ok(()),
            Err(error)
                if matches!(error.raw_os_error(), Some(libc::EACCES | libc::EAGAIN))
                    && Instant::now() < deadline =>
            {
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(error) => return Err(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::process::{Command, Stdio};

    #[test]
    fn wal_writer_commits_during_main_file_copy() {
        // SQLite's Unix locks are per-process. Keep the live connection in a
        // second process, just as it would be when the snapshot worker runs.
        const WRITER_DIR: &str = "TAKO_TEST_WAL_COPY_WRITER";
        if let Some(dir) = std::env::var_os(WRITER_DIR) {
            let dir = Path::new(&dir);
            let db = rusqlite::Connection::open(dir.join("source")).unwrap();
            db.busy_timeout(Duration::ZERO).unwrap();
            db.execute_batch(
                "PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0;
                 CREATE TABLE items (id INTEGER); PRAGMA wal_checkpoint(TRUNCATE);
                 INSERT INTO items VALUES (1);",
            )
            .unwrap();
            std::fs::write(dir.join("ready"), []).unwrap();
            std::io::stdin().read_exact(&mut [0]).unwrap();
            db.execute("INSERT INTO items VALUES (2)", []).unwrap();
            let busy: i64 = db
                .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| row.get(0))
                .unwrap();
            assert_eq!(busy, 1, "checkpoint overwrote the snapshot's main file");
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let mut writer = Command::new(std::env::current_exe().unwrap())
            .args(["wal_writer_commits_during_main_file_copy", "--nocapture"])
            .env(WRITER_DIR, dir.path())
            .stdin(Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !dir.path().join("ready").exists() {
            assert!(writer.try_wait().unwrap().is_none() && Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(10));
        }
        let staged = dir.path().join("database");
        let parent = File::open(dir.path()).unwrap();
        let name = OsStr::new("source");
        stage(
            &parent,
            name,
            open_regular(&parent, name).unwrap(),
            &staged,
            |source, dest| {
                writer.stdin.take().unwrap().write_all(&[1]).unwrap();
                assert!(writer.wait().unwrap().success(), "concurrent writer failed");
                copy::copy(source, dest)
            },
        )
        .unwrap();

        let restored = rusqlite::Connection::open(staged).unwrap();
        let values: String = restored
            .query_row("SELECT group_concat(id) FROM items", [], |row| row.get(0))
            .unwrap();
        assert_eq!(values, "1");
    }
}
