use std::ffi::OsStr;
use std::fs::File;
use std::io::{self, Read};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, FileExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use rustix::fs::{self, AtFlags, FileType, Mode, OFlags};
use sha2::{Digest, Sha256};

mod sqlite;

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

pub(crate) fn snapshot_data_tree(
    anchor: &Path,
    relative: &Path,
    destination: &Path,
) -> Result<(), String> {
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
    let parent = destination
        .parent()
        .ok_or("snapshot destination has no parent")?;
    std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(destination)
        .map_err(|e| format!("create snapshot dir {}: {e}", destination.display()))?;
    copy_snapshot_dir(&File::from(root), Path::new(""), destination)
        .map_err(|e| format!("snapshot {}: {e}", anchor.join(relative).display()))
}

fn copy_snapshot_dir(parent: &File, relative: &Path, destination: &Path) -> io::Result<()> {
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
            FileType::Symlink => {
                let target = fs::readlinkat(parent, name, Vec::new())?;
                std::os::unix::fs::symlink(OsStr::from_bytes(target.to_bytes()), dest)?;
            }
            FileType::Directory => {
                let dir = fs::openat(parent, name, READ_FLAGS | OFlags::DIRECTORY, Mode::empty())?;
                std::fs::create_dir(&dest)?;
                copy_snapshot_dir(&File::from(dir), &path, &dest)?;
            }
            FileType::RegularFile => {
                let mut file = open_regular(parent, name)?;
                if is_sqlite_database(&file)? {
                    sqlite::snapshot(parent, name, file, &dest).map_err(|e| {
                        io::Error::other(format!("snapshot {}: {e}", path.display()))
                    })?;
                } else {
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

fn is_sqlite_companion(parent: &File, name: &OsStr) -> io::Result<bool> {
    let Some(base) = [b"-wal".as_slice(), b"-shm", b"-journal"]
        .into_iter()
        .find_map(|suffix| name.as_bytes().strip_suffix(suffix))
    else {
        return Ok(false);
    };
    let base = OsStr::from_bytes(base);
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
                .append_dir_all(name, &path)
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

#[derive(Debug)]
#[must_use = "commit the validated restore or roll it back after stopping the app"]
pub(super) struct RestoredData {
    data_root: PathBuf,
    previous: PathBuf,
}

impl RestoredData {
    pub(super) fn commit(self) -> Result<(), String> {
        if self.previous.exists() {
            std::fs::remove_dir_all(&self.previous).map_err(|error| {
                format!("remove previous data {}: {error}", self.previous.display())
            })?;
        }
        Ok(())
    }

    /// Call only after stopping all processes that may access the restored tree.
    pub(super) fn rollback(self) -> Result<(), String> {
        let failed = self.previous.with_extension("failed");
        std::fs::rename(&self.data_root, &failed)
            .map_err(|error| format!("preserve failed restore at {}: {error}", failed.display()))?;
        if self.previous.exists()
            && let Err(error) = std::fs::rename(&self.previous, &self.data_root)
        {
            return Err(format!(
                "recover original data from {}: {error}; restored data remains at {}",
                self.previous.display(),
                failed.display()
            ));
        }
        let _ = std::fs::remove_dir_all(failed);
        Ok(())
    }
}

pub(super) fn restore_data_tree(
    extracted_dir: &Path,
    data_root: &Path,
) -> Result<RestoredData, String> {
    if ["app", "tako"].iter().any(|name| {
        !std::fs::symlink_metadata(extracted_dir.join(name)).is_ok_and(|meta| meta.is_dir())
    }) {
        return Err("Backup archive is missing app/ or tako/ data directories.".to_string());
    }
    remove_transient_tako_sqlite_stores(extracted_dir)?;
    let parent = data_root
        .parent()
        .ok_or_else(|| format!("data root has no parent: {}", data_root.display()))?;
    std::fs::create_dir_all(parent)
        .map_err(|e| format!("create data parent {}: {e}", parent.display()))?;
    let previous = parent.join(format!(".data-restore-prev-{}", nanoid::nanoid!(8)));
    if data_root.exists() {
        std::fs::rename(data_root, &previous).map_err(|e| {
            format!(
                "move existing data dir {} to {}: {e}",
                data_root.display(),
                previous.display()
            )
        })?;
    }
    match std::fs::rename(extracted_dir, data_root) {
        Ok(()) => Ok(RestoredData {
            data_root: data_root.to_path_buf(),
            previous,
        }),
        Err(error) => {
            if previous.exists() {
                std::fs::rename(&previous, data_root).map_err(|rollback| format!(
                    "restore failed: {error}; original data remains at {} because rollback failed: {rollback}", previous.display()))?;
            }
            Err(format!("restore data dir {}: {error}", data_root.display()))
        }
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
            std::fs::read_to_string(previous.join("app/marker")).unwrap(),
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
      test: [CMD, wget, -q, -O, /dev/null, 'http://127.0.0.1:8080/']
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
            .args(["exec", &id, "wget", "-qO-", "http://127.0.0.1:8080/"])
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
        copy_snapshot_dir(&pinned, Path::new("app"), &dest).unwrap();
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

        let error = restore_data_tree(&extracted, &data_root).unwrap_err();

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
