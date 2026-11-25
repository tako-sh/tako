//! Restrict native I/O to one database and its journaling files.
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};
use turso_core::{
    Completion, File, IO, LimboError, OpenFlags, Result,
    io::{
        Clock, FileId, PlatformIO,
        clock::{MonotonicInstant, WallClockInstant},
    },
};

pub(super) fn with_suffix(database: &Path, suffix: &str) -> PathBuf {
    let mut path = database.as_os_str().to_owned();
    path.push(suffix);
    path.into()
}

pub(super) struct DirectoryIo {
    platform: PlatformIO,
    files: [PathBuf; 4],
}
impl DirectoryIo {
    pub fn new(database: &Path) -> Result<Self> {
        Ok(Self {
            platform: PlatformIO::new()?,
            files: [
                database.to_owned(),
                database.with_extension("db-log"),
                with_suffix(database, "-wal"),
                with_suffix(database, "-shm"),
            ],
        })
    }

    fn check(&self, name: &str) -> Result<()> {
        let path = Path::new(name);
        if self.files.iter().any(|file| file == path) {
            Ok(())
        } else {
            Err(LimboError::ReadOnly)
        }
    }
}
impl Clock for DirectoryIo {
    fn current_time_monotonic(&self) -> MonotonicInstant {
        self.platform.current_time_monotonic()
    }
    fn current_time_wall_clock(&self) -> WallClockInstant {
        self.platform.current_time_wall_clock()
    }
}
impl IO for DirectoryIo {
    fn open_file(&self, path: &str, flags: OpenFlags, direct: bool) -> Result<Arc<dyn File>> {
        self.check(path)?;
        self.platform.open_file(path, flags, direct)
    }
    fn remove_file(&self, path: &str) -> Result<()> {
        self.check(path)?;
        self.platform.remove_file(path)
    }
    fn file_id(&self, path: &str) -> Result<FileId> {
        self.check(path)?;
        self.platform.file_id(path)
    }
    fn step(&self) -> Result<()> {
        self.platform.step()
    }
    fn cancel(&self, completions: &[Completion]) -> Result<()> {
        self.platform.cancel(completions)
    }
}
