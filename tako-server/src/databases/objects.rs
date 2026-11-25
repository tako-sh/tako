//! Object storage transfers shared by hosted database backups.

use std::{future::Future, path::Path, sync::Arc, time::Duration};

use futures_util::TryStreamExt;
use object_store::{
    Attribute, AttributeValue, Attributes, MultipartUpload, ObjectStore, ObjectStoreExt,
    PutOptions, path::Path as ObjectPath,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const TRANSFER_TIMEOUT: Duration = Duration::from_secs(30 * 60);
pub(crate) const METADATA_TIMEOUT: Duration = Duration::from_secs(30);

pub(crate) fn backup_store(
    binding: &tako_core::StorageBinding,
) -> Result<object_store::aws::AmazonS3, String> {
    let options = object_store::ClientOptions::new()
        .with_timeout(TRANSFER_TIMEOUT)
        .with_read_timeout(Duration::from_secs(60))
        .with_connect_timeout(Duration::from_secs(5));
    #[cfg(test)]
    let options = options.with_root_certificate(failure_tests::fixture_store_certificate());
    tako_storage::s3_builder(binding)?
        .with_client_options(options)
        .build()
        .map_err(|e| format!("configure backup storage: {e}"))
}

pub(crate) async fn with_deadline<T>(
    timeout: Duration,
    operation: impl Future<Output = Result<T, String>>,
) -> Result<T, String> {
    tokio::time::timeout(timeout, operation)
        .await
        .map_err(|_| "backup object request timed out".to_string())?
}

pub(crate) async fn upload_file(
    store: Arc<dyn ObjectStore>,
    key: &str,
    path: &Path,
) -> Result<(), String> {
    let mut file = tokio::fs::File::open(path)
        .await
        .map_err(|e| format!("open backup upload: {e}"))?;
    let key = ObjectPath::parse(key).map_err(|e| e.to_string())?;
    const PART_BYTES: u64 = 8 * 1024 * 1024;
    let mut upload: Option<Box<dyn MultipartUpload>> = None;
    let result = with_deadline(TRANSFER_TIMEOUT, async {
        let mut part = Vec::new();
        loop {
            (&mut file)
                .take(PART_BYTES)
                .read_to_end(&mut part)
                .await
                .map_err(|e| format!("read backup upload: {e}"))?;
            match &mut upload {
                None if part.len() < PART_BYTES as usize => {
                    store
                        .put(&key, part.into())
                        .await
                        .map_err(|e| format!("upload backup object: {e}"))?;
                    return Ok(());
                }
                None => {
                    upload = Some(
                        store
                            .put_multipart(&key)
                            .await
                            .map_err(|e| format!("start backup upload: {e}"))?,
                    );
                }
                Some(_) => {}
            }
            let pending = upload.as_mut().ok_or("backup upload missing")?;
            if part.is_empty() {
                pending
                    .complete()
                    .await
                    .map_err(|e| format!("commit backup object: {e}"))?;
                return Ok(());
            }
            pending
                .put_part(part.into())
                .await
                .map_err(|e| format!("upload backup part: {e}"))?;
            part = Vec::new();
        }
    })
    .await;
    if result.is_err()
        && let Some(mut upload) = upload
    {
        // Retain ownership across the deadline, including a cancelled complete().
        let _ = with_deadline(METADATA_TIMEOUT, async {
            upload.abort().await.map_err(|e| e.to_string())
        })
        .await;
    }
    result
}

pub(crate) async fn put_json_object<T: serde::Serialize>(
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

pub(crate) async fn download_object(
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

#[cfg(test)]
mod failure_tests;
#[cfg(test)]
mod timeout_tests;

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
