//! Restore of a database snapshot: replaces only the SQLite databases it
//! contains inside the live app data tree and leaves every other file alone.

use std::collections::BTreeSet;
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{self, Read};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};

use rustix::fs::{self, AtFlags, CWD, FileType, Mode, OFlags};
use tako_core::RestoredDatabases;

use super::{
    DATABASE_SNAPSHOT_ARCHIVE_ROOT, READ_FLAGS, TAKO_DATA_DIR, is_sqlite_database,
    is_transient_tako_sqlite_file, list_database_tree, open_regular,
};

const DATA_ROOTS: [&str; 2] = ["app", TAKO_DATA_DIR];
/// Files SQLite pairs with a database. Left beside a replaced database they
/// could roll newer or foreign pages into it, so they move with the original.
const SIDECAR_SUFFIXES: [&str; 3] = ["-wal", "-shm", "-journal"];

/// Extract a decrypted database snapshot into the new directory `dest` and
/// return the relative paths of its databases, e.g. `app/data.sqlite`.
///
/// The archive is untrusted input: only `databases/app/` and
/// `databases/tako/` with directories and SQLite files below them are
/// accepted. Links, special files, absolute or `..` paths, sidecars,
/// transient Tako stores, and duplicates are rejected, and no more than
/// `budget` bytes are written.
pub(in crate::operations::backup) fn extract_database_snapshot(
    archive_path: &Path,
    dest: &Path,
    budget: u64,
) -> Result<Vec<PathBuf>, String> {
    let invalid = |detail: String| format!("Invalid database snapshot: {detail}");
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(dest)
        .map_err(|e| format!("create extraction dir {}: {e}", dest.display()))?;
    let file = File::open(archive_path)
        .map_err(|e| format!("open archive {}: {e}", archive_path.display()))?;
    let decoder = zstd::stream::read::Decoder::new(file)
        .map_err(|e| format!("initialize zstd decoder: {e}"))?;
    let mut archive = tar::Archive::new(decoder);
    let root = dest.join(DATABASE_SNAPSHOT_ARCHIVE_ROOT);
    let mut written = 0_u64;
    let mut databases = Vec::new();
    for entry in archive
        .entries()
        .map_err(|e| invalid(format!("read archive: {e}")))?
    {
        let mut entry = entry.map_err(|e| invalid(format!("read archive: {e}")))?;
        let path = entry
            .path()
            .map_err(|e| invalid(format!("read entry path: {e}")))?
            .into_owned();
        let relative = snapshot_entry_path(&path).map_err(invalid)?;
        let entry_type = entry.header().entry_type();
        let target = root.join(&relative);
        if entry_type.is_dir() {
            create_private_dirs(&target)?;
        } else if entry_type.is_file() {
            validate_database_path(&relative).map_err(invalid)?;
            let size = entry
                .header()
                .size()
                .map_err(|e| invalid(format!("read size of {}: {e}", path.display())))?;
            written = written.saturating_add(size);
            if written > budget {
                return Err(format!(
                    "Not enough free disk space to restore the database snapshot: its databases need more than the {budget} bytes available."
                ));
            }
            if let Some(parent) = target.parent() {
                create_private_dirs(parent)?;
            }
            let mut output = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&target)
                .map_err(|e| match e.kind() {
                    io::ErrorKind::AlreadyExists => {
                        invalid(format!("{} appears more than once", path.display()))
                    }
                    _ => format!("create {}: {e}", target.display()),
                })?;
            let copied = io::copy(&mut (&mut entry).take(size), &mut output)
                .map_err(|e| invalid(format!("read {}: {e}", path.display())))?;
            if copied != size {
                return Err(invalid(format!("{} is truncated", path.display())));
            }
            output
                .sync_all()
                .map_err(|e| format!("flush {}: {e}", target.display()))?;
            if !is_sqlite_database(&output)
                .map_err(|e| format!("read {}: {e}", target.display()))?
            {
                return Err(invalid(format!(
                    "{} is not a SQLite database",
                    path.display()
                )));
            }
            databases.push(relative);
        } else {
            return Err(invalid(format!(
                "{} is not a regular file or directory",
                path.display()
            )));
        }
    }
    for name in DATA_ROOTS {
        if !std::fs::symlink_metadata(root.join(name)).is_ok_and(|meta| meta.is_dir()) {
            return Err(invalid(format!(
                "missing {DATABASE_SNAPSHOT_ARCHIVE_ROOT}/{name}/"
            )));
        }
    }
    if databases.is_empty() {
        return Err(invalid("it contains no databases".into()));
    }
    databases.sort();
    Ok(databases)
}

