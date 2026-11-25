//! The SQL engine may open only the files of its private generation.
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

pub(super) struct DirectoryIo {
    platform: PlatformIO,
    directory: PathBuf,
}
impl DirectoryIo {
    pub fn new(directory: &Path) -> Result<Self> {
        Ok(Self {
            platform: PlatformIO::new()?,
            directory: directory.to_owned(),
        })
    }

    fn check(&self, name: &str) -> Result<()> {
        let path = Path::new(name);
        if path.parent() == Some(&self.directory)
            && matches!(
                path.file_name().and_then(|s| s.to_str()),
                Some("data.db" | "data.db-log" | "data.db-wal" | "data.db-shm")
            )
        {
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
