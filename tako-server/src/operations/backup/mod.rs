use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tako_core::{
    BACKUP_INTERVAL_SECS, BackupDownloadUrlResponse, BackupFailure, BackupFailureReason,
    BackupInfo, BackupKind, BackupListResponse, BackupRestoreResponse, BackupStatusResponse,
    Response,
};

use crate::object_storage::{S3Method, presign_s3_url};
use crate::release::{
    app_runtime_data_paths, ensure_app_runtime_data_dirs, release_app_path,
    requested_deployment_identity, resolve_release_runtime_bin,
};

mod databases;
mod encryption;
mod index;
mod journal;
mod mounts;
mod s3;
mod scheduler;
mod snapshot;
mod worker;

use databases::DISK_RESERVE_BYTES;
pub(crate) use databases::DatabaseSnapshotState;
use encryption::{
    active_backup_key, database_digest_key, decrypt_backup_file, encrypt_backup_file,
    find_backup_key, validate_backup_key,
};
use index::{backup_index_key, backup_object_prefix, latest_backup};
use journal::{
    DatabaseSwap, RESTORE_WORKER_TIMEOUT, refuse_unfinished_restores, swap_databases_off_runtime,
};
pub(crate) use journal::{
    RecoveryMode, recover_interrupted_restores, run_restore_worker, start_restore_generation,
    sweep_backup_work_dirs,
};
use s3::{
    DOWNLOAD_URL_EXPIRES_SECONDS, S3_URL_EXPIRES_SECONDS, TRANSFER_TIMEOUT, backup_http_client,
    download_object, prune_retention, put_json_object, queue_retention, upload_file,
};
pub(crate) use scheduler::PostDeployBackup;
use snapshot::{
    DATABASE_SNAPSHOT_ARCHIVE_ROOT, RestoredData, SwapError, create_backup_archive,
    database_digest, extract_database_snapshot, restore_data_tree, sha256_file_hex,
};
pub(crate) use snapshot::{measure_database_tree, snapshot_data_tree, snapshot_database_tree};

/// A full archive holds the snapshot copy, the compressed archive, and its
/// encrypted copy at once.
const FULL_BACKUP_DISK_COPIES: u64 = 3;
/// Why a scheduled backup did not take the deploy lock: a deploy holds it,
/// or another operation does or was turned away and is about to retry.
const DEPLOY_LOCK_BUSY: &str = "A deploy or another operation on the app is in progress.";
/// How long workflow workers may finish their runs before a restore kills them.
const WORKFLOW_STOP_TIMEOUT: Duration = if cfg!(test) {
    Duration::from_secs(1)
} else {
    Duration::from_secs(120)
};

impl crate::ServerState {
    pub(crate) async fn backup_now(&self, app: &str) -> Response {
        match self.backup_app_now(app).await {
            Ok(info) => Response::ok(info),
            Err(error) => Response::error(error),
        }
    }

    pub(crate) async fn list_backups(&self, app: &str) -> Response {
        match self.list_backups_inner(app).await {
            Ok(backups) => Response::ok(BackupListResponse {
                app: app.to_string(),
                backups,
            }),
            Err(error) => Response::error(error),
        }
    }

    pub(crate) async fn backup_status(&self, app: &str) -> Response {
        match self.backup_status_inner(app).await {
            Ok(status) => Response::ok(status),
            Err(error) => Response::error(error),
        }
    }

    pub(crate) async fn backup_download_url(&self, app: &str, backup_id: &str) -> Response {
        match self.backup_download_url_inner(app, backup_id).await {
            Ok(response) => Response::ok(response),
            Err(error) => Response::error(error),
        }
    }

    pub(crate) async fn restore_backup(&self, app: &str, backup_id: &str) -> Response {
        match self.restore_backup_inner(app, backup_id).await {
            Ok(restored) => Response::ok(restored),
            Err(error) => Response::error(error),
        }
    }

    /// Create a full archive for an operator unless another backup operation
    /// holds the lock.
    async fn backup_app_now(&self, app: &str) -> Result<BackupInfo, String> {
        let backup = self.acquire_backup_lock(app)?;
        self.operator_full_backup_with_lock(app, backup).await
    }

    /// `backups now` and the post-deploy archive inside finalize are operator
    /// operations: they wait for the deploy lock.
    pub(super) async fn operator_full_backup_with_lock(
        &self,
        app: &str,
        backup: tokio::sync::OwnedMutexGuard<()>,
    ) -> Result<BackupInfo, String> {
        self.full_backup_with_lock(app, backup, FullBackupStart::Operator)
            .await?
            .ok_or_else(|| DEPLOY_LOCK_BUSY.to_string())
    }

    /// Records the outcome for the scheduler's retry backoff and status.
    /// `Ok(None)`: a scheduled archive did not take the deploy lock; nothing
    /// ran, nothing is recorded, and the scheduler tries again on its next
    /// tick. Operator archives wait for the lock instead.
    pub(super) async fn full_backup_with_lock(
        &self,
        app: &str,
        _backup: tokio::sync::OwnedMutexGuard<()>,
        start: FullBackupStart,
    ) -> Result<Option<BackupInfo>, String> {
        let result = match self.full_backup_disk_preflight(app).await {
            Ok(()) => self
                .full_backup_locked(app, start)
                .await
                .map_err(|message| (BackupFailureReason::Failed, message)),
            Err(failure) => Err(failure),
        };
        let mut states = self.database_snapshots.write();
        let state = states.entry(app.to_string()).or_default();
        match result {
            Ok(None) => Ok(None),
            Ok(Some(info)) => {
                state.record_full_success();
                Ok(Some(info))
            }
            Err((reason, message)) => {
                state.record_full_failure(now_unix_secs(), reason, message.clone());
                Err(message)
            }
        }
    }

    /// A full archive holds the snapshot copy, the compressed archive, and its
    /// encrypted copy at once; each is at most the measured size of the data
    /// tree, which counts block rounding and archive headers per file.
    async fn full_backup_disk_preflight(
        &self,
        app: &str,
    ) -> Result<(), (BackupFailureReason, String)> {
        let data_dir = self.runtime.data_dir.clone();
        let data_root = app_runtime_data_paths(std::path::Path::new(""), app).root;
        let measured = tokio::task::spawn_blocking(move || {
            let size = snapshot::measure_data_tree(&data_dir, &data_root)?;
            Ok::<_, String>((size, super::release_artifacts::available_bytes(&data_dir)?))
        })
        .await
        .map_err(|e| format!("backup size worker failed: {e}"))
        .and_then(|result| result)
        .map_err(|message| (BackupFailureReason::Failed, message))?;
        full_backup_preflight(measured.0, measured.1)
            .map_err(|message| (BackupFailureReason::InsufficientDisk, message))
    }

