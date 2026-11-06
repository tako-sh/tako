use super::*;
use hyper::{Method, body::Frame};
use std::{
    pin::Pin,
    sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    task::{Context, Poll},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::{mpsc, oneshot},
};

struct StreamBody {
    frames: mpsc::Receiver<io::Result<Frame<Bytes>>>,
    dropped: Arc<AtomicUsize>,
}

impl Body for StreamBody {
    type Data = Bytes;
    type Error = io::Error;
    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, io::Error>>> {
        self.frames.poll_recv(cx)
    }
}

impl Drop for StreamBody {
    fn drop(&mut self) {
        self.dropped.fetch_add(1, Ordering::SeqCst);
    }
}

fn stream_body() -> (
    mpsc::Sender<io::Result<Frame<Bytes>>>,
    StreamBody,
    Arc<AtomicUsize>,
) {
    let (sender, frames) = mpsc::channel(1);
    let dropped = Arc::new(AtomicUsize::new(0));
    (
        sender,
        StreamBody {
            frames,
            dropped: dropped.clone(),
        },
        dropped,
    )
}

fn request<B>(body: B) -> Request<B> {
    Request::builder()
        .method(Method::POST)
        .uri("/v1/management/rpc")
        .header("host", "node.tako.internal")
        .header("x-tako-network-authorization", "original-signed-envelope")
        .body(body)
        .unwrap()
}

