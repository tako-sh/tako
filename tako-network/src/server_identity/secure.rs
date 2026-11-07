use super::*;
use crate::Error;
use rustix::{
    fd::OwnedFd,
    fs::{self, Mode, OFlags},
    process::geteuid,
};
use std::{
    fs::File,
    io::{Read, Write},
    os::unix::fs::MetadataExt,
    path::Component,
};

pub(super) struct Directory(OwnedFd);

impl Directory {
    // Lock the directory inode itself: no replaceable lock-file path, and
    // read-only load acquires the same lock without creating any files.
    pub(super) fn lock(&self) -> Result<()> {
        fs::flock(&self.0, fs::FlockOperation::LockExclusive)?;
        Ok(())
    }

    pub(super) fn discard_stage(&self) -> Result<()> {
        if self.read_optional(RENEWAL_STAGE)?.is_some() {
            fs::unlinkat(&self.0, RENEWAL_STAGE, fs::AtFlags::empty())?;
            renewal_boundary()?;
            fs::fsync(&self.0)?;
            renewal_boundary()?;
        }
        Ok(())
    }

    pub(super) fn publish_journal(&self, bytes: &[u8]) -> Result<()> {
        if self.read_optional(RENEWAL_JOURNAL)?.is_some() {
            return Err(invalid("leaf renewal", "journal already exists"));
        }
        self.write_atomic(RENEWAL_JOURNAL, bytes)
    }

    pub(super) fn replace_path(&self, path: &Path, bytes: &[u8]) -> Result<()> {
        if self.read_path(path)?.as_slice() != bytes {
            self.write_atomic(filename(path)?, bytes)?;
        }
        Ok(())
    }

    fn write_atomic(&self, name: &str, bytes: &[u8]) -> Result<()> {
        if bytes.len() > MAX_FILE_BYTES {
            return Err(invalid("identity file", "too large"));
        }
        let fd = fs::openat(
            &self.0,
            RENEWAL_STAGE,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::RUSR | Mode::WUSR,
        )?;
        let mut file = File::from(fd);
        fs::fchmod(&file, Mode::RUSR | Mode::WUSR)?;
        renewal_boundary()?;
        file.write_all(bytes)?;
        renewal_boundary()?;
        file.sync_all()?;
        renewal_boundary()?;
        fs::renameat(&self.0, RENEWAL_STAGE, &self.0, name)?;
        renewal_boundary()?;
        fs::fsync(&self.0)?;
        renewal_boundary()?;
        Ok(())
    }

    pub(super) fn remove_journal(&self) -> Result<()> {
        // Recheck protected-file invariants before unlinking.
        self.read_optional(RENEWAL_JOURNAL)?
            .ok_or_else(|| invalid("leaf renewal", "missing journal"))?;
        fs::unlinkat(&self.0, RENEWAL_JOURNAL, fs::AtFlags::empty())?;
        renewal_boundary()?;
        fs::fsync(&self.0)?;
        renewal_boundary()?;
        Ok(())
    }

    pub(super) fn open(path: &Path, create: bool) -> Result<Self> {
        let mut fd = fs::open(
            "/",
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )?;
        let components: Vec<_> = path.components().collect();
        if !path.is_absolute() || path.file_name().is_none() {
            return Err(invalid(
                "identity directory",
                "expected a named absolute directory",
            ));
        }
        for (index, part) in components.iter().enumerate() {
            match part {
                Component::RootDir | Component::CurDir => {}
                Component::Normal(name) => {
                    if create && index == components.len() - 1 {
                        match fs::mkdirat(&fd, *name, Mode::RUSR | Mode::WUSR | Mode::XUSR) {
                            Ok(()) => fs::fsync(&fd)?,
                            Err(rustix::io::Errno::EXIST) => {}
                            Err(error) => return Err(error.into()),
                        }
                    }
                    fd = fs::openat(
                        &fd,
                        *name,
                        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                        Mode::empty(),
                    )?;
                }
                _ => {
                    return Err(invalid(
                        "identity directory",
                        "parent traversal is forbidden",
                    ));
                }
            }
        }
        let stat = fs::fstat(&fd)?;
        if stat.st_uid != geteuid().as_raw() || stat.st_mode & 0o7777 != 0o700 {
            return Err(Error::UnsafeKeyFile);
        }
        Ok(Self(fd))
    }

    pub(super) fn check_path(&self, path: &Path) -> Result<()> {
        let current = Self::open(path, false)?;
        let before = fs::fstat(&self.0)?;
        let after = fs::fstat(&current.0)?;
        if before.st_dev != after.st_dev || before.st_ino != after.st_ino {
            return Err(invalid(
                "identity directory",
                "directory changed during preparation",
            ));
        }
        Ok(())
    }

    pub(super) fn is_empty(&self) -> Result<bool> {
        for entry in fs::Dir::read_from(&self.0)? {
            let entry = entry?;
            if !matches!(entry.file_name().to_bytes(), b"." | b"..") {
                return Ok(false);
            }
        }
        Ok(true)
    }

    pub(super) fn read_optional(&self, name: &str) -> Result<Option<Zeroizing<Vec<u8>>>> {
        let fd = match fs::openat(
            &self.0,
            name,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        ) {
            Ok(fd) => fd,
            Err(rustix::io::Errno::NOENT) => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let file = File::from(fd);
        let meta = file.metadata()?;
        if !meta.is_file()
            || meta.uid() != geteuid().as_raw()
            || meta.mode() & 0o7777 != 0o600
            || meta.nlink() != 1
        {
            return Err(Error::UnsafeKeyFile);
        }
        let mut bytes = Zeroizing::new(Vec::new());
        file.take(MAX_FILE_BYTES as u64 + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() > MAX_FILE_BYTES {
            return Err(invalid("identity file", "too large"));
        }
        Ok(Some(bytes))
    }

    pub(super) fn read_path(&self, path: &Path) -> Result<Zeroizing<Vec<u8>>> {
        self.read_optional(filename(path)?)?
            .ok_or_else(|| invalid("server identity", "partial preparation: missing file"))
    }

    pub(super) fn write_path(&self, path: &Path, bytes: &[u8]) -> Result<()> {
        self.write_new(filename(path)?, bytes)
    }

    pub(super) fn write_new(&self, name: &str, bytes: &[u8]) -> Result<()> {
        if bytes.len() > MAX_FILE_BYTES {
            return Err(invalid("identity file", "too large"));
        }
        let fd = fs::openat(
            &self.0,
            name,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::RUSR | Mode::WUSR,
        )?;
        let mut file = File::from(fd);
        fs::fchmod(&file, Mode::RUSR | Mode::WUSR)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::fsync(&self.0)?;
        Ok(())
    }
}

fn filename(path: &Path) -> Result<&str> {
    path.file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| invalid("identity path", "invalid filename"))
}
