//! Capture through pinned files, then compact in a private directory. Only a
//! reader confined to the verified inodes may open a live app database by name.
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
#[cfg(target_os = "linux")]
// About one default SQLite auto-checkpoint at a 4 KiB page size. Larger
// logs should use a live reader before taking a writer-blocking copy lock.
const MAX_LOCKED_WAL_BYTES: u64 = 4 * 1024 * 1024;

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
    // Follow the Unix VFS SHARED-lock protocol. Briefly lock PENDING to
    // respect queued commits, then keep only the shared range. RESERVED
    // writers may prepare changes, but cannot modify the database pages.
    read_lock(&db, DB_PENDING_BYTE, 1, deadline)?;
    read_lock(&db, DB_PENDING_BYTE + 2, 510, deadline)?;
    set_lock(&db, DB_PENDING_BYTE, 1, libc::F_UNLCK as _)?;
    let sidecar = |suffix: &str| {
        let mut path = OsString::from(name);
        path.push(suffix);
        path
    };
    let shm_name = sidecar("-shm");
    let wal_name = sidecar("-wal");
    let journal_name = sidecar("-journal");
    let shm = open_optional(parent, &shm_name)?;
    let mut wal = open_optional(parent, &wal_name)?;
    if let Some(shm) = &shm {
        // Acquire both bytes atomically. Recovery also requires exclusive
        // access to the checkpoint byte.
        let len = WAL_CHECKPOINT_LOCK - WAL_WRITE_LOCK + 1;
        #[cfg(target_os = "linux")]
        if let Some(wal) = &wal
            && is_wal_mode(&db)?
            && open_optional(parent, &journal_name)?.is_none()
            && (wal.metadata()?.len() > MAX_LOCKED_WAL_BYTES
                || !try_read_lock(shm, WAL_WRITE_LOCK, len)?)
            && crate::snapshot_live_wal::try_capture(parent, name, [&db, wal, shm], staged)?
        {
            verify_entry(parent, name, Some(&db))?;
            verify_entry(parent, &shm_name, Some(shm))?;
            verify_entry(parent, &wal_name, Some(wal))?;
            verify_entry(parent, &journal_name, None)?;
            return Ok(());
        }
        read_lock(shm, WAL_WRITE_LOCK, len, deadline)?;
    }
    let _initial_journal = open_cold_journal(parent, &journal_name, &db)?;
    if let Some(wal) = &mut wal {
        copy::copy(wal, &staged.with_file_name("database-wal"))?;
        if let Some(shm) = &shm
            && is_wal_mode(&db)?
        {
            // The staged WAL contains the snapshot's commits. Writers may
            // append again while the checkpoint lock freezes the main file.
            set_lock(shm, WAL_WRITE_LOCK, 1, libc::F_UNLCK as _)?;
        }
    }
    copy_main(&mut db, staged)?;

    // In particular, a previously absent SHM may have been created by the
    // first WAL connection during the copy. Its locks weren't held.
    for (name, file) in [
        (name, Some(&db)),
        (&shm_name, shm.as_ref()),
        (&wal_name, wal.as_ref()),
    ] {
        verify_entry(parent, name, file)?;
    }
    // A RESERVED writer can create or discard its journal while our SHARED
    // lock keeps every main database page unchanged. We never replay this
    // journal. Check the current entry again in case its name was replaced.
    open_cold_journal(parent, &journal_name, &db)?;
    Ok(())
}

fn is_wal_mode(db: &File) -> io::Result<bool> {
    let mut versions = [0; 2];
    db.read_exact_at(&mut versions, 18)?;
    Ok(versions == [2, 2])
}

fn verify_entry(parent: &File, name: &OsStr, file: Option<&File>) -> io::Result<()> {
    let current = match fs::statat(parent, name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(stat) => Some((stat.st_dev, stat.st_ino)),
        Err(rustix::io::Errno::NOENT) => None,
        Err(error) => return Err(error.into()),
    };
    let pinned = file.map(fs::fstat).transpose()?;
    if pinned.as_ref().is_some_and(|stat| stat.st_nlink != 1)
        || current != pinned.map(|stat| (stat.st_dev, stat.st_ino))
    {
        return Err(io::Error::other(format!(
            "database entry {name:?} changed during snapshot"
        )));
    }
    Ok(())
}

