//! SQLite must never resolve a pathname in the app's writable tree. Capture
//! the main database and WAL under SQLite-compatible POSIX locks and let
//! SQLite read/recover the private copy (including committed WAL frames).
use super::*;
use std::ffi::OsString;
use std::time::{Duration, Instant};

pub(super) fn snapshot(parent: &File, name: &OsStr, file: File, dest: &Path) -> io::Result<()> {
    let work = tempfile::tempdir_in(dest.parent().unwrap())?;
    let staged = work.path().join("database");
    stage(parent, name, file, &staged)?;
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

fn stage(parent: &File, name: &OsStr, mut db: File, staged: &Path) -> io::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(10);
    // The main-file lock excludes rollback-mode writers and the EXCLUSIVE
    // lock SQLite needs to switch journal mode or remove the last WAL/SHM.
    read_lock(&db, deadline)?;
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
        // WAL-mode writers and checkpoints take exclusive byte-range locks here.
        read_lock(shm, deadline)?;
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
    io::copy(&mut db, &mut File::create_new(staged)?)?;
    if let Some(wal) = &mut wal {
        io::copy(
            wal,
            &mut File::create_new(staged.with_file_name("database-wal"))?,
        )?;
    }

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

fn read_lock(file: &File, deadline: Instant) -> io::Result<()> {
    loop {
        match fs::fcntl_lock(file, fs::FlockOperation::NonBlockingLockShared) {
            Ok(()) => return Ok(()),
            Err(rustix::io::Errno::ACCESS | rustix::io::Errno::AGAIN)
                if Instant::now() < deadline =>
            {
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(error) => return Err(error.into()),
        }
    }
}
