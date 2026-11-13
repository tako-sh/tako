use super::*;
use openssl::{hash::MessageDigest, pkey::PKey, sign::Signer};
use pingora_proxy::Session;
use tako_storage::local_http::LocalStorage;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn storage(root: &Path) -> LocalStorage {
    LocalStorage {
        root: root.join("tako/storages/objects"),
        legacy_root: root.join("app"),
        bindings: std::collections::HashMap::from([(
            "uploads".into(),
            serde_json::from_value(serde_json::json!({
                "provider": "local", "path": "storage/uploads", "signing_key": "secret"
            }))
            .unwrap(),
        )]),
    }
}

async fn request(
    storage: &LocalStorage,
    method: &str,
    encoded_key: &str,
    body: &[u8],
) -> (u16, Vec<u8>) {
    let key = PKey::hmac(b"secret").unwrap();
    let mut signer = Signer::new(MessageDigest::sha256(), &key).unwrap();
    signer
        .update(format!("{method}\nuploads\n{encoded_key}\n4102444800").as_bytes())
        .unwrap();
    let token = hex::encode(signer.sign_to_vec().unwrap());
    let storage = storage.clone();
    let (mut client, stream) = tokio::io::duplex(8192);
    let server = tokio::spawn(async move {
        let mut session = Session::new_h1(Box::new(stream));
        session.read_request().await.unwrap();
        assert!(storage.serve(&mut session).await.unwrap());
        session.downstream_session.finish().await.unwrap();
    });
    client.write_all(format!("{method} /_tako/storages/uploads/{encoded_key}?expires=4102444800&token={token} HTTP/1.1\r\nHost: app.test\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).as_bytes()).await.unwrap();
    client.write_all(body).await.unwrap();
    let mut response = Vec::new();
    client.read_to_end(&mut response).await.unwrap();
    server.await.unwrap();
    let end = response
        .windows(4)
        .position(|part| part == b"\r\n\r\n")
        .unwrap();
    let header = std::str::from_utf8(&response[..end]).unwrap();
    (
        header.split_whitespace().nth(1).unwrap().parse().unwrap(),
        response[end + 4..].to_vec(),
    )
}

#[tokio::test]
async fn migrated_reserved_keys_keep_private_updates_after_archive_restore() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("data");
    let original = storage(&source);
    let keys = [
        ("report#123", "report%23123"),
        ("report%23123", "report%2523123"),
        ("folder#123%/file#9", "folder%23123%25/file%239"),
        (r"slash\name#1", "slash%5Cname%231"),
    ];
    for (index, (key, _)) in keys.iter().enumerate() {
        let path = original.legacy_root.join("storage/uploads").join(key);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, format!("legacy-{index}")).unwrap();
    }
    for (index, (_, key)) in keys.iter().enumerate() {
        assert_eq!(
            request(&original, "GET", key, &[]).await,
            (200, format!("legacy-{index}").into_bytes())
        );
        assert_eq!(
            request(&original, "PUT", key, format!("private-{index}").as_bytes())
                .await
                .0,
            204
        );
    }

    let snapshot = temp.path().join("snapshot");
    snapshot_data_tree(&source, Path::new(""), &snapshot).unwrap();
    let archive = temp.path().join("backup.tar.zst");
    create_backup_archive(&snapshot, &archive).unwrap();
    let extracted = temp.path().join("extracted");
    crate::extract_zstd_archive(&archive, &extracted).unwrap();
    let restored = temp.path().join("restored");
    restore_data_tree(&extracted, &restored)
        .unwrap()
        .commit()
        .unwrap();
    std::fs::remove_dir_all(&source).unwrap();
    std::fs::remove_dir_all(restored.join("app/storage")).unwrap();
    let restored = storage(&restored);
    for (index, (_, key)) in keys.iter().enumerate() {
        assert_eq!(
            request(&restored, "GET", key, &[]).await,
            (200, format!("private-{index}").into_bytes())
        );
    }
}