/// The entry's path below `databases/`, which must stay inside `app/` or
/// `tako/`. The root itself is the empty path.
fn snapshot_entry_path(path: &Path) -> Result<PathBuf, String> {
    let mut components = path.components();
    match components.next() {
        Some(Component::Normal(name)) if name == DATABASE_SNAPSHOT_ARCHIVE_ROOT => {}
        _ => {
            return Err(format!(
                "{} is outside {DATABASE_SNAPSHOT_ARCHIVE_ROOT}/",
                path.display()
            ));
        }
    }
    let mut relative = PathBuf::new();
    for component in components {
        match component {
            Component::Normal(name) => relative.push(name),
            _ => return Err(format!("unsafe path {}", path.display())),
        }
    }
    if let Some(top) = relative.iter().next()
        && !DATA_ROOTS.iter().any(|name| top == OsStr::new(name))
    {
        return Err(format!(
            "{} is outside {DATABASE_SNAPSHOT_ARCHIVE_ROOT}/app/ and {DATABASE_SNAPSHOT_ARCHIVE_ROOT}/tako/",
            path.display()
        ));
    }
    Ok(relative)
}

fn validate_database_path(relative: &Path) -> Result<(), String> {
    let Some(name) = relative
        .file_name()
        .filter(|_| relative.components().count() >= 2)
    else {
        return Err(format!(
            "file {} is not inside app/ or tako/",
            relative.display()
        ));
    };
    let name = name.as_encoded_bytes();
    if SIDECAR_SUFFIXES
        .iter()
        .any(|suffix| name.ends_with(suffix.as_bytes()))
    {
        return Err(format!("{} is a SQLite sidecar file", relative.display()));
    }
    if is_transient_tako_sqlite_file(relative) {
        return Err(format!(
            "{} is a transient Tako store that backups never contain",
            relative.display()
        ));
    }
    Ok(())
}

fn create_private_dirs(path: &Path) -> Result<(), String> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
        .map_err(|e| format!("create {}: {e}", path.display()))
}

/// Live databases replaced by a database snapshot restore. The originals and
/// their sidecars stay in `previous`, outside the data tree, until
/// [`DatabaseSwap::commit`] or [`DatabaseSwap::rollback`].
#[derive(Debug)]
#[must_use = "commit the validated restore or roll it back after stopping the app"]
pub(in crate::operations::backup) struct DatabaseSwap {
    previous: PathBuf,
    entries: Vec<SwappedDatabase>,
    /// Directories created for databases that did not exist live, deepest last.
    created_dirs: Vec<(File, OsString)>,
    summary: RestoredDatabases,
}

#[derive(Debug)]
struct SwappedDatabase {
    relative: PathBuf,
    /// The live parent directory, pinned so later path changes cannot
    /// redirect a rollback.
    dir: File,
    name: OsString,
    /// The original database is linked into `previous`.
    kept_original: bool,
    /// The snapshot copy is in place under the live name.
    replaced: bool,
    /// Suffixes of the sidecars moved from the live directory to `previous`.
    sidecars: Vec<&'static str>,
}

/// A failed [`swap_databases`].
#[derive(Debug)]
pub(in crate::operations::backup) struct SwapError {
    pub(in crate::operations::backup) message: String,
    /// Every live database and sidecar is as it was before the swap.
    pub(in crate::operations::backup) originals_intact: bool,
}

impl SwapError {
    fn intact(message: String) -> Self {
        Self {
            message,
            originals_intact: true,
        }
    }
}

