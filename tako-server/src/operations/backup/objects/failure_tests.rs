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
) -> (object_store::aws::AmazonS3, tokio::task::JoinHandle<()>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let fixture = tokio::task::spawn_blocking(move || {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        for status in statuses {
            let mut stream = loop {
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
            let mut bytes = Vec::new();
            while !bytes.windows(4).any(|value| value == b"\r\n\r\n") {
                let mut chunk = [0; 4096];
                let size = stream.read(&mut chunk).unwrap();
                assert_ne!(size, 0);
                bytes.extend_from_slice(&chunk[..size]);
            }
            // object_store uses S3's bulk-delete API even for one key.
            assert!(bytes.starts_with(b"POST "));
            let header_end = bytes
                .windows(4)
                .position(|part| part == b"\r\n\r\n")
                .unwrap()
                + 4;
            let header = std::str::from_utf8(&bytes[..header_end]).unwrap();
            let length: usize = header
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse().unwrap())
                })
                .unwrap();
            while bytes.len() < header_end + length {
                let mut chunk = [0; 4096];
                let size = stream.read(&mut chunk).unwrap();
                assert_ne!(size, 0);
                bytes.extend_from_slice(&chunk[..size]);
            }
            let request = std::str::from_utf8(&bytes[header_end..]).unwrap();
            let key = request
                .split_once("<Key>")
                .unwrap()
                .1
                .split_once("</Key>")
                .unwrap()
                .0;
            let (status, body) = if status == 204 {
                (
                    200,
                    format!("<DeleteResult><Deleted><Key>{key}</Key></Deleted></DeleteResult>"),
                )
            } else {
                (status, String::new())
            };
            write!(
                stream,
                "HTTP/1.1 {status} Fixture\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()
            )
            .unwrap();
        }
    });
    (fixture_store(&endpoint), fixture)
}

fn fixture_store(endpoint: &str) -> object_store::aws::AmazonS3 {
    object_store::aws::AmazonS3Builder::new()
        .with_endpoint(endpoint)
        .with_bucket_name("fixture")
        .with_region("local")
        .with_access_key_id("fixture")
        .with_secret_access_key("fixture")
        .with_retry(object_store::RetryConfig {
            max_retries: 0,
            ..Default::default()
        })
        .with_client_options(
            object_store::ClientOptions::new().with_timeout(Duration::from_secs(5)),
        )
        // Plain HTTP is confined to this loopback error fixture, never production config.
        .with_allow_http(true)
        .build()
        .unwrap()
}

#[tokio::test]
async fn retention_retries_failed_object_deletes() {
    for status in [403, 500] {
        let (storage, fixture) = storage_fixture(vec![status, status]);
        let mut index = old_index();
        queue_retention(&mut index, 1);
        prune_retention(&storage, &mut index).await;
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
        let (storage, fixture) = storage_fixture(vec![204, 404]);
        prune_retention(&storage, &mut index).await;
        fixture.await.unwrap();
        assert!(index.pending_deletions.is_empty());
    }
}

#[tokio::test]
async fn partial_delete_retains_cleanup_intent() {
    let (storage, fixture) = storage_fixture(vec![204, 500]);
    let mut index = old_index();
    queue_retention(&mut index, 1);
    prune_retention(&storage, &mut index).await;
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
    let store = fixture_store(&url);
    let error = with_deadline(Duration::from_millis(100), async {
        let response = store
            .get(&ObjectPath::from("stalled"))
            .await
            .map_err(|e| e.to_string())?;
        response.bytes().await.map_err(|e| e.to_string())
    })
    .await
    .unwrap_err();
    assert!(error.contains("timed out"), "{error}");
    fixture.abort();
}
