use super::*;
use std::io::{Read, Write};
use std::time::Duration;

fn old_index() -> BackupIndex {
    serde_json::from_value(serde_json::json!({
        "version": 1, "backups": [{
            "id": "old", "app": "demo", "environment": "test", "server": "local",
            "created_at_unix_secs": 1, "size_bytes": 1, "sha256_hex": "unused",
            "archive_key": "old.enc", "manifest_key": "old.json",
            "encryption": {"algorithm":"unused", "key_id":"unused", "nonce_base64":"", "tag_base64":""}
        }]
    })).unwrap()
}

fn storage_fixture(
    statuses: Vec<u16>,
) -> (
    tako_core::StorageBinding,
    reqwest::Client,
    tokio::task::JoinHandle<()>,
) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let endpoint = format!("https://{}", listener.local_addr().unwrap());
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let mut acceptor =
        openssl::ssl::SslAcceptor::mozilla_intermediate(openssl::ssl::SslMethod::tls()).unwrap();
    acceptor
        .set_certificate(&openssl::x509::X509::from_pem(cert.cert.pem().as_bytes()).unwrap())
        .unwrap();
    acceptor
        .set_private_key(
            &openssl::pkey::PKey::private_key_from_pem(cert.signing_key.serialize_pem().as_bytes())
                .unwrap(),
        )
        .unwrap();
    let acceptor = acceptor.build();
    let fixture = tokio::task::spawn_blocking(move || {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        for status in statuses {
            let stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(
                            std::time::Instant::now() < deadline,
                            "storage fixture timed out"
                        );
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("accept: {error}"),
                }
            };
            stream.set_nonblocking(false).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut stream = acceptor.accept(stream).unwrap();
            let mut bytes = Vec::new();
            while !bytes.windows(4).any(|value| value == b"\r\n\r\n") {
                let mut chunk = [0; 4096];
                let size = stream.read(&mut chunk).unwrap();
                assert_ne!(size, 0);
                bytes.extend_from_slice(&chunk[..size]);
            }
            assert!(bytes.starts_with(b"DELETE "));
            write!(
                stream,
                "HTTP/1.1 {status} Fixture\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            )
            .unwrap();
        }
    });
    let storage = serde_json::from_value(serde_json::json!({
        "provider": "s3", "bucket": "fixture", "endpoint": endpoint,
        "region": "local", "access_key_id": "fixture", "secret_access_key": "fixture",
        "force_path_style": true
    }))
    .unwrap();
    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .no_proxy()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    (storage, client, fixture)
}

#[tokio::test]
async fn retention_retries_failed_object_deletes() {
    for status in [403, 500] {
        let (storage, client, fixture) = storage_fixture(vec![status, status]);
        let mut index = old_index();
        queue_retention(&mut index, 1);
        prune_retention(&client, &storage, &mut index).await;
        fixture.await.unwrap();
        assert!(
            index.backups.is_empty(),
            "partially deleted backups must not be offered for restore"
        );
        assert_eq!(
            index.pending_deletions.len(),
            1,
            "failed deletes must remain tracked"
        );

        // Retry from a persisted index, including an already-removed object.
        let mut index: BackupIndex =
            serde_json::from_slice(&serde_json::to_vec(&index).unwrap()).unwrap();
        let (storage, client, fixture) = storage_fixture(vec![204, 404]);
        prune_retention(&client, &storage, &mut index).await;
        fixture.await.unwrap();
        assert!(index.pending_deletions.is_empty());
    }
}

#[tokio::test]
async fn partial_delete_retains_cleanup_intent() {
    let (storage, client, fixture) = storage_fixture(vec![204, 500]);
    let mut index = old_index();
    queue_retention(&mut index, 1);
    prune_retention(&client, &storage, &mut index).await;
    fixture.await.unwrap();
    assert!(index.backups.is_empty());
    assert_eq!(index.pending_deletions.len(), 1);
}

#[tokio::test]
async fn backup_request_deadline_covers_a_stalled_response_body() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let fixture = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        while !request.ends_with(b"\r\n\r\n") {
            let mut byte = [0];
            stream.read_exact(&mut byte).await.unwrap();
            request.push(byte[0]);
        }
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 20\r\n\r\npartial")
            .await
            .unwrap();
        let _ = stream.read(&mut [0]).await;
    });
    let client = backup_http_client(Duration::from_millis(100)).unwrap();
    let response = client.get(url).send().await.unwrap();
    let error = tokio::time::timeout(Duration::from_secs(2), response.bytes())
        .await
        .unwrap()
        .unwrap_err();
    assert!(error.is_timeout());
    fixture.abort();
}

#[test]
fn retention_expires_database_snapshots_after_48_hours_only() {
    let now = 1_800_000_000;
    let hour = 60 * 60;
    let entry = |id: &str, kind: &str, age: i64| {
        serde_json::json!({
            "id": id, "app": "demo", "environment": "test", "server": "local",
            "created_at_unix_secs": now - age, "size_bytes": 1, "sha256_hex": "unused",
            "archive_key": format!("{id}.enc"), "manifest_key": format!("{id}.json"),
            "encryption": {"algorithm":"unused", "key_id":"unused", "nonce_base64":"", "tag_base64":""},
            "kind": kind
        })
    };
    let mut index: BackupIndex = serde_json::from_value(serde_json::json!({
        "version": 1, "backups": [
            entry("db-fresh", "databases", 47 * hour),
            entry("db-expired", "databases", 49 * hour),
            entry("full-3d", "full", 72 * hour),
            entry("full-expired", "full", 31 * 24 * hour),
        ]
    }))
    .unwrap();
    queue_retention_at(&mut index, 30, now);
    let ids = |entries: &[tako_core::BackupInfo]| {
        entries.iter().map(|b| b.id.clone()).collect::<Vec<_>>()
    };
    assert_eq!(ids(&index.backups), ["db-fresh", "full-3d"]);
    assert_eq!(
        ids(&index.pending_deletions),
        ["db-expired", "full-expired"]
    );
}
