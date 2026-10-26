use super::*;
use tokio_test::io::Builder;

#[tokio::test]
async fn completed_upload_releases_buffer_before_response() {
    let body = vec![b'x'; 128 * 1024];
    let mock = Builder::new()
        .read(b"POST / HTTP/1.1\r\nHost: bench.test\r\nContent-Length: 131072\r\n\r\n")
        .read(&body)
        .build();
    let mut session = HttpSession::new(Box::new(mock));
    session.read_request().await.unwrap();
    let mut received = Vec::new();
    while let Some(bytes) = session.read_body_bytes().await.unwrap() {
        received.extend_from_slice(&bytes);
    }
    assert_eq!(received, body);
    assert!(
        session.body_reader.body_buf.is_none(),
        "completed upload still owns its read buffer"
    );
    assert!(
        session.response_written.is_none(),
        "test must inspect before response completes"
    );
}

#[tokio::test]
async fn chunked_completion_releases_buffer_and_preserves_pipeline() {
    let mock = Builder::new()
        .read(b"POST / HTTP/1.1\r\nHost: bench.test\r\nTransfer-Encoding: chunked\r\n\r\n")
        .read(b"3\r\nabc\r\n0\r\n\r\nGET /next HTTP/1.1\r\nHost: bench.test\r\n\r\n")
        .build();
    let mut session = HttpSession::new(Box::new(mock));
    session.read_request().await.unwrap();
    let mut received = Vec::new();
    while let Some(bytes) = session.read_body_bytes().await.unwrap() {
        received.extend_from_slice(&bytes);
    }
    assert_eq!(received, b"abc");
    assert!(session.body_reader.body_buf.is_none());
    assert_eq!(
        session.body_reader.take_body_overread().as_deref(),
        Some(&b"GET /next HTTP/1.1\r\nHost: bench.test\r\n\r\n"[..])
    );
}

#[tokio::test]
async fn content_length_completion_keeps_header_read_ahead() {
    let mock = Builder::new()
        .read(b"POST / HTTP/1.1\r\nHost: bench.test\r\nContent-Length: 3\r\n\r\nabcGET /next HTTP/1.1\r\nHost: bench.test\r\n\r\n").build();
    let mut session = HttpSession::new(Box::new(mock));
    session.read_request().await.unwrap();
    assert_eq!(
        session.read_body_bytes().await.unwrap().unwrap(),
        &b"abc"[..]
    );
    assert!(session.body_reader.body_buf.is_none());
    assert_eq!(
        session.body_reader.take_body_overread().as_deref(),
        Some(&b"GET /next HTTP/1.1\r\nHost: bench.test\r\n\r\n"[..])
    );
}
