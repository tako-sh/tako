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
    files: Vec<PathBuf>,
    existing: Option<PathBuf>,
}
impl DirectoryIo {
    pub fn new(database: &Path) -> Result<Self> {
        Self::for_databases(&[database])
    }

    pub fn existing(database: &Path) -> Result<Self> {
        let mut io = Self::new(database)?;
        io.existing = Some(database.to_owned());
        Ok(io)
    }

    pub(super) fn for_databases(databases: &[&Path]) -> Result<Self> {
        let files = databases
            .iter()
            .flat_map(|database| {
                [
                    database.to_path_buf(),
                    database.with_extension("db-log"),
                    with_suffix(database, "-wal"),
                    with_suffix(database, "-shm"),
                ]
            })
            .collect();
        Ok(Self {
            platform: PlatformIO::new()?,
            files,
            existing: None,
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
        let flags = if self.existing.as_deref() == Some(Path::new(path)) {
            flags - OpenFlags::Create
        } else {
            flags
        };
        self.platform.open_file(path, flags, direct)
    }
    fn open_sorter_temp_file(&self, path: &str) -> Result<Arc<dyn File>> {
        // Only Turso supplies this path, inside a fresh private TempDir.
        self.platform.open_sorter_temp_file(path)
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
