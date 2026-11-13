#![cfg(feature = "http")]

use std::{collections::HashMap, fs};

use openssl::{hash::MessageDigest, pkey::PKey, sign::Signer};
use pingora_proxy::Session;
use tako_core::{StorageBinding, StorageProvider};
use tako_storage::local_http::LocalStorage;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn storage(dir: &std::path::Path, secret: &str) -> LocalStorage {
    LocalStorage {
        root: dir.join("private/storages/objects"),
        legacy_root: dir.join("app"),
        bindings: HashMap::from([(
            "uploads".into(),
            StorageBinding {
                provider: StorageProvider::Local,
                bucket: None,
                endpoint: None,
                region: None,
                access_key_id: None,
                secret_access_key: None,
                force_path_style: false,
                public_base_url: None,
                path: Some("storage/uploads".into()),
                signing_key: Some(secret.into()),
            },
        )]),
    }
}

fn signed(method: &str, key: &str, secret: &str, expires: u64) -> String {
    let key_material = PKey::hmac(secret.as_bytes()).unwrap();
    let mut signer = Signer::new(MessageDigest::sha256(), &key_material).unwrap();
    signer
        .update(format!("{method}\nuploads\n{key}\n{expires}").as_bytes())
        .unwrap();
    let token = hex::encode(signer.sign_to_vec().unwrap());
    format!("/_tako/storages/uploads/{key}?expires={expires}&token={token}")
}

