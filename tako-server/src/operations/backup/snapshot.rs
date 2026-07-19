use std::io::Read;
use std::path::Path;
use std::time::Duration;

use sha2::{Digest, Sha256};

const TAKO_DATA_DIR: &str = "tako";
const CHANNELS_DB_FILENAME: &str = "channels.sqlite";
const CHANNELS_DB_COMPANION_FILENAMES: [&str; 3] = [
    "channels.sqlite-wal",
    "channels.sqlite-shm",
    "channels.sqlite-tshm",
];
const CACHE_DB_FILENAME: &str = "cache.sqlite";
const CACHE_DB_COMPANION_FILENAMES: [&str; 3] =
    ["cache.sqlite-wal", "cache.sqlite-shm", "cache.sqlite-tshm"];
const WORKFLOWS_DB_COMPANION_FILENAMES: [&str; 3] = [
    "workflows.sqlite-wal",
    "workflows.sqlite-shm",
    "workflows.sqlite-tshm",
];

pub(super) fn snapshot_data_tree(source: &Path, destination: &Path) -> Result<(), String> {
    if destination.exists() {
        std::fs::remove_dir_all(destination)
            .map_err(|e| format!("remove stale snapshot {}: {e}", destination.display()))?;
    }
    std::fs::create_dir_all(destination)
        .map_err(|e| format!("create snapshot dir {}: {e}", destination.display()))?;
    copy_snapshot_dir(source, source, destination)
}

fn copy_snapshot_dir(root: &Path, source: &Path, destination: &Path) -> Result<(), String> {
    for entry in
        std::fs::read_dir(source).map_err(|e| format!("read data dir {}: {e}", source.display()))?
    {
        let entry = entry.map_err(|e| format!("read data entry: {e}"))?;
        let source_path = entry.path();
        let relative_path = source_path.strip_prefix(root).unwrap_or(&source_path);
        if is_transient_tako_sqlite_file(relative_path) {
            continue;
        }
        let dest_path = destination.join(entry.file_name());
        let metadata = std::fs::symlink_metadata(&source_path)
            .map_err(|e| format!("read metadata {}: {e}", source_path.display()))?;
        let file_type = metadata.file_type();

        if file_type.is_symlink() {
            copy_symlink(&source_path, &dest_path)?;
        } else if file_type.is_dir() {
            std::fs::create_dir_all(&dest_path)
                .map_err(|e| format!("create snapshot dir {}: {e}", dest_path.display()))?;
            copy_snapshot_dir(root, &source_path, &dest_path)?;
        } else if file_type.is_file() {
            if is_sqlite_companion_file(&source_path) {
                continue;
            }
            if is_sqlite_database(&source_path) {
                // Tako's own stores are written by turso, so their snapshot
                // must go through turso too — a foreign-engine reader is not
                // synchronized with turso's live writers. App-owned databases
                // are written with regular SQLite and keep the rusqlite path.
                if relative_path.starts_with(TAKO_DATA_DIR) {
                    snapshot_turso_database(&source_path, &dest_path)?;
                } else {
                    snapshot_sqlite_database(&source_path, &dest_path)?;
                }
            } else {
                std::fs::copy(&source_path, &dest_path).map_err(|e| {
                    format!(
                        "copy data file {} to {}: {e}",
                        source_path.display(),
                        dest_path.display()
                    )
                })?;
            }
        }
    }
    Ok(())
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

#[cfg(unix)]
fn copy_symlink(source: &Path, destination: &Path) -> Result<(), String> {
    let target = std::fs::read_link(source)
        .map_err(|e| format!("read symlink {}: {e}", source.display()))?;
    std::os::unix::fs::symlink(&target, destination)
        .map_err(|e| format!("copy symlink {}: {e}", source.display()))
}

#[cfg(not(unix))]
fn copy_symlink(source: &Path, _destination: &Path) -> Result<(), String> {
    Err(format!(
        "cannot back up symlink on this platform: {}",
        source.display()
    ))
}

fn is_sqlite_companion_file(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    let Some(base) = name
        .strip_suffix("-wal")
        .or_else(|| name.strip_suffix("-shm"))
        .or_else(|| name.strip_suffix("-tshm"))
    else {
        return false;
    };
    is_sqlite_database(&path.with_file_name(base))
}

fn is_sqlite_database(path: &Path) -> bool {
    let Ok(mut file) = std::fs::File::open(path) else {
        return false;
    };
    let mut header = [0_u8; 16];
    file.read_exact(&mut header).is_ok() && &header == b"SQLite format 3\0"
}

fn snapshot_sqlite_database(source: &Path, destination: &Path) -> Result<(), String> {
    if let Some(parent) = destination.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("create sqlite backup dir {}: {e}", parent.display()))?;
    }
    let conn =
        rusqlite::Connection::open_with_flags(source, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(|e| format!("open sqlite database {}: {e}", source.display()))?;
    conn.busy_timeout(Duration::from_secs(10))
        .map_err(|e| format!("set sqlite busy timeout {}: {e}", source.display()))?;
    let escaped = destination.to_string_lossy().replace('\'', "''");
    conn.execute_batch(&format!("VACUUM main INTO '{escaped}';"))
        .map_err(|e| format!("snapshot sqlite database {}: {e}", source.display()))
}

