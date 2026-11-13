use std::sync::Arc;
use std::time::Duration;

use tako_core::{
    BACKUP_INTERVAL_SECS, BackupFailure, BackupFailureReason, BackupInfo, BackupKind, BackupSkip,
    BackupSkipReason,
};

use super::databases::{
    DATABASE_SNAPSHOT_INTERVAL_SECS, DatabaseSnapshotOutcome, DatabaseSnapshotState,
};
use super::{DEPLOY_LOCK_BUSY, FullBackupStart, latest_backup, now_unix_secs};

/// Short enough that hourly database snapshots stay close to an hour apart.
/// Due checks use the remote index, so a restart neither bursts nor resets.
const SCHEDULER_TICK: Duration = Duration::from_secs(10 * 60);
/// A failed full archive is retried after an hour, then after twice the
/// previous delay, up to this cap. Database snapshots keep their own cadence.
const FULL_RETRY_BASE_SECS: i64 = 60 * 60;
const FULL_RETRY_CAP_SECS: i64 = 6 * 60 * 60;
/// An unreadable index is read again after one tick, then backs off.
const INDEX_RETRY_BASE_SECS: i64 = 10 * 60;
const INDEX_RETRY_CAP_SECS: i64 = 60 * 60;
/// Beside the app data directory, at `apps/<name>/<environment>/`.
const POST_DEPLOY_BACKUP_MARKER: &str = "post-deploy-backup.queued";

/// Consecutive failures of one scheduled step, kept in memory only. After a
/// restart the first tick waits a full interval, so a crash loop never turns
/// into a loop of heavy work.
#[derive(Debug, Clone, Default)]
pub(crate) struct Backoff {
    failures: u32,
    failure: Option<BackupFailure>,
}

impl Backoff {
    fn record(
        &mut self,
        now: i64,
        reason: BackupFailureReason,
        message: String,
        base_secs: i64,
        cap_secs: i64,
    ) {
        self.failures = self.failures.saturating_add(1);
        let delay = base_secs
            .saturating_mul(1_i64 << (self.failures - 1).min(20))
            .min(cap_secs);
        self.failure = Some(BackupFailure {
            at_unix_secs: now,
            reason,
            message,
            retry_at_unix_secs: now.saturating_add(delay),
        });
    }

    fn clear(&mut self) {
        *self = Self::default();
    }

    fn waiting(&self, now: i64) -> bool {
        self.failure
            .as_ref()
            .is_some_and(|failure| now < failure.retry_at_unix_secs)
    }

    pub(super) fn failure(&self) -> Option<&BackupFailure> {
        self.failure.as_ref()
    }
}

impl DatabaseSnapshotState {
    pub(super) fn record_full_failure(
        &mut self,
        now: i64,
        reason: BackupFailureReason,
        message: String,
    ) {
        self.full_failures.record(
            now,
            reason,
            message,
            FULL_RETRY_BASE_SECS,
            FULL_RETRY_CAP_SECS,
        );
    }

    pub(super) fn record_full_success(&mut self) {
        self.full_failures.clear();
    }

    fn record_index_failure(&mut self, now: i64, message: String) {
        self.index_failures.record(
            now,
            BackupFailureReason::IndexUnreadable,
            message,
            INDEX_RETRY_BASE_SECS,
            INDEX_RETRY_CAP_SECS,
        );
    }

