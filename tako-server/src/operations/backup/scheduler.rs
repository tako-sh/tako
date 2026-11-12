use std::sync::Arc;
use std::time::Duration;

use tako_core::{BACKUP_INTERVAL_SECS, BackupInfo, BackupKind, BackupSkipReason};

use super::databases::{DATABASE_SNAPSHOT_INTERVAL_SECS, DatabaseSnapshotOutcome};
use super::{latest_backup, now_unix_secs};

/// Short enough that hourly database snapshots stay close to an hour apart.
/// Due checks use the remote index, so a restart neither bursts nor resets.
const SCHEDULER_TICK: Duration = Duration::from_secs(10 * 60);
/// A post-deploy backup waits this long for an hourly database snapshot that
/// holds the backup lock, typically during its upload. It stays well inside
/// the CLI's deadline for finalizing a deploy.
const POST_DEPLOY_DATABASE_SNAPSHOT_WAIT: Duration = Duration::from_secs(5 * 60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DueBackup {
    Full,
    Databases,
}

/// A full archive is due a day after the latest one. Otherwise a database
/// snapshot is due an hour after the latest backup of either kind or the
/// latest completed check that uploaded nothing.
pub(super) fn due_backup(
    backups: &[BackupInfo],
    last_database_check: Option<i64>,
    now: i64,
) -> Option<DueBackup> {
    let elapsed_since = |time: i64| now.saturating_sub(time);
    let latest_full = latest_backup(
        backups
            .iter()
            .filter(|backup| backup.kind == BackupKind::Full),
    );
    if latest_full
        .is_none_or(|backup| elapsed_since(backup.created_at_unix_secs) >= BACKUP_INTERVAL_SECS)
    {
        return Some(DueBackup::Full);
    }
    let latest = latest_backup(backups.iter())
        .map(|backup| backup.created_at_unix_secs)
        .max(last_database_check);
    latest
        .is_none_or(|time| elapsed_since(time) >= DATABASE_SNAPSHOT_INTERVAL_SECS)
        .then_some(DueBackup::Databases)
}

impl crate::ServerState {
    pub(crate) async fn backup_after_deploy(
        &self,
        app: &str,
    ) -> Option<Result<BackupInfo, String>> {
        match self.state_store.get_backup(app) {
            Ok(Some(_)) => Some(
                self.backup_app_now(app, Some(POST_DEPLOY_DATABASE_SNAPSHOT_WAIT))
                    .await,
            ),
            Ok(None) => None,
            Err(error) => Some(Err(format!("read backup config: {error}"))),
        }
    }

    pub(crate) fn start_backup_scheduler(self: Arc<Self>, handle: &tokio::runtime::Handle) {
        handle.spawn(async move {
            let mut interval = tokio::time::interval(SCHEDULER_TICK);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                interval.tick().await;
                self.run_due_backups().await;
            }
        });
    }

    async fn run_due_backups(&self) {
        let apps = match self.state_store.load_apps() {
            Ok(apps) => apps,
            Err(error) => {
                tracing::warn!("Failed to load apps for backup scheduler: {error}");
                return;
            }
        };

        for persisted in apps {
            if self.is_draining() {
                return;
            }
            let app = persisted.config.deployment_id();
            let backup = match self.state_store.get_backup(&app) {
                Ok(Some(backup)) => backup,
                Ok(None) => continue,
                Err(error) => {
                    tracing::warn!(app = %app, "Failed to read backup config: {error}");
                    continue;
                }
            };
            let last_database_check = self
                .database_snapshots
                .read()
                .get(&app)
                .and_then(|state| state.checked_at_unix_secs);
            let due = match self.read_backup_index(&backup, &app).await {
                Ok(index) => due_backup(&index.backups, last_database_check, now_unix_secs()),
                Err(error) => {
                    tracing::warn!(app = %app, "Failed to read backup index: {error}");
                    Some(DueBackup::Full)
                }
            };
            match due {
                Some(DueBackup::Full) => {
                    if let Err(error) = self.backup_app_now(&app, None).await {
                        tracing::warn!(app = %app, "Scheduled backup failed: {error}");
                    }
                }
                Some(DueBackup::Databases) => self.run_scheduled_database_snapshot(&app).await,
                None => {}
            }
        }
    }

    async fn run_scheduled_database_snapshot(&self, app: &str) {
        let started = now_unix_secs();
        let outcome = self.database_snapshot_now(app).await;
        let mut states = self.database_snapshots.write();
        let state = states.entry(app.to_string()).or_default();
        match outcome {
            Ok(DatabaseSnapshotOutcome::Created(info)) => {
                tracing::info!(app, backup = %info.id, "Created database snapshot");
                state.checked_at_unix_secs = Some(started);
                state.skip = None;
            }
            Ok(DatabaseSnapshotOutcome::Skipped(skip)) => {
                // A busy app is retried on the next tick, not in an hour.
                if !matches!(
                    skip.reason,
                    BackupSkipReason::DeployInProgress | BackupSkipReason::BackupInProgress
                ) {
                    state.checked_at_unix_secs = Some(started);
                }
                match skip.reason {
                    BackupSkipReason::TooLarge | BackupSkipReason::InsufficientDisk => {
                        tracing::warn!(app, "Skipped database snapshot: {}", skip.message)
                    }
                    _ => tracing::info!(app, "Skipped database snapshot: {}", skip.message),
                }
                state.skip = Some(skip);
            }
            Err(error) => {
                tracing::warn!(app, "Scheduled database snapshot failed: {error}");
                state.checked_at_unix_secs = Some(started);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn backup(kind: BackupKind, created_at_unix_secs: i64) -> BackupInfo {
        serde_json::from_value(serde_json::json!({
            "id": format!("{kind:?}-{created_at_unix_secs}"), "app": "demo", "environment": "production",
            "server": "local", "created_at_unix_secs": created_at_unix_secs, "size_bytes": 1,
            "sha256_hex": "", "archive_key": "", "manifest_key": "",
            "encryption": {"algorithm": "", "key_id": "", "nonce_base64": "", "tag_base64": ""},
            "kind": kind,
        }))
        .unwrap()
    }

    #[test]
    fn due_logic_interleaves_hourly_database_snapshots_with_daily_archives() {
        let hour = 60 * 60;
        let now = 1_800_000_000;
        assert_eq!(due_backup(&[], None, now), Some(DueBackup::Full));
        // Database snapshots never postpone the daily archive.
        let stale_full = [
            backup(BackupKind::Databases, now - 60),
            backup(BackupKind::Full, now - 24 * hour),
        ];
        assert_eq!(due_backup(&stale_full, None, now), Some(DueBackup::Full));

        let full = backup(BackupKind::Full, now - 3 * hour);
        assert_eq!(
            due_backup(std::slice::from_ref(&full), None, now),
            Some(DueBackup::Databases)
        );
        let recent = [backup(BackupKind::Databases, now - hour + 60), full.clone()];
        assert_eq!(due_backup(&recent, None, now), None);
        assert_eq!(
            due_backup(&recent, None, now + 60),
            Some(DueBackup::Databases)
        );
        // A fresh full archive also covers the databases.
        assert_eq!(
            due_backup(&[backup(BackupKind::Full, now - 30 * 60)], None, now),
            None
        );
        // A check that uploaded nothing waits an hour, even with old entries.
        assert_eq!(
            due_backup(std::slice::from_ref(&full), Some(now - 30 * 60), now),
            None
        );
        assert_eq!(
            due_backup(std::slice::from_ref(&full), Some(now - hour), now),
            Some(DueBackup::Databases)
        );
        // After a restart the in-memory check is gone; the index still holds
        // the cadence, so a recent snapshot is not repeated.
        assert_eq!(due_backup(&recent, None, now - 60), None);
        // A clock behind the latest entry is never due.
        assert_eq!(
            due_backup(&[backup(BackupKind::Full, now + hour)], None, now),
            None
        );
    }

    #[tokio::test]
    async fn stalled_storage_does_not_hold_management_lock() {
        let temp = tempfile::tempdir().unwrap();
        let state = Arc::new(
            crate::ServerState::new(
                temp.path().into(),
                Arc::new(crate::tls::CertManager::new(
                    crate::tls::CertManagerConfig {
                        cert_dir: temp.path().join("certs"),
                        ..Default::default()
                    },
                )),
                None,
                Arc::new(parking_lot::RwLock::new(Default::default())),
            )
            .unwrap(),
        );
        let config = crate::instances::AppConfig {
            name: "scheduled".into(),
            ..Default::default()
        };
        let app = config.deployment_id();
        state.state_store.upsert_app(&config, &[]).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let backup = serde_json::from_value(serde_json::json!({
            "storage": {"provider": "s3", "bucket": "fixture", "endpoint": format!("https://{}", listener.local_addr().unwrap()),
                "region": "local", "access_key_id": "fixture", "secret_access_key": "fixture", "force_path_style": true},
            "backup_keys": [{"id": "backup-key-0123456789abcdef", "key_base64": "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="}]
        })).unwrap();
        super::super::validate_backup_binding(&backup).unwrap();
        state.state_store.set_backup(&app, Some(&backup)).unwrap();
        let scheduler = tokio::spawn({
            let state = state.clone();
            async move { state.run_due_backups().await }
        });
        // Hold the socket without completing TLS: the storage operation is pending.
        let (_socket, _) = tokio::time::timeout(Duration::from_secs(5), listener.accept())
            .await
            .unwrap()
            .unwrap();
        let available = state.management_lock.try_lock().is_ok();
        scheduler.abort();
        let _ = scheduler.await;
        assert!(
            available,
            "a remote storage stall blocked all management commands"
        );
    }
}
