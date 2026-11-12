//! Hourly database snapshots: only the SQLite databases of the app data tree,
//! captured like the full archive, between daily full archives.

use std::ffi::OsStr;
use std::path::Path;

use tako_core::{BackupInfo, BackupKeyBinding, BackupKind, BackupSkip, BackupSkipReason};

use crate::release::{app_runtime_data_paths, requested_deployment_identity};

use super::encryption::{active_backup_key, database_digest_key, encrypt_backup_file};
use super::index::{BackupIndex, backup_object_prefix, latest_backup};
use super::s3::{TRANSFER_TIMEOUT, backup_http_client, put_json_object, queue_retention};
use super::snapshot::{create_database_snapshot_archive, database_digest, sha256_file_hex};
use super::{create_private_work_dir, now_unix_secs, validate_backup_binding};

pub(super) const DATABASE_SNAPSHOT_INTERVAL_SECS: i64 = 60 * 60;
pub(super) const DATABASE_SNAPSHOT_RETENTION_SECS: i64 = 48 * 60 * 60;
/// Each hourly snapshot copies, compacts, compresses, and encrypts every
/// database, and 48 hours of them are retained. Above this size that costs
/// more disk I/O, transfer, and storage than an hourly job should, and the
/// daily full archive remains the recovery point.
pub(super) const MAX_DATABASE_SNAPSHOT_BYTES: u64 = 512 * 1024 * 1024;
/// At peak a snapshot holds the compacted databases, the compressed archive,
/// and its encrypted copy; each is at most the measured database size.
const DISK_COPIES: u64 = 3;
/// Free space a database snapshot or its restore must leave for the apps.
pub(super) const DISK_RESERVE_BYTES: u64 = 512 * 1024 * 1024;

#[derive(Debug, Clone, Default)]
pub(crate) struct DatabaseSnapshotState {
    /// When the scheduler last evaluated a database snapshot to the end.
    pub(crate) checked_at_unix_secs: Option<i64>,
    /// Cleared when a database snapshot is uploaded.
    pub(crate) skip: Option<BackupSkip>,
    /// Set from just before a database snapshot takes the backup lock until
    /// just after it releases it.
    pub(crate) running: bool,
}

/// The backup lock, held by a database snapshot. Marks the snapshot running
/// before taking the lock and clears the mark after releasing it, so while a
/// snapshot holds the lock it is always marked.
pub(super) struct DatabaseSnapshotLock<'a> {
    state: &'a crate::ServerState,
    app: &'a str,
    guard: Option<tokio::sync::OwnedMutexGuard<()>>,
}

impl Drop for DatabaseSnapshotLock<'_> {
    fn drop(&mut self) {
        drop(self.guard.take());
        self.state.set_database_snapshot_running(self.app, false);
    }
}

#[derive(Debug)]
pub(super) enum DatabaseSnapshotOutcome {
    Created(Box<BackupInfo>),
    Skipped(BackupSkip),
}

#[derive(Debug)]
pub(super) enum Produced {
    Archive {
        database_digest: String,
        /// Relative paths of the databases in the archive.
        databases: Vec<String>,
        sha256_hex: String,
        encryption: tako_core::BackupEncryptionInfo,
    },
    Skipped(BackupSkipReason, String),
}

pub(super) fn preflight(
    total_bytes: u64,
    available_bytes: u64,
) -> Result<(), (BackupSkipReason, String)> {
    if total_bytes == 0 {
        return Err((
            BackupSkipReason::NoDatabases,
            "No SQLite databases found in the app data directory.".into(),
        ));
    }
    if total_bytes > MAX_DATABASE_SNAPSHOT_BYTES {
        return Err((
            BackupSkipReason::TooLarge,
            format!(
                "SQLite databases use {total_bytes} bytes, above the {MAX_DATABASE_SNAPSHOT_BYTES}-byte hourly snapshot limit; daily full backups continue."
            ),
        ));
    }
    let required = total_bytes
        .saturating_mul(DISK_COPIES)
        .saturating_add(DISK_RESERVE_BYTES);
    if available_bytes < required {
        return Err((
            BackupSkipReason::InsufficientDisk,
            format!("Database snapshot needs {required} free bytes; {available_bytes} available."),
        ));
    }
    Ok(())
}

/// Never reports "unchanged" unless the new databases are byte-identical to
/// the ones the latest backup recorded under the same key.
pub(super) fn is_unchanged(previous: Option<&BackupInfo>, key_id: &str, digest: &str) -> bool {
    previous.is_some_and(|previous| {
        previous.encryption.key_id == key_id && previous.database_digest.as_deref() == Some(digest)
    })
}

