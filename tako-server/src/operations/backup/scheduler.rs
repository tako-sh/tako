use std::sync::Arc;
use std::time::Duration;

use tako_core::{BACKUP_INTERVAL_SECS, BackupInfo};

use super::{latest_backup, now_unix_secs};

impl crate::ServerState {
    pub(crate) async fn backup_after_deploy(
        &self,
        app: &str,
    ) -> Option<Result<BackupInfo, String>> {
        self.state_store.get_backup(app).ok().flatten()?;
        Some(self.backup_app_now(app).await)
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
            let app = persisted.config.deployment_id();
            let Some(backup) = self.state_store.get_backup(&app).ok().flatten() else {
                continue;
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