    /// Call with the app's backup lock.
    async fn full_backup_locked(
        &self,
        app: &str,
        start: FullBackupStart,
    ) -> Result<Option<BackupInfo>, String> {
        let _guard = match start {
            FullBackupStart::Operator => self.lock_deploy_for_backup(app).await,
            FullBackupStart::Scheduled => {
                match self
                    .try_lock_deploy_for_scheduled_backup(app, std::time::Instant::now())
                    .await
                {
                    Some(guard) => guard,
                    None => return Ok(None),
                }
            }
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

        let tmp_root = self.runtime.data_dir.join("tmp").join("backups");
        tokio::fs::create_dir_all(&tmp_root)
            .await
            .map_err(|e| format!("create backup temp dir {}: {e}", tmp_root.display()))?;

        let backup_id = format!("b{}-{}", now_unix_secs(), nanoid::nanoid!(8));
        let work_dir = tmp_root.join(&backup_id);
        let snapshot_dir = work_dir.join("snapshot");
        let archive_path = work_dir.join("data.tar.zst");
        let encrypted_archive_path = work_dir.join("data.tar.zst.enc");
        let backup_key = active_backup_key(&backup)?.clone();

        create_private_work_dir(&work_dir).await?;

        let result: Result<BackupInfo, String> = async {
            let archive_result = tokio::task::spawn_blocking({
                let data_dir = self.runtime.data_dir.clone();
                let data_root = app_runtime_data_paths(std::path::Path::new(""), app).root;
                let snapshot_dir = snapshot_dir.clone();
                let archive_path = archive_path.clone();
                let encrypted_archive_path = encrypted_archive_path.clone();
                let backup_key = backup_key.clone();
                move || {
                    worker::run_worker(&[
                        "--snapshot-data".as_ref(),
                        data_dir.as_os_str(),
                        data_root.as_os_str(),
                        snapshot_dir.as_os_str(),
                    ])?;
                    // Lets the next hourly database snapshot skip if nothing
                    // changed. Without it that snapshot is simply uploaded.
                    let database_digest = database_digest_key(&backup_key)
                        .and_then(|key| database_digest(&snapshot_dir, &key))
                        .map(|digest| digest.hex)
                        .inspect_err(|error| {
                            tracing::warn!("Backup database digest unavailable: {error}")
                        })
                        .ok();
                    create_backup_archive(&snapshot_dir, &archive_path)?;
                    let encryption =
                        encrypt_backup_file(&archive_path, &encrypted_archive_path, &backup_key)?;
                    let sha256_hex = sha256_file_hex(&encrypted_archive_path)?;
                    Ok::<_, String>((sha256_hex, encryption, database_digest))
                }
            })
            .await
            .map_err(|e| format!("backup worker failed: {e}"))?;
            let (sha256_hex, encryption, database_digest) = archive_result?;
            // The snapshot is independent of the live app now. Remote I/O must
            // not keep stop/delete/deploy waiting on this lifecycle lock.
            drop(_guard);
            let size_bytes = tokio::fs::metadata(&encrypted_archive_path)
                .await
                .map_err(|e| {
                    format!(
                        "read backup archive metadata {}: {e}",
                        encrypted_archive_path.display()
                    )
                })?
                .len();

            let (app_name, environment) = requested_deployment_identity(app);
            let server = self.backup_server_label();
            let prefix = backup_object_prefix(&app_name, &environment, &server);
            let archive_key = format!("{prefix}/{backup_id}.tar.zst.enc");
            let manifest_key = format!("{prefix}/{backup_id}.json");

            let info = BackupInfo {
                id: backup_id,
                app: app_name,
                environment,
                server,
                created_at_unix_secs: now_unix_secs(),
                size_bytes,
                sha256_hex,
                archive_key,
                manifest_key,
                encryption,
                kind: BackupKind::Full,
                database_digest,
                databases: Vec::new(),
            };
            self.publish_backup(&backup, app, &info, &encrypted_archive_path)
                .await?;

            Ok(info)
        }
        .await;

        let _ = tokio::fs::remove_dir_all(&work_dir).await;
        result.map(Some)
    }

    /// Upload an encrypted archive and its manifest, then record it in the
    /// index and apply retention. Call with the app's backup operation lock.
    async fn publish_backup(
        &self,
        backup: &tako_core::BackupBinding,
        app: &str,
        info: &BackupInfo,
        encrypted_archive_path: &std::path::Path,
    ) -> Result<(), String> {
        let client = backup_http_client(TRANSFER_TIMEOUT)?;
        let mut index = self.read_backup_index(backup, app).await?;
        upload_file(
            &client,
            &backup.storage,
            &info.archive_key,
            encrypted_archive_path,
        )
        .await?;
        put_json_object(&client, &backup.storage, &info.manifest_key, info).await?;

        index.backups.retain(|existing| existing.id != info.id);
        index.backups.push(info.clone());
        index.backups.sort_by(|a, b| {
            b.created_at_unix_secs
                .cmp(&a.created_at_unix_secs)
                .then_with(|| b.id.cmp(&a.id))
        });
        queue_retention(&mut index, backup.retention_days);
        put_json_object(
            &client,
            &backup.storage,
            &backup_index_key(app, self),
            &index,
        )
        .await?;

        // Persist the new backup and deletion intent before deleting any
        // objects. A failed final index write leaves safe, retryable work.
        self.prune_queued_retention(&client, backup, app, &mut index)
            .await;
        Ok(())
    }

    async fn prune_queued_retention(
        &self,
        client: &reqwest::Client,
        backup: &tako_core::BackupBinding,
        app: &str,
        index: &mut index::BackupIndex,
    ) {
        if index.pending_deletions.is_empty() {
            return;
        }
        prune_retention(client, &backup.storage, index).await;
        if let Err(error) =
            put_json_object(client, &backup.storage, &backup_index_key(app, self), index).await
        {
            tracing::warn!(
                app,
                "Failed to acknowledge backup retention cleanup: {error}"
            );
        }
    }

    async fn list_backups_inner(&self, app: &str) -> Result<Vec<BackupInfo>, String> {
        let backup = self
            .state_store
            .get_backup(app)
            .map_err(|error| format!("read backup config: {error}"))?
            .ok_or_else(|| format!("Backups are not configured for {app}."))?;
        let mut index = self.read_backup_index(&backup, app).await?;
        index.backups.sort_by(|a, b| {
            b.created_at_unix_secs
                .cmp(&a.created_at_unix_secs)
                .then_with(|| b.id.cmp(&a.id))
        });
        Ok(index.backups)
    }

    async fn backup_status_inner(&self, app: &str) -> Result<BackupStatusResponse, String> {
        let Some(backup) = self
            .state_store
            .get_backup(app)
            .map_err(|error| format!("read backup config: {error}"))?
        else {
            return Ok(BackupStatusResponse {
                app: app.to_string(),
                enabled: false,
                retention_days: None,
                last_backup: None,
                next_backup_at_unix_secs: None,
                last_database_snapshot_at_unix_secs: None,
                last_database_snapshot_skip: None,
                last_backup_failure: None,
            });
        };
        let now = now_unix_secs();
        let schedule = self.backup_schedule_state(app);
        let index = match self.read_backup_index(&backup, app).await {
            Ok(index) => index,
            Err(error) => {
                // Report the index itself as the problem: nothing it lists
                // can be shown, and no scheduled backup runs without it.
                let retry_at_unix_secs = schedule
                    .index_failures
                    .failure()
                    .map_or(now, |failure| failure.retry_at_unix_secs);
                return Ok(BackupStatusResponse {
                    app: app.to_string(),
                    enabled: true,
                    retention_days: Some(backup.retention_days),
                    last_backup: None,
                    next_backup_at_unix_secs: None,
                    last_database_snapshot_at_unix_secs: None,
                    last_database_snapshot_skip: schedule.skip.clone(),
                    last_backup_failure: Some(BackupFailure {
                        at_unix_secs: now,
                        reason: BackupFailureReason::IndexUnreadable,
                        message: format!("Failed to read backup index: {error}"),
                        retry_at_unix_secs,
                    }),
                });
            }
        };
        Ok(enabled_backup_status(
            app,
            backup.retention_days,
            &index.backups,
            &schedule,
            now,
        ))
    }

    async fn backup_download_url_inner(
        &self,
        app: &str,
        backup_id: &str,
    ) -> Result<BackupDownloadUrlResponse, String> {
        let backup = self
            .state_store
            .get_backup(app)
            .map_err(|error| format!("read backup config: {error}"))?
            .ok_or_else(|| format!("Backups are not configured for {app}."))?;
        let info = self.find_backup(&backup, app, backup_id).await?;
        let url = presign_s3_url(
            &backup.storage,
            &info.archive_key,
            S3Method::Get,
            DOWNLOAD_URL_EXPIRES_SECONDS,
        )?;
        Ok(BackupDownloadUrlResponse {
            backup: info,
            url,
            expires_in_seconds: DOWNLOAD_URL_EXPIRES_SECONDS,
        })
    }

    async fn restore_backup_inner(
        &self,
        app: &str,
        backup_id: &str,
    ) -> Result<BackupRestoreResponse, String> {
        let _backup = self.acquire_backup_lock(app)?;
        let lock = self.get_deploy_lock(app).await;
        let _guard = lock.lock().await;
        let backup = self
            .state_store
            .get_backup(app)
            .map_err(|error| format!("read backup config: {error}"))?
            .ok_or_else(|| format!("Backups are not configured for {app}."))?;
        let info = self.find_backup(&backup, app, backup_id).await?;
        let data_root = app_runtime_data_paths(&self.runtime.data_dir, app).root;
        // Before downloading anything; the swap checks again.
        refuse_unfinished_restores(&data_root)?;
        if info.kind == BackupKind::Full {
            snapshot::refuse_mounts_in_data_tree(&data_root)?;
        }

        let app_ref = self
            .app_manager
            .get_app(app)
            .ok_or_else(|| format!("App not found: {app}"))?;
        let config = app_ref.config.read().clone();

        let tmp_root = self.runtime.data_dir.join("tmp").join("restore");
        tokio::fs::create_dir_all(&tmp_root)
            .await
            .map_err(|e| format!("create restore temp dir {}: {e}", tmp_root.display()))?;
        let work_dir = tmp_root.join(format!("restore-{}", nanoid::nanoid!(8)));
        let archive_path = work_dir.join("data.tar.zst");
        let encrypted_archive_path = work_dir.join("data.tar.zst.enc");
        let extract_dir = work_dir.join("extracted");
        create_private_work_dir(&work_dir).await?;

        let result: Result<BackupRestoreResponse, String> = async {
            if info.kind == BackupKind::Databases {
                restore_download_preflight(
                    info.size_bytes,
                    super::release_artifacts::available_bytes(&work_dir)?,
                )?;
            }
            let client = backup_http_client(TRANSFER_TIMEOUT)?;
            download_object(
                &client,
                &backup.storage,
                &info.archive_key,
                &encrypted_archive_path,
            )
            .await?;
            let actual_sha256 = tokio::task::spawn_blocking({
                let encrypted_archive_path = encrypted_archive_path.clone();
                move || sha256_file_hex(&encrypted_archive_path)
            })
            .await
            .map_err(|e| format!("restore checksum worker failed: {e}"))??;
            if actual_sha256 != info.sha256_hex {
                Err("Downloaded backup checksum did not match manifest.".to_string())?;
            }

            let backup_key = find_backup_key(&backup, &info.encryption.key_id)?.clone();
            tokio::task::spawn_blocking({
                let encrypted_archive_path = encrypted_archive_path.clone();
                let archive_path = archive_path.clone();
                let encryption = info.encryption.clone();
                move || {
                    decrypt_backup_file(
                        &encrypted_archive_path,
                        &archive_path,
                        &backup_key,
                        &encryption,
                    )
                }
            })
            .await
            .map_err(|e| format!("restore decrypt worker failed: {e}"))??;
            let _ = tokio::fs::remove_file(&encrypted_archive_path).await;

            // Each kind has its own archive layout; never apply one as another.
            let staged = match info.kind {
                BackupKind::Full => {
                    crate::extract_zstd_archive(&archive_path, &extract_dir)?;
                    StagedRestore::Full
                }
                BackupKind::Databases => {
                    // The originals stay on disk as the rollback copy until the
                    // app restarts, so the extracted databases need free space
                    // of their own.
                    let budget = super::release_artifacts::available_bytes(&work_dir)?
                        .saturating_sub(DISK_RESERVE_BYTES);
                    let databases = tokio::task::spawn_blocking({
                        let archive_path = archive_path.clone();
                        let extract_dir = extract_dir.clone();
                        move || extract_database_snapshot(&archive_path, &extract_dir, budget)
                    })
                    .await
                    .map_err(|e| format!("restore extract worker failed: {e}"))??;
                    StagedRestore::Databases(databases)
                }
            };
            let _ = tokio::fs::remove_file(&archive_path).await;

            let release_path = release_app_path(&self.runtime.data_dir, &config);
            let runtime_bin_path =
                resolve_release_runtime_bin(&release_path, &self.runtime.data_dir)
                    .await
                    .ok()
                    .flatten();
            let restart = || {
                self.restart_after_restore(
                    app,
                    &app_ref,
                    &config,
                    &release_path,
                    runtime_bin_path.as_deref(),
                )
            };
            let _lifecycle = app_ref.lifecycle_gate.write().await;
            self.cold_start.mark_failed(app, "restoring_backup");
            // Workflow workers, including those of a replaced release still
            // finishing runs, and the workflow store hold
            // tako/workflows.sqlite; both restore kinds may replace it.
            let closed = match self.workflows.close_store(app, WORKFLOW_STOP_TIMEOUT).await {
                Ok(closed) => closed,
                Err(error) => {
                    let message = format!("Stop workflows before restore: {error}");
                    return Err(match restart().await {
                        Ok(()) => {
                            self.finish_restart(app, &app_ref, &config);
                            format!("{message}; original data kept and app restarted")
                        }
                        Err(restart) => {
                            self.leave_stopped(
                                app,
                                &app_ref,
                                format!(
                                    "{message}; original data kept, but restart failed: {restart}"
                                ),
                            )
                            .await
                        }
                    });
                }
            };
            self.app_manager
                .stop_app_locked(&app_ref)
                .await
                .map_err(|error| format!("Stop failed before restore: {error}"))?;
            let swapped = match staged {
                StagedRestore::Full => restore_data_tree(&extract_dir, &data_root)
                    .map(|swap| (RestoreSwap::Tree(swap), None)),
                StagedRestore::Databases(databases) => swap_databases_off_runtime(
                    extract_dir.join(DATABASE_SNAPSHOT_ARCHIVE_ROOT),
                    databases,
                    data_root.clone(),
                    RESTORE_WORKER_TIMEOUT,
                )
                .await
                .map(|swap| {
                    let summary = swap.summary().clone();
                    (RestoreSwap::Databases(swap), Some(summary))
                }),
            };
            drop(closed);
            let (swap, databases) = match swapped {
                Ok(swapped) => swapped,
                // Never start the app on partially restored data.
                Err(SwapError {
                    message,
                    originals_intact: false,
                }) => return Err(format!("{message}; app left stopped")),
                Err(SwapError { message, .. }) => {
                    return Err(match restart().await {
                        Ok(()) => {
                            self.finish_restart(app, &app_ref, &config);
                            format!("{message}; original data kept and app restarted")
                        }
                        Err(restart) => {
                            self.leave_stopped(
                                app,
                                &app_ref,
                                format!(
                                    "{message}; original data kept, but restart failed: {restart}"
                                ),
                            )
                            .await
                        }
                    });
                }
            };

            if let Err(error) = restart().await {
                // Never swap files underneath a process whose stop failed:
                // give up and leave both copies for the operator.
                let closed = match self.workflows.close_store(app, WORKFLOW_STOP_TIMEOUT).await {
                    Ok(closed) => closed,
                    Err(stop) => {
                        let message = format!("{error}; could not stop restored workflows: {stop}");
                        let kept = swap.abandon(&message);
                        return Err(format!(
                            "{message}; restored data left live, original data kept in {}",
                            kept.display()
                        ));
                    }
                };
                if let Err(stop) = self.app_manager.stop_app_locked(&app_ref).await {
                    let message = format!("{error}; could not stop restored app: {stop}");
                    let kept = swap.abandon(&message);
                    return Err(format!(
                        "{message}; restored data left live, original data kept in {}",
                        kept.display()
                    ));
                }
                swap.rollback()
                    .map_err(|rollback| format!("{error}; {rollback}; app left stopped"))?;
                drop(closed);
                return Err(match restart().await {
                    Ok(()) => {
                        self.finish_restart(app, &app_ref, &config);
                        format!("{error}; original data restored and app restarted")
                    }
                    Err(restart) => {
                        self.leave_stopped(
                            app,
                            &app_ref,
                            format!(
                                "{error}; original data restored, but restart failed: {restart}"
                            ),
                        )
                        .await
                    }
                });
            }
            self.finish_restart(app, &app_ref, &config);
            if let Err(error) = swap.commit() {
                tracing::warn!(
                    app,
                    "Restore succeeded but previous data cleanup failed: {error}"
                );
            }

            Ok(BackupRestoreResponse {
                backup: info,
                databases,
            })
        }
        .await;

        let _ = tokio::fs::remove_dir_all(&work_dir).await;
        result
    }

    async fn restart_after_restore(
        &self,
        app: &str,
        app_ref: &std::sync::Arc<crate::instances::App>,
        config: &crate::instances::AppConfig,
        release_path: &std::path::Path,
        runtime_bin_path: Option<&str>,
    ) -> Result<(), String> {
        // Also repairs ownership and modes of restored files for the app user.
        ensure_app_runtime_data_dirs(&self.runtime.data_dir, app)?;
        if self.is_draining() {
            return Err("Server began draining during restore".to_string());
        }
        self.try_sync_app_workflows(app, release_path, runtime_bin_path)
            .await
            .map_err(|error| format!("Restart workflows after restore: {error}"))?;
        if config.min_instances > 0 {
            self.app_manager
                .start_app_locked(app_ref)
                .await
                .map_err(|error| format!("Restart failed after restore: {error}"))?;
        }
        Ok(())
    }

    fn finish_restart(
        &self,
        app: &str,
        app_ref: &crate::instances::App,
        config: &crate::instances::AppConfig,
    ) {
        if config.min_instances == 0 {
            app_ref.set_state(crate::socket::AppState::Idle);
        }
        self.cold_start.reset(app);
    }

    /// Stop whatever a failed restart left running. Cold starts stay blocked.
    async fn leave_stopped(
        &self,
        app: &str,
        app_ref: &std::sync::Arc<crate::instances::App>,
        error: String,
    ) -> String {
        self.workflows.stop(app, WORKFLOW_STOP_TIMEOUT).await;
        match self.app_manager.stop_app_locked(app_ref).await {
            Ok(()) => format!("{error}; app left stopped"),
            Err(stop) => format!("{error}; could not stop app: {stop}"),
        }
    }

    async fn find_backup(
        &self,
        backup: &tako_core::BackupBinding,
        app: &str,
        backup_id: &str,
    ) -> Result<BackupInfo, String> {
        let index = self.read_backup_index(backup, app).await?;
        index
            .backups
            .into_iter()
            .find(|backup| backup.id == backup_id)
            .ok_or_else(|| format!("Backup not found: {backup_id}"))
    }

    fn backup_operation_lock(&self, app: &str) -> std::sync::Arc<tokio::sync::Mutex<()>> {
        self.backup_locks
            .write()
            .entry(app.to_string())
            .or_default()
            .clone()
    }

    /// Take the app's backup lock without waiting.
    fn acquire_backup_lock(&self, app: &str) -> Result<tokio::sync::OwnedMutexGuard<()>, String> {
        self.backup_operation_lock(app)
            .try_lock_owned()
            .map_err(|_| format!("A backup operation is already in progress for {app}."))
    }

    /// Take the app's backup lock, or, while an hourly database snapshot
    /// holds it, queue a full archive for the scheduler to run as soon as
    /// the snapshot releases it. `Ok(None)` means queued.
    pub(super) fn acquire_backup_lock_or_queue(
        &self,
        app: &str,
    ) -> Result<Option<tokio::sync::OwnedMutexGuard<()>>, String> {
        let lock = self.backup_operation_lock(app);
        // Under the state map's lock a database snapshot holds the backup
        // lock exactly while it is marked running, and its scheduler task
        // checks the queue only after releasing both.
        let mut states = self.database_snapshots.write();
        if let Ok(guard) = lock.try_lock_owned() {
            return Ok(Some(guard));
        }
        let state = states.entry(app.to_string()).or_default();
        if !state.running {
            return Err(format!(
                "A backup operation is already in progress for {app}."
            ));
        }
        state.post_deploy_backup_queued = true;
        self.record_post_deploy_backup_queued(app, true);
        Ok(None)
    }
}

/// Who started a full archive, which decides how it takes the deploy lock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FullBackupStart {
    /// Waits for the deploy lock.
    Operator,
    /// Never waits, and yields the deploy lock to operator operations.
    Scheduled,
}