    /// The failure status reports: an unreadable index stops everything,
    /// so it comes first.
    pub(super) fn failure(&self) -> Option<&BackupFailure> {
        self.index_failures
            .failure()
            .or(self.full_failures.failure())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DueBackup {
    Full,
    Databases,
}

/// A full archive is due a day after the latest one, unless its last attempt
/// failed and the retry is not yet due. Otherwise a database snapshot is due
/// an hour after the latest backup of either kind or the latest completed
/// check that uploaded nothing.
pub(super) fn due_backup(
    backups: &[BackupInfo],
    state: &DatabaseSnapshotState,
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
        && !state.full_failures.waiting(now)
    {
        return Some(DueBackup::Full);
    }
    let latest = latest_backup(backups.iter())
        .map(|backup| backup.created_at_unix_secs)
        .max(state.checked_at_unix_secs);
    latest
        .is_none_or(|time| elapsed_since(time) >= DATABASE_SNAPSHOT_INTERVAL_SECS)
        .then_some(DueBackup::Databases)
}

/// What finalizing a deploy did about the post-deploy full archive.
#[derive(Debug)]
pub(crate) enum PostDeployBackup {
    Created(Box<BackupInfo>),
    /// An hourly database snapshot holds the backup lock; the scheduler runs
    /// the archive as soon as the snapshot finishes.
    Queued,
    Failed(String),
}

impl crate::ServerState {
    /// Never waits for a database snapshot: finalizing a deploy runs inside
    /// the CLI's deadline, and a snapshot upload can take minutes.
    pub(crate) async fn backup_after_deploy(&self, app: &str) -> Option<PostDeployBackup> {
        match self.state_store.get_backup(app) {
            Ok(Some(_)) => {}
            Ok(None) => return None,
            Err(error) => {
                return Some(PostDeployBackup::Failed(format!(
                    "read backup config: {error}"
                )));
            }
        }
        Some(match self.acquire_backup_lock_or_queue(app) {
            Ok(Some(backup)) => match self.operator_full_backup_with_lock(app, backup).await {
                Ok(info) => PostDeployBackup::Created(Box::new(info)),
                Err(error) => PostDeployBackup::Failed(error),
            },
            Ok(None) => {
                // Outside the schedule's lock and off the runtime. The queue
                // removes the marker only after the snapshot it waits for and
                // then a full archive finish.
                let marker = self.post_deploy_backup_marker(app);
                let name = app.to_string();
                let _ = tokio::task::spawn_blocking(move || {
                    record_post_deploy_marker(&name, &marker, true)
                })
                .await;
                tracing::info!(app, "Post-deploy backup queued behind a database snapshot");
                PostDeployBackup::Queued
            }
            Err(error) => PostDeployBackup::Failed(error),
        })
    }

    /// Run a post-deploy full archive queued behind the database snapshot
    /// that just finished. Its wait is that snapshot, which its transfer
    /// timeouts bound. If another backup operation took the lock first, give
    /// up with a status-visible failure rather than wait again. If a deploy
    /// or an operator operation has the deploy lock, it stays queued for the
    /// next tick.
    pub(super) async fn run_queued_post_deploy_backup(&self, app: &str) {
        // It stays queued until the restore is resolved.
        if self.skip_unresolved_restore(app) {
            return;
        }
        let (queued, stale) = {
            let mut states = self.database_snapshots.write();
            let state = states.entry(app.to_string()).or_default();
            (
                std::mem::take(&mut state.post_deploy_backup_queued),
                state.post_deploy_marker_stale,
            )
        };
        // Queued before a restart, which kept only the marker.
        if !queued && (stale || !self.post_deploy_backup_marker(app).exists()) {
            return;
        }
        let result = match self.acquire_backup_lock(app) {
            Ok(backup) => {
                self.full_backup_with_lock(app, backup, FullBackupStart::Scheduled)
                    .await
            }
            Err(error) => {
                let message = format!(
                    "The post-deploy backup queued behind a database snapshot could not start: {error}"
                );
                self.database_snapshots
                    .write()
                    .entry(app.to_string())
                    .or_default()
                    .record_full_failure(
                        now_unix_secs(),
                        BackupFailureReason::Failed,
                        message.clone(),
                    );
                Err(message)
            }
        };
        if !matches!(result, Ok(None)) {
            self.clear_post_deploy_marker(app);
        }
        match result {
            Ok(Some(info)) => {
                tracing::info!(app, backup = %info.id, "Created queued post-deploy backup")
            }
            Ok(None) => {
                self.database_snapshots
                    .write()
                    .entry(app.to_string())
                    .or_default()
                    .post_deploy_backup_queued = true;
                tracing::info!(
                    app,
                    "Queued post-deploy backup waits for the next tick: {DEPLOY_LOCK_BUSY}"
                );
            }
            Err(error) => tracing::warn!(app, "Queued post-deploy backup failed: {error}"),
        }
    }

    pub(crate) fn start_backup_scheduler(self: Arc<Self>, handle: &tokio::runtime::Handle) {
        handle.spawn(async move {
            // Not at startup: a server that keeps restarting must not start
            // a full archive each time.
            let mut interval = tokio::time::interval_at(
                tokio::time::Instant::now() + SCHEDULER_TICK,
                SCHEDULER_TICK,
            );
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                interval.tick().await;
                self.run_due_backups().await;
            }
        });
    }

    pub(super) async fn run_due_backups(&self) {
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
            if self.skip_unresolved_restore(&app) {
                continue;
            }
            let now = now_unix_secs();
            let state = self.backup_schedule_state(&app);
            if state.index_failures.waiting(now) {
                continue;
            }
            let due = match self.read_backup_index(&backup, &app).await {
                Ok(index) => {
                    let mut states = self.database_snapshots.write();
                    let state = states.entry(app.clone()).or_default();
                    state.index_failures.clear();
                    Some(due_backup(&index.backups, state, now))
                }
                Err(error) => {
                    // Without the index nothing is known to be due; never
                    // guess a full archive.
                    tracing::warn!(app = %app, "Failed to read backup index: {error}");
                    self.database_snapshots
                        .write()
                        .entry(app.clone())
                        .or_default()
                        .record_index_failure(now, format!("Failed to read backup index: {error}"));
                    None
                }
            };
            let Some(due) = due else {
                continue;
            };
            match due {
                // An archive that ran is also the queued post-deploy one: it
                // started after the deploy, and a full archive stays due
                // until one is created.
                Some(DueBackup::Full) => {
                    if self.run_scheduled_full_backup(&app).await {
                        self.database_snapshots
                            .write()
                            .entry(app.clone())
                            .or_default()
                            .post_deploy_backup_queued = false;
                        self.clear_post_deploy_marker(&app);
                    }
                }
                Some(DueBackup::Databases) => self.run_scheduled_database_snapshot(&app).await,
                None => {}
            }
            self.run_queued_post_deploy_backup(&app).await;
        }
    }

