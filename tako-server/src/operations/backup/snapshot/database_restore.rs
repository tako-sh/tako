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

// Left beside a replaced database, its sidecars could roll newer or foreign
// pages into it, so they move with the original.
use super::{
    DATABASE_SNAPSHOT_ARCHIVE_ROOT, READ_FLAGS, SIDECAR_SUFFIXES, TAKO_DATA_DIR,
    is_sqlite_database, is_transient_tako_sqlite_file, list_database_tree, open_regular,
    require_same_filesystem, sidecar_base,
};
use crate::operations::backup::journal::{self, IntentDatabase, RestoreIntent, Steps};

const DATA_ROOTS: [&str; 2] = ["app", TAKO_DATA_DIR];

/// Extract a decrypted database snapshot into the new directory `dest` and
/// return the relative paths of its databases, e.g. `app/data.sqlite`.
///
/// The archive is untrusted input: only `databases/app/` and
/// `databases/tako/` with directories and SQLite files below them are
/// accepted. Links, special files, absolute or `..` paths, sidecars of
/// another database in the archive, transient Tako stores, and duplicates
/// are rejected, and no more than
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
    let set: BTreeSet<&Path> = databases.iter().map(PathBuf::as_path).collect();
    for relative in &databases {
        if let Some(base) = sidecar_of(relative, |path| set.contains(path)) {
            return Err(invalid(format!(
                "{} is a SQLite sidecar of {}",
                relative.display(),
                base.display()
            )));
        }
    }
    Ok(databases)
}

