use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tako_core::{
    BACKUP_INTERVAL_SECS, BackupDownloadUrlResponse, BackupInfo, BackupListResponse,
    BackupStatusResponse, Response,
};

use crate::object_storage::{S3Method, presign_s3_url};
use crate::release::{
    app_runtime_data_paths, ensure_app_runtime_data_dirs, release_app_path,
    requested_deployment_identity, resolve_release_runtime_bin,
};

mod encryption;
mod index;
mod s3;
mod scheduler;
mod snapshot;

use encryption::{
    active_backup_key, decrypt_backup_file, encrypt_backup_file, find_backup_key,
    validate_backup_key,
};
use index::{backup_index_key, backup_object_prefix, latest_backup};
use s3::{
    DOWNLOAD_URL_EXPIRES_SECONDS, S3_URL_EXPIRES_SECONDS, download_object, prune_retention,
    put_json_object, upload_file,
};
use snapshot::{create_backup_archive, restore_data_tree, sha256_file_hex, snapshot_data_tree};

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
            Ok(info) => Response::ok(info),
            Err(error) => Response::error(error),
        }
    }

    async fn backup_app_now(&self, app: &str) -> Result<BackupInfo, String> {
        let backup = self
            .state_store
            .get_backup(app)
            .map_err(|error| format!("read backup config: {error}"))?
            .ok_or_else(|| format!("Backups are not configured for {app}."))?;
        validate_backup_binding(&backup)?;

        let data_paths = ensure_app_runtime_data_dirs(&self.runtime.data_dir, app)?;
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

        let result: Result<BackupInfo, String> = async {
            let archive_result = tokio::task::spawn_blocking({
                let data_root = data_paths.root.clone();
                let snapshot_dir = snapshot_dir.clone();
                let archive_path = archive_path.clone();
                let encrypted_archive_path = encrypted_archive_path.clone();
                let backup_key = backup_key.clone();
                move || {
                    snapshot_data_tree(&data_root, &snapshot_dir)?;
                    create_backup_archive(&snapshot_dir, &archive_path)?;
                    let encryption =
                        encrypt_backup_file(&archive_path, &encrypted_archive_path, &backup_key)?;
                    let sha256_hex = sha256_file_hex(&encrypted_archive_path)?;
                    Ok::<_, String>((sha256_hex, encryption))
                }
            })
            .await
            .map_err(|e| format!("backup worker failed: {e}"))?;
            let (sha256_hex, encryption) = archive_result?;
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
            };

            let client = reqwest::Client::new();
            let mut index = self.read_backup_index(&backup, app).await?;
            upload_file(
                &client,
                &backup.storage,
                &info.archive_key,
                &encrypted_archive_path,
            )
            .await?;
            put_json_object(&client, &backup.storage, &info.manifest_key, &info).await?;

            index.backups.retain(|existing| existing.id != info.id);
            index.backups.push(info.clone());
            index.backups.sort_by(|a, b| {
                b.created_at_unix_secs
                    .cmp(&a.created_at_unix_secs)
                    .then_with(|| b.id.cmp(&a.id))
            });
            prune_retention(&client, &backup.storage, &mut index, backup.retention_days).await;
            put_json_object(
                &client,
                &backup.storage,
                &backup_index_key(app, self),
                &index,
            )
            .await?;

            Ok(info)
        }
        .await;

        let _ = tokio::fs::remove_dir_all(&work_dir).await;
        result
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
            });
        };
        let index = self.read_backup_index(&backup, app).await?;
        let last_backup = latest_backup(index.backups.iter()).cloned();
        let next_backup_at_unix_secs = last_backup
            .as_ref()
            .map(|backup| {
                backup
                    .created_at_unix_secs
                    .saturating_add(BACKUP_INTERVAL_SECS)
            })
            .or_else(|| Some(now_unix_secs()));
        Ok(BackupStatusResponse {
            app: app.to_string(),
            enabled: true,
            retention_days: Some(backup.retention_days),
            last_backup,
            next_backup_at_unix_secs,
        })
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

    async fn restore_backup_inner(&self, app: &str, backup_id: &str) -> Result<BackupInfo, String> {
        let backup = self
            .state_store
            .get_backup(app)
            .map_err(|error| format!("read backup config: {error}"))?
            .ok_or_else(|| format!("Backups are not configured for {app}."))?;
        let info = self.find_backup(&backup, app, backup_id).await?;

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
        tokio::fs::create_dir_all(&work_dir)
            .await
            .map_err(|e| format!("create restore work dir {}: {e}", work_dir.display()))?;

        let result: Result<BackupInfo, String> = async {
            let client = reqwest::Client::new();
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

            crate::extract_zstd_archive(&archive_path, &extract_dir)?;
            let data_root = app_runtime_data_paths(&self.runtime.data_dir, app).root;
            self.workflows.stop(app, Duration::from_secs(120)).await;
            self.app_manager
                .stop_app(app)
                .await
                .map_err(|error| format!("Stop failed before restore: {error}"))?;
            restore_data_tree(&extract_dir, &data_root)?;
            ensure_app_runtime_data_dirs(&self.runtime.data_dir, app)?;

            let release_path = release_app_path(&self.runtime.data_dir, &config);
            let runtime_bin_path =
                resolve_release_runtime_bin(&release_path, &self.runtime.data_dir)
                    .await
                    .ok()
                    .flatten();
            self.sync_app_workflows(app, &release_path, runtime_bin_path.as_deref())
                .await;
            if config.min_instances > 0 {
                self.app_manager
                    .start_app(app)
                    .await
                    .map_err(|error| format!("Restart failed after restore: {error}"))?;
                if let Some(app_ref) = self.app_manager.get_app(app) {
                    app_ref.set_state(crate::socket::AppState::Running);
                }
            } else if let Some(app_ref) = self.app_manager.get_app(app) {
                app_ref.set_state(crate::socket::AppState::Idle);
                self.cold_start.reset(app);
            }

            Ok(info)
        }
        .await;

        let _ = tokio::fs::remove_dir_all(&work_dir).await;
        result
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