/// Put each extracted database in `staged_root` (`databases/` of the
/// extraction) in place of its live counterpart below `data_root`. Call only
/// with every process that may open these databases stopped.
///
/// Every target is checked before anything changes. A database whose path
/// does not exist live is created with any missing parent directories. A
/// live path that is a symlink, crosses a symlink or a non-directory, is a
/// hard link, or holds something other than a SQLite database is refused.
pub(in crate::operations::backup) fn swap_databases(
    staged_root: &Path,
    databases: &[PathBuf],
    data_root: &Path,
) -> Result<DatabaseSwap, SwapError> {
    let root = File::from(
        fs::open(data_root, READ_FLAGS | OFlags::DIRECTORY, Mode::empty()).map_err(|e| {
            SwapError::intact(format!("open data dir {}: {e}", data_root.display()))
        })?,
    );
    let in_snapshot: BTreeSet<&Path> = databases.iter().map(PathBuf::as_path).collect();
    let untouched = list_database_tree(data_root)
        .map_err(SwapError::intact)?
        .into_iter()
        .filter(|path| !in_snapshot.contains(path.as_path()))
        .map(|path| path.to_string_lossy().into_owned())
        .collect();
    for relative in databases {
        check_target(&root, relative).map_err(|e| {
            SwapError::intact(format!("Cannot restore {}: {e}", relative.display()))
        })?;
    }

    let parent = data_root.parent().ok_or_else(|| {
        SwapError::intact(format!("data root has no parent: {}", data_root.display()))
    })?;
    let previous = parent.join(format!(".data-restore-prev-{}", nanoid::nanoid!(8)));
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&previous)
        .map_err(|e| SwapError::intact(format!("create {}: {e}", previous.display())))?;
    let mut swap = DatabaseSwap {
        previous,
        entries: Vec::new(),
        created_dirs: Vec::new(),
        summary: RestoredDatabases {
            untouched,
            ..RestoredDatabases::default()
        },
    };
    for relative in databases {
        if let Err(error) = swap.replace(&root, staged_root, relative) {
            let error = format!("restore {}: {error}", relative.display());
            return Err(match swap.rollback() {
                Ok(()) => SwapError::intact(error),
                Err(rollback) => SwapError {
                    message: format!("{error}; {rollback}"),
                    originals_intact: false,
                },
            });
        }
    }
    for entry in &swap.entries {
        let path = entry.relative.to_string_lossy().into_owned();
        if entry.kept_original {
            swap.summary.replaced.push(path);
        } else {
            swap.summary.created.push(path);
        }
    }
    Ok(swap)
}