fn full_backup_preflight(data_bytes: u64, available_bytes: u64) -> Result<(), String> {
    let required = data_bytes
        .saturating_mul(FULL_BACKUP_DISK_COPIES)
        .saturating_add(DISK_RESERVE_BYTES);
    if available_bytes < required {
        return Err(format!(
            "Not enough free disk space for a full backup: needs {required} bytes, {available_bytes} available."
        ));
    }
    Ok(())
}

fn enabled_backup_status(
    app: &str,
    retention_days: u16,
    backups: &[BackupInfo],
    schedule: &DatabaseSnapshotState,
    now: i64,
) -> BackupStatusResponse {
    let latest_of = |kind| latest_backup(backups.iter().filter(|backup| backup.kind == kind));
    let last_backup = latest_of(BackupKind::Full).cloned();
    let last_backup_failure = schedule.failure().cloned();
    let next_backup_at_unix_secs = last_backup
        .as_ref()
        .map(|backup| {
            backup
                .created_at_unix_secs
                .saturating_add(BACKUP_INTERVAL_SECS)
        })
        .or(Some(now))
        .map(|due| {
            // A failed attempt is retried no earlier than its backoff allows.
            last_backup_failure
                .as_ref()
                .map_or(due, |failure| due.max(failure.retry_at_unix_secs))
        });
    BackupStatusResponse {
        app: app.to_string(),
        enabled: true,
        retention_days: Some(retention_days),
        last_backup,
        next_backup_at_unix_secs,
        last_database_snapshot_at_unix_secs: latest_of(BackupKind::Databases)
            .map(|backup| backup.created_at_unix_secs),
        last_database_snapshot_skip: schedule.skip.clone(),
        last_backup_failure,
    }
}

