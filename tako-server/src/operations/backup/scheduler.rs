use std::sync::Arc;
use std::time::Duration;

use tako_core::{BACKUP_INTERVAL_SECS, BackupInfo};

use super::{latest_backup, now_unix_secs};

impl crate::ServerState {
    pub(crate) async fn backup_after_deploy(
        &self,
        app: &str,
    ) -> Option<Result<BackupInfo, String>> {
        match self.state_store.get_backup(app) {
            Ok(Some(_)) => Some(self.backup_app_now(app).await),
            Ok(None) => None,
            Err(error) => Some(Err(format!("read backup config: {error}"))),
        }
    }

    pub(crate) fn start_backup_scheduler(self: Arc<Self>, handle: &tokio::runtime::Handle) {
        handle.spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(60 * 60));
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
            let due = match self.read_backup_index(&backup, &app).await {
                Ok(index) => {
                    let last = latest_backup(index.backups.iter())
                        .map(|backup| backup.created_at_unix_secs);
                    last.is_none_or(|last| {
                        now_unix_secs().saturating_sub(last) >= BACKUP_INTERVAL_SECS
                    })
                }
                Err(error) => {
                    tracing::warn!(app = %app, "Failed to read backup index: {error}");
                    true
                }
            };
            if due && let Err(error) = self.backup_app_now(&app).await {
                tracing::warn!(app = %app, "Scheduled backup failed: {error}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
