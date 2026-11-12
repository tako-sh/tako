use std::ffi::OsStr;
use std::fs::File;
use std::io::{self, Read};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, FileExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use rustix::fs::{self, AtFlags, FileType, Mode, OFlags};
use sha2::{Digest, Sha256};

mod database_restore;
mod sqlite;

use super::journal::{self, RestoreIntent, RollbackDir, Steps};
use super::mounts;
pub(super) use database_restore::{
    SwapError, database_rollback, extract_database_snapshot, swap_databases,
};

const TAKO_DATA_DIR: &str = "tako";
const CHANNELS_DB_FILENAME: &str = "channels.sqlite";
const CHANNELS_DB_COMPANION_FILENAMES: [&str; 2] = ["channels.sqlite-wal", "channels.sqlite-shm"];
const CACHE_DB_FILENAME: &str = "cache.sqlite";
const CACHE_DB_COMPANION_FILENAMES: [&str; 2] = ["cache.sqlite-wal", "cache.sqlite-shm"];
const WORKFLOWS_DB_COMPANION_FILENAMES: [&str; 2] =
    ["workflows.sqlite-wal", "workflows.sqlite-shm"];

const READ_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::CLOEXEC)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::NONBLOCK);

/// What a snapshot of the app data tree contains.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SnapshotScope {
    /// Every file, directory, and symlink.
    Full,
    /// Only SQLite databases, at their relative paths. Symlinks and other
    /// files are left out; directories that end up empty are removed.
    Databases,
}

pub(crate) fn snapshot_data_tree(
    anchor: &Path,
    relative: &Path,
    destination: &Path,
) -> Result<(), String> {
    snapshot_tree(anchor, relative, destination, SnapshotScope::Full)
}

/// Same detection and capture as [`snapshot_data_tree`], restricted to the
/// SQLite databases the full archive would contain.
pub(crate) fn snapshot_database_tree(
    anchor: &Path,
    relative: &Path,
    destination: &Path,
) -> Result<(), String> {
    snapshot_tree(anchor, relative, destination, SnapshotScope::Databases)
}

fn snapshot_tree(
    anchor: &Path,
    relative: &Path,
    destination: &Path,
    scope: SnapshotScope,
) -> Result<(), String> {
    let root = open_snapshot_root(anchor, relative)?;
    let parent = destination
        .parent()
        .ok_or("snapshot destination has no parent")?;
    std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(destination)
        .map_err(|e| format!("create snapshot dir {}: {e}", destination.display()))?;
    copy_snapshot_dir(&root, Path::new(""), destination, scope)
        .map_err(|e| format!("snapshot {}: {e}", anchor.join(relative).display()))
}

fn open_snapshot_root(anchor: &Path, relative: &Path) -> Result<File, String> {
    let flags = READ_FLAGS | OFlags::DIRECTORY;
    let mut root = fs::open(anchor, flags, Mode::empty())
        .map_err(|e| format!("open data root {}: {e}", anchor.display()))?;
    for component in relative.components() {
        let std::path::Component::Normal(name) = component else {
            return Err("invalid snapshot source".into());
        };
        root = fs::openat(&root, name, flags, Mode::empty())
            .map_err(|e| format!("open data directory {}: {e}", relative.display()))?;
    }
    Ok(File::from(root))
}

/// Bytes of the regular files a [`snapshot_data_tree`] would copy. Only stats
/// entries and never opens a regular file, so it is safe in the server.
pub(crate) fn measure_data_tree(anchor: &Path, relative: &Path) -> Result<u64, String> {
    fn walk(dir: &File, relative: &Path, total: &mut u64) -> io::Result<()> {
        for entry in fs::Dir::read_from(dir)? {
            let entry = entry?;
            let name = OsStr::from_bytes(entry.file_name().to_bytes());
            if name == "." || name == ".." {
                continue;
            }
            let path = relative.join(name);
            if is_transient_tako_sqlite_file(&path) {
                continue;
            }
            let stat = match fs::statat(dir, name, AtFlags::SYMLINK_NOFOLLOW) {
                Ok(stat) => stat,
                Err(rustix::io::Errno::NOENT) => continue,
                Err(error) => return Err(error.into()),
            };
            match FileType::from_raw_mode(stat.st_mode) {
                FileType::Directory => {
                    let child =
                        fs::openat(dir, name, READ_FLAGS | OFlags::DIRECTORY, Mode::empty())?;
                    walk(&File::from(child), &path, total)?;
                }
                FileType::RegularFile => *total = total.saturating_add(stat.st_size as u64),
                _ => {}
            }
        }
        Ok(())
    }
    let root = open_snapshot_root(anchor, relative)?;
    let mut total = 0_u64;
    walk(&root, Path::new(""), &mut total)
        .map_err(|e| format!("measure {}: {e}", anchor.join(relative).display()))?;
    Ok(total)
}

/// Bytes of every SQLite database a [`snapshot_database_tree`] would capture,
/// counting each main file and its `-wal`. Reads only file headers.
pub(crate) fn measure_database_tree(anchor: &Path, relative: &Path) -> Result<u64, String> {
    let root = open_snapshot_root(anchor, relative)?;
    let mut total = 0_u64;
    visit_database_dir(&root, Path::new(""), &mut |_, parent, name, file| {
        let mut wal = name.to_os_string();
        wal.push("-wal");
        let wal_len = match fs::statat(parent, wal.as_os_str(), AtFlags::SYMLINK_NOFOLLOW) {
            Ok(stat) => stat.st_size as u64,
            Err(rustix::io::Errno::NOENT) => 0,
            Err(error) => return Err(error.into()),
        };
        total = total
            .saturating_add(file.metadata()?.len())
            .saturating_add(wal_len);
        Ok(())
    })
    .map_err(|e| format!("measure {}: {e}", anchor.join(relative).display()))?;
    Ok(total)
}

/// Relative paths of the SQLite databases a [`snapshot_database_tree`] would
/// capture. Opens every database file: never call it while this process
/// holds SQLite locks on one of them.
pub(super) fn list_database_tree(anchor: &Path) -> Result<Vec<PathBuf>, String> {
    let root = open_snapshot_root(anchor, Path::new(""))?;
    let mut databases = Vec::new();
    visit_database_dir(&root, Path::new(""), &mut |path, _, _, _| {
        databases.push(path.to_path_buf());
        Ok(())
    })
    .map_err(|e| format!("scan databases in {}: {e}", anchor.display()))?;
    databases.sort();
    Ok(databases)
}

