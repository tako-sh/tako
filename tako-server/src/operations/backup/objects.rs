use std::{future::Future, path::Path, sync::Arc, time::Duration};

use futures_util::TryStreamExt;
use object_store::{
    Attribute, AttributeValue, Attributes, ObjectStore, ObjectStoreExt, PutOptions,
    buffered::BufWriter, path::Path as ObjectPath,
};
use tokio::io::AsyncWriteExt;

use super::{index::BackupIndex, now_unix_secs};

pub(super) const DOWNLOAD_URL_EXPIRES_SECONDS: u32 = 15 * 60;
const TRANSFER_TIMEOUT: Duration = Duration::from_secs(30 * 60);
pub(super) const METADATA_TIMEOUT: Duration = Duration::from_secs(30);

pub(super) async fn with_deadline<T>(
    timeout: Duration,
    operation: impl Future<Output = Result<T, String>>,
) -> Result<T, String> {
    tokio::time::timeout(timeout, operation)
        .await
        .map_err(|_| "backup object request timed out".to_string())?
}

pub(super) async fn upload_file(
    store: Arc<dyn ObjectStore>,
    key: &str,
    path: &Path,
) -> Result<(), String> {
    let mut file = tokio::fs::File::open(path)
        .await
        .map_err(|e| format!("open backup upload: {e}"))?;
    let key = ObjectPath::parse(key).map_err(|e| e.to_string())?;
    let mut writer = BufWriter::with_capacity(store, key, 8 * 1024 * 1024).with_max_concurrency(1);
    let result = with_deadline(TRANSFER_TIMEOUT, async {
        tokio::io::copy(&mut file, &mut writer)
            .await
            .map_err(|e| format!("upload backup object: {e}"))?;
        writer
            .shutdown()
            .await
            .map_err(|e| format!("commit backup object: {e}"))
    })
    .await;
    if result.is_err() {
        let _ = writer.abort().await;
    }
    result
}

pub(super) async fn put_json_object<T: serde::Serialize>(
    store: &dyn ObjectStore,
    key: &str,
    value: &T,
) -> Result<(), String> {
    let body = serde_json::to_vec(value).map_err(|e| format!("serialize backup JSON: {e}"))?;
    let key = ObjectPath::parse(key).map_err(|e| e.to_string())?;
    let options = PutOptions {
        attributes: Attributes::from_iter([(
            Attribute::ContentType,
            AttributeValue::from("application/json"),
        )]),
        ..Default::default()
    };
    with_deadline(METADATA_TIMEOUT, async {
        store
            .put_opts(&key, body.into(), options)
            .await
            .map_err(|e| format!("upload backup JSON: {e}"))?;
        Ok(())
    })
    .await
}

pub(super) async fn download_object(
    store: &dyn ObjectStore,
    key: &str,
    path: &Path,
) -> Result<(), String> {
    with_deadline(TRANSFER_TIMEOUT, async {
        let key = ObjectPath::parse(key).map_err(|e| e.to_string())?;
        let mut stream = store
            .get(&key)
            .await
            .map_err(|e| format!("download backup object: {e}"))?
            .into_stream();
        let mut file = tokio::fs::File::create(path)
            .await
            .map_err(|e| format!("create backup download: {e}"))?;
        while let Some(chunk) = stream
            .try_next()
            .await
            .map_err(|e| format!("read backup object: {e}"))?
        {
            file.write_all(&chunk)
                .await
                .map_err(|e| format!("write backup download: {e}"))?;
        }
        file.shutdown()
            .await
            .map_err(|e| format!("flush backup download: {e}"))
    })
    .await
}

pub(super) fn queue_retention(index: &mut BackupIndex, retention_days: u16) {
    let cutoff = now_unix_secs().saturating_sub(i64::from(retention_days) * 24 * 60 * 60);
    index.backups.retain(|backup| {
        if backup.created_at_unix_secs < cutoff {
            index.pending_deletions.push(backup.clone());
            false
        } else {
            true
        }
    });
}

pub(super) async fn prune_retention(store: &dyn ObjectStore, index: &mut BackupIndex) {
    let mut pending = Vec::new();
    for backup in std::mem::take(&mut index.pending_deletions) {
        let mut deleted = true;
        for key in [&backup.archive_key, &backup.manifest_key] {
            let result = with_deadline(METADATA_TIMEOUT, async {
                let key = ObjectPath::parse(key).map_err(|e| e.to_string())?;
                match store.delete(&key).await {
                    Ok(()) | Err(object_store::Error::NotFound { .. }) => Ok(()),
                    Err(error) => Err(error.to_string()),
                }
            })
            .await;
            if let Err(error) = result {
                tracing::warn!(backup = %backup.id, %error, "Retention cleanup will retry");
                deleted = false;
            }
        }
        if !deleted {
            pending.push(backup);
        }
    }
    index.pending_deletions = pending;
}

#[cfg(test)]
mod failure_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn library_backend_round_trips_a_multipart_sized_backup_and_json() {
        let dir = tempfile::tempdir().unwrap();
        let store: Arc<dyn ObjectStore> =
            Arc::new(tako_storage::local(&dir.path().join("objects")).unwrap());
        let input = dir.path().join("archive");
        let expected = vec![37_u8; 9 * 1024 * 1024 + 13];
        tokio::fs::write(&input, &expected).await.unwrap();
        upload_file(Arc::clone(&store), "app/archive.enc", &input)
            .await
            .unwrap();
        let restored = dir.path().join("download");
        download_object(store.as_ref(), "app/archive.enc", &restored)
            .await
            .unwrap();
        assert_eq!(tokio::fs::read(restored).await.unwrap(), expected);
        let value = serde_json::json!({"version": 1, "backups": []});
        let json_store = object_store::memory::InMemory::new();
        put_json_object(&json_store, "app/index.json", &value)
            .await
            .unwrap();
        let object = json_store
            .get(&ObjectPath::from("app/index.json"))
            .await
            .unwrap();
        assert_eq!(
            object.attributes.get(&Attribute::ContentType),
            Some(&"application/json".into())
        );
        let bytes = object.bytes().await.unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&bytes).unwrap(),
            value
        );
    }
}