#[tokio::test]
async fn relay_streams_before_upload_eof_and_dispatches_once_with_original_request() {
    timeout(Duration::from_secs(3), async {
        let (client, server) = tokio::io::duplex(1024);
        let (frames, body, dropped) = stream_body();
        let (seen, first_frame) = oneshot::channel();
        let seen = Mutex::new(Some(seen));
        let calls = AtomicUsize::new(0);
        let primary = serve_connection(
            server,
            |mut incoming| {
                let seen = seen.lock().unwrap().take().unwrap();
                calls.fetch_add(1, Ordering::SeqCst);
                async move {
                    assert_eq!(incoming.method(), Method::PATCH);
                    assert_eq!(
                        incoming.uri().path_and_query().unwrap().as_str(),
                        "/v1/management/rpc?unchanged=%2F"
                    );
                    assert_eq!(
                        incoming.headers()["x-tako-network-authorization"],
                        "original-signed-envelope"
                    );
                    assert_eq!(incoming.headers().get_all("x-repeated").iter().count(), 2);
                    let first = incoming
                        .body_mut()
                        .frame()
                        .await
                        .unwrap()
                        .unwrap()
                        .into_data()
                        .unwrap();
                    assert_eq!(first, "first");
                    seen.send(()).unwrap();
                    let rest = incoming.into_body().collect().await.unwrap().to_bytes();
                    assert_eq!(rest.len(), 128 * 8192);
                    assert!(rest.iter().all(|byte| *byte == 7));
                    Response::builder()
                        .status(StatusCode::CREATED)
                        .header("x-primary", "selected")
                        .body(Full::new(Bytes::from_static(b"recorded")))
                        .unwrap()
                }
            },
            EXCHANGE_TIME,
        );
        let producer = async {
            frames
                .send(Ok(Frame::data(Bytes::from_static(b"first"))))
                .await
                .unwrap();
            first_frame.await.unwrap();
            for _ in 0..128 {
                frames
                    .send(Ok(Frame::data(Bytes::from(vec![7; 8192]))))
                    .await
                    .unwrap();
            }
            drop(frames);
        };
        let mut command = request(body);
        *command.method_mut() = Method::PATCH;
        *command.uri_mut() = "/v1/management/rpc?unchanged=%2F".parse().unwrap();
        command
            .headers_mut()
            .append("x-repeated", "one".parse().unwrap());
        command
            .headers_mut()
            .append("x-repeated", "two".parse().unwrap());
        let (_, response, _) =
            tokio::join!(primary, relay_io(client, command, EXCHANGE_TIME), producer);
        assert_eq!(response.status(), StatusCode::CREATED);
        assert_eq!(response.headers()["x-primary"], "selected");
        assert_eq!(
            response.into_body().collect().await.unwrap().to_bytes(),
            "recorded"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn early_rejection_drops_an_unfinished_upload_without_waiting_for_eof() {
    timeout(Duration::from_secs(2), async {
        let (client, server) = tokio::io::duplex(1024);
        let (frames, body, dropped) = stream_body();
        let primary = serve_connection(
            server,
            |request| async {
                drop(request);
                error(StatusCode::UNAUTHORIZED, "denied")
            },
            EXCHANGE_TIME,
        );
        let (_, response) = tokio::join!(primary, relay_io(client, request(body), EXCHANGE_TIME));
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert!(frames.is_closed());
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn cancelling_relay_drops_driver_stream_and_upload() {
    let (client, mut peer) = tokio::io::duplex(1024);
    let (frames, body, dropped) = stream_body();
    let relay = tokio::spawn(relay_io(client, request(body), EXCHANGE_TIME));
    let mut header = [0; 1024];
    assert!(
        timeout(Duration::from_secs(1), peer.read(&mut header))
            .await
            .unwrap()
            .unwrap()
            > 0
    );
    relay.abort();
    assert!(relay.await.unwrap_err().is_cancelled());
    assert!(frames.is_closed());
    assert_eq!(dropped.load(Ordering::SeqCst), 1);
    let mut rest = Vec::new();
    timeout(Duration::from_secs(1), peer.read_to_end(&mut rest))
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn exchange_deadline_returns_unconfirmed_and_releases_stream() {
    let (client, mut peer) = tokio::io::duplex(1024);
    let response = relay_io(
        client,
        request(Full::new(Bytes::new())),
        Duration::from_millis(20),
    )
    .await;
    assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
    let text = response.into_body().collect().await.unwrap().to_bytes();
    assert!(std::str::from_utf8(&text).unwrap().contains("unconfirmed"));
    let mut received = Vec::new();
    timeout(Duration::from_secs(1), peer.read_to_end(&mut received))
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn oversized_request_headers_fail_before_writing_or_polling_the_body() {
    let (client, mut peer) = tokio::io::duplex(1024);
    let (frames, body, dropped) = stream_body();
    let mut command = request(body);
    command
        .headers_mut()
        .insert("x-large", "x".repeat(MAX_HEADER_BYTES).parse().unwrap());
    let response = relay_io(client, command, EXCHANGE_TIME).await;
    assert_eq!(
        response.status(),
        StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE
    );
    assert_eq!(peer.read(&mut [0; 1]).await.unwrap(), 0);
    assert!(frames.is_closed());
    assert_eq!(dropped.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn response_length_and_streamed_bytes_are_bounded() {
    for chunked in [false, true] {
        timeout(Duration::from_secs(3), async {
            let (client, mut peer) = tokio::io::duplex(4096);
            let fake_primary = async {
                let mut head = Vec::new();
                while !head.ends_with(b"\r\n\r\n") {
                    head.push(peer.read_u8().await.unwrap());
                }
                if chunked {
                    peer.write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n")
                        .await
                        .unwrap();
                    let chunk = vec![9; 64 * 1024];
                    for _ in 0..=MAX_RESPONSE_BYTES / chunk.len() {
                        if peer.write_all(b"10000\r\n").await.is_err()
                            || peer.write_all(&chunk).await.is_err()
                            || peer.write_all(b"\r\n").await.is_err()
                        {
                            break;
                        }
                    }
                } else {
                    peer.write_all(
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n",
                            MAX_RESPONSE_BYTES + 1
                        )
                        .as_bytes(),
                    )
                    .await
                    .unwrap();
                    let mut rest = Vec::new();
                    peer.read_to_end(&mut rest).await.unwrap();
                }
            };
            let (response, _) = tokio::join!(
                relay_io(client, request(Full::new(Bytes::new())), EXCHANGE_TIME),
                fake_primary
            );
            assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        })
        .await
        .unwrap();
    }
}

#[tokio::test]
async fn primary_rejects_oversized_headers_and_never_pipelines_dispatch() {
    for oversized in [false, true] {
        timeout(Duration::from_secs(2), async {
            let (mut peer, server) = tokio::io::duplex(4096);
            let calls = AtomicUsize::new(0);
            let primary = serve_connection(server, |_| {
                calls.fetch_add(1, Ordering::SeqCst);
                async { error(StatusCode::OK, "one response") }
            }, EXCHANGE_TIME);
            let caller = async {
                let raw = if oversized {
                    format!("GET /v1/management/rpc HTTP/1.1\r\nHost: node.tako.internal\r\nX-Large: {}\r\n\r\n", "x".repeat(MAX_HEADER_BYTES))
                } else {
                    "GET /v1/management/rpc HTTP/1.1\r\nHost: node.tako.internal\r\n\r\n".repeat(2)
                };
                let _ = peer.write_all(raw.as_bytes()).await;
                let mut response = Vec::new();
                peer.read_to_end(&mut response).await.unwrap();
                response
            };
            let (_, response) = tokio::join!(primary, caller);
            assert_eq!(calls.load(Ordering::SeqCst), usize::from(!oversized));
            assert_eq!(String::from_utf8_lossy(&response).matches("HTTP/1.1").count(), 1);
        }).await.unwrap();
    }
}

#[tokio::test]
async fn listener_task_set_bounds_admission_and_owns_cancellation() {
    struct Dropped(Arc<AtomicUsize>);
    impl Drop for Dropped {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    let dropped = Arc::new(AtomicUsize::new(0));
    let mut tasks = JoinSet::new();
    for n in 0..=MAX_CONNECTIONS {
        let owned = Dropped(dropped.clone());
        assert_eq!(
            spawn_connection(&mut tasks, async move {
                let _owned = owned;
                std::future::pending::<()>().await;
            }),
            n < MAX_CONNECTIONS
        );
    }
    assert_eq!(tasks.len(), MAX_CONNECTIONS);
    assert_eq!(dropped.load(Ordering::SeqCst), 1);
    tasks.shutdown().await;
    assert_eq!(dropped.load(Ordering::SeqCst), MAX_CONNECTIONS + 1);
    assert!(tasks.is_empty());
}

#[test]
fn old_owner_cleanup_keeps_a_replacement_and_cleans_its_own_inode() {
    let temp = crate::isolation::fixture::private_tempdir();
    let path = temp.path().join(SOCKET);
    fs::write(&path, b"old inode").unwrap();
    let old = SocketOwner {
        directory: Directory::open(temp.path(), true).unwrap(),
        identity: FileIdentity::from(&fs::symlink_metadata(&path).unwrap()),
        path: path.clone(),
    };
    let replacement = temp.path().join("replacement");
    fs::write(&replacement, b"new inode").unwrap();
    let lock = old.directory.lock(PUBLICATION_LOCK).unwrap();
    fs::rename(&replacement, &path).unwrap();
    let new = SocketOwner {
        directory: Directory::open(temp.path(), true).unwrap(),
        identity: FileIdentity::from(&fs::symlink_metadata(&path).unwrap()),
        path: path.clone(),
    };
    drop(lock);
    drop(old);
    assert_eq!(fs::read(&path).unwrap(), b"new inode");
    drop(new);
    assert!(!path.exists());
}

#[test]
fn cleanup_never_unlinks_while_another_publisher_holds_the_short_lock() {
    let temp = crate::isolation::fixture::private_tempdir();
    let path = temp.path().join(SOCKET);
    fs::write(&path, b"owned").unwrap();
    let owner = SocketOwner {
        directory: Directory::open(temp.path(), true).unwrap(),
        identity: FileIdentity::from(&fs::symlink_metadata(&path).unwrap()),
        path: path.clone(),
    };
    let lock = owner.directory.lock(PUBLICATION_LOCK).unwrap();
    drop(owner);
    assert!(path.exists());
    drop(lock);
}

#[test]
fn credentials_allow_only_the_installed_service_uid_or_root_and_paths_require_sockets() {
    assert!(allowed_uid(0, 1234));
    assert!(allowed_uid(1234, 1234));
    assert!(!allowed_uid(1235, 1234));
    let temp = crate::isolation::fixture::private_tempdir();
    let path = temp.path().join(SOCKET);
    fs::write(&path, b"not a socket").unwrap();
    assert!(check_socket(&path, unsafe { libc::geteuid() }).is_err());
    fs::remove_file(&path).unwrap();
    std::os::unix::fs::symlink(temp.path(), &path).unwrap();
    assert!(check_socket(&path, unsafe { libc::geteuid() }).is_err());
}