fn check_target(root: &File, relative: &Path) -> io::Result<()> {
    let (parents, name) = split_relative(relative)?;
    let mut dir = root.try_clone()?;
    for component in parents {
        match fs::openat(
            &dir,
            component,
            READ_FLAGS | OFlags::DIRECTORY,
            Mode::empty(),
        ) {
            Ok(next) => dir = File::from(next),
            // The rest of the path is created.
            Err(rustix::io::Errno::NOENT) => return Ok(()),
            Err(rustix::io::Errno::LOOP | rustix::io::Errno::NOTDIR) => {
                return Err(io::Error::other(format!(
                    "{} is not a directory",
                    component.display()
                )));
            }
            Err(error) => return Err(error.into()),
        }
    }
    let stat = match fs::statat(&dir, name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(stat) => stat,
        Err(rustix::io::Errno::NOENT) => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile {
        return Err(io::Error::other("the live path is not a regular file"));
    }
    let file = open_regular(&dir, name)?;
    if file.metadata()?.len() > 0 && !is_sqlite_database(&file)? {
        return Err(io::Error::other(
            "the live file is not a SQLite database; move it away and retry",
        ));
    }
    Ok(())
}

fn split_relative(relative: &Path) -> io::Result<(Vec<&OsStr>, &OsStr)> {
    let mut parts = Vec::new();
    for component in relative.components() {
        let Component::Normal(name) = component else {
            return Err(io::Error::other("invalid database path"));
        };
        parts.push(name);
    }
    let name = parts
        .pop()
        .ok_or_else(|| io::Error::other("invalid database path"))?;
    Ok((parts, name))
}

fn with_suffix(name: &OsStr, suffix: &str) -> OsString {
    let mut value = name.to_os_string();
    value.push(suffix);
    value
}

impl DatabaseSwap {
    pub(in crate::operations::backup) fn summary(&self) -> &RestoredDatabases {
        &self.summary
    }

    fn replace(&mut self, root: &File, staged_root: &Path, relative: &Path) -> io::Result<()> {
        let (parents, name) = split_relative(relative)?;
        let dir = self.open_or_create_parents(root, &parents)?;
        self.entries.push(SwappedDatabase {
            relative: relative.to_path_buf(),
            dir,
            name: name.to_os_string(),
            kept_original: false,
            replaced: false,
            sidecars: Vec::new(),
        });
        let entry = self.entries.last_mut().expect("entry was just pushed");
        let kept = self.previous.join(relative);
        if let Some(parent) = kept.parent() {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(parent)?;
        }

        // Move sidecars out first: a crash can then leave the original
        // database without its log, never the snapshot copy with it.
        for suffix in SIDECAR_SUFFIXES {
            let sidecar = with_suffix(name, suffix);
            match fs::statat(&entry.dir, sidecar.as_os_str(), AtFlags::SYMLINK_NOFOLLOW) {
                Ok(_) => {}
                Err(rustix::io::Errno::NOENT) => continue,
                Err(error) => return Err(error.into()),
            }
            fs::renameat(
                &entry.dir,
                sidecar.as_os_str(),
                CWD,
                with_suffix(kept.as_os_str(), suffix),
            )?;
            entry.sidecars.push(suffix);
        }

        let staged = staged_root.join(relative);
        let staged_file = std::fs::OpenOptions::new().read(true).open(&staged)?;
        let original = match fs::statat(&entry.dir, name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(stat) if FileType::from_raw_mode(stat.st_mode) == FileType::RegularFile => {
                Some(open_regular(&entry.dir, name)?.metadata()?)
            }
            Ok(_) => return Err(io::Error::other("the live path is not a regular file")),
            Err(rustix::io::Errno::NOENT) => None,
            Err(error) => return Err(error.into()),
        };
        match &original {
            Some(meta) => {
                staged_file
                    .set_permissions(std::fs::Permissions::from_mode(meta.mode() & 0o777))?;
                // Restoring the owner needs privileges the service may lack;
                // the app data provisioning on restart repairs it either way.
                let _ = std::os::unix::fs::fchown(&staged_file, Some(meta.uid()), Some(meta.gid()));
                // A second name for the original: the replacement below stays
                // atomic, and the original remains the rollback copy.
                fs::linkat(&entry.dir, name, CWD, &kept, AtFlags::empty())?;
                entry.kept_original = true;
            }
            None => staged_file.set_permissions(std::fs::Permissions::from_mode(0o600))?,
        }
        staged_file.sync_all()?;
        fs::renameat(CWD, &staged, &entry.dir, name)?;
        entry.replaced = true;
        fs::fsync(&entry.dir)?;
        Ok(())
    }

    fn open_or_create_parents(&mut self, root: &File, parents: &[&OsStr]) -> io::Result<File> {
        let mut dir = root.try_clone()?;
        for component in parents {
            let flags = READ_FLAGS | OFlags::DIRECTORY;
            let next = match fs::openat(&dir, *component, flags, Mode::empty()) {
                Ok(next) => next,
                Err(rustix::io::Errno::NOENT) => {
                    fs::mkdirat(&dir, *component, Mode::from_raw_mode(0o700))?;
                    self.created_dirs
                        .push((dir.try_clone()?, component.to_os_string()));
                    fs::openat(&dir, *component, flags, Mode::empty())?
                }
                Err(error) => return Err(error.into()),
            };
            dir = File::from(next);
        }
        Ok(dir)
    }

    /// Discard the originals once the app runs on the restored databases.
    pub(in crate::operations::backup) fn commit(self) -> Result<(), String> {
        std::fs::remove_dir_all(&self.previous).map_err(|error| {
            format!(
                "remove previous databases {}: {error}",
                self.previous.display()
            )
        })
    }

    /// Put the original databases and sidecars back and remove databases and
    /// directories the restore created. Call only after stopping all
    /// processes that may open these databases.
    pub(in crate::operations::backup) fn rollback(self) -> Result<(), String> {
        let mut errors = Vec::new();
        for entry in self.entries.iter().rev() {
            if let Err(error) = entry.rollback(&self.previous) {
                errors.push(format!("{}: {error}", entry.relative.display()));
            }
        }
        for (parent, name) in self.created_dirs.iter().rev() {
            match fs::unlinkat(parent, name.as_os_str(), AtFlags::REMOVEDIR) {
                Ok(()) | Err(rustix::io::Errno::NOENT | rustix::io::Errno::NOTEMPTY) => {}
                Err(error) => errors.push(format!("remove {}: {error}", name.to_string_lossy())),
            }
        }
        if !errors.is_empty() {
            return Err(format!(
                "could not put back original databases ({}); originals remain at {}",
                errors.join("; "),
                self.previous.display()
            ));
        }
        let _ = std::fs::remove_dir_all(&self.previous);
        Ok(())
    }
}

impl SwappedDatabase {
    fn rollback(&self, previous: &Path) -> io::Result<()> {
        let kept = previous.join(&self.relative);
        if self.replaced {
            // A restart attempt may have created sidecars for the snapshot copy.
            for suffix in SIDECAR_SUFFIXES {
                match fs::unlinkat(
                    &self.dir,
                    with_suffix(&self.name, suffix).as_os_str(),
                    AtFlags::empty(),
                ) {
                    Ok(()) | Err(rustix::io::Errno::NOENT) => {}
                    Err(error) => return Err(error.into()),
                }
            }
            if self.kept_original {
                fs::renameat(CWD, &kept, &self.dir, self.name.as_os_str())?;
            } else {
                fs::unlinkat(&self.dir, self.name.as_os_str(), AtFlags::empty())?;
            }
        } else if self.kept_original {
            // Linked but not replaced: the live name is still the original.
            std::fs::remove_file(&kept)?;
        }
        for suffix in &self.sidecars {
            fs::renameat(
                CWD,
                with_suffix(kept.as_os_str(), suffix),
                &self.dir,
                with_suffix(&self.name, suffix).as_os_str(),
            )?;
        }
        fs::fsync(&self.dir)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests;
