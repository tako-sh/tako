use std::path::Path;

use crate::object_storage::{
    S3Method, S3PresignOptions, presign_s3_url, presign_s3_url_with_options,
};

use super::{index::BackupIndex, now_unix_secs};

pub(super) const DOWNLOAD_URL_EXPIRES_SECONDS: u32 = 15 * 60;
pub(super) const S3_URL_EXPIRES_SECONDS: u32 = 60 * 60;
const JSON_CONTENT_TYPE: &str = "application/json";

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

async fn delete_object(client: &reqwest::Client, storage: &tako_core::StorageBinding, key: &str) {
    let Ok(url) = presign_s3_url(storage, key, S3Method::Delete, S3_URL_EXPIRES_SECONDS) else {
        return;
    };
    if let Err(error) = client.delete(url).send().await {
        tracing::debug!(key, "Failed to delete expired backup object: {error}");
    }
}

pub(super) async fn prune_retention(
    client: &reqwest::Client,
    storage: &tako_core::StorageBinding,
    index: &mut BackupIndex,
    retention_days: u16,
) {
    let cutoff = now_unix_secs().saturating_sub(i64::from(retention_days) * 24 * 60 * 60);
    let mut retained = Vec::with_capacity(index.backups.len());
    for backup in std::mem::take(&mut index.backups) {
        if backup.created_at_unix_secs < cutoff {
            delete_object(client, storage, &backup.archive_key).await;
            delete_object(client, storage, &backup.manifest_key).await;
        } else {
            retained.push(backup);
        }
    }
    index.backups = retained;
}