    /// Whether an unfinished restore keeps `app` stopped, which pauses its
    /// scheduled backups: an archive would copy its partly restored data,
    /// and the retention every upload applies deletes database snapshots
    /// after 48 hours, perhaps the one the operator restores from. Status
    /// says why.
    fn skip_unresolved_restore(&self, app: &str) -> bool {
        let Some(reason) = self.unresolved_restore_block(app) else {
            return false;
        };
        let message =
            format!("Scheduled backups are paused while the app is kept stopped. {reason}");
        tracing::warn!(app, "{message}");
        self.database_snapshots
            .write()
            .entry(app.to_string())
            .or_default()
            .skip = Some(BackupSkip {
            at_unix_secs: now_unix_secs(),
            reason: BackupSkipReason::UnfinishedRestore,
            message,
        });
        true
    }

    /// A due full archive that does not get the deploy lock is still due on
    /// the next tick; that is no failure and starts no backoff. Returns
    /// whether the archive ran, successfully or not.
    async fn run_scheduled_full_backup(&self, app: &str) -> bool {
        let result = match self.acquire_backup_lock(app) {
            Ok(backup) => {
                self.full_backup_with_lock(app, backup, FullBackupStart::Scheduled)
                    .await
            }
            Err(error) => {
                tracing::warn!(app, "Scheduled backup failed: {error}");
                return false;
            }
        };
        match result {
            Ok(Some(_)) => true,
            Ok(None) => {
                tracing::info!(
                    app,
                    "Skipped scheduled backup until the next tick: {DEPLOY_LOCK_BUSY}"
                );
                false
            }
            Err(error) => {
                tracing::warn!(app, "Scheduled backup failed: {error}");
                true
            }
        }
    }

    /// Present while a post-deploy archive is queued, so a restart keeps
    /// it. It sits in the app's directory, which deleting the app removes.
    pub(super) fn post_deploy_backup_marker(&self, app: &str) -> std::path::PathBuf {
        let root = crate::release::app_runtime_data_paths(&self.runtime.data_dir, app).root;
        root.with_file_name(POST_DEPLOY_BACKUP_MARKER)
    }

    #[cfg(test)]
    pub(super) fn record_post_deploy_backup_queued(&self, app: &str, queued: bool) {
        record_post_deploy_marker(app, &self.post_deploy_backup_marker(app), queued);
    }