/// A downloaded and extracted backup, before it touches live data.
enum StagedRestore {
    Full,
    /// Relative paths of the extracted databases.
    Databases(Vec<std::path::PathBuf>),
}

/// Live data replaced by a restore, with the originals kept until restart.
enum RestoreSwap {
    Tree(RestoredData),
    Databases(DatabaseSwap),
}

impl RestoreSwap {
    fn commit(self) -> Result<(), String> {
        match self {
            Self::Tree(swap) => swap.commit(),
            Self::Databases(swap) => swap.commit(),
        }
    }

    fn rollback(self) -> Result<(), String> {
        match self {
            Self::Tree(swap) => swap.rollback(),
            Self::Databases(swap) => swap.rollback(),
        }
    }

    /// Give up while the server keeps running; returns where the originals are.
    fn abandon(self, reason: &str) -> std::path::PathBuf {
        match self {
            Self::Tree(swap) => swap.abandon(reason),
            Self::Databases(swap) => swap.abandon(reason),
        }
    }
}

/// A database snapshot restore holds the encrypted object and its decrypted
/// archive at once, each about the object's size, before extraction checks
/// its own budget.
fn restore_download_preflight(size_bytes: u64, available_bytes: u64) -> Result<(), String> {
    let required = size_bytes
        .saturating_mul(2)
        .saturating_add(DISK_RESERVE_BYTES);
    if available_bytes < required {
        return Err(format!(
            "Not enough free disk space to restore the database snapshot: needs {required} bytes, {available_bytes} available."
        ));
    }
    Ok(())
}

async fn create_private_work_dir(path: &std::path::Path) -> Result<(), String> {
    tokio::fs::DirBuilder::new()
        .mode(0o700)
        .create(path)
        .await
        .map_err(|e| format!("create backup work dir {}: {e}", path.display()))
}

