use super::{SqliteStateStore, StateStoreError};
use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

/// Every server process that serves app data holds a shared lease on this
/// file, beside `state.sqlite`, until it drains or retires.
const DRAIN_ACTIVITY_LOCK_FILE: &str = "drain-activity.lock";

pub(crate) struct DrainLease(File);

impl DrainLease {
    fn try_acquire(
        path: &std::path::Path,
        exclusive: bool,
    ) -> Result<Option<Self>, StateStoreError> {
        let io_error = |error| StateStoreError::InvalidData(format!("drain lease: {error}"));
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)
            .map_err(io_error)?;
        let metadata = file.metadata().map_err(io_error)?;
        if !metadata.is_file()
            || metadata.nlink() != 1
            || metadata.mode() & 0o777 != 0o600
            || metadata.uid() != unsafe { libc::geteuid() }
        {
            return Err(StateStoreError::InvalidData(
                "unsafe drain lease file".into(),
            ));
        }
        let operation = if exclusive {
            libc::LOCK_EX
        } else {
            libc::LOCK_SH
        };
        if unsafe { libc::flock(file.as_raw_fd(), operation | libc::LOCK_NB) } == 0 {
            return Ok(Some(Self(file)));
        }
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::WouldBlock {
            Ok(None)
        } else {
            Err(io_error(error))
        }
    }
}

impl DrainLease {
    /// The exclusive lease on the data directory `data_dir`, unless a server
    /// process that serves it holds a shared one.
    pub(crate) fn try_exclusive_in(
        data_dir: &std::path::Path,
    ) -> Result<Option<Self>, StateStoreError> {
        Self::try_acquire(&data_dir.join(DRAIN_ACTIVITY_LOCK_FILE), true)
    }
}

impl Drop for DrainLease {
    fn drop(&mut self) {
        // Closing the owned descriptor also releases the lock on process exit.
        unsafe {
            libc::flock(self.0.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

impl SqliteStateStore {
    pub fn drain_requested(&self) -> Result<bool, StateStoreError> {
        let conn = self.lock_conn()?;
        Ok(conn.query_row(
            "SELECT requested FROM server_drain WHERE id = 1",
            [],
            |row| row.get(0),
        )?)
    }

    pub(crate) fn drain_deadline_unix_ms(&self) -> Result<u64, StateStoreError> {
        let conn = self.lock_conn()?;
        let deadline: i64 = conn.query_row(
            "SELECT deadline_unix_ms FROM server_drain WHERE id = 1",
            [],
            |row| row.get(0),
        )?;
        u64::try_from(deadline)
            .map_err(|_| StateStoreError::InvalidData("negative drain deadline".into()))
    }

    pub(crate) fn request_drain_until(&self, deadline_unix_ms: u64) -> Result<(), StateStoreError> {
        let deadline = i64::try_from(deadline_unix_ms).map_err(|_| {
            StateStoreError::InvalidData("drain deadline exceeds SQLite integer range".into())
        })?;
        let conn = self.lock_conn()?;
        let changed = conn.execute(
            "UPDATE server_drain SET requested = 1, deadline_unix_ms = ?1 WHERE id = 1",
            [deadline],
        )?;
        if changed != 1 {
            return Err(StateStoreError::InvalidData(
                "missing server drain state".into(),
            ));
        }
        Ok(())
    }

    pub(crate) fn try_drain_activity(
        &self,
        exclusive: bool,
    ) -> Result<Option<DrainLease>, StateStoreError> {
        DrainLease::try_acquire(
            &self.path.with_file_name(DRAIN_ACTIVITY_LOCK_FILE),
            exclusive,
        )
    }

    pub(crate) fn try_drain_operation(&self) -> Result<Option<DrainLease>, StateStoreError> {
        DrainLease::try_acquire(&self.path.with_file_name("drain-operation.lock"), true)
    }

    pub fn set_drain_requested(&self, requested: bool) -> Result<(), StateStoreError> {
        let conn = self.lock_conn()?;
        let changed = conn.execute(
            "UPDATE server_drain SET requested = ?1, deadline_unix_ms = 0 WHERE id = 1",
            [requested],
        )?;
        if changed != 1 {
            return Err(StateStoreError::InvalidData(
                "missing server drain state".into(),
            ));
        }
        Ok(())
    }
}
