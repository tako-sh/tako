use super::{StateStore, StateStoreError};
use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::time::{Duration, Instant};

/// Every server process that serves app data holds a shared lease on this
/// file, beside `state.redb`, until it drains or retires.
const DRAIN_ACTIVITY_LOCK_FILE: &str = "drain-activity.lock";
/// Every standby server process holds a shared lease on this file, in the
/// data directory, for as long as it may run app instances. Release cleanup
/// holds it exclusively, so it never removes a release a standby serves.
const STANDBY_LOCK_FILE: &str = "standby.lock";
/// How often a start that waits for a lease tries again.
const LEASE_RETRY_INTERVAL: Duration = Duration::from_millis(100);
/// How often a start that waits for a lease says so.
const LEASE_WAIT_LOG_INTERVAL: Duration = Duration::from_secs(10);

pub(crate) struct DrainLease(File);

impl DrainLease {
    fn try_acquire(
        path: &std::path::Path,
        exclusive: bool,
    ) -> Result<Option<Self>, StateStoreError> {
        let io_error =
            |error| StateStoreError::InvalidData(format!("lease {}: {error}", path.display()));
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
            return Err(StateStoreError::InvalidData(format!(
                "unsafe lease file {}",
                path.display()
            )));
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

    /// A standby's shared lease on the standby lock of the data directory
    /// `data_dir`, or release cleanup's exclusive one, unless a conflicting
    /// one is held.
    pub(crate) fn try_standby_in(
        data_dir: &std::path::Path,
        exclusive: bool,
    ) -> Result<Option<Self>, StateStoreError> {
        Self::try_acquire(&data_dir.join(STANDBY_LOCK_FILE), exclusive)
    }
}

/// Run `attempt` until it returns `Some`, for at most `wait`, logging that
/// this process waits for `waiting_for`. `Ok(None)` when the wait ran out.
pub(crate) fn wait_for_lease<T>(
    wait: Duration,
    waiting_for: &str,
    mut attempt: impl FnMut() -> Result<Option<T>, StateStoreError>,
) -> Result<Option<T>, StateStoreError> {
    let deadline = Instant::now() + wait;
    let mut next_log = Instant::now();
    loop {
        if let Some(found) = attempt()? {
            return Ok(Some(found));
        }
        let now = Instant::now();
        if now >= deadline {
            return Ok(None);
        }
        if now >= next_log {
            tracing::info!("Waiting up to {} s for {waiting_for}", wait.as_secs());
            next_log = now + LEASE_WAIT_LOG_INTERVAL;
        }
        std::thread::sleep(LEASE_RETRY_INTERVAL.min(deadline - now));
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

impl StateStore {
    pub fn drain_requested(&self) -> Result<bool, StateStoreError> {
        self.read(|r| {
            Ok(super::get::<(bool, u64)>(r, ("drain", "", ""))?
                .unwrap_or_default()
                .0)
        })
    }
    pub(crate) fn drain_deadline_unix_ms(&self) -> Result<u64, StateStoreError> {
        self.read(|r| {
            Ok(super::get::<(bool, u64)>(r, ("drain", "", ""))?
                .unwrap_or_default()
                .1)
        })
    }
    pub(crate) fn request_drain_until(&self, deadline_unix_ms: u64) -> Result<(), StateStoreError> {
        self.write(|r| super::put(r, ("drain", "", ""), &(true, deadline_unix_ms)))
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
        self.write(|r| super::put(r, ("drain", "", ""), &(requested, 0_u64)))
    }
}