/// Capture into `snapshot_dir`, then archive and encrypt unless nothing changed
/// since `previous`, the latest backup of either kind.
pub(super) fn produce_database_snapshot(
    work_dir: &Path,
    key: &BackupKeyBinding,
    previous: Option<&BackupInfo>,
    capture: impl FnOnce(&Path) -> Result<(), String>,
) -> Result<Produced, String> {
    let snapshot_dir = work_dir.join("snapshot");
    capture(&snapshot_dir)?;
    let digest = database_digest(&snapshot_dir, &database_digest_key(key)?)?;
    if digest.databases.is_empty() {
        return Ok(Produced::Skipped(
            BackupSkipReason::NoDatabases,
            "No SQLite databases found in the app data directory.".into(),
        ));
    }
    if is_unchanged(previous, &key.id, &digest.hex) {
        return Ok(Produced::Skipped(
            BackupSkipReason::Unchanged,
            "No database changed since the latest backup.".into(),
        ));
    }
    let archive_path = work_dir.join("databases.tar.zst");
    let encrypted_path = encrypted_archive_path(work_dir);
    create_database_snapshot_archive(&snapshot_dir, &archive_path)?;
    let encryption = encrypt_backup_file(&archive_path, &encrypted_path, key)?;
    Ok(Produced::Archive {
        database_digest: digest.hex,
        databases: digest
            .databases
            .iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect(),
        sha256_hex: sha256_file_hex(&encrypted_path)?,
        encryption,
    })
}

fn encrypted_archive_path(work_dir: &Path) -> std::path::PathBuf {
    work_dir.join("databases.tar.zst.enc")
}