type DatabaseVisitor<'a> = dyn FnMut(&Path, &File, &OsStr, &File) -> io::Result<()> + 'a;

/// Call `visit` with the relative path, parent directory, name, and open file
/// of every SQLite database below `parent`, with the snapshot's exclusions.
fn visit_database_dir(
    parent: &File,
    relative: &Path,
    visit: &mut DatabaseVisitor<'_>,
) -> io::Result<()> {
    for entry in fs::Dir::read_from(parent)? {
        let entry = entry?;
        let name = OsStr::from_bytes(entry.file_name().to_bytes());
        if name == "." || name == ".." {
            continue;
        }
        let path = relative.join(name);
        if is_transient_tako_sqlite_file(&path) || is_sqlite_companion(parent, name)? {
            continue;
        }
        let stat = match fs::statat(parent, name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(stat) => stat,
            // The live tree may change while it is measured.
            Err(rustix::io::Errno::NOENT) => continue,
            Err(error) => return Err(error.into()),
        };
        match FileType::from_raw_mode(stat.st_mode) {
            FileType::Directory => {
                let dir = fs::openat(parent, name, READ_FLAGS | OFlags::DIRECTORY, Mode::empty())?;
                visit_database_dir(&File::from(dir), &path, visit)?;
            }
            FileType::RegularFile => {
                let file = open_regular(parent, name)?;
                if is_sqlite_database(&file)? {
                    visit(&path, parent, name, &file)?;
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn copy_snapshot_dir(
    parent: &File,
    relative: &Path,
    destination: &Path,
    scope: SnapshotScope,
) -> io::Result<()> {
    for entry in fs::Dir::read_from(parent)? {
        let entry = entry?;
        let name = OsStr::from_bytes(entry.file_name().to_bytes());
        if name == "." || name == ".." {
            continue;
        }
        let path = relative.join(name);
        // SQLite may unlink a journal after read_dir buffered its name. Only
        // the pinned database snapshot consumes its companion files.
        if is_transient_tako_sqlite_file(&path) || is_sqlite_companion(parent, name)? {
            continue;
        }
        let dest = destination.join(name);
        let stat = fs::statat(parent, name, AtFlags::SYMLINK_NOFOLLOW)?;
        match FileType::from_raw_mode(stat.st_mode) {
            FileType::Symlink if scope == SnapshotScope::Full => {
                let target = fs::readlinkat(parent, name, Vec::new())?;
                std::os::unix::fs::symlink(OsStr::from_bytes(target.to_bytes()), dest)?;
            }
            FileType::Directory => {
                let dir = fs::openat(parent, name, READ_FLAGS | OFlags::DIRECTORY, Mode::empty())?;
                std::fs::create_dir(&dest)?;
                copy_snapshot_dir(&File::from(dir), &path, &dest, scope)?;
                // Keep the top-level app/ and tako/ roots so the archive
                // layout matches the full archive.
                if scope == SnapshotScope::Databases && !relative.as_os_str().is_empty() {
                    match std::fs::remove_dir(&dest) {
                        Ok(()) => {}
                        Err(error) if error.kind() == io::ErrorKind::DirectoryNotEmpty => {}
                        Err(error) => return Err(error),
                    }
                }
            }
            FileType::RegularFile => {
                let mut file = open_regular(parent, name)?;
                if is_sqlite_database(&file)? {
                    sqlite::snapshot(parent, name, file, &dest).map_err(|e| {
                        io::Error::other(format!("snapshot {}: {e}", path.display()))
                    })?;
                } else if scope == SnapshotScope::Full {
                    let mut output = File::create_new(dest)?;
                    io::copy(&mut file, &mut output)?;
                    // The copy is owned by the service. Never publish set-ID
                    // bits from an app-controlled executable in the archive.
                    let mode = file.metadata()?.mode() & 0o777;
                    output.set_permissions(std::fs::Permissions::from_mode(mode))?;
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn open_regular(parent: &File, name: &OsStr) -> io::Result<File> {
    let file = File::from(fs::openat(parent, name, READ_FLAGS, Mode::empty())?);
    let meta = file.metadata()?;
    if !meta.is_file() {
        return Err(io::Error::other(format!("expected regular file {name:?}")));
    }
    // A hard link can name a service file outside the app even with O_NOFOLLOW.
    if meta.nlink() != 1 {
        return Err(io::Error::other(format!("refusing hard link {name:?}")));
    }
    Ok(file)
}

fn is_sqlite_database(file: &File) -> io::Result<bool> {
    let mut header = [0; 16];
    match file.read_exact_at(&mut header, 0) {
        Ok(()) => Ok(&header == b"SQLite format 3\0"),
        Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => Ok(false),
        Err(error) => Err(error),
    }
}

/// Files SQLite pairs with a database, named by appending these.
pub(super) const SIDECAR_SUFFIXES: [&str; 3] = ["-wal", "-shm", "-journal"];

/// The name of the database `name` would be a sidecar of. The one rule for
/// capture and restore: a file is a sidecar only when that database exists
/// beside it, so a database such as `audit-journal` without an `audit` is a
/// database like any other.
pub(super) fn sidecar_base(name: &OsStr) -> Option<&OsStr> {
    SIDECAR_SUFFIXES
        .iter()
        .find_map(|suffix| name.as_bytes().strip_suffix(suffix.as_bytes()))
        .map(OsStr::from_bytes)
}

fn is_sqlite_companion(parent: &File, name: &OsStr) -> io::Result<bool> {
    let Some(base) = sidecar_base(name) else {
        return Ok(false);
    };
    let stat = match fs::statat(parent, base, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(stat) => stat,
        Err(rustix::io::Errno::NOENT) => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile {
        return Ok(false);
    }
    is_sqlite_database(&open_regular(parent, base)?)
}

fn is_transient_tako_sqlite_file(relative_path: &Path) -> bool {
    if !matches!(relative_path.parent(), Some(parent) if parent == Path::new(TAKO_DATA_DIR)) {
        return false;
    }
    let Some(name) = relative_path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    name == CHANNELS_DB_FILENAME
        || CHANNELS_DB_COMPANION_FILENAMES.contains(&name)
        || name == CACHE_DB_FILENAME
        || CACHE_DB_COMPANION_FILENAMES.contains(&name)
}

pub(super) fn create_backup_archive(
    snapshot_dir: &Path,
    archive_path: &Path,
) -> Result<(), String> {
    create_archive(snapshot_dir, archive_path, Path::new(""))
}

/// Database snapshots nest `app/` and `tako/` below this root, so a full
/// restore, which requires both at the top level, can never apply one.
pub(super) const DATABASE_SNAPSHOT_ARCHIVE_ROOT: &str = "databases";

pub(super) fn create_database_snapshot_archive(
    snapshot_dir: &Path,
    archive_path: &Path,
) -> Result<(), String> {
    create_archive(
        snapshot_dir,
        archive_path,
        Path::new(DATABASE_SNAPSHOT_ARCHIVE_ROOT),
    )
}

fn create_archive(snapshot_dir: &Path, archive_path: &Path, root: &Path) -> Result<(), String> {
    if let Some(parent) = archive_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("create archive dir {}: {e}", parent.display()))?;
    }
    let file = std::fs::File::create(archive_path)
        .map_err(|e| format!("create backup archive {}: {e}", archive_path.display()))?;
    let encoder = zstd::stream::write::Encoder::new(file, 3)
        .map_err(|e| format!("initialize zstd encoder: {e}"))?;
    let mut archive = tar::Builder::new(encoder);
    archive.follow_symlinks(false);
    for name in ["app", "tako"] {
        let path = snapshot_dir.join(name);
        match std::fs::symlink_metadata(&path) {
            Ok(meta) if meta.is_dir() => archive
                .append_dir_all(root.join(name), &path)
                .map_err(|e| format!("append {name} data to backup archive: {e}"))?,
            Ok(_) => return Err(format!("backup data root {name} is not a directory")),
            Err(error) => return Err(format!("read backup data root {name}: {error}")),
        }
    }
    let encoder = archive
        .into_inner()
        .map_err(|e| format!("finish backup tar stream: {e}"))?;
    encoder
        .finish()
        .map_err(|e| format!("finish backup zstd stream: {e}"))?;
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct DatabaseDigest {
    pub(super) hex: String,
    /// Relative paths of the digested databases, in path order.
    pub(super) databases: Vec<PathBuf>,
}

/// Keyed digest of the SQLite databases in a private snapshot directory:
/// each database's relative path and exact bytes, in path order. The files
/// are compacted copies, so equal digests mean byte-identical databases.
/// Keyed so the plaintext index cannot confirm guesses about the contents.
pub(super) fn database_digest(snapshot_dir: &Path, key: &[u8]) -> Result<DatabaseDigest, String> {
    let mut databases = Vec::new();
    for name in ["app", "tako"] {
        collect_snapshot_databases(&snapshot_dir.join(name), Path::new(name), &mut databases)
            .map_err(|e| format!("scan snapshot databases: {e}"))?;
    }
    databases.sort();
    let pkey = openssl::pkey::PKey::hmac(key).map_err(|e| format!("digest key: {e}"))?;
    let mut signer = openssl::sign::Signer::new(openssl::hash::MessageDigest::sha256(), &pkey)
        .map_err(|e| format!("digest: {e}"))?;
    let mut buf = vec![0_u8; 128 * 1024];
    for relative in &databases {
        let path = snapshot_dir.join(relative);
        let mut file =
            std::fs::File::open(&path).map_err(|e| format!("open {}: {e}", path.display()))?;
        let len = file
            .metadata()
            .map_err(|e| format!("stat {}: {e}", path.display()))?
            .len();
        let relative = relative.as_os_str().as_bytes();
        for part in [
            &(relative.len() as u64).to_le_bytes()[..],
            relative,
            &len.to_le_bytes()[..],
        ] {
            signer.update(part).map_err(|e| format!("digest: {e}"))?;
        }
        let mut read_total = 0_u64;
        loop {
            let read = file
                .read(&mut buf)
                .map_err(|e| format!("read {}: {e}", path.display()))?;
            if read == 0 {
                break;
            }
            read_total += read as u64;
            signer
                .update(&buf[..read])
                .map_err(|e| format!("digest: {e}"))?;
        }
        if read_total != len {
            return Err(format!("{} changed while hashing", path.display()));
        }
    }
    let digest = signer.sign_to_vec().map_err(|e| format!("digest: {e}"))?;
    Ok(DatabaseDigest {
        hex: hex::encode(digest),
        databases,
    })
}

fn collect_snapshot_databases(
    dir: &Path,
    relative: &Path,
    out: &mut Vec<PathBuf>,
) -> io::Result<()> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    for entry in entries {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let path = relative.join(entry.file_name());
        if file_type.is_dir() {
            collect_snapshot_databases(&entry.path(), &path, out)?;
        } else if file_type.is_file() && is_sqlite_database(&File::open(entry.path())?)? {
            out.push(path);
        }
    }
    Ok(())
}

/// A full restore: the restored tree is live and the original tree is kept
/// in the rollback directory until [`RestoredData::commit`] or
/// [`RestoredData::rollback`]; until then a crash is undone at the next
/// server start.
#[derive(Debug)]
#[must_use = "commit the validated restore or roll it back after stopping the app"]
pub(super) struct RestoredData {
    dir: RollbackDir,
}

impl RestoredData {
    pub(super) fn commit(self) -> Result<(), String> {
        self.dir.finish()
    }

    /// Call only after stopping all processes that may access the restored tree.
    pub(super) fn rollback(self) -> Result<(), String> {
        self.dir.roll_back()
    }

    /// See [`RollbackDir::abandon`].
    pub(super) fn abandon(mut self, reason: &str) -> PathBuf {
        self.dir.abandon(reason)
    }

    #[cfg(test)]
    fn leave_for_recovery(self) {
        self.dir.leave_for_recovery();
    }
}

/// Where a full restore keeps the original tree.
fn kept_tree(dir: &Path) -> PathBuf {
    dir.join("data")
}

/// Put the original tree of a full restore back. With `displace_live`, a
/// tree at the data root is the restored one and moves into the rollback
/// directory first; without it, the rollback fails rather than move a tree
/// it did not put there.
pub(super) fn full_rollback(
    intent: &RestoreIntent,
    dir: &Path,
    displace_live: bool,
) -> Result<(), String> {
    let data_root = &intent.data_root;
    let original = kept_tree(dir);
    let exists = |path: &Path| match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(format!("inspect {}: {error}", path.display())),
    };
    let kept = exists(&original)?;
    if !kept && intent.had_data {
        // The original tree never moved.
        return Ok(());
    }
    if displace_live && exists(data_root)? {
        let restored = dir.join("restored");
        std::fs::rename(data_root, &restored).map_err(|error| {
            format!("move restored data {} aside: {error}", data_root.display())
        })?;
    }
    if kept {
        std::fs::rename(&original, data_root).map_err(|error| {
            format!(
                "move original data back to {}: {error}",
                data_root.display()
            )
        })?;
    }
    sync_dir(data_root.parent().unwrap_or(data_root))
}

fn sync_dir(path: &Path) -> Result<(), String> {
    File::open(path)
        .and_then(|dir| dir.sync_all())
        .map_err(|e| format!("sync {}: {e}", path.display()))
}

/// A restore moves files with rename and link, which cannot cross
/// filesystems or mounts: refuse before changing anything unless every path
/// is on the mount of the rollback directory. An app data directory mounted
/// on its own (even a bind mount of the same filesystem) cannot be restored
/// in place.
pub(super) fn require_same_filesystem(rollback_dir: &Path, paths: &[&Path]) -> Result<(), String> {
    let identity = |path: &Path| {
        mounts::identity_of(path).map_err(|e| format!("inspect {}: {e}", path.display()))
    };
    let expected = identity(rollback_dir)?;
    for path in paths {
        if identity(path)? != expected {
            return Err(format!(
                "Cannot restore in place: {} is on a different filesystem or mount than {}. Restores move files with rename, so the app data directory must be on the same filesystem as the Tako data directory, not a separate mount.",
                path.display(),
                rollback_dir.parent().unwrap_or(rollback_dir).display()
            ));
        }
    }
    Ok(())
}

/// A full restore renames the whole data tree, which would carry a mount
/// inside it into the rollback directory and leave the app writing to a
/// plain directory: refuse while anything is mounted at or below it.
pub(super) fn refuse_mounts_in_data_tree(data_root: &Path) -> Result<(), String> {
    if std::fs::symlink_metadata(data_root).is_err() {
        return Ok(());
    }
    let mounts = mounts::mount_points_at_or_below(data_root)?;
    match mounts.first() {
        None => Ok(()),
        Some(mount) => Err(format!(
            "Cannot restore a full backup in place: {} is a mount point in the app data directory {}. A full restore replaces the whole directory and would move the mount with it. Unmount it and retry.",
            mount.display(),
            data_root.display()
        )),
    }
}

pub(super) fn restore_data_tree(
    extracted_dir: &Path,
    data_root: &Path,
) -> Result<RestoredData, SwapError> {
    restore_data_tree_with(extracted_dir, data_root, Steps::new(None))
}

/// [`restore_data_tree`] with a test hook before each mutating step. A
/// stopped swap returns without cleanup, as if the process died there.
fn restore_data_tree_with(
    extracted_dir: &Path,
    data_root: &Path,
    mut steps: Steps<'_>,
) -> Result<RestoredData, SwapError> {
    if ["app", "tako"].iter().any(|name| {
        !std::fs::symlink_metadata(extracted_dir.join(name)).is_ok_and(|meta| meta.is_dir())
    }) {
        return Err(SwapError::intact(
            "Backup archive is missing app/ or tako/ data directories.".to_string(),
        ));
    }
    remove_transient_tako_sqlite_stores(extracted_dir).map_err(SwapError::intact)?;
    let parent = data_root
        .parent()
        .ok_or_else(|| format!("data root has no parent: {}", data_root.display()))
        .map_err(SwapError::intact)?;
    std::fs::create_dir_all(parent)
        .map_err(|e| SwapError::intact(format!("create data parent {}: {e}", parent.display())))?;
    refuse_mounts_in_data_tree(data_root).map_err(SwapError::intact)?;
    let mut dir = RollbackDir::create(data_root).map_err(SwapError::intact)?;
    let had_data = std::fs::symlink_metadata(data_root).is_ok();
    let mut paths = vec![extracted_dir];
    if had_data {
        paths.push(data_root);
    }
    if let Err(error) = require_same_filesystem(dir.path(), &paths) {
        if let Err(cleanup) = dir.finish() {
            tracing::warn!("Refused restore left its rollback directory: {cleanup}");
        }
        return Err(SwapError::intact(error));
    }
    let intent = RestoreIntent::full(data_root, had_data);
    let original = kept_tree(dir.path());

    let result = (|| -> Result<(), String> {
        steps
            .step()
            .and_then(|()| journal::write_intent(dir.path(), &intent))
            .map_err(|e| format!("record restore intent in {}: {e}", dir.path().display()))?;
        if had_data {
            steps
                .step()
                .and_then(|()| std::fs::rename(data_root, &original))
                .map_err(|e| {
                    format!(
                        "move existing data dir {} to {}: {e}",
                        data_root.display(),
                        original.display()
                    )
                })?;
        }
        steps
            .step()
            .and_then(|()| std::fs::rename(extracted_dir, data_root))
            .map_err(|e| format!("restore data dir {}: {e}", data_root.display()))?;
        sync_dir(parent)
    })();
    match result {
        Ok(()) => Ok(RestoredData { dir }),
        Err(_) if steps.stopped() => {
            dir.leave_for_recovery();
            Err(SwapError {
                message: "stopped".into(),
                originals_intact: false,
            })
        }
        // Something may have put a tree at the data root meanwhile; only
        // the original moving back counts as kept.
        Err(error) => match full_rollback(&intent, dir.path(), false) {
            Ok(()) => {
                if let Err(cleanup) = dir.finish() {
                    tracing::warn!("Restore rolled back but its rollback copy remains: {cleanup}");
                }
                Err(SwapError::intact(error))
            }
            Err(rollback) => {
                let kept = dir.abandon(&format!("{error}; {rollback}"));
                Err(SwapError {
                    message: format!(
                        "{error}; {rollback}; original data remains at {}",
                        kept_tree(&kept).display()
                    ),
                    originals_intact: false,
                })
            }
        },
    }
}

fn remove_transient_tako_sqlite_stores(data_root: &Path) -> Result<(), String> {
    for name in [CHANNELS_DB_FILENAME, CACHE_DB_FILENAME]
        .into_iter()
        .chain(CHANNELS_DB_COMPANION_FILENAMES)
        .chain(CACHE_DB_COMPANION_FILENAMES)
        .chain(WORKFLOWS_DB_COMPANION_FILENAMES)
    {
        let path = data_root.join(TAKO_DATA_DIR).join(name);
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(format!(
                    "remove transient Tako sqlite file {}: {error}",
                    path.display()
                ));
            }
        }
    }
    Ok(())
}

pub(super) fn sha256_file_hex(path: &Path) -> Result<String, String> {
    let mut file =
        std::fs::File::open(path).map_err(|e| format!("open {}: {e}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0_u8; 128 * 1024];
    loop {
        let read = file
            .read(&mut buf)
            .map_err(|e| format!("read {}: {e}", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buf[..read]);
    }
    Ok(hex::encode(hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn restore_retains_previous_data_until_restart_is_validated() {
        let temp = TempDir::new().unwrap();
        let live = temp.path().join("data");
        let extracted = temp.path().join("extracted");
        for root in [&live, &extracted] {
            std::fs::create_dir_all(root.join("app")).unwrap();
            std::fs::create_dir_all(root.join("tako")).unwrap();
        }
        std::fs::write(live.join("app/marker"), "original").unwrap();
        std::fs::write(extracted.join("app/marker"), "backup").unwrap();
        let swap = restore_data_tree(&extracted, &live).unwrap();
        let previous = std::fs::read_dir(temp.path())
            .unwrap()
            .filter_map(Result::ok)
            .find(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".data-restore-prev-")
            })
            .expect("original data must survive until restart succeeds")
            .path();
        assert_eq!(
            std::fs::read_to_string(previous.join("data/app/marker")).unwrap(),
            "original"
        );
        assert_eq!(
            std::fs::read_to_string(live.join("app/marker")).unwrap(),
            "backup"
        );
        swap.rollback().unwrap();
        assert_eq!(
            std::fs::read_to_string(live.join("app/marker")).unwrap(),
            "original"
        );
        assert!(!previous.exists());
    }

    fn full_restore_fixture(temp: &Path) -> (PathBuf, PathBuf) {
        let live = temp.join("data");
        let extracted = temp.join("extracted");
        for root in [&live, &extracted] {
            std::fs::create_dir_all(root.join("app")).unwrap();
            std::fs::create_dir_all(root.join("tako")).unwrap();
        }
        std::fs::write(live.join("app/marker"), "original").unwrap();
        std::fs::write(extracted.join("app/marker"), "backup").unwrap();
        (live, extracted)
    }

    fn rollback_dirs(temp: &Path) -> Vec<PathBuf> {
        dirs_named(temp, journal::ROLLBACK_DIR_PREFIX)
    }

    fn dirs_named(temp: &Path, prefix: &str) -> Vec<PathBuf> {
        std::fs::read_dir(temp)
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with(prefix)
            })
            .collect()
    }

    fn restore_until(extracted: &Path, live: &Path, stop_at: usize) -> SwapError {
        let mut stop = |step| step == stop_at;
        restore_data_tree_with(extracted, live, Steps::new(Some(&mut stop))).unwrap_err()
    }

    /// A tree that appears at the data root between the two renames makes
    /// both the restore and the move back fail: the originals are not in
    /// place, and the error says where they are.
    #[test]
    fn a_failed_full_swap_reports_originals_it_could_not_move_back() {
        let temp = TempDir::new().unwrap();
        let (live, extracted) = full_restore_fixture(temp.path());
        let mut intrude = |step| {
            if step == 2 {
                std::fs::create_dir_all(live.join("app")).unwrap();
                std::fs::write(live.join("app/provisioned"), "").unwrap();
            }
            false
        };
        let error =
            restore_data_tree_with(&extracted, &live, Steps::new(Some(&mut intrude))).unwrap_err();
        assert!(!error.originals_intact, "{}", error.message);
        // Never rolled back by a later start: the server kept running.
        assert!(rollback_dirs(temp.path()).is_empty());
        let [dir] = dirs_named(temp.path(), journal::ABANDONED_DIR_PREFIX)
            .try_into()
            .unwrap();
        let kept = dir.join("data");
        assert!(
            error
                .message
                .ends_with(&format!("original data remains at {}", kept.display())),
            "{}",
            error.message
        );
        assert_eq!(
            std::fs::read_to_string(kept.join("app/marker")).unwrap(),
            "original"
        );
        assert!(!live.join("app/marker").exists());
    }

    #[test]
    fn a_failed_full_swap_moves_the_originals_back_when_it_can() {
        let temp = TempDir::new().unwrap();
        // A data root inside the extracted tree: moving a directory into
        // its own subtree fails, and the original can move back.
        let extracted = temp.path().join("extracted");
        let live = extracted.join("data");
        for root in [&extracted, &live] {
            std::fs::create_dir_all(root.join("app")).unwrap();
            std::fs::create_dir_all(root.join("tako")).unwrap();
        }
        std::fs::write(live.join("app/marker"), "original").unwrap();
        let error = restore_data_tree(&extracted, &live).unwrap_err();
        assert!(error.originals_intact, "{}", error.message);
        assert!(
            error.message.starts_with("restore data dir"),
            "{}",
            error.message
        );
        assert_eq!(
            std::fs::read_to_string(live.join("app/marker")).unwrap(),
            "original"
        );
        assert!(rollback_dirs(&extracted).is_empty());
    }

    #[test]
    fn restores_refuse_paths_on_another_filesystem() {
        let temp = TempDir::new().unwrap();
        std::fs::create_dir(temp.path().join("same")).unwrap();
        assert!(require_same_filesystem(temp.path(), &[&temp.path().join("same")]).is_ok());
        let error = require_same_filesystem(temp.path(), &[Path::new("/proc")]).unwrap_err();
        assert!(
            error.contains("/proc is on a different filesystem"),
            "{error}"
        );
    }

    /// A full restore would carry a mount inside the data tree away with
    /// the rename; it refuses before changing anything.
    #[test]
    #[cfg(target_os = "linux")]
    fn a_full_restore_refuses_a_data_tree_with_a_mount_inside() {
        // `/proc` and others are mounted below `/`.
        let error = refuse_mounts_in_data_tree(Path::new("/")).unwrap_err();
        assert!(
            error.contains("is a mount point in the app data directory /"),
            "{error}"
        );
        let temp = TempDir::new().unwrap();
        let (live, _) = full_restore_fixture(temp.path());
        refuse_mounts_in_data_tree(&live).unwrap();
    }

    /// Stop a full restore before each step, then run the startup recovery.
    #[test]
    fn recovery_after_a_crash_in_a_full_swap_restores_the_original_tree() {
        for stop_at in 0..3 {
            let temp = TempDir::new().unwrap();
            let (live, extracted) = full_restore_fixture(temp.path());
            assert_eq!(restore_until(&extracted, &live, stop_at).message, "stopped");
            let [dir] = rollback_dirs(temp.path()).try_into().unwrap();
            journal::recover_rollback_dir(&dir).unwrap();
            assert_eq!(
                std::fs::read_to_string(live.join("app/marker")).unwrap(),
                "original",
                "step {stop_at}"
            );
            assert!(rollback_dirs(temp.path()).is_empty(), "step {stop_at}");
        }
        // After both renames: the unvalidated restored tree is replaced too.
        let temp = TempDir::new().unwrap();
        let (live, extracted) = full_restore_fixture(temp.path());
        let swap = restore_data_tree(&extracted, &live).unwrap();
        assert_eq!(
            std::fs::read_to_string(live.join("app/marker")).unwrap(),
            "backup"
        );
        let [dir] = rollback_dirs(temp.path()).try_into().unwrap();
        swap.leave_for_recovery();
        assert_eq!(
            journal::recover_rollback_dir(&dir).unwrap(),
            journal::Recovery::RolledBack
        );
        assert_eq!(
            std::fs::read_to_string(live.join("app/marker")).unwrap(),
            "original"
        );
        assert!(rollback_dirs(temp.path()).is_empty());
    }

    /// A full restore below a server data directory, as startup recovery
    /// finds it.
    fn app_restore_fixture(temp: &Path) -> (PathBuf, PathBuf) {
        let env_dir = temp.join("apps/demo/production");
        std::fs::create_dir_all(&env_dir).unwrap();
        let (live, extracted) = full_restore_fixture(&env_dir);
        (live, extracted)
    }

    fn marker(live: &Path) -> String {
        std::fs::read_to_string(live.join("app/marker")).unwrap()
    }

    /// A commit that cannot write a new file (as when the disk is full)
    /// must not leave a record that a later start rolls back over the
    /// data the app wrote since.
    #[test]
    fn a_commit_without_free_space_never_leads_to_a_rollback_at_a_later_start() {
        let temp = TempDir::new().unwrap();
        let (live, extracted) = app_restore_fixture(temp.path());
        let swap = restore_data_tree(&extracted, &live).unwrap();
        let [dir] = rollback_dirs(&temp.path().join("apps/demo/production"))
            .try_into()
            .unwrap();
        // Writing a new record fails as it would on ENOSPC.
        std::fs::create_dir(dir.join("restore.json.tmp")).unwrap();
        let _ = swap.commit();
        std::fs::write(live.join("app/marker"), "written after the restore").unwrap();
        journal::recover_as_next_start(temp.path());
        assert_eq!(marker(&live), "written after the restore");
    }

    /// A server that gives up on a restore and keeps running (an early
    /// return, a cancelled restore, a panic) leaves a record that a later
    /// start reports and never applies.
    #[test]
    fn a_restore_given_up_by_a_running_server_is_never_rolled_back_later() {
        let temp = TempDir::new().unwrap();
        let (live, extracted) = app_restore_fixture(temp.path());
        let swap = restore_data_tree(&extracted, &live).unwrap();
        drop(swap);
        std::fs::write(live.join("app/marker"), "written after the restore").unwrap();
        journal::recover_as_next_start(temp.path());
        assert_eq!(marker(&live), "written after the restore");

        let temp = TempDir::new().unwrap();
        let (live, extracted) = app_restore_fixture(temp.path());
        let panicked = std::panic::catch_unwind(|| {
            let _swap = restore_data_tree(&extracted, &live).unwrap();
            panic!("restore panicked");
        });
        assert!(panicked.is_err());
        journal::recover_as_next_start(temp.path());
        assert_eq!(marker(&live), "backup");
    }

    /// A new restore never ignores the record an earlier one left.
    #[test]
    fn a_restore_refuses_while_an_earlier_restore_left_its_record() {
        let temp = TempDir::new().unwrap();
        let (live, extracted) = app_restore_fixture(temp.path());
        drop(restore_data_tree(&extracted, &live).unwrap());
        let extracted = temp.path().join("apps/demo/production/extracted-again");
        std::fs::create_dir_all(extracted.join("app")).unwrap();
        std::fs::create_dir_all(extracted.join("tako")).unwrap();
        let error = restore_data_tree(&extracted, &live).unwrap_err();
        assert!(error.originals_intact);
        assert!(
            error
                .message
                .contains("A previous restore of this app did not finish"),
            "{}",
            error.message
        );
        assert_eq!(marker(&live), "backup");
    }

    #[tokio::test]
    #[ignore = "requires Docker Compose and shared host/container project paths"]
    async fn compose_preparation_survives_app_data_restore() {
        use std::collections::HashMap;
        use std::process::Stdio;
        let temp = TempDir::new().unwrap();
        let release = temp.path().join("releases/v1");
        let data_root = temp.path().join("data");
        std::fs::create_dir_all(release.join("public")).unwrap();
        std::fs::create_dir_all(data_root.join("app")).unwrap();
        std::fs::create_dir_all(data_root.join("tako")).unwrap();
        std::fs::write(release.join("public/index.html"), "prepared-content").unwrap();
        std::fs::write(
            release.join("compose.yaml"),
            r#"
services:
  web:
    image: docker.io/library/busybox:1.37
    command: [httpd, -f, -p, '8080', -h, /www]
    volumes: ['./public:/www:ro']
    healthcheck:
      test: [CMD, wget, -Y, 'off', -q, -O, /dev/null, 'http://127.0.0.1:8080/']
      interval: 1s
      timeout: 1s
      retries: 3
    deploy:
      update_config: {monitor: 0s}
"#,
        )
        .unwrap();
        let runtime = tako_compose::ComposeRuntime::new(
            release,
            tako_core::compose::ComposeConfig {
                file: "compose.yaml".into(),
                service: "web".into(),
                port: 8080,
                health_path: "/".into(),
                health_host: None,
            },
            HashMap::from([(
                crate::release::TAKO_APP_DATA_DIR_ENV.into(),
                data_root.join("app").display().to_string(),
            )]),
        )
        .unwrap();
        struct Cleanup(tako_compose::ComposeRuntime);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = self
                    .0
                    .command(false)
                    .into_std()
                    .args(["down", "--volumes", "--remove-orphans"])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
            }
        }
        let _cleanup = Cleanup(runtime.clone());
        runtime.prepare("v1").await.unwrap();
        let mut first = runtime.begin_rollout().await.unwrap();
        first.apply().await.unwrap();
        first.finish().await.unwrap();
        let id = first.ingress().unwrap().id.clone();
        runtime.stop().await.unwrap();

        std::fs::write(data_root.join("app/marker"), "from-backup").unwrap();
        let snapshot = temp.path().join("snapshot");
        snapshot_data_tree(&data_root, Path::new(""), &snapshot).unwrap();
        let archive = temp.path().join("backup.tar.zst");
        create_backup_archive(&snapshot, &archive).unwrap();
        let extracted = temp.path().join("extracted");
        crate::extract_zstd_archive(&archive, &extracted).unwrap();
        std::fs::write(data_root.join("app/marker"), "after-backup").unwrap();
        restore_data_tree(&extracted, &data_root)
            .unwrap()
            .commit()
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(data_root.join("app/marker")).unwrap(),
            "from-backup"
        );

        let mut restored = runtime.begin_rollout().await.unwrap();
        restored.apply().await.unwrap();
        restored.finish().await.unwrap();
        assert_eq!(restored.ingress().unwrap().id, id);
        let output = tokio::process::Command::new("docker")
            .args([
                "exec",
                &id,
                "wget",
                "-Y",
                "off",
                "-qO-",
                "http://127.0.0.1:8080/",
            ])
            .output()
            .await
            .unwrap();
        assert!(output.status.success());
        assert_eq!(
            String::from_utf8(output.stdout).unwrap(),
            "prepared-content"
        );
    }

    #[test]
    fn snapshot_restores_an_app_sqlite_database() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("data");
        let app = source.join("app");
        let dest = temp.path().join("snapshot");
        std::fs::create_dir_all(&app).unwrap();
        let db_path = app.join("app.sqlite");
        {
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            conn.execute("CREATE TABLE items (id INTEGER PRIMARY KEY, name TEXT)", [])
                .unwrap();
            conn.execute("INSERT INTO items (name) VALUES ('one')", [])
                .unwrap();
        }

        snapshot_data_tree(&source, Path::new(""), &dest).unwrap();

        let restored = rusqlite::Connection::open(dest.join("app/app.sqlite")).unwrap();
        let count: i64 = restored
            .query_row("SELECT COUNT(*) FROM items", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn snapshot_uses_pinned_directories_after_a_path_is_replaced() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source");
        let dest = temp.path().join("snapshot");
        let outside = temp.path().join("outside");
        for path in [&source, &dest, &outside] {
            std::fs::create_dir(path).unwrap();
        }
        std::fs::write(source.join("value"), b"inside").unwrap();
        std::fs::write(outside.join("value"), b"outside").unwrap();
        std::os::unix::fs::symlink(outside.join("value"), source.join("link")).unwrap();
        let pinned = File::open(&source).unwrap();
        std::fs::rename(&source, temp.path().join("moved")).unwrap();
        std::os::unix::fs::symlink(&outside, &source).unwrap();
        copy_snapshot_dir(&pinned, Path::new("app"), &dest, SnapshotScope::Full).unwrap();
        assert_eq!(std::fs::read(dest.join("value")).unwrap(), b"inside");
        assert_eq!(
            std::fs::read_link(dest.join("link")).unwrap(),
            outside.join("value")
        );
        assert!(open_regular(&pinned, OsStr::new("link")).is_err());
    }

    #[test]
    fn snapshot_preserves_execute_permissions_without_set_id_bits() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("app");
        let dest = temp.path().join("snapshot");
        std::fs::create_dir(&source).unwrap();
        let program = source.join("program");
        std::fs::write(&program, b"fixture").unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o6755)).unwrap();
        snapshot_data_tree(&source, Path::new(""), &dest).unwrap();
        assert_eq!(
            std::fs::metadata(dest.join("program")).unwrap().mode() & 0o7777,
            0o755
        );
    }

    #[test]
    fn snapshot_rejects_hardlinked_data_and_sqlite_sidecars() {
        let temp = TempDir::new().unwrap();
        let app = temp.path().join("app");
        std::fs::create_dir(&app).unwrap();
        let outside = temp.path().join("service-private");
        // A zero header also looks like an inactive SQLite journal.
        std::fs::write(&outside, b"\0private marker").unwrap();
        let db = app.join("database");
        rusqlite::Connection::open(&db)
            .unwrap()
            .execute("CREATE TABLE items (id INTEGER)", [])
            .unwrap();
        for name in [
            "ordinary",
            "database-shm",
            "database-wal",
            "database-journal",
        ] {
            let planted = app.join(name);
            std::fs::hard_link(&outside, &planted).unwrap();
            let dest = temp.path().join(format!("snapshot-{name}"));
            let error = snapshot_data_tree(&app, Path::new(""), &dest).unwrap_err();
            assert!(error.contains("hard link"), "{name}: {error}");
            assert!(!dest.join(name).exists());
            std::fs::remove_file(planted).unwrap();
        }
    }

    #[test]
    fn sqlite_snapshot_rejects_unsafe_schema_before_publishing() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("app");
        std::fs::create_dir(&source).unwrap();
        let db = rusqlite::Connection::open(source.join("database")).unwrap();
        db.execute_batch(
            "CREATE TABLE payload(value);
             PRAGMA writable_schema=ON;
             UPDATE sqlite_schema SET sql='CREATE TABLE payload(value CHECK(load_extension(value)))'
               WHERE name='payload';",
        )
        .unwrap();
        drop(db);
        let dest = temp.path().join("snapshot");
        assert!(snapshot_data_tree(&source, Path::new(""), &dest).is_err());
        assert!(!dest.join("database").exists());
    }

    #[test]
    fn sqlite_snapshot_rejects_redirected_sidecars() {
        let temp = TempDir::new().unwrap();
        let app = temp.path().join("app");
        std::fs::create_dir(&app).unwrap();
        let database = app.join("db");
        {
            let conn = rusqlite::Connection::open(&database).unwrap();
            conn.execute("CREATE TABLE items (value TEXT)", []).unwrap();
        }
        let outside = temp.path().join("outside");
        std::fs::write(&outside, b"private content").unwrap();
        for suffix in ["-wal", "-shm", "-journal"] {
            let sidecar = app.join(format!("db{suffix}"));
            std::os::unix::fs::symlink(&outside, &sidecar).unwrap();
            let dest = temp.path().join(format!("snapshot{suffix}"));
            assert!(snapshot_data_tree(&app, Path::new(""), &dest).is_err());
            assert!(!dest.join("db").exists());
            assert_eq!(std::fs::read(&outside).unwrap(), b"private content");
            std::fs::remove_file(sidecar).unwrap();
        }
    }

    #[test]
    fn snapshot_excludes_transient_tako_sqlite_stores() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("data");
        let tako = source.join("tako");
        let dest = temp.path().join("snapshot");
        std::fs::create_dir_all(&tako).unwrap();

        let channels_db = tako.join("channels.sqlite");
        {
            let conn = rusqlite::Connection::open(&channels_db).unwrap();
            conn.execute("CREATE TABLE messages (id INTEGER PRIMARY KEY)", [])
                .unwrap();
        }
        std::fs::write(tako.join("channels.sqlite-wal"), b"wal").unwrap();
        std::fs::write(tako.join("channels.sqlite-shm"), b"shm").unwrap();

        let cache_db = tako.join("cache.sqlite");
        {
            let conn = rusqlite::Connection::open(&cache_db).unwrap();
            conn.execute("CREATE TABLE cache_entries (key TEXT PRIMARY KEY)", [])
                .unwrap();
        }
        std::fs::write(tako.join("cache.sqlite-wal"), b"wal").unwrap();
        std::fs::write(tako.join("cache.sqlite-shm"), b"shm").unwrap();

        let workflows_db = tako.join("workflows.sqlite");
        {
            let conn = rusqlite::Connection::open(&workflows_db).unwrap();
            conn.execute("CREATE TABLE runs (id INTEGER PRIMARY KEY)", [])
                .unwrap();
        }

        snapshot_data_tree(&source, Path::new(""), &dest).unwrap();

        assert!(!dest.join("tako/channels.sqlite").exists());
        assert!(!dest.join("tako/channels.sqlite-wal").exists());
        assert!(!dest.join("tako/channels.sqlite-shm").exists());
        assert!(!dest.join("tako/cache.sqlite").exists());
        assert!(!dest.join("tako/cache.sqlite-wal").exists());
        assert!(!dest.join("tako/cache.sqlite-shm").exists());
        assert!(dest.join("tako/workflows.sqlite").exists());
    }

    #[test]
    fn snapshot_sqlite_for_tako_owned_databases() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("data");
        let tako = source.join("tako");
        let dest = temp.path().join("snapshot");
        std::fs::create_dir_all(&tako).unwrap();
        let db_path = tako.join("workflows.sqlite");
        {
            let conn = tako_sqlite::open_local(&db_path).unwrap();
            conn.execute("CREATE TABLE runs (id INTEGER PRIMARY KEY, name TEXT)", [])
                .unwrap();
            conn.execute("INSERT INTO runs (name) VALUES ('r1')", [])
                .unwrap();
        }

        snapshot_data_tree(&source, Path::new(""), &dest).unwrap();

        let restored = dest.join("tako/workflows.sqlite");
        let conn = tako_sqlite::open_local(&restored).unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM runs", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }

    #[cfg(unix)]
    #[test]
    fn backup_archive_preserves_symlinks_without_copying_targets() {
        let temp = TempDir::new().unwrap();
        let snapshot = temp.path().join("snapshot");
        let app = snapshot.join("app");
        let external = temp.path().join("service-only.txt");
        let archive = temp.path().join("data.tar.zst");
        let extracted = temp.path().join("extracted");

        std::fs::create_dir_all(&app).unwrap();
        assert!(create_backup_archive(&snapshot, &archive).is_err());
        std::fs::create_dir(snapshot.join("tako")).unwrap();
        std::fs::write(&external, b"service-only").unwrap();
        std::os::unix::fs::symlink(&external, app.join("leak")).unwrap();

        create_backup_archive(&snapshot, &archive).unwrap();
        crate::extract_zstd_archive(&archive, &extracted).unwrap();

        let metadata = std::fs::symlink_metadata(extracted.join("app/leak")).unwrap();
        assert!(metadata.file_type().is_symlink());
        assert_eq!(
            std::fs::read_link(extracted.join("app/leak")).unwrap(),
            external
        );
    }

    #[test]
    fn restore_excludes_transient_tako_sqlite_stores() {
        let temp = TempDir::new().unwrap();
        let extracted = temp.path().join("extracted");
        let data_root = temp.path().join("apps/demo/production/data");
        std::fs::create_dir_all(extracted.join("app")).unwrap();
        std::fs::create_dir_all(extracted.join("tako")).unwrap();
        std::fs::write(extracted.join("app/app.db"), b"app").unwrap();
        std::fs::write(extracted.join("tako/channels.sqlite"), b"channels").unwrap();
        std::fs::write(extracted.join("tako/channels.sqlite-wal"), b"wal").unwrap();
        std::fs::write(extracted.join("tako/channels.sqlite-shm"), b"shm").unwrap();
        std::fs::write(extracted.join("tako/cache.sqlite"), b"cache").unwrap();
        std::fs::write(extracted.join("tako/cache.sqlite-wal"), b"wal").unwrap();
        std::fs::write(extracted.join("tako/cache.sqlite-shm"), b"shm").unwrap();
        std::fs::write(extracted.join("tako/workflows.sqlite"), b"workflows").unwrap();

        restore_data_tree(&extracted, &data_root)
            .unwrap()
            .commit()
            .unwrap();

        assert!(data_root.join("app/app.db").exists());
        assert!(!data_root.join("tako/channels.sqlite").exists());
        assert!(!data_root.join("tako/channels.sqlite-wal").exists());
        assert!(!data_root.join("tako/channels.sqlite-shm").exists());
        assert!(!data_root.join("tako/cache.sqlite").exists());
        assert!(!data_root.join("tako/cache.sqlite-wal").exists());
        assert!(!data_root.join("tako/cache.sqlite-shm").exists());
        assert!(data_root.join("tako/workflows.sqlite").exists());
    }

    #[test]
    fn restore_keeps_existing_data_when_transient_channel_cleanup_fails() {
        let temp = TempDir::new().unwrap();
        let extracted = temp.path().join("extracted");
        let data_root = temp.path().join("apps/demo/production/data");

        std::fs::create_dir_all(extracted.join("app")).unwrap();
        std::fs::create_dir_all(extracted.join("tako/channels.sqlite")).unwrap();
        std::fs::write(extracted.join("app/restored.db"), b"restored").unwrap();
        std::fs::write(extracted.join("tako/workflows.sqlite"), b"workflows").unwrap();

        std::fs::create_dir_all(data_root.join("app")).unwrap();
        std::fs::create_dir_all(data_root.join("tako")).unwrap();
        std::fs::write(data_root.join("app/current.db"), b"current").unwrap();
        std::fs::write(
            data_root.join("tako/workflows.sqlite"),
            b"current-workflows",
        )
        .unwrap();

        let error = restore_data_tree(&extracted, &data_root)
            .unwrap_err()
            .message;

        assert!(
            error.contains("remove transient Tako sqlite file"),
            "{error}"
        );
        assert_eq!(
            std::fs::read(data_root.join("app/current.db")).unwrap(),
            b"current"
        );
        assert!(!data_root.join("app/restored.db").exists());
        assert!(extracted.exists());
    }
}
