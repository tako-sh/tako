use std::sync::Arc;

use object_store::{ObjectStore, ObjectStoreExt, PutMode, PutOptions, path::Path};
use tako_core::{StorageBinding, StorageProvider};

fn binding(force_path_style: bool) -> StorageBinding {
    StorageBinding {
        provider: StorageProvider::S3,
        bucket: Some("uploads".into()),
        endpoint: Some("https://s3.example.com/root".into()),
        region: Some("us-east-1".into()),
        access_key_id: Some("test-key".into()),
        secret_access_key: Some("test-secret".into()),
        force_path_style,
        public_base_url: None,
        path: None,
        signing_key: None,
    }
}

#[tokio::test]
async fn local_create_is_atomic_and_another_handle_reads_the_published_value() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(tako_storage::local(dir.path()).unwrap());
    let key = Path::from("app/segment.ltx");
    let options = PutOptions {
        mode: PutMode::Create,
        ..Default::default()
    };
    let (first, second) = tokio::join!(
        store.put_opts(&key, b"first".as_slice().into(), options.clone()),
        store.put_opts(&key, b"second".as_slice().into(), options),
    );
    assert_ne!(first.is_ok(), second.is_ok());
    let loser = if first.is_ok() { second } else { first };
    assert!(matches!(
        loser,
        Err(object_store::Error::AlreadyExists { .. })
    ));
    let reopened = tako_storage::local(dir.path()).unwrap();
    let value = reopened.get(&key).await.unwrap().bytes().await.unwrap();
    assert!(value == "first" || value == "second");
}

#[tokio::test]
async fn library_signer_preserves_bucket_style_and_encodes_object_keys() {
    use object_store::signer::Signer;
    for path_style in [true, false] {
        let store = tako_storage::s3(&binding(path_style)).unwrap();
        let url = store
            .signed_url(
                http::Method::GET,
                &Path::from("data/a b.tar.zst"),
                std::time::Duration::from_secs(900),
            )
            .await
            .unwrap();
        assert_eq!(
            url.host_str(),
            Some(if path_style {
                "s3.example.com"
            } else {
                "uploads.s3.example.com"
            })
        );
        assert_eq!(
            url.path(),
            if path_style {
                "/root/uploads/data/a%20b.tar.zst"
            } else {
                "/root/data/a%20b.tar.zst"
            }
        );
        assert!(
            url.query_pairs()
                .any(|(key, value)| key == "X-Amz-Algorithm" && value == "AWS4-HMAC-SHA256")
        );
    }
}

#[test]
fn missing_or_invalid_s3_configuration_never_falls_back_to_local_or_ambient_credentials() {
    let mut config = binding(true);
    config.secret_access_key = None;
    assert!(tako_storage::s3(&config).is_err());
    config = binding(true);
    config.endpoint = Some("http://s3.example.com".into());
    assert!(tako_storage::s3(&config).is_err());
}