fn open_cold_journal(parent: &File, name: &OsStr, db: &File) -> io::Result<Option<File>> {
    let journal = open_optional(parent, name)?;
    if let Some(file) = &journal {
        let mut first = [0];
        file.read_at(&mut first, 0)?;
        if first != [0] {
            // SQLite writes a journal header under RESERVED before it needs
            // EXCLUSIVE to modify any main pages (e.g. synchronous=OFF).
            // A nonzero header without a live writer needs recovery.
            let mut reserved = lock_range(DB_PENDING_BYTE + 1, 1, libc::F_WRLCK as _);
            if unsafe { libc::fcntl(db.as_raw_fd(), libc::F_GETLK, &mut reserved) } == -1 {
                return Err(io::Error::last_os_error());
            }
            if reserved.l_type != libc::F_WRLCK as libc::c_short {
                return Err(io::Error::other("database has a hot rollback journal"));
            }
        }
    }
    Ok(journal)
}

fn open_optional(parent: &File, name: &OsStr) -> io::Result<Option<File>> {
    match open_regular(parent, name) {
        Ok(file) => Ok(Some(file)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn lock_range(start: libc::off_t, len: libc::off_t, kind: libc::c_short) -> libc::flock {
    // All fields of flock are integers. Zero any platform-specific fields.
    let mut lock: libc::flock = unsafe { std::mem::zeroed() };
    lock.l_type = kind;
    lock.l_whence = libc::SEEK_SET as _;
    lock.l_start = start;
    lock.l_len = len;
    lock
}

fn set_lock(
    file: &File,
    start: libc::off_t,
    len: libc::off_t,
    kind: libc::c_short,
) -> io::Result<()> {
    let lock = lock_range(start, len, kind);
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETLK, &lock) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn try_read_lock(file: &File, start: libc::off_t, len: libc::off_t) -> io::Result<bool> {
    match set_lock(file, start, len, libc::F_RDLCK as _) {
        Ok(()) => Ok(true),
        Err(error) if matches!(error.raw_os_error(), Some(libc::EACCES | libc::EAGAIN)) => {
            Ok(false)
        }
        Err(error) => Err(error),
    }
}

fn read_lock(
    file: &File,
    start: libc::off_t,
    len: libc::off_t,
    deadline: Instant,
) -> io::Result<()> {
    while !try_read_lock(file, start, len)? {
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "timed out waiting for SQLite lock",
            ));
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::process::{Command, Stdio};

    #[test]
    fn rollback_journal_changes_during_main_copy_are_checked() {
        for change in [
            "created", "removed", "replaced", "hot", "symlink", "hardlink",
        ] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("source");
            rusqlite::Connection::open(&path)
                .unwrap()
                .execute_batch("CREATE TABLE items(id INTEGER)")
                .unwrap();
            let journal = dir.path().join("source-journal");
            if change != "created" {
                std::fs::write(&journal, [0]).unwrap();
            }
            let outside = dir.path().join("other");
            std::fs::write(&outside, [0]).unwrap();
            let parent = File::open(dir.path()).unwrap();
            let name = OsStr::new("source");
            let result = stage(
                &parent,
                name,
                open_regular(&parent, name).unwrap(),
                &dir.path().join("database"),
                |source, dest| {
                    if change != "created" {
                        std::fs::remove_file(&journal)?;
                    }
                    match change {
                        "created" | "replaced" => std::fs::write(&journal, [0])?,
                        "removed" => (),
                        "hot" => std::fs::write(&journal, [1])?,
                        "symlink" => std::os::unix::fs::symlink(&outside, &journal)?,
                        "hardlink" => std::fs::hard_link(&outside, &journal)?,
                        _ => unreachable!(),
                    }
                    copy::copy(source, dest)
                },
            );
            assert_eq!(
                result.is_ok(),
                matches!(change, "created" | "removed" | "replaced"),
                "{change}: {result:?}"
            );
        }
    }

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