async fn request(storage: &LocalStorage, method: &str, uri: &str, body: &[u8]) -> (u16, Vec<u8>) {
    let storage = storage.clone();
    let (mut client, stream) = tokio::io::duplex(64 * 1024);
    let task = tokio::spawn(async move {
        let mut session = Session::new_h1(Box::new(stream));
        assert!(session.read_request().await.unwrap());
        assert!(storage.serve(&mut session).await.unwrap());
        session.downstream_session.finish().await.unwrap();
    });
    client.write_all(format!("{method} {uri} HTTP/1.1\r\nHost: app.test\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).as_bytes()).await.unwrap();
    client.write_all(body).await.unwrap();
    let mut response = Vec::new();
    client.read_to_end(&mut response).await.unwrap();
    task.await.unwrap();
    let split = response.windows(4).position(|s| s == b"\r\n\r\n").unwrap();
    let header = std::str::from_utf8(&response[..split]).unwrap();
    (
        header.split_whitespace().nth(1).unwrap().parse().unwrap(),
        response[split + 4..].to_vec(),
    )
}

#[tokio::test]
async fn rust_handler_accepts_sdk_signed_urls_and_streams_large_uploads() {
    let dir = tempfile::tempdir().unwrap();
    let storage = storage(dir.path(), "test-signing-key");
    // Golden URLs generated independently by the JavaScript SDK's WebCrypto signing scheme.
    let put = "/_tako/storages/uploads/avatars/a%20b.txt?expires=4102444800&token=40502c9892751a32ecce2770acb19f5fea741f3288df8c9a425d4664c842b166";
    let get = "/_tako/storages/uploads/avatars/a%20b.txt?expires=4102444800&token=970ee037d59e09374aca4ce2cea30fe57b397bed8e40ecd921dc49496cf05eb9";
    let bytes = vec![42_u8; 2 * 1024 * 1024 + 7];
    assert_eq!(request(&storage, "PUT", put, &bytes).await.0, 204);
    let (status, restored) = request(&storage, "GET", get, &[]).await;
    assert_eq!(status, 200);
    assert_eq!(restored, bytes);
    assert_eq!(
        fs::read(storage.root.join("storage/uploads/avatars/a b.txt")).unwrap(),
        bytes
    );
    assert!(
        !storage
            .legacy_root
            .join("storage/uploads/avatars/a b.txt")
            .exists()
    );
}

#[tokio::test]
async fn failed_commit_returns_an_error_and_later_uploads_still_work() {
    let dir = tempfile::tempdir().unwrap();
    let storage = storage(dir.path(), "secret");
    let put = signed("PUT", "good", "secret", 4_102_444_800);
    assert_eq!(request(&storage, "PUT", &put, b"before").await.0, 204);
    fs::create_dir(storage.root.join("storage/uploads/blocked")).unwrap();
    let blocked = signed("PUT", "blocked", "secret", 4_102_444_800);
    assert_eq!(
        request(&storage, "PUT", &blocked, b"cannot replace a directory")
            .await
            .0,
        500
    );
    assert_eq!(request(&storage, "PUT", &put, b"after").await.0, 204);
    let get = signed("GET", "good", "secret", 4_102_444_800);
    assert_eq!(
        request(&storage, "GET", &get, &[]).await,
        (200, b"after".to_vec())
    );
}

#[tokio::test]
async fn authorization_rejects_wrong_method_key_app_and_expiry_before_storage_io() {
    let dir = tempfile::tempdir().unwrap();
    let storage = storage(dir.path(), "secret-a");
    let put = signed("PUT", "file", "secret-a", 4_102_444_800);
    assert_eq!(request(&storage, "GET", &put, &[]).await.0, 403);
    assert_eq!(
        request(&storage, "PUT", &put.replace("/file?", "/other?"), &[])
            .await
            .0,
        403
    );
    assert_eq!(
        request(
            &storage,
            "PUT",
            &signed("PUT", "file", "secret-b", 4_102_444_800),
            &[]
        )
        .await
        .0,
        403
    );
    assert_eq!(
        request(&storage, "PUT", &signed("PUT", "file", "secret-a", 1), &[])
            .await
            .0,
        403
    );
    assert!(!storage.root.exists());
}

#[tokio::test]
async fn migration_preserves_existing_objects_and_is_not_repeated_after_publication() {
    let dir = tempfile::tempdir().unwrap();
    let storage = storage(dir.path(), "secret");
    fs::create_dir_all(storage.legacy_root.join("storage/uploads/nested")).unwrap();
    fs::write(
        storage.legacy_root.join("storage/uploads/nested/file"),
        b"before",
    )
    .unwrap();
    let get = signed("GET", "nested/file", "secret", 4_102_444_800);
    assert_eq!(
        request(&storage, "GET", &get, &[]).await,
        (200, b"before".to_vec())
    );
    let put = signed("PUT", "nested/file", "secret", 4_102_444_800);
    assert_eq!(request(&storage, "PUT", &put, b"after").await.0, 204);
    assert_eq!(
        request(&storage, "GET", &get, &[]).await,
        (200, b"after".to_vec())
    );
    assert_eq!(
        fs::read(storage.legacy_root.join("storage/uploads/nested/file")).unwrap(),
        b"before"
    );
    // A different binding's migration cannot block reads of an already published one.
    let lock = fs::File::options()
        .read(true)
        .write(true)
        .open(
            storage
                .root
                .parent()
                .unwrap()
                .join("storage-migrations/migration.lock"),
        )
        .unwrap();
    lock.lock().unwrap();
    assert_eq!(
        request(&storage, "GET", &get, &[]).await,
        (200, b"after".to_vec())
    );
}

#[tokio::test]
async fn overlapping_binding_paths_import_missing_objects_without_replacing_new_writes() {
    let dir = tempfile::tempdir().unwrap();
    let mut storage = storage(dir.path(), "secret");
    fs::create_dir_all(storage.legacy_root.join("storage/uploads/nested")).unwrap();
    fs::write(
        storage.legacy_root.join("storage/uploads/nested/file"),
        "old",
    )
    .unwrap();
    fs::write(
        storage.legacy_root.join("storage/uploads/other"),
        "imported",
    )
    .unwrap();
    storage.bindings.get_mut("uploads").unwrap().path = Some("storage/uploads/nested".into());
    let put = signed("PUT", "file", "secret", 4_102_444_800);
    assert_eq!(request(&storage, "PUT", &put, b"new").await.0, 204);
    storage.bindings.get_mut("uploads").unwrap().path = Some("storage/uploads".into());
    assert_eq!(
        request(
            &storage,
            "GET",
            &signed("GET", "other", "secret", 4_102_444_800),
            &[]
        )
        .await,
        (200, b"imported".to_vec())
    );
    assert_eq!(
        request(
            &storage,
            "GET",
            &signed("GET", "nested/file", "secret", 4_102_444_800),
            &[]
        )
        .await,
        (200, b"new".to_vec())
    );
}

#[cfg(unix)]
#[tokio::test]
async fn migration_rejects_hardlinks_to_files_outside_the_app_tree() {
    let dir = tempfile::tempdir().unwrap();
    let storage = storage(dir.path(), "secret");
    let private = dir.path().join("private-file");
    fs::write(&private, b"private").unwrap();
    fs::create_dir_all(storage.legacy_root.join("storage/uploads")).unwrap();
    fs::hard_link(&private, storage.legacy_root.join("storage/uploads/linked")).unwrap();
    let uri = signed("GET", "linked", "secret", 4_102_444_800);
    assert_eq!(request(&storage, "GET", &uri, &[]).await.0, 503);
    assert!(!storage.root.join("storage/uploads/linked").exists());
    assert_eq!(fs::read(private).unwrap(), b"private");
}

#[cfg(unix)]
#[tokio::test]
async fn migration_rejects_symlink_escape_and_never_publishes_a_partial_tree() {
    let dir = tempfile::tempdir().unwrap();
    let storage = storage(dir.path(), "secret");
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("private"), "secret").unwrap();
    fs::create_dir_all(storage.legacy_root.join("storage/uploads")).unwrap();
    std::os::unix::fs::symlink(
        outside.path().join("private"),
        storage.legacy_root.join("storage/uploads/file"),
    )
    .unwrap();
    assert_eq!(
        request(
            &storage,
            "GET",
            &signed("GET", "file", "secret", 4_102_444_800),
            &[]
        )
        .await
        .0,
        503
    );
    assert!(!storage.root.join("storage/uploads").exists());
}

#[tokio::test]
async fn malformed_or_traversing_keys_are_rejected_before_opening_the_backend() {
    let dir = tempfile::tempdir().unwrap();
    let storage = storage(dir.path(), "secret");
    for key in [
        "%E0%A4%A/file",
        "%2e%2e/private",
        "nested/%2Fprivate",
        "nested/%00",
    ] {
        assert_eq!(
            request(
                &storage,
                "GET",
                &signed("GET", key, "secret", 4_102_444_800),
                &[]
            )
            .await
            .0,
            400
        );
    }
    assert!(!storage.root.exists());
}

#[tokio::test]
async fn incomplete_upload_does_not_replace_an_acknowledged_object() {
    let dir = tempfile::tempdir().unwrap();
    let storage = storage(dir.path(), "secret");
    let put = signed("PUT", "file", "secret", 4_102_444_800);
    assert_eq!(request(&storage, "PUT", &put, b"original").await.0, 204);
    let owned = storage.clone();
    let (mut client, stream) = tokio::io::duplex(64 * 1024);
    let task = tokio::spawn(async move {
        let mut session = Session::new_h1(Box::new(stream));
        session.read_request().await.unwrap();
        assert!(owned.serve(&mut session).await.is_err());
    });
    client
        .write_all(
            format!("PUT {put} HTTP/1.1\r\nHost: app.test\r\nContent-Length: 3000000\r\n\r\n")
                .as_bytes(),
        )
        .await
        .unwrap();
    client.write_all(&vec![17; 1_100_000]).await.unwrap();
    drop(client);
    task.await.unwrap();
    let get = signed("GET", "file", "secret", 4_102_444_800);
    assert_eq!(
        request(&storage, "GET", &get, &[]).await,
        (200, b"original".to_vec())
    );
}
