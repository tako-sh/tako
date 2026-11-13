use super::failure_tests::{
    accept, fixture_store_certificate, read_request, storage_binding, tls_listener,
};
use super::*;
use std::{io::Write, sync::mpsc};

async fn advance_clock(duration: Duration) {
    tokio::time::pause();
    tokio::time::advance(duration).await;
    tokio::time::resume();
}

async fn wait_for_length(path: &Path, length: u64) {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if tokio::fs::metadata(path)
                .await
                .is_ok_and(|metadata| metadata.len() >= length)
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("download did not make progress");
}

fn body_fixture(length: usize) -> (String, mpsc::Sender<Vec<u8>>, tokio::task::JoinHandle<()>) {
    let (listener, endpoint, acceptor) = tls_listener();
    let (sender, receiver) = mpsc::channel::<Vec<u8>>();
    let fixture = tokio::task::spawn_blocking(move || {
        let mut stream = accept(&listener, &acceptor);
        let request = read_request(&mut stream);
        assert!(request.starts_with(b"GET "));
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Length: {length}\r\nConnection: close\r\n\r\n"
        )
        .unwrap();
        while let Ok(bytes) = receiver.recv_timeout(Duration::from_secs(5)) {
            if stream
                .write_all(&bytes)
                .and_then(|_| stream.flush())
                .is_err()
            {
                break;
            }
        }
    });
    (endpoint, sender, fixture)
}

#[tokio::test]
async fn progressing_backup_download_can_exceed_one_minute() {
    let temp = tempfile::tempdir().unwrap();
    let output = temp.path().join("archive");
    let (endpoint, sender, fixture) = body_fixture(3);
    sender.send(vec![b'a']).unwrap();
    let store = backup_store(&storage_binding(&endpoint)).unwrap();
    let task_output = output.clone();
    let transfer =
        tokio::spawn(async move { download_object(&store, "archive", &task_output).await });
    wait_for_length(&output, 1).await;
    advance_clock(Duration::from_secs(40)).await;
    sender.send(vec![b'b']).unwrap();
    wait_for_length(&output, 2).await;
    advance_clock(Duration::from_secs(40)).await;
    sender.send(vec![b'c']).unwrap();
    let result = tokio::time::timeout(Duration::from_secs(3), transfer)
        .await
        .unwrap()
        .unwrap();
    drop(sender);
    fixture.await.unwrap();
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(tokio::fs::read(output).await.unwrap(), b"abc");
}

#[tokio::test]
async fn stalled_backup_download_stops_after_one_minute_without_progress() {
    let temp = tempfile::tempdir().unwrap();
    let output = temp.path().join("archive");
    let (endpoint, sender, fixture) = body_fixture(2);
    sender.send(vec![b'a']).unwrap();
    let store = backup_store(&storage_binding(&endpoint)).unwrap();
    let task_output = output.clone();
    let transfer =
        tokio::spawn(async move { download_object(&store, "archive", &task_output).await });
    wait_for_length(&output, 1).await;
    advance_clock(Duration::from_secs(61)).await;
    let result = tokio::time::timeout(Duration::from_secs(3), transfer)
        .await
        .unwrap()
        .unwrap();
    drop(sender);
    fixture.await.unwrap();
    assert!(result.is_err(), "stalled response ignored the read timeout");
}

#[tokio::test]
async fn timed_out_multipart_completion_aborts_the_known_upload() {
    let temp = tempfile::tempdir().unwrap();
    let input = temp.path().join("archive");
    tokio::fs::write(&input, vec![7_u8; 8 * 1024 * 1024])
        .await
        .unwrap();
    let (listener, endpoint, acceptor) = tls_listener();
    let (started, completing) = tokio::sync::oneshot::channel();
    let fixture = tokio::task::spawn_blocking(move || {
        let mut stream = accept(&listener, &acceptor);
        let request = read_request(&mut stream);
        assert!(
            request.starts_with(b"POST ") && String::from_utf8_lossy(&request).contains("uploads")
        );
        let body = "<InitiateMultipartUploadResult><UploadId>fixture-upload</UploadId></InitiateMultipartUploadResult>";
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
        drop(stream);
        let mut stream = accept(&listener, &acceptor);
        assert!(read_request(&mut stream).starts_with(b"PUT "));
        write!(stream, "HTTP/1.1 200 OK\r\nETag: \"part-one\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
        drop(stream);
        let mut completion = accept(&listener, &acceptor);
        assert!(read_request(&mut completion).starts_with(b"POST "));
        started.send(()).unwrap();
        let mut abort = accept(&listener, &acceptor);
        let request = read_request(&mut abort);
        assert!(
            request.starts_with(b"DELETE ")
                && String::from_utf8_lossy(&request).contains("uploadId=fixture-upload")
        );
        write!(
            abort,
            "HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        )
        .unwrap();
    });
    let store = tako_storage::s3_builder(&storage_binding(&endpoint))
        .unwrap()
        .with_client_options(
            object_store::ClientOptions::new()
                .with_timeout(TRANSFER_TIMEOUT * 2)
                .with_read_timeout(TRANSFER_TIMEOUT * 2)
                .with_root_certificate(fixture_store_certificate()),
        )
        .build()
        .unwrap();
    let transfer =
        tokio::spawn(async move { upload_file(Arc::new(store), "archive", &input).await });
    tokio::time::timeout(Duration::from_secs(3), completing)
        .await
        .unwrap()
        .unwrap();
    advance_clock(TRANSFER_TIMEOUT + Duration::from_secs(1)).await;
    let result = tokio::time::timeout(Duration::from_secs(3), transfer)
        .await
        .unwrap()
        .unwrap();
    assert!(result.is_err(), "upload exceeded its total deadline");
    tokio::time::timeout(Duration::from_secs(3), fixture)
        .await
        .expect("upload was not aborted")
        .unwrap();
}