/// Snapshot a turso-written database with turso itself so the read
/// coordinates with live writers through turso's multiprocess WAL.
fn snapshot_turso_database(source: &Path, destination: &Path) -> Result<(), String> {
    if let Some(parent) = destination.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("create sqlite backup dir {}: {e}", parent.display()))?;
    }
    let source_str = source
        .to_str()
        .ok_or_else(|| format!("non-UTF-8 sqlite path {}", source.display()))?;
    let escaped = destination.to_string_lossy().replace('\'', "''");
    tako_sqlite::block_on(async {
        let conn = tako_sqlite::open_local(source_str)
            .await
            .map_err(|e| format!("open turso database {}: {e}", source.display()))?;
        conn.execute_batch(&format!("VACUUM main INTO '{escaped}';"))
            .await
            .map_err(|e| format!("snapshot turso database {}: {e}", source.display()))
    })
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
        if path.exists() {
            archive
                .append_dir_all(name, &path)
                .map_err(|e| format!("append {name} data to backup archive: {e}"))?;
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

pub(super) fn restore_data_tree(extracted_dir: &Path, data_root: &Path) -> Result<(), String> {
    if !extracted_dir.join("app").is_dir() || !extracted_dir.join("tako").is_dir() {
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
        Ok(()) => {
            let _ = std::fs::remove_dir_all(previous);
            Ok(())
        }
        Err(error) => {
            if previous.exists() {
                let _ = std::fs::rename(&previous, data_root);
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
    fn snapshot_uses_sqlite_online_backup_for_sqlite_files() {
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

        snapshot_data_tree(&source, &dest).unwrap();

        let restored = rusqlite::Connection::open(dest.join("app/app.sqlite")).unwrap();
        let count: i64 = restored
            .query_row("SELECT COUNT(*) FROM items", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);
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
        std::fs::write(tako.join("channels.sqlite-tshm"), b"tshm").unwrap();

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
        std::fs::write(tako.join("workflows.sqlite-tshm"), b"tshm").unwrap();

        snapshot_data_tree(&source, &dest).unwrap();

        assert!(!dest.join("tako/channels.sqlite").exists());
        assert!(!dest.join("tako/channels.sqlite-wal").exists());
        assert!(!dest.join("tako/channels.sqlite-shm").exists());
        assert!(!dest.join("tako/channels.sqlite-tshm").exists());
        assert!(!dest.join("tako/cache.sqlite").exists());
        assert!(!dest.join("tako/cache.sqlite-wal").exists());
        assert!(!dest.join("tako/cache.sqlite-shm").exists());
        assert!(dest.join("tako/workflows.sqlite").exists());
        assert!(!dest.join("tako/workflows.sqlite-tshm").exists());
    }

    #[test]
    fn snapshot_uses_turso_for_tako_owned_databases() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("data");
        let tako = source.join("tako");
        let dest = temp.path().join("snapshot");
        std::fs::create_dir_all(&tako).unwrap();
        let db_path = tako.join("workflows.sqlite");
        tako_sqlite::block_on(async {
            let conn = tako_sqlite::open_local(db_path.to_str().unwrap())
                .await
                .unwrap();
            conn.execute("CREATE TABLE runs (id INTEGER PRIMARY KEY, name TEXT)", ())
                .await
                .unwrap();
            conn.execute("INSERT INTO runs (name) VALUES ('r1')", ())
                .await
                .unwrap();
        });

        snapshot_data_tree(&source, &dest).unwrap();

        let restored = dest.join("tako/workflows.sqlite");
        let count: i64 = tako_sqlite::block_on(async {
            let conn = tako_sqlite::open_local(restored.to_str().unwrap())
                .await
                .unwrap();
            let mut rows = conn.query("SELECT COUNT(*) FROM runs", ()).await.unwrap();
            rows.next().await.unwrap().unwrap().get(0).unwrap()
        });
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
    fn restore_removes_transient_channel_replay_store_from_old_archives() {
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
        std::fs::write(extracted.join("tako/workflows.sqlite-tshm"), b"tshm").unwrap();

        restore_data_tree(&extracted, &data_root).unwrap();

        assert!(data_root.join("app/app.db").exists());
        assert!(!data_root.join("tako/channels.sqlite").exists());
        assert!(!data_root.join("tako/channels.sqlite-wal").exists());
        assert!(!data_root.join("tako/channels.sqlite-shm").exists());
        assert!(!data_root.join("tako/cache.sqlite").exists());
        assert!(!data_root.join("tako/cache.sqlite-wal").exists());
        assert!(!data_root.join("tako/cache.sqlite-shm").exists());
        assert!(data_root.join("tako/workflows.sqlite").exists());
        assert!(!data_root.join("tako/workflows.sqlite-tshm").exists());
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