fn validate_backup_binding(backup: &tako_core::BackupBinding) -> Result<(), String> {
    if backup.storage.provider != tako_core::StorageProvider::S3 {
        return Err("Backup storage must be S3-compatible.".to_string());
    }
    if backup.storage.public_base_url.is_some() {
        return Err("Backup storage must be private.".to_string());
    }
    require_s3_field(&backup.storage.bucket, "bucket")?;
    require_s3_field(&backup.storage.endpoint, "endpoint")?;
    require_s3_field(&backup.storage.region, "region")?;
    require_s3_field(&backup.storage.access_key_id, "access_key_id")?;
    require_s3_field(&backup.storage.secret_access_key, "secret_access_key")?;
    if backup.backup_keys.is_empty() {
        return Err("Backup encryption key is missing.".to_string());
    }
    for key in &backup.backup_keys {
        validate_backup_key(key)?;
    }
    Ok(())
}

fn require_s3_field(value: &Option<String>, field: &str) -> Result<(), String> {
    if value.as_deref().is_none_or(|value| value.trim().is_empty()) {
        return Err(format!("Backup storage is missing {field}."));
    }
    Ok(())
}

pub(super) fn now_unix_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, kind: BackupKind, created_at_unix_secs: i64) -> BackupInfo {
        serde_json::from_value(serde_json::json!({
            "id": id, "app": "demo", "environment": "production", "server": "local",
            "created_at_unix_secs": created_at_unix_secs, "size_bytes": 1, "sha256_hex": "",
            "archive_key": "", "manifest_key": "",
            "encryption": {"algorithm": "", "key_id": "", "nonce_base64": "", "tag_base64": ""},
            "kind": kind,
        }))
        .unwrap()
    }

    #[test]
    fn status_separates_full_archives_from_database_snapshots() {
        let skip = tako_core::BackupSkip {
            at_unix_secs: 400,
            reason: tako_core::BackupSkipReason::Unchanged,
            message: "unchanged".into(),
        };
        let backups = [
            entry("d2", BackupKind::Databases, 300),
            entry("b1", BackupKind::Full, 100),
            entry("d1", BackupKind::Databases, 200),
        ];
        let schedule = DatabaseSnapshotState {
            skip: Some(skip.clone()),
            ..Default::default()
        };
        let status = enabled_backup_status("demo/production", 30, &backups, &schedule, 500);
        assert_eq!(status.last_backup.unwrap().id, "b1");
        assert_eq!(
            status.next_backup_at_unix_secs,
            Some(100 + BACKUP_INTERVAL_SECS)
        );
        assert_eq!(status.last_database_snapshot_at_unix_secs, Some(300));
        assert_eq!(status.last_database_snapshot_skip, Some(skip));

        let status = enabled_backup_status(
            "demo/production",
            30,
            &backups[..1],
            &DatabaseSnapshotState::default(),
            500,
        );
        assert!(status.last_backup.is_none());
        assert_eq!(status.next_backup_at_unix_secs, Some(500));
        assert!(status.last_backup_failure.is_none());

        // A failing full archive reports why and when it is retried.
        let mut schedule = DatabaseSnapshotState::default();
        schedule.record_full_failure(450, BackupFailureReason::Failed, "upload timed out".into());
        let status = enabled_backup_status("demo/production", 30, &backups[..1], &schedule, 500);
        let failure = status.last_backup_failure.unwrap();
        assert_eq!(failure.message, "upload timed out");
        assert_eq!(
            status.next_backup_at_unix_secs,
            Some(failure.retry_at_unix_secs)
        );
    }

    #[test]
    fn full_backup_preflight_counts_three_copies_of_the_data() {
        let mib = 1024 * 1024;
        let needed = 3 * 100 * mib + DISK_RESERVE_BYTES;
        assert!(full_backup_preflight(100 * mib, needed).is_ok());
        let error = full_backup_preflight(100 * mib, needed - 1).unwrap_err();
        assert!(
            error.starts_with("Not enough free disk space for a full backup"),
            "{error}"
        );
    }

    #[test]
    fn measure_data_tree_counts_files_without_transient_stores() {
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("data");
        std::fs::create_dir_all(data.join("app/nested")).unwrap();
        std::fs::create_dir_all(data.join("tako")).unwrap();
        std::fs::write(data.join("app/nested/a"), vec![0; 100]).unwrap();
        std::fs::write(data.join("tako/workflows.sqlite"), vec![0; 20]).unwrap();
        std::fs::write(data.join("tako/channels.sqlite"), vec![0; 1000]).unwrap();
        std::os::unix::fs::symlink("/etc/hostname", data.join("app/link")).unwrap();
        let entry = snapshot::ARCHIVE_ENTRY_BYTES;
        let block = snapshot::DISK_BLOCK_BYTES;
        // Two files of a block each, the link, and three directories, each
        // with an archive entry.
        assert_eq!(
            snapshot::measure_data_tree(temp.path(), std::path::Path::new("data")).unwrap(),
            2 * (block + entry) + entry + 3 * (block + entry)
        );
    }

    /// Many small files cost far more than their bytes: a disk block for
    /// each copy and an archive header for each entry.
    #[test]
    fn measure_data_tree_counts_the_overhead_of_small_files() {
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("data");
        std::fs::create_dir_all(data.join("app")).unwrap();
        for index in 0..1000 {
            std::fs::write(data.join(format!("app/{index}")), b"x").unwrap();
        }
        let measured =
            snapshot::measure_data_tree(temp.path(), std::path::Path::new("data")).unwrap();
        assert!(measured >= 1000 * (4096 + 512), "{measured}");
    }

    #[tokio::test]
    async fn an_unreadable_index_runs_no_backup_and_status_says_why() {
        let mut fixture = RestoreFixture::new();
        let index_key = backup_index_key(&fixture.app, &fixture.state);
        fixture
            .objects
            .insert(index_key.clone(), b"{ not json".to_vec());
        // Only the scheduler's index read: no snapshot, no upload.
        let server = fixture.serve(1);
        fixture.state.run_due_backups().await;
        assert_eq!(server.await.unwrap(), [format!("GET {index_key}")]);
        let schedule = fixture.state.backup_schedule_state(&fixture.app);
        assert!(schedule.full_failures.failure().is_none());
        assert_eq!(
            schedule.failure().unwrap().reason,
            BackupFailureReason::IndexUnreadable
        );
        // The next tick does not read the index again before the backoff.
        fixture.state.run_due_backups().await;
        assert_eq!(
            fixture.state.backup_schedule_state(&fixture.app).failure(),
            schedule.failure()
        );

        let server = fixture.serve(1);
        let status = fixture
            .state
            .backup_status_inner(&fixture.app)
            .await
            .unwrap();
        server.await.unwrap();
        let failure = status.last_backup_failure.unwrap();
        assert_eq!(failure.reason, BackupFailureReason::IndexUnreadable);
        assert!(failure.message.starts_with("Failed to read backup index"));
        assert_eq!(
            failure.retry_at_unix_secs,
            schedule.failure().unwrap().retry_at_unix_secs
        );
    }

    #[test]
    fn database_restore_preflight_counts_the_object_and_its_archive() {
        let mib = 1024 * 1024;
        let needed = 2 * 100 * mib + DISK_RESERVE_BYTES;
        assert!(restore_download_preflight(100 * mib, needed).is_ok());
        let error = restore_download_preflight(100 * mib, needed - 1).unwrap_err();
        assert!(error.starts_with("Not enough free disk space"), "{error}");
    }

    const KEY_ID: &str = "backup-key-0123456789abcdef";
    const KEY: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";

    fn test_state(data_dir: &std::path::Path) -> std::sync::Arc<crate::ServerState> {
        std::sync::Arc::new(
            crate::ServerState::new(
                data_dir.into(),
                std::sync::Arc::new(crate::tls::CertManager::new(
                    crate::tls::CertManagerConfig {
                        cert_dir: data_dir.join("certs"),
                        ..Default::default()
                    },
                )),
                None,
                std::sync::Arc::new(parking_lot::RwLock::new(Default::default())),
            )
            .unwrap(),
        )
    }

    fn sqlite(path: &std::path::Path, value: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let db = rusqlite::Connection::open(path).unwrap();
        db.execute_batch("CREATE TABLE IF NOT EXISTS items(value TEXT); DELETE FROM items;")
            .unwrap();
        db.execute("INSERT INTO items VALUES (?1)", [value])
            .unwrap();
    }

    fn sqlite_value(path: &std::path::Path) -> String {
        rusqlite::Connection::open(path)
            .unwrap()
            .query_row("SELECT value FROM items", [], |row| row.get(0))
            .unwrap()
    }

    struct RestoreFixture {
        temp: crate::isolation::fixture::TestDataDir,
        state: std::sync::Arc<crate::ServerState>,
        app: String,
        data: std::path::PathBuf,
        info: BackupInfo,
        objects: std::collections::HashMap<String, Vec<u8>>,
    }

    impl RestoreFixture {
        /// A registered app whose data changed after a database snapshot.
        fn new() -> Self {
            let temp = crate::isolation::fixture::TestDataDir::new().unwrap();
            let state = test_state(temp.path());
            let config = crate::instances::AppConfig {
                name: "demo".into(),
                environment: "production".into(),
                version: "v1".into(),
                min_instances: 0,
                ..Default::default()
            };
            let app = config.deployment_id();
            let release = release_app_path(temp.path(), &config);
            std::fs::create_dir_all(&release).unwrap();
            std::fs::write(
                release.join("app.json"),
                r#"{"protocol_version":0,"runtime":"node","main":"index.js","idle_timeout":300,"start":["/bin/sh","-lc","sleep 600"]}"#,
            )
            .unwrap();
            state.app_manager.register_app(crate::instances::AppConfig {
                path: release,
                ..config.clone()
            });
            state.state_store.upsert_app(&config, &[]).unwrap();

            let data = app_runtime_data_paths(temp.path(), &app).root;
            sqlite(&data.join("app/data.sqlite"), "snapshot");
            sqlite(&data.join("tako/workflows.sqlite"), "snapshot runs");
            std::fs::write(data.join("app/upload.bin"), b"upload").unwrap();

            let work = tempfile::tempdir().unwrap();
            let snapshot = work.path().join("snapshot");
            snapshot_database_tree(&data, std::path::Path::new(""), &snapshot).unwrap();
            let archive = work.path().join("databases.tar.zst");
            snapshot::create_database_snapshot_archive(&snapshot, &archive).unwrap();
            let encrypted = work.path().join("databases.tar.zst.enc");
            let key = tako_core::BackupKeyBinding {
                id: KEY_ID.into(),
                key_base64: KEY.into(),
            };
            let encryption = encrypt_backup_file(&archive, &encrypted, &key).unwrap();
            let bytes = std::fs::read(&encrypted).unwrap();

            let (app_name, environment) = requested_deployment_identity(&app);
            let prefix =
                backup_object_prefix(&app_name, &environment, &state.backup_server_label());
            let id = "d1800000000-abcdefgh".to_string();
            let info = BackupInfo {
                archive_key: format!("{prefix}/{id}.databases.tar.zst.enc"),
                manifest_key: format!("{prefix}/{id}.json"),
                id,
                app: app_name,
                environment,
                server: state.backup_server_label(),
                created_at_unix_secs: 1_800_000_000,
                size_bytes: bytes.len() as u64,
                sha256_hex: sha256_file_hex(&encrypted).unwrap(),
                encryption,
                kind: BackupKind::Databases,
                database_digest: Some("unused".into()),
                databases: vec!["app/data.sqlite".into(), "tako/workflows.sqlite".into()],
            };
            let index = serde_json::to_vec(&serde_json::json!({
                "version": 1, "backups": [info],
            }))
            .unwrap();
            let objects = std::collections::HashMap::from([
                (backup_index_key(&app, &state), index),
                (info.archive_key.clone(), bytes),
            ]);

            // Newer live state than the snapshot.
            sqlite(&data.join("app/data.sqlite"), "live");
            std::fs::write(data.join("app/data.sqlite-wal"), b"newer wal").unwrap();
            std::fs::write(data.join("app/data.sqlite-shm"), b"newer shm").unwrap();
            sqlite(&data.join("tako/workflows.sqlite"), "live runs");
            sqlite(&data.join("app/other.sqlite"), "other");
            std::fs::write(data.join("app/upload.bin"), b"changed upload").unwrap();
            Self {
                temp,
                state,
                app,
                data,
                info,
                objects,
            }
        }

        fn serve(&self, requests: usize) -> tokio::task::JoinHandle<Vec<String>> {
            let (endpoint, fixture) = s3::tests::object_fixture(self.objects.clone(), requests);
            let backup = serde_json::from_value(serde_json::json!({
                "storage": {"provider": "s3", "bucket": "fixture", "endpoint": endpoint,
                    "region": "local", "access_key_id": "fixture", "secret_access_key": "fixture",
                    "force_path_style": true},
                "backup_keys": [{"id": KEY_ID, "key_base64": KEY}]
            }))
            .unwrap();
            self.state
                .state_store
                .set_backup(&self.app, Some(&backup))
                .unwrap();
            fixture
        }

        fn previous_dirs(&self) -> usize {
            let app_root = self.data.parent().unwrap();
            std::fs::read_dir(app_root)
                .unwrap()
                .filter_map(Result::ok)
                .filter(|entry| {
                    entry
                        .file_name()
                        .to_string_lossy()
                        .starts_with(".data-restore-prev-")
                })
                .count()
        }
    }

    #[tokio::test]
    async fn restoring_a_database_snapshot_replaces_only_its_databases() {
        let fixture = RestoreFixture::new();
        let server = fixture.serve(2);
        let restored = fixture
            .state
            .restore_backup_inner(&fixture.app, &fixture.info.id)
            .await
            .unwrap();
        assert_eq!(
            server.await.unwrap(),
            [
                format!("GET {}", backup_index_key(&fixture.app, &fixture.state)),
                format!("GET {}", fixture.info.archive_key),
            ]
        );
        assert_eq!(restored.backup, fixture.info);
        assert_eq!(
            restored.databases,
            Some(tako_core::RestoredDatabases {
                replaced: vec!["app/data.sqlite".into(), "tako/workflows.sqlite".into()],
                created: Vec::new(),
                untouched: vec!["app/other.sqlite".into()],
            })
        );

        let data = &fixture.data;
        assert_eq!(sqlite_value(&data.join("app/data.sqlite")), "snapshot");
        assert_eq!(
            sqlite_value(&data.join("tako/workflows.sqlite")),
            "snapshot runs"
        );
        assert!(!data.join("app/data.sqlite-wal").exists());
        assert!(!data.join("app/data.sqlite-shm").exists());
        assert_eq!(sqlite_value(&data.join("app/other.sqlite")), "other");
        assert_eq!(
            std::fs::read(data.join("app/upload.bin")).unwrap(),
            b"changed upload"
        );
        assert_eq!(fixture.previous_dirs(), 0);
        assert!(
            std::fs::read_dir(fixture.temp.path().join("tmp/restore"))
                .unwrap()
                .next()
                .is_none()
        );
    }

    #[tokio::test]
    async fn restore_stops_a_retired_workflow_runtime_before_swapping() {
        let fixture = RestoreFixture::new();
        let workflows = &fixture.state.workflows;
        let ready = fixture.temp.path().join("worker-ready");
        let spec = |command: &str| tako_workflows::WorkerSpec {
            app: fixture.app.clone(),
            workers: 0,
            concurrency: 1,
            idle_timeout_ms: 0,
            command: vec!["/bin/sh".into(), "-c".into(), command.into()],
            cwd: fixture.temp.path().into(),
            env: Default::default(),
            secrets: Default::default(),
            storages: Default::default(),
            log_sink: None,
            isolation: None,
            lanes: Vec::new(),
        };
        // A release whose worker ignores SIGTERM while it finishes a run.
        let stubborn = format!("trap '' TERM; touch {}; exec sleep 60", ready.display());
        workflows
            .ensure(&fixture.app, None, |_| spec(&stubborn))
            .await
            .unwrap();
        let retired = workflows.supervisor_for(&fixture.app).unwrap();
        retired.wake().unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while !ready.exists() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        workflows
            .ensure(&fixture.app, None, |_| spec("exec sleep 60"))
            .await
            .unwrap();

        let server = fixture.serve(2);
        fixture
            .state
            .restore_backup_inner(&fixture.app, &fixture.info.id)
            .await
            .unwrap();
        server.await.unwrap();
        assert!(
            !retired.is_running(),
            "the replaced release's worker outlived the restore"
        );
        assert_eq!(
            sqlite_value(&fixture.data.join("tako/workflows.sqlite")),
            "snapshot runs"
        );
    }

    #[tokio::test]
    async fn a_failed_restart_puts_the_original_databases_and_sidecars_back() {
        let fixture = RestoreFixture::new();
        let release = fixture
            .state
            .app_manager
            .get_app(&fixture.app)
            .unwrap()
            .config
            .read()
            .path
            .clone();
        std::fs::remove_file(release.join("app.json")).unwrap();
        let server = fixture.serve(2);
        let error = fixture
            .state
            .restore_backup_inner(&fixture.app, &fixture.info.id)
            .await
            .unwrap_err();
        server.await.unwrap();
        assert!(
            error.starts_with("Restart workflows after restore:")
                && error.contains("original data restored, but restart failed")
                && error.ends_with("app left stopped"),
            "{error}"
        );

        // Sidecars first: opening a database may discard a log it rejects.
        let data = &fixture.data;
        assert_eq!(
            std::fs::read(data.join("app/data.sqlite-wal")).unwrap(),
            b"newer wal"
        );
        assert_eq!(
            std::fs::read(data.join("app/data.sqlite-shm")).unwrap(),
            b"newer shm"
        );
        assert_eq!(sqlite_value(&data.join("app/data.sqlite")), "live");
        assert_eq!(
            sqlite_value(&data.join("tako/workflows.sqlite")),
            "live runs"
        );
        assert_eq!(sqlite_value(&data.join("app/other.sqlite")), "other");
        assert_eq!(fixture.previous_dirs(), 0);
    }

    #[tokio::test]
    async fn a_refused_database_restore_keeps_live_data_and_restarts_the_app() {
        let fixture = RestoreFixture::new();
        let data = &fixture.data;
        std::fs::remove_file(data.join("app/data.sqlite")).unwrap();
        std::fs::write(data.join("app/data.sqlite"), b"not a database").unwrap();
        let server = fixture.serve(2);
        let error = fixture
            .state
            .restore_backup_inner(&fixture.app, &fixture.info.id)
            .await
            .unwrap_err();
        server.await.unwrap();
        assert!(
            error.starts_with("Cannot restore app/data.sqlite:")
                && error.ends_with("original data kept and app restarted"),
            "{error}"
        );
        assert_eq!(
            std::fs::read(data.join("app/data.sqlite")).unwrap(),
            b"not a database"
        );
        assert_eq!(
            sqlite_value(&data.join("tako/workflows.sqlite")),
            "live runs"
        );
        let app = fixture.state.app_manager.get_app(&fixture.app).unwrap();
        assert_eq!(app.state(), crate::socket::AppState::Idle);
        assert_eq!(fixture.previous_dirs(), 0);
    }

    #[tokio::test]
    async fn database_snapshots_download_as_their_own_object() {
        let fixture = RestoreFixture::new();
        let server = fixture.serve(2);
        let download = fixture
            .state
            .backup_download_url_inner(&fixture.app, &fixture.info.id)
            .await
            .unwrap();
        assert_eq!(
            download.backup.archive_file_name().unwrap(),
            "d1800000000-abcdefgh.databases.tar.zst.enc"
        );
        let body = s3::backup_http_client(Duration::from_secs(5))
            .unwrap()
            .get(&download.url)
            .send()
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap();
        assert_eq!(body.as_ref(), fixture.objects[&fixture.info.archive_key]);
        server.await.unwrap();
    }

    /// Backups configured against storage that refuses connections, so a
    /// full backup runs its local steps and fails at the upload.
    fn unreachable_storage(fixture: &RestoreFixture) {
        let backup = serde_json::from_value(serde_json::json!({
            "storage": {"provider": "s3", "bucket": "fixture", "endpoint": "https://127.0.0.1:1",
                "region": "local", "access_key_id": "fixture", "secret_access_key": "fixture",
                "force_path_style": true},
            "backup_keys": [{"id": KEY_ID, "key_base64": KEY}]
        }))
        .unwrap();
        fixture
            .state
            .state_store
            .set_backup(&fixture.app, Some(&backup))
            .unwrap();
    }

    #[tokio::test]
    async fn post_deploy_backup_is_queued_behind_a_database_snapshot_without_waiting() {
        let fixture = RestoreFixture::new();
        unreachable_storage(&fixture);
        let (state, app) = (&fixture.state, fixture.app.as_str());

        let snapshot = state.lock_for_database_snapshot(app).unwrap();
        let outcome = tokio::time::timeout(Duration::from_secs(1), state.backup_after_deploy(app))
            .await
            .expect("finalize must not wait for a database snapshot");
        assert!(
            matches!(outcome, Some(PostDeployBackup::Queued)),
            "{outcome:?}"
        );
        assert!(state.backup_schedule_state(app).post_deploy_backup_queued);
        drop(snapshot);

        // The scheduler runs it once the snapshot released the lock; its
        // failure is visible in status like any other full archive.
        state.run_queued_post_deploy_backup(app).await;
        let schedule = state.backup_schedule_state(app);
        assert!(!schedule.post_deploy_backup_queued);
        let failure = schedule.failure().unwrap();
        assert_eq!(failure.reason, BackupFailureReason::Failed);
        // Nothing queued: nothing runs.
        state.run_queued_post_deploy_backup(app).await;
        assert_eq!(state.backup_schedule_state(app).failure(), Some(failure));

        // A queued backup that finds the lock taken again gives up visibly.
        let snapshot = state.lock_for_database_snapshot(app).unwrap();
        state.backup_after_deploy(app).await;
        let full = state.backup_operation_lock(app);
        drop(snapshot);
        let _held = full.try_lock_owned().unwrap();
        state.run_queued_post_deploy_backup(app).await;
        let failure = state.backup_schedule_state(app).failure().cloned().unwrap();
        assert!(
            failure.message.starts_with(
                "The post-deploy backup queued behind a database snapshot could not start"
            ),
            "{}",
            failure.message
        );
    }

    /// The deploy reported the backup as queued, so a restart before the
    /// snapshot finished must not drop it.
    #[tokio::test]
    async fn a_queued_post_deploy_backup_survives_a_restart() {
        let fixture = RestoreFixture::new();
        unreachable_storage(&fixture);
        let (state, app) = (&fixture.state, fixture.app.as_str());
        let snapshot = state.lock_for_database_snapshot(app).unwrap();
        assert!(matches!(
            state.backup_after_deploy(app).await,
            Some(PostDeployBackup::Queued)
        ));
        drop(snapshot);
        // A restart forgets everything in memory.
        state.database_snapshots.write().clear();
        state.run_queued_post_deploy_backup(app).await;
        let schedule = state.backup_schedule_state(app);
        assert_eq!(
            schedule.failure().map(|failure| failure.reason),
            Some(BackupFailureReason::Failed),
            "the queued backup did not run"
        );
        // It ran once; it is not queued again.
        state.database_snapshots.write().clear();
        state.run_queued_post_deploy_backup(app).await;
        assert!(state.backup_schedule_state(app).failure().is_none());
    }

    /// An app deleted and created again with the same name starts with a
    /// clean schedule.
    #[tokio::test]
    async fn deleting_an_app_forgets_its_backup_schedule() {
        let fixture = RestoreFixture::new();
        let (state, app) = (&fixture.state, fixture.app.as_str());
        {
            let mut states = state.database_snapshots.write();
            let schedule = states.entry(app.to_string()).or_default();
            schedule.post_deploy_backup_queued = true;
            schedule.record_full_failure(now_unix_secs(), BackupFailureReason::Failed, "x".into());
        }
        let response = state.delete_app(app).await;
        assert!(response.is_ok(), "{response:?}");
        let schedule = state.backup_schedule_state(app);
        assert!(!schedule.post_deploy_backup_queued);
        assert!(schedule.failure().is_none());
    }

    #[tokio::test]
    async fn post_deploy_backup_runs_at_once_when_the_lock_is_free_and_fails_fast_otherwise() {
        let fixture = RestoreFixture::new();
        unreachable_storage(&fixture);
        let (state, app) = (&fixture.state, fixture.app.as_str());

        // Free lock: the backup runs in finalize, as before.
        let outcome = state.backup_after_deploy(app).await;
        assert!(
            matches!(outcome, Some(PostDeployBackup::Failed(_))),
            "{outcome:?}"
        );
        assert!(!state.backup_schedule_state(app).post_deploy_backup_queued);
        assert!(state.backup_schedule_state(app).failure().is_some());

        // A full backup or restore holding the lock: busy, nothing queued.
        let full = state.backup_operation_lock(app).try_lock_owned().unwrap();
        let Some(PostDeployBackup::Failed(error)) = state.backup_after_deploy(app).await else {
            panic!("expected a busy error");
        };
        assert_eq!(
            error,
            "A backup operation is already in progress for demo/production."
        );
        assert!(!state.backup_schedule_state(app).post_deploy_backup_queued);
        assert!(state.lock_for_database_snapshot(app).is_none());
        drop(full);
    }

    /// A waiter that finds the backup lock taken reads the running flag to
    /// tell a database snapshot from a full backup or restore. The flag must
    /// never be raised for a snapshot attempt that cannot take the lock.
    #[test]
    fn a_database_snapshot_is_marked_running_only_while_it_holds_the_lock() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let _entered = runtime.enter();
        let state = test_state(temp.path());
        let app = "demo/production";
        let full = state.backup_operation_lock(app).try_lock_owned().unwrap();
        let done = std::sync::atomic::AtomicBool::new(false);
        let seen = std::thread::scope(|scope| {
            scope.spawn(|| {
                for _ in 0..20_000 {
                    assert!(state.lock_for_database_snapshot(app).is_none());
                }
                done.store(true, std::sync::atomic::Ordering::SeqCst);
            });
            let mut seen = false;
            while !done.load(std::sync::atomic::Ordering::SeqCst) {
                seen |= state.database_snapshot_running(app);
            }
            seen
        });
        assert!(!seen, "a waiter would wait behind a full backup or restore");
        drop(full);
    }

    #[tokio::test]
    async fn database_snapshots_skip_a_running_deploy_without_waiting() {
        let fixture = RestoreFixture::new();
        // Only the index is read before the deploy lock check.
        let server = fixture.serve(1);
        let deploy = fixture.state.get_deploy_lock(&fixture.app).await;
        let _deploying = deploy.lock().await;
        let outcome = tokio::time::timeout(
            Duration::from_secs(10),
            fixture.state.database_snapshot_now(&fixture.app),
        )
        .await
        .expect("a database snapshot must not wait for a deploy")
        .unwrap();
        assert!(
            matches!(
                &outcome,
                databases::DatabaseSnapshotOutcome::Skipped(skip)
                    if skip.reason == tako_core::BackupSkipReason::DeployInProgress
            ),
            "{outcome:?}"
        );
        assert!(!fixture.state.database_snapshot_running(&fixture.app));
        server.await.unwrap();
    }

    /// The deploy lock is free, no background holder is recorded, and native
    /// recovery's claim window is closed.
    async fn assert_deploy_lock_released(state: &crate::ServerState, app: &str) {
        let lock = state.get_deploy_lock(app).await;
        let held = lock.try_lock().expect("the deploy lock leaked");
        // An operator that finds the lock held by anyone but background work
        // is turned away at once, with the generic message.
        let error = state
            .lock_deploy_for_operator(app, &lock)
            .await
            .unwrap_err();
        assert!(error.starts_with("Deploy already in progress"), "{error}");
        drop(held);
        let app_ref = state.app_manager.get_app(app).unwrap();
        assert!(!*app_ref.recovery_claims_open.lock());
    }

    #[tokio::test]
    async fn scheduled_backups_yield_the_deploy_lock_to_an_operator_and_run_on_a_later_tick() {
        let fixture = RestoreFixture::new();
        let (state, app) = (&fixture.state, fixture.app.as_str());
        let index_key = backup_index_key(app, state);
        let update_secrets = || state.update_secrets(app, std::collections::HashMap::new());

        // An operator turned away by a scheduled backup opens its window.
        let held = state
            .try_lock_deploy_for_scheduled_backup(app, std::time::Instant::now())
            .await
            .unwrap();
        let response = update_secrets().await;
        assert!(
            matches!(&response, Response::Error { message, .. } if message.starts_with("A scheduled backup of")),
            "operator must receive a scheduled backup contention error"
        );
        drop(held);

        // While it is open, the hourly snapshot skips without waiting.
        let server = fixture.serve(1);
        let outcome = state.database_snapshot_now(app).await.unwrap();
        assert!(
            matches!(
                &outcome,
                databases::DatabaseSnapshotOutcome::Skipped(skip)
                    if skip.reason == tako_core::BackupSkipReason::DeployInProgress
            ),
            "{outcome:?}"
        );
        assert_eq!(server.await.unwrap(), [format!("GET {index_key}")]);

        // So do a due full archive and a queued post-deploy archive: neither
        // counts as a failure, and the queued one stays queued.
        state
            .database_snapshots
            .write()
            .entry(app.to_string())
            .or_default()
            .post_deploy_backup_queued = true;
        let server = fixture.serve(1);
        state.run_due_backups().await;
        assert_eq!(server.await.unwrap(), [format!("GET {index_key}")]);
        let schedule = state.backup_schedule_state(app);
        assert!(schedule.failure().is_none(), "{:?}", schedule.failure());
        assert!(schedule.post_deploy_backup_queued);
        assert_deploy_lock_released(state, app).await;

        // The operator's retry gets the lock and closes the window.
        let response = update_secrets().await;
        assert!(
            response.is_ok(),
            "operator retry must succeed after a backup"
        );

        // The next tick runs both archives. The fixture refuses uploads, so
        // each fails at its upload, and the lock is released all the same.
        let server = fixture.serve(5);
        state.run_due_backups().await;
        let served = server.await.unwrap();
        assert_eq!(served.len(), 5, "{served:?}");
        for archive in [&served[..3], &served[3..]] {
            assert!(archive.last().unwrap().starts_with("PUT "), "{served:?}");
        }
        let schedule = state.backup_schedule_state(app);
        assert_eq!(
            schedule.failure().unwrap().reason,
            BackupFailureReason::Failed
        );
        assert!(!schedule.post_deploy_backup_queued);
        assert_deploy_lock_released(state, app).await;

        // A database snapshot that fails after its capture leaks nothing either.
        let server = fixture.serve(3);
        let error = state.database_snapshot_now(app).await.unwrap_err();
        assert!(error.contains("returned 404"), "{error}");
        assert_eq!(server.await.unwrap().len(), 3);
        assert!(!state.database_snapshot_running(app));
        assert_deploy_lock_released(state, app).await;
    }
}