/// The database `relative` is a sidecar of, when `is_database` says it
/// exists. See [`sidecar_base`].
fn sidecar_of(relative: &Path, is_database: impl Fn(&Path) -> bool) -> Option<PathBuf> {
    let base = relative.with_file_name(sidecar_base(relative.file_name()?)?);
    is_database(&base).then_some(base)
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
    if relative.components().count() < 2 {
        return Err(format!(
            "file {} is not inside app/ or tako/",
            relative.display()
        ));
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

/// A failed [`swap_databases`].
#[derive(Debug)]
pub(in crate::operations::backup) struct SwapError {
    pub(in crate::operations::backup) message: String,
    /// Every live database and sidecar is as it was before the swap.
    pub(in crate::operations::backup) originals_intact: bool,
}

impl SwapError {
    pub(in crate::operations::backup) fn intact(message: String) -> Self {
        Self {
            message,
            originals_intact: true,
        }
    }
}

/// Put each extracted database in `staged_root` (`databases/` of the
/// extraction) in place of its live counterpart below `data_root`, keeping
/// the originals in the rollback directory `dir`. Call only with every
/// process that may open these databases stopped, and never in a process
/// that holds SQLite locks: the checks open the live databases.
///
/// Every target is checked before anything changes. A database whose path
/// does not exist live is created with any missing parent directories. A
/// live path that is a symlink, crosses a symlink or a non-directory, is a
/// hard link, or holds something other than a SQLite database is refused.
///
/// The intent record is written before the first change. For each database
/// the original and its sidecars first get a second name in `dir`, so `dir`
/// always holds a complete original set; only then are the live sidecars
/// removed and the snapshot copy renamed over the database. A failure rolls
/// back here; a crash is rolled back by the next server start.
pub(in crate::operations::backup) fn swap_databases(
    staged_root: &Path,
    databases: &[PathBuf],
    data_root: &Path,
    dir: &Path,
) -> Result<RestoredDatabases, SwapError> {
    swap_databases_until(staged_root, databases, data_root, dir, None)
}

/// [`swap_databases`], stopping without any cleanup before mutating step
/// `stop_at` as if the process died there.
fn swap_databases_until(
    staged_root: &Path,
    databases: &[PathBuf],
    data_root: &Path,
    dir: &Path,
    stop_at: Option<usize>,
) -> Result<RestoredDatabases, SwapError> {
    let mut stop = |step| Some(step) == stop_at;
    let mut steps = Steps::new(
        stop_at
            .is_some()
            .then_some(&mut stop as &mut dyn FnMut(usize) -> bool),
    );
    match plan_and_swap(staged_root, databases, data_root, dir, &mut steps) {
        Ok(summary) => Ok(summary),
        Err(_) if steps.stopped() => Err(SwapError {
            message: "stopped".into(),
            originals_intact: false,
        }),
        // Also removes the rollback directory, which nothing may need.
        Err(error) => Err(match journal::roll_back(dir) {
            Ok(()) => SwapError::intact(error),
            Err(rollback) => SwapError {
                message: format!("{error}; {rollback}"),
                originals_intact: false,
            },
        }),
    }
}

fn plan_and_swap(
    staged_root: &Path,
    databases: &[PathBuf],
    data_root: &Path,
    dir: &Path,
    steps: &mut Steps,
) -> Result<RestoredDatabases, String> {
    let root = open_data_root(data_root)?;
    let in_snapshot: BTreeSet<&Path> = databases.iter().map(PathBuf::as_path).collect();
    let live = list_database_tree(data_root)?;
    let live_set: BTreeSet<&Path> = live.iter().map(PathBuf::as_path).collect();
    let untouched = live
        .iter()
        .filter(|path| !in_snapshot.contains(path.as_path()))
        .map(|path| path.to_string_lossy().into_owned())
        .collect();
    let mut planned = Vec::new();
    let mut created_dirs: Vec<PathBuf> = Vec::new();
    for relative in databases {
        // Never let a restored name pair with another live database: SQLite
        // would treat one as the other's sidecar.
        let paired = sidecar_of(relative, |path| live_set.contains(path)).or_else(|| {
            SIDECAR_SUFFIXES
                .iter()
                .map(|suffix| PathBuf::from(with_suffix(relative.as_os_str(), suffix)))
                .find(|sidecar| live_set.contains(sidecar.as_path()))
        });
        if let Some(paired) = paired {
            return Err(format!(
                "Cannot restore {}: {} is a live database, so SQLite would pair the two; move it away and retry",
                relative.display(),
                paired.display()
            ));
        }
        let target = check_target(&root, relative)
            .map_err(|e| format!("Cannot restore {}: {e}", relative.display()))?;
        for missing in target.missing_dirs {
            if !created_dirs.contains(&missing) {
                created_dirs.push(missing);
            }
        }
        planned.push(IntentDatabase {
            relative: relative.clone(),
            existed: target.existed,
        });
    }
    let mut summary = RestoredDatabases {
        untouched,
        ..RestoredDatabases::default()
    };
    for database in &planned {
        let path = database.relative.to_string_lossy().into_owned();
        if database.existed {
            summary.replaced.push(path);
        } else {
            summary.created.push(path);
        }
    }

    // Every live directory a database sits in (or the nearest existing
    // ancestor of one the restore creates) and the staged copies, which
    // this restore extracted into one directory.
    let mut paths = vec![data_root.to_path_buf(), staged_root.to_path_buf()];
    for database in &planned {
        let live = data_root.join(&database.relative);
        let mut parent = live.parent().unwrap_or(data_root).to_path_buf();
        while !parent.exists() && parent != data_root {
            parent.pop();
        }
        paths.push(parent);
    }
    require_same_filesystem(dir, &paths.iter().map(PathBuf::as_path).collect::<Vec<_>>())?;

    let intent = RestoreIntent::databases(data_root, planned, created_dirs);
    steps
        .step()
        .and_then(|()| journal::write_intent(dir, &intent))
        .map_err(|e| format!("record restore intent in {}: {e}", dir.display()))?;
    for database in &intent.databases {
        replace(&root, staged_root, dir, &database.relative, steps)
            .map_err(|e| format!("restore {}: {e}", database.relative.display()))?;
    }
    Ok(summary)
}

fn open_data_root(data_root: &Path) -> Result<File, String> {
    fs::open(data_root, READ_FLAGS | OFlags::DIRECTORY, Mode::empty())
        .map(File::from)
        .map_err(|e| format!("open data dir {}: {e}", data_root.display()))
}

struct Target {
    existed: bool,
    /// Missing parent directories, relative to the data root, parents first.
    missing_dirs: Vec<PathBuf>,
}

fn check_target(root: &File, relative: &Path) -> io::Result<Target> {
    let (parents, name) = split_relative(relative)?;
    let mut dir = root.try_clone()?;
    for (depth, component) in parents.iter().enumerate() {
        match fs::openat(
            &dir,
            *component,
            READ_FLAGS | OFlags::DIRECTORY,
            Mode::empty(),
        ) {
            Ok(next) => dir = File::from(next),
            // The rest of the path is created.
            Err(rustix::io::Errno::NOENT) => {
                let missing_dirs = (depth..parents.len())
                    .map(|end| parents[..=end].iter().collect())
                    .collect();
                return Ok(Target {
                    existed: false,
                    missing_dirs,
                });
            }
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
        Err(rustix::io::Errno::NOENT) => {
            return Ok(Target {
                existed: false,
                missing_dirs: Vec::new(),
            });
        }
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
    Ok(Target {
        existed: true,
        missing_dirs: Vec::new(),
    })
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

/// Open the live directory `parents` below the data root without following
/// symlinks, creating missing components when `create` is set.
fn open_live_dir(root: &File, parents: &[&OsStr], create: bool) -> io::Result<File> {
    let mut dir = root.try_clone()?;
    for component in parents {
        let flags = READ_FLAGS | OFlags::DIRECTORY;
        let next = match fs::openat(&dir, *component, flags, Mode::empty()) {
            Ok(next) => next,
            Err(rustix::io::Errno::NOENT) if create => {
                fs::mkdirat(&dir, *component, Mode::from_raw_mode(0o700))?;
                fs::fsync(&dir)?;
                fs::openat(&dir, *component, flags, Mode::empty())?
            }
            Err(error) => return Err(error.into()),
        };
        dir = File::from(next);
    }
    Ok(dir)
}

fn lstat(path: &Path) -> io::Result<Option<fs::Stat>> {
    match fs::lstat(path) {
        Ok(stat) => Ok(Some(stat)),
        Err(rustix::io::Errno::NOENT) => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn statat(dir: &File, name: &OsStr) -> io::Result<Option<fs::Stat>> {
    match fs::statat(dir, name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(stat) => Ok(Some(stat)),
        Err(rustix::io::Errno::NOENT) => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn same_file(a: &fs::Stat, b: &fs::Stat) -> bool {
    a.st_dev == b.st_dev && a.st_ino == b.st_ino
}

fn sync_parent(path: &Path) -> io::Result<()> {
    match path.parent() {
        Some(parent) => File::open(parent)?.sync_all(),
        None => Ok(()),
    }
}

fn replace(
    root: &File,
    staged_root: &Path,
    dir: &Path,
    relative: &Path,
    steps: &mut Steps,
) -> io::Result<()> {
    let (parents, name) = split_relative(relative)?;
    let mut live = root.try_clone()?;
    for (depth, component) in parents.iter().enumerate() {
        match open_live_dir(&live, &[*component], false) {
            Ok(next) => live = next,
            Err(error) if error.raw_os_error() == Some(libc::ENOENT) => {
                for missing in &parents[depth..] {
                    steps.step()?;
                    live = open_live_dir(&live, &[*missing], true)?;
                }
                break;
            }
            Err(error) => return Err(error),
        }
    }
    let kept = dir.join(relative);
    if let Some(parent) = kept.parent() {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)?;
    }

    // A complete second set of the originals first.
    let original = match statat(&live, name)? {
        Some(stat) if FileType::from_raw_mode(stat.st_mode) == FileType::RegularFile => {
            Some(open_regular(&live, name)?.metadata()?)
        }
        Some(_) => return Err(io::Error::other("the live path is not a regular file")),
        None => None,
    };
    if original.is_some() {
        steps.step()?;
        fs::linkat(&live, name, CWD, &kept, AtFlags::empty())?;
    }
    let mut sidecars = Vec::new();
    for suffix in SIDECAR_SUFFIXES {
        let sidecar = with_suffix(name, suffix);
        if statat(&live, &sidecar)?.is_none() {
            continue;
        }
        steps.step()?;
        fs::linkat(
            &live,
            sidecar.as_os_str(),
            CWD,
            with_suffix(kept.as_os_str(), suffix),
            AtFlags::empty(),
        )?;
        sidecars.push(sidecar);
    }
    sync_parent(&kept)?;

    // Then the live sidecars go: left beside the snapshot copy they could
    // roll newer pages into it.
    for sidecar in &sidecars {
        steps.step()?;
        fs::unlinkat(&live, sidecar.as_os_str(), AtFlags::empty())?;
    }

    let staged = staged_root.join(relative);
    let staged_file = std::fs::OpenOptions::new().read(true).open(&staged)?;
    // The modes app data provisioning gives a database, never the live
    // file's: the app controls those and could have made it world-writable.
    staged_file.set_permissions(std::fs::Permissions::from_mode(restored_mode(relative)))?;
    if let Some(meta) = &original {
        // Restoring the owner needs privileges the service may lack;
        // the app data provisioning on restart repairs it either way.
        let _ = std::os::unix::fs::fchown(&staged_file, Some(meta.uid()), Some(meta.gid()));
    }
    staged_file.sync_all()?;
    steps.step()?;
    fs::renameat(CWD, &staged, &live, name)?;
    fs::fsync(&live)?;
    Ok(())
}

/// Group read/write below `app/`, which the app's group shares; service
/// only below `tako/`. Matches `secure_data_tree` in app provisioning.
fn restored_mode(relative: &Path) -> u32 {
    if relative.starts_with(TAKO_DATA_DIR) {
        0o600
    } else {
        0o660
    }
}

/// Undo a database swap from its intent record and what the file system
/// shows. Each database is in one of the states its steps leave behind,
/// told apart by whether the kept copy and the live file are one inode:
///
/// - kept copy present and the live name is the same file: not replaced
///   yet; drop the extra name.
/// - kept copy present and the live name is another file: replaced; the
///   live sidecars belong to the snapshot copy, so they go, and the kept
///   copy moves back.
/// - no kept copy: the database did not exist before (remove what the swap
///   created), or its first step never ran.
///
/// Then each kept sidecar is moved back unless the live one is the same
/// file. Repeating a rollback after it stopped part way is safe.
pub(in crate::operations::backup) fn database_rollback(
    intent: &RestoreIntent,
    dir: &Path,
) -> Result<(), String> {
    let root = open_data_root(&intent.data_root)?;
    let mut errors = Vec::new();
    for database in intent.databases.iter().rev() {
        if let Err(error) = roll_back_database(&root, dir, database) {
            errors.push(format!("{}: {error}", database.relative.display()));
        }
    }
    for created in intent.created_dirs.iter().rev() {
        let result = split_relative(created).and_then(|(parents, name)| {
            let parent = match open_live_dir(&root, &parents, false) {
                Ok(parent) => parent,
                Err(error) if error.raw_os_error() == Some(libc::ENOENT) => return Ok(()),
                Err(error) => return Err(error),
            };
            match fs::unlinkat(&parent, name, AtFlags::REMOVEDIR) {
                Ok(()) | Err(rustix::io::Errno::NOENT | rustix::io::Errno::NOTEMPTY) => Ok(()),
                Err(error) => Err(error.into()),
            }
        });
        if let Err(error) = result {
            errors.push(format!("remove {}: {error}", created.display()));
        }
    }
    if !errors.is_empty() {
        return Err(format!(
            "could not put back original databases ({})",
            errors.join("; ")
        ));
    }
    Ok(())
}

fn roll_back_database(root: &File, dir: &Path, database: &IntentDatabase) -> io::Result<()> {
    let (parents, name) = split_relative(&database.relative)?;
    let live = match open_live_dir(root, &parents, false) {
        Ok(live) => live,
        // Never created, and nothing was kept for it.
        Err(error) if error.raw_os_error() == Some(libc::ENOENT) && !database.existed => {
            return Ok(());
        }
        Err(error) => return Err(error),
    };
    let kept = dir.join(&database.relative);
    let remove_live_sidecars = || -> io::Result<()> {
        for suffix in SIDECAR_SUFFIXES {
            match fs::unlinkat(
                &live,
                with_suffix(name, suffix).as_os_str(),
                AtFlags::empty(),
            ) {
                Ok(()) | Err(rustix::io::Errno::NOENT) => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    };
    match (lstat(&kept)?, statat(&live, name)?) {
        (Some(kept_stat), Some(live_stat)) if same_file(&kept_stat, &live_stat) => {
            std::fs::remove_file(&kept)?;
        }
        (Some(_), _) => {
            remove_live_sidecars()?;
            fs::renameat(CWD, &kept, &live, name)?;
        }
        (None, Some(_)) if !database.existed => {
            remove_live_sidecars()?;
            fs::unlinkat(&live, name, AtFlags::empty())?;
        }
        (None, _) => {}
    }
    for suffix in SIDECAR_SUFFIXES {
        let kept_sidecar = with_suffix(kept.as_os_str(), suffix);
        let Some(kept_stat) = lstat(Path::new(&kept_sidecar))? else {
            continue;
        };
        let live_sidecar = with_suffix(name, suffix);
        match statat(&live, &live_sidecar)? {
            Some(live_stat) if same_file(&kept_stat, &live_stat) => {
                std::fs::remove_file(&kept_sidecar)?;
            }
            Some(_) => {
                return Err(io::Error::other(format!(
                    "{} exists beside the original database",
                    live_sidecar.to_string_lossy()
                )));
            }
            None => fs::renameat(CWD, &kept_sidecar, &live, live_sidecar.as_os_str())?,
        }
    }
    fs::fsync(&live)?;
    Ok(())
}

#[cfg(test)]
mod tests;