/// SQLite POSIX locks belong to the process. Closing a descriptor to a live
/// workflow DB in the server could release the server's own locks.
fn run_worker(flag: &str, args: &[&OsStr]) -> Result<Vec<u8>, String> {
    let worker = std::env::current_exe().map_err(|e| e.to_string())?;
    let output = std::process::Command::new(worker)
        .arg(flag)
        .args(args)
        .output()
        .map_err(|e| format!("start snapshot worker: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "snapshot worker failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(output.stdout)
}

impl crate::ServerState {
    /// Create one database snapshot unless it would be unsafe or redundant.
    /// Never waits: a running backup, restore, or deploy is reported as a skip.
    pub(super) async fn database_snapshot_now(
        &self,
        app: &str,
    ) -> Result<DatabaseSnapshotOutcome, String> {
        let now = now_unix_secs();
        let skip = |reason, message: &str| {
            Ok(DatabaseSnapshotOutcome::Skipped(BackupSkip {
                at_unix_secs: now,
                reason,
                message: message.to_string(),
            }))
        };
        let Some(_backup) = self.lock_for_database_snapshot(app) else {
            return skip(
                BackupSkipReason::BackupInProgress,
                "Another backup or restore is in progress.",
            );
        };
        if self.is_draining() {
            return Err("Server is draining".into());
        }
        let backup = self
            .state_store
            .get_backup(app)
            .map_err(|error| format!("read backup config: {error}"))?
            .ok_or_else(|| format!("Backups are not configured for {app}."))?;
        validate_backup_binding(&backup)?;
        // Read remote state before taking the deploy lock. Index writes need
        // the backup lock, which is held, so this view stays current.
        let index = self.read_backup_index(&backup, app).await?;
        let previous = latest_backup(index.backups.iter()).cloned();

        let lock = self.get_deploy_lock(app).await;
        let Ok(guard) = lock.try_lock() else {
            return skip(
                BackupSkipReason::DeployInProgress,
                "A deploy is in progress.",
            );
        };

        let tmp_root = self.runtime.data_dir.join("tmp").join("backups");
        tokio::fs::create_dir_all(&tmp_root)
            .await
            .map_err(|e| format!("create backup temp dir {}: {e}", tmp_root.display()))?;
        let backup_id = format!("d{now}-{}", nanoid::nanoid!(8));
        let work_dir = tmp_root.join(&backup_id);
        let backup_key = active_backup_key(&backup)?.clone();
        create_private_work_dir(&work_dir).await?;

        let result: Result<DatabaseSnapshotOutcome, String> = async {
            let data_dir = self.runtime.data_dir.clone();
            let data_root = app_runtime_data_paths(Path::new(""), app).root;
            let measured = tokio::task::spawn_blocking({
                let (data_dir, data_root) = (data_dir.clone(), data_root.clone());
                move || {
                    let stdout = run_worker(
                        "--measure-databases",
                        &[data_dir.as_os_str(), data_root.as_os_str()],
                    )?;
                    String::from_utf8_lossy(&stdout)
                        .trim()
                        .parse::<u64>()
                        .map_err(|e| format!("parse database size: {e}"))
                }
            })
            .await
            .map_err(|e| format!("database size worker failed: {e}"))??;
            let available = super::super::release_artifacts::available_bytes(&tmp_root)?;
            let produced = match preflight(measured, available) {
                Err((reason, message)) => Produced::Skipped(reason, message),
                Ok(()) => tokio::task::spawn_blocking({
                    let work_dir = work_dir.clone();
                    let backup_key = backup_key.clone();
                    move || {
                        produce_database_snapshot(
                            &work_dir,
                            &backup_key,
                            previous.as_ref(),
                            |snapshot_dir| {
                                run_worker(
                                    "--snapshot-databases",
                                    &[
                                        data_dir.as_os_str(),
                                        data_root.as_os_str(),
                                        snapshot_dir.as_os_str(),
                                    ],
                                )
                                .map(drop)
                            },
                        )
                    }
                })
                .await
                .map_err(|e| format!("database snapshot worker failed: {e}"))??,
            };
            // The snapshot is independent of the live app now. Remote I/O must
            // not keep stop/delete/deploy waiting on this lifecycle lock.
            drop(guard);

            let (database_digest, databases, sha256_hex, encryption) = match produced {
                Produced::Skipped(reason, message) => {
                    self.expire_backups(&backup, app, index).await;
                    return skip(reason, &message);
                }
                Produced::Archive {
                    database_digest,
                    databases,
                    sha256_hex,
                    encryption,
                } => (database_digest, databases, sha256_hex, encryption),
            };
            let encrypted_path = encrypted_archive_path(&work_dir);
            let size_bytes = tokio::fs::metadata(&encrypted_path)
                .await
                .map_err(|e| {
                    format!(
                        "read database snapshot metadata {}: {e}",
                        encrypted_path.display()
                    )
                })?
                .len();
            let (app_name, environment) = requested_deployment_identity(app);
            let server = self.backup_server_label();
            let prefix = backup_object_prefix(&app_name, &environment, &server);
            let info = BackupInfo {
                archive_key: format!("{prefix}/{backup_id}.databases.tar.zst.enc"),
                manifest_key: format!("{prefix}/{backup_id}.json"),
                id: backup_id,
                app: app_name,
                environment,
                server,
                created_at_unix_secs: now_unix_secs(),
                size_bytes,
                sha256_hex,
                encryption,
                kind: BackupKind::Databases,
                database_digest: Some(database_digest),
                databases,
            };
            self.publish_backup(&backup, app, &info, &encrypted_path)
                .await?;
            Ok(DatabaseSnapshotOutcome::Created(Box::new(info)))
        }
        .await;

        let _ = tokio::fs::remove_dir_all(&work_dir).await;
        result
    }

    pub(super) fn lock_for_database_snapshot<'a>(
        &'a self,
        app: &'a str,
    ) -> Option<DatabaseSnapshotLock<'a>> {
        self.set_database_snapshot_running(app, true);
        let mut lock = DatabaseSnapshotLock {
            state: self,
            app,
            guard: None,
        };
        lock.guard = Some(self.backup_operation_lock(app).try_lock_owned().ok()?);
        Some(lock)
    }

    fn set_database_snapshot_running(&self, app: &str, running: bool) {
        self.database_snapshots
            .write()
            .entry(app.to_string())
            .or_default()
            .running = running;
    }

    pub(super) fn database_snapshot_running(&self, app: &str) -> bool {
        self.database_snapshots
            .read()
            .get(app)
            .is_some_and(|state| state.running)
    }

    /// Retention otherwise runs only after an upload. Unchanged databases can
    /// skip uploads for days, so expire old snapshots on skips too.
    async fn expire_backups(
        &self,
        backup: &tako_core::BackupBinding,
        app: &str,
        mut index: BackupIndex,
    ) {
        let queued = index.pending_deletions.len();
        queue_retention(&mut index, backup.retention_days);
        if index.pending_deletions.len() == queued {
            return;
        }
        let result = async {
            let client = backup_http_client(TRANSFER_TIMEOUT)?;
            put_json_object(
                &client,
                &backup.storage,
                &super::backup_index_key(app, self),
                &index,
            )
            .await?;
            self.prune_queued_retention(&client, backup, app, &mut index)
                .await;
            Ok::<(), String>(())
        }
        .await;
        if let Err(error) = result {
            tracing::warn!(app, "Backup retention cleanup failed: {error}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> BackupKeyBinding {
        BackupKeyBinding {
            id: "backup-key-0123456789abcdef".into(),
            key_base64: "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=".into(),
        }
    }

    /// Returns the open app connection, which keeps its WAL in place.
    fn data_tree(root: &Path) -> rusqlite::Connection {
        std::fs::create_dir_all(root.join("app/nested/empty")).unwrap();
        std::fs::create_dir_all(root.join("tako")).unwrap();
        let db = rusqlite::Connection::open(root.join("app/nested/app.sqlite")).unwrap();
        db.execute_batch(
            "PRAGMA journal_mode=WAL; CREATE TABLE items(id INTEGER); INSERT INTO items VALUES (1);",
        )
        .unwrap();
        rusqlite::Connection::open(root.join("tako/workflows.sqlite"))
            .unwrap()
            .execute_batch("CREATE TABLE runs(id INTEGER)")
            .unwrap();
        rusqlite::Connection::open(root.join("tako/channels.sqlite"))
            .unwrap()
            .execute_batch("CREATE TABLE messages(id INTEGER)")
            .unwrap();
        std::fs::write(root.join("app/upload.bin"), b"not a database").unwrap();
        std::os::unix::fs::symlink("/etc/hostname", root.join("app/link")).unwrap();
        db
    }

    /// Stand-in for the published index entry after an upload.
    fn entry(produced: &Produced, kind: BackupKind) -> BackupInfo {
        let Produced::Archive {
            database_digest,
            databases,
            encryption,
            ..
        } = produced
        else {
            panic!("expected an archive, got {produced:?}");
        };
        BackupInfo {
            id: "d1".into(),
            app: "demo".into(),
            environment: "production".into(),
            server: "local".into(),
            created_at_unix_secs: 1,
            size_bytes: 1,
            sha256_hex: String::new(),
            archive_key: String::new(),
            manifest_key: String::new(),
            encryption: encryption.clone(),
            kind,
            database_digest: Some(database_digest.clone()),
            databases: databases.clone(),
        }
    }

    fn produce(data: &Path, previous: Option<&BackupInfo>) -> (tempfile::TempDir, Produced) {
        let work = tempfile::tempdir().unwrap();
        let produced = produce_database_snapshot(work.path(), &key(), previous, |dest| {
            super::super::snapshot_database_tree(data, Path::new(""), dest)
        })
        .unwrap();
        (work, produced)
    }

    #[test]
    fn database_snapshot_contains_only_sqlite_databases() {
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("data");
        let _db = data_tree(&data);
        let (work, produced) = produce(&data, None);
        assert!(matches!(produced, Produced::Archive { .. }));

        let plain = work.path().join("decrypted.tar.zst");
        let Produced::Archive {
            encryption,
            databases,
            ..
        } = &produced
        else {
            unreachable!()
        };
        assert_eq!(
            databases,
            &["app/nested/app.sqlite", "tako/workflows.sqlite"]
        );
        super::super::decrypt_backup_file(
            &encrypted_archive_path(work.path()),
            &plain,
            &key(),
            encryption,
        )
        .unwrap();
        let extracted = work.path().join("extracted");
        crate::extract_zstd_archive(&plain, &extracted).unwrap();
        let mut files = Vec::new();
        let mut dirs = vec![extracted.clone()];
        while let Some(dir) = dirs.pop() {
            for entry in std::fs::read_dir(dir).unwrap() {
                let entry = entry.unwrap();
                let kind = entry.file_type().unwrap();
                if kind.is_dir() {
                    dirs.push(entry.path());
                } else {
                    let path = entry.path();
                    files.push(path.strip_prefix(&extracted).unwrap().to_path_buf());
                }
            }
        }
        files.sort();
        assert_eq!(
            files,
            [
                Path::new("databases/app/nested/app.sqlite"),
                Path::new("databases/tako/workflows.sqlite"),
            ]
        );
        assert!(!extracted.join("databases/app/nested/empty").exists());
        assert!(
            !extracted.join("app").exists(),
            "full restore must refuse it"
        );
        let db =
            rusqlite::Connection::open(extracted.join("databases/app/nested/app.sqlite")).unwrap();
        let count: i64 = db
            .query_row("SELECT count(*) FROM items", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn unchanged_databases_produce_no_new_archive_and_changes_do() {
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("data");
        let db = data_tree(&data);
        let (_work, first) = produce(&data, None);
        let previous = entry(&first, BackupKind::Databases);

        std::fs::write(data.join("app/upload.bin"), b"other files do not matter").unwrap();
        let (work, second) = produce(&data, Some(&previous));
        assert!(
            matches!(second, Produced::Skipped(BackupSkipReason::Unchanged, _)),
            "{second:?}"
        );
        assert!(!encrypted_archive_path(work.path()).exists());

        db.execute("INSERT INTO items VALUES (2)", []).unwrap();
        let (work, third) = produce(&data, Some(&previous));
        assert!(matches!(third, Produced::Archive { .. }), "{third:?}");
        assert!(encrypted_archive_path(work.path()).exists());

        // A different key never matches a digest recorded under another one.
        let mut rotated = entry(&third, BackupKind::Databases);
        rotated.encryption.key_id = "backup-key-rotated".into();
        let (_work, fourth) = produce(&data, Some(&rotated));
        assert!(matches!(fourth, Produced::Archive { .. }));
    }

    #[test]
    fn full_archive_digest_lets_the_next_database_snapshot_skip() {
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("data");
        let _db = data_tree(&data);
        let full = temp.path().join("full");
        super::super::snapshot_data_tree(&data, Path::new(""), &full).unwrap();
        let digest = database_digest(&full, &database_digest_key(&key()).unwrap()).unwrap();
        assert_eq!(
            digest.databases,
            [
                Path::new("app/nested/app.sqlite"),
                Path::new("tako/workflows.sqlite")
            ]
        );

        let (_work, produced) = produce(&data, None);
        let mut previous = entry(&produced, BackupKind::Full);
        previous.database_digest = Some(digest.hex);
        let (_work, produced) = produce(&data, Some(&previous));
        assert!(matches!(
            produced,
            Produced::Skipped(BackupSkipReason::Unchanged, _)
        ));

        // Entries from before digests existed always count as changed.
        previous.database_digest = None;
        let (_work, produced) = produce(&data, Some(&previous));
        assert!(matches!(produced, Produced::Archive { .. }));
    }

    #[test]
    fn data_without_databases_is_skipped() {
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("data");
        std::fs::create_dir_all(data.join("app")).unwrap();
        std::fs::create_dir_all(data.join("tako")).unwrap();
        std::fs::write(data.join("app/file"), b"plain").unwrap();
        assert_eq!(
            super::super::measure_database_tree(&data, Path::new("")).unwrap(),
            0
        );
        let (_work, produced) = produce(&data, None);
        assert!(matches!(
            produced,
            Produced::Skipped(BackupSkipReason::NoDatabases, _)
        ));
    }

    #[test]
    fn measure_counts_databases_and_their_wal_only() {
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("data");
        let _db = data_tree(&data);
        let size = |path: &str| std::fs::metadata(data.join(path)).unwrap().len();
        assert!(size("app/nested/app.sqlite-wal") > 0);
        assert_eq!(
            super::super::measure_database_tree(&data, Path::new("")).unwrap(),
            size("app/nested/app.sqlite")
                + size("app/nested/app.sqlite-wal")
                + size("tako/workflows.sqlite")
        );
    }

    #[test]
    fn preflight_skips_large_databases_and_low_disk() {
        let mib = 1024 * 1024;
        assert!(preflight(10 * mib, 10 * 1024 * mib).is_ok());
        assert_eq!(
            preflight(0, u64::MAX).unwrap_err().0,
            BackupSkipReason::NoDatabases
        );
        assert_eq!(
            preflight(MAX_DATABASE_SNAPSHOT_BYTES + 1, u64::MAX)
                .unwrap_err()
                .0,
            BackupSkipReason::TooLarge
        );
        assert!(preflight(MAX_DATABASE_SNAPSHOT_BYTES, u64::MAX).is_ok());
        let needed = 100 * mib * DISK_COPIES + DISK_RESERVE_BYTES;
        assert!(preflight(100 * mib, needed).is_ok());
        assert_eq!(
            preflight(100 * mib, needed - 1).unwrap_err().0,
            BackupSkipReason::InsufficientDisk
        );
    }
}