    /// Remove the marker of a queued post-deploy archive that ran. One that
    /// cannot be removed is remembered, so it does not run an archive at
    /// every tick, and logged once.
    fn clear_post_deploy_marker(&self, app: &str) {
        let marker = self.post_deploy_backup_marker(app);
        let removed = match std::fs::remove_file(&marker) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            result => result,
        };
        let mut states = self.database_snapshots.write();
        let state = states.entry(app.to_string()).or_default();
        match removed {
            Ok(()) => state.post_deploy_marker_stale = false,
            Err(error) => {
                if !std::mem::replace(&mut state.post_deploy_marker_stale, true) {
                    tracing::warn!(
                        app,
                        path = %marker.display(),
                        "Failed to remove the marker of a queued post-deploy backup that ran; it queues no more backups until the server restarts: {error}"
                    );
                }
            }
        }
    }

    /// Forget the in-memory schedule of a deleted app, so an app created
    /// again under the same name starts without its queue and backoff.
    pub(crate) fn forget_backup_schedule(&self, app: &str) {
        self.database_snapshots.write().remove(app);
    }

    pub(super) fn backup_schedule_state(&self, app: &str) -> DatabaseSnapshotState {
        self.database_snapshots
            .read()
            .get(app)
            .cloned()
            .unwrap_or_default()
    }

    async fn run_scheduled_database_snapshot(&self, app: &str) {
        let started = now_unix_secs();
        let outcome = self.database_snapshot_now(app).await;
        self.record_database_snapshot(app, started, outcome);
    }

    fn record_database_snapshot(
        &self,
        app: &str,
        started: i64,
        outcome: Result<DatabaseSnapshotOutcome, String>,
    ) {
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

fn record_post_deploy_marker(app: &str, marker: &std::path::Path, queued: bool) {
    let result = if queued {
        std::fs::File::create(marker).and_then(|file| file.sync_all())
    } else {
        match std::fs::remove_file(marker) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            result => result,
        }
    };
    if let Err(error) = result {
        tracing::warn!(
            app,
            path = %marker.display(),
            "Failed to record the queued post-deploy backup; a restart may drop it: {error}"
        );
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

    fn checked(at: Option<i64>) -> DatabaseSnapshotState {
        DatabaseSnapshotState {
            checked_at_unix_secs: at,
            ..Default::default()
        }
    }

    #[test]
    fn due_logic_interleaves_hourly_database_snapshots_with_daily_archives() {
        let hour = 60 * 60;
        let now = 1_800_000_000;
        let none = checked(None);
        assert_eq!(due_backup(&[], &none, now), Some(DueBackup::Full));
        // Database snapshots never postpone the daily archive.
        let stale_full = [
            backup(BackupKind::Databases, now - 60),
            backup(BackupKind::Full, now - 24 * hour),
        ];
        assert_eq!(due_backup(&stale_full, &none, now), Some(DueBackup::Full));

        let full = backup(BackupKind::Full, now - 3 * hour);
        assert_eq!(
            due_backup(std::slice::from_ref(&full), &none, now),
            Some(DueBackup::Databases)
        );
        let recent = [backup(BackupKind::Databases, now - hour + 60), full.clone()];
        assert_eq!(due_backup(&recent, &none, now), None);
        assert_eq!(
            due_backup(&recent, &none, now + 60),
            Some(DueBackup::Databases)
        );
        // A fresh full archive also covers the databases.
        assert_eq!(
            due_backup(&[backup(BackupKind::Full, now - 30 * 60)], &none, now),
            None
        );
        // A check that uploaded nothing waits an hour, even with old entries.
        assert_eq!(
            due_backup(
                std::slice::from_ref(&full),
                &checked(Some(now - 30 * 60)),
                now
            ),
            None
        );
        assert_eq!(
            due_backup(std::slice::from_ref(&full), &checked(Some(now - hour)), now),
            Some(DueBackup::Databases)
        );
        // After a restart the in-memory check is gone; the index still holds
        // the cadence, so a recent snapshot is not repeated.
        assert_eq!(due_backup(&recent, &none, now - 60), None);
        // A clock behind the latest entry is never due.
        assert_eq!(
            due_backup(&[backup(BackupKind::Full, now + hour)], &none, now),
            None
        );
    }

    /// One scheduler tick against an index, with an injected clock: run what
    /// is due and record the outcome the way the scheduler does.
    fn tick(
        state: &mut DatabaseSnapshotState,
        backups: &mut Vec<BackupInfo>,
        now: i64,
        full_works: bool,
    ) -> Option<DueBackup> {
        let due = due_backup(backups, state, now);
        match due {
            Some(DueBackup::Full) if full_works => {
                backups.push(backup(BackupKind::Full, now));
                state.record_full_success();
            }
            Some(DueBackup::Full) => state.record_full_failure(
                now,
                BackupFailureReason::Failed,
                "upload timed out".into(),
            ),
            Some(DueBackup::Databases) => {
                backups.push(backup(BackupKind::Databases, now));
                state.checked_at_unix_secs = Some(now);
            }
            None => {}
        }
        due
    }

    #[test]
    fn a_failing_full_archive_backs_off_and_database_snapshots_keep_their_hour() {
        let hour = 60 * 60;
        let tick_secs = SCHEDULER_TICK.as_secs() as i64;
        let start = 1_800_000_000;
        let mut state = DatabaseSnapshotState::default();
        let mut backups = vec![backup(BackupKind::Full, start - 25 * hour)];

        let mut fulls = Vec::new();
        let mut snapshots = Vec::new();
        let mut now = start;
        while now < start + 24 * hour {
            match tick(&mut state, &mut backups, now, false) {
                Some(DueBackup::Full) => fulls.push(now - start),
                Some(DueBackup::Databases) => snapshots.push(now - start),
                None => {}
            }
            now += tick_secs;
        }
        // Retries after 1 h, 2 h, 4 h, then every 6 h: never every tick.
        let gaps: Vec<i64> = fulls.windows(2).map(|pair| pair[1] - pair[0]).collect();
        assert_eq!(gaps, [hour, 2 * hour, 4 * hour, 6 * hour, 6 * hour]);
        // A database snapshot every hour, whatever the full archive does.
        assert!(snapshots.len() >= 23, "{snapshots:?}");
        assert!(
            snapshots
                .windows(2)
                .all(|pair| pair[1] - pair[0] <= hour + tick_secs),
            "{snapshots:?}"
        );
        let failure = state.failure().unwrap();
        assert_eq!(failure.reason, BackupFailureReason::Failed);
        assert_eq!(
            failure.retry_at_unix_secs,
            failure.at_unix_secs + FULL_RETRY_CAP_SECS
        );

        // Recovery: the retry succeeds, the failure clears, and the next full
        // archive is due a day later again.
        let retry = failure.retry_at_unix_secs;
        assert_eq!(
            tick(&mut state, &mut backups, retry, true),
            Some(DueBackup::Full)
        );
        assert!(state.failure().is_none());
        assert_ne!(
            due_backup(&backups, &state, retry + 23 * hour),
            Some(DueBackup::Full)
        );
        assert_eq!(
            due_backup(&backups, &state, retry + 24 * hour),
            Some(DueBackup::Full)
        );
    }

    #[test]
    fn an_unreadable_index_backs_off_and_is_reported_before_full_failures() {
        let now = 1_800_000_000;
        let mut state = DatabaseSnapshotState::default();
        state.record_full_failure(
            now - 60,
            BackupFailureReason::InsufficientDisk,
            "disk".into(),
        );
        state.record_index_failure(now, "Failed to read backup index: corrupt".into());
        assert!(
            state
                .index_failures
                .waiting(now + INDEX_RETRY_BASE_SECS - 1)
        );
        assert!(!state.index_failures.waiting(now + INDEX_RETRY_BASE_SECS));
        let failure = state.failure().unwrap();
        assert_eq!(failure.reason, BackupFailureReason::IndexUnreadable);
        assert!(failure.message.contains("corrupt"));

        state.record_index_failure(now + INDEX_RETRY_BASE_SECS, "again".into());
        assert_eq!(
            state.failure().unwrap().retry_at_unix_secs,
            now + 3 * INDEX_RETRY_BASE_SECS
        );
        for _ in 0..10 {
            state.record_index_failure(now, "again".into());
        }
        assert_eq!(
            state.failure().unwrap().retry_at_unix_secs,
            now + INDEX_RETRY_CAP_SECS
        );
        state.index_failures.clear();
        assert_eq!(
            state.failure().unwrap().reason,
            BackupFailureReason::InsufficientDisk
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
