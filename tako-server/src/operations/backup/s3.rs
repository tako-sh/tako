use std::path::Path;
use std::time::Duration;

use crate::object_storage::{
    S3Method, S3PresignOptions, presign_s3_url, presign_s3_url_with_options,
};

use super::{databases::DATABASE_SNAPSHOT_RETENTION_SECS, index::BackupIndex, now_unix_secs};

pub(super) const DOWNLOAD_URL_EXPIRES_SECONDS: u32 = 15 * 60;
pub(super) const S3_URL_EXPIRES_SECONDS: u32 = 60 * 60;
const JSON_CONTENT_TYPE: &str = "application/json";
pub(super) const TRANSFER_TIMEOUT: Duration = Duration::from_secs(30 * 60);
pub(super) const METADATA_TIMEOUT: Duration = Duration::from_secs(30);

pub(super) fn backup_http_client(timeout: Duration) -> Result<reqwest::Client, String> {
    let builder = reqwest::Client::builder();
    // Storage fixtures in tests serve a self-signed certificate.
    #[cfg(test)]
    let builder = builder.danger_accept_invalid_certs(true);
    builder
        .connect_timeout(Duration::from_secs(10))
        .read_timeout(Duration::from_secs(60))
        .timeout(timeout)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|error| format!("build backup HTTP client: {error}"))
}

#[cfg(test)]
pub(super) mod tests;

pub(super) async fn upload_file(
    client: &reqwest::Client,
    storage: &tako_core::StorageBinding,
    key: &str,
    path: &Path,
) -> Result<(), String> {
    let url = presign_s3_url(storage, key, S3Method::Put, S3_URL_EXPIRES_SECONDS)?;
    let file = tokio::fs::File::open(path)
        .await
        .map_err(|e| format!("open upload file {}: {e}", path.display()))?;
    let response = client
        .put(url)
        .body(reqwest::Body::from(file))
        .send()
        .await
        .map_err(|e| format!("upload backup object {key}: {e}"))?;
    if response.status().is_success() {
        Ok(())
    } else {
        Err(format!(
            "upload backup object {key} returned {}",
            response.status()
        ))
    }
}

pub(super) async fn put_json_object<T: serde::Serialize>(
    client: &reqwest::Client,
    storage: &tako_core::StorageBinding,
    key: &str,
    value: &T,
) -> Result<(), String> {
    let body = serde_json::to_vec(value).map_err(|e| format!("serialize backup JSON: {e}"))?;
    let headers = [("content-type", JSON_CONTENT_TYPE)];
    let url = presign_s3_url_with_options(
        storage,
        key,
        S3Method::Put,
        S3_URL_EXPIRES_SECONDS,
        S3PresignOptions {
            headers: &headers,
            ..Default::default()
        },
    )?;
    let response = client
        .put(url)
        .header("content-type", JSON_CONTENT_TYPE)
        .timeout(METADATA_TIMEOUT)
        .body(body)
        .send()
        .await
        .map_err(|e| format!("upload backup JSON {key}: {e}"))?;
    if response.status().is_success() {
        Ok(())
    } else {
        Err(format!(
            "upload backup JSON {key} returned {}",
            response.status()
        ))
    }
}

pub(super) async fn download_object(
    client: &reqwest::Client,
    storage: &tako_core::StorageBinding,
    key: &str,
    path: &Path,
) -> Result<(), String> {
    let url = presign_s3_url(storage, key, S3Method::Get, S3_URL_EXPIRES_SECONDS)?;
    let mut response = client
        .get(url)
        .send()
        .await
        .map_err(|e| format!("download backup object {key}: {e}"))?;
    if !response.status().is_success() {
        return Err(format!(
            "download backup object {key} returned {}",
            response.status()
        ));
    }
    let mut file = tokio::fs::File::create(path)
        .await
        .map_err(|e| format!("create download file {}: {e}", path.display()))?;
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| format!("read backup object {key}: {e}"))?
    {
        tokio::io::AsyncWriteExt::write_all(&mut file, &chunk)
            .await
            .map_err(|e| format!("write download file {}: {e}", path.display()))?;
    }
    tokio::io::AsyncWriteExt::shutdown(&mut file)
        .await
        .map_err(|e| format!("flush download file {}: {e}", path.display()))
}

async fn delete_object(
    client: &reqwest::Client,
    storage: &tako_core::StorageBinding,
    key: &str,
) -> Result<(), String> {
    let url = presign_s3_url(storage, key, S3Method::Delete, S3_URL_EXPIRES_SECONDS)?;
    let response = client
        .delete(url)
        .timeout(METADATA_TIMEOUT)
        .send()
        .await
        .map_err(|error| format!("delete expired backup object {key}: {error}"))?;
    if response.status().is_success() || response.status() == reqwest::StatusCode::NOT_FOUND {
        Ok(())
    } else {
        Err(format!(
            "delete expired backup object {key} returned {}",
            response.status()
        ))
    }
}

pub(super) fn queue_retention(index: &mut BackupIndex, retention_days: u16) {
    queue_retention_at(index, retention_days, now_unix_secs());
}

/// Full archives follow the configured retention; database snapshots only
/// bridge the gap between full archives and expire after a fixed window.
pub(super) fn queue_retention_at(index: &mut BackupIndex, retention_days: u16, now: i64) {
    let full_cutoff = now.saturating_sub(i64::from(retention_days) * 24 * 60 * 60);
    let database_cutoff = now.saturating_sub(DATABASE_SNAPSHOT_RETENTION_SECS);
    index.backups.retain(|backup| {
        let cutoff = match backup.kind {
            tako_core::BackupKind::Full => full_cutoff,
            tako_core::BackupKind::Databases => database_cutoff,
        };
        if backup.created_at_unix_secs < cutoff {
            index.pending_deletions.push(backup.clone());
            false
        } else {
            true
        }
    });
}

pub(super) async fn prune_retention(
    client: &reqwest::Client,
    storage: &tako_core::StorageBinding,
    index: &mut BackupIndex,
) {
    let mut pending = Vec::new();
    for backup in std::mem::take(&mut index.pending_deletions) {
        let archive = delete_object(client, storage, &backup.archive_key).await;
        let manifest = delete_object(client, storage, &backup.manifest_key).await;
        if let Err(error) = archive.and(manifest) {
            tracing::warn!(backup = %backup.id, "Retention cleanup will retry: {error}");
            pending.push(backup);
        }
    }
    index.pending_deletions = pending;
}
