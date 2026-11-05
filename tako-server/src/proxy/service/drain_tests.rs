//! Include from service/mod.rs with `#[cfg(test)] mod drain_tests;`.
use super::*;
use crate::instances::AppManager;
use crate::lb::LoadBalancer;
use crate::proxy::{CloudflareIpRanges, ProxyConfig};
use crate::routing::RouteTable;
use crate::scaling::{ColdStartConfig, ColdStartManager};
use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::atomic::Ordering;
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};

fn proxy(path: std::path::PathBuf) -> TakoProxy {
    let config = ProxyConfig {
        redirect_http_to_https: false,
        ..Default::default()
    };
    TakoProxy::new(
        Arc::new(LoadBalancer::new(Arc::new(AppManager::new(path)))),
        Arc::new(parking_lot::RwLock::new(RouteTable::default())),
        config,
        Arc::new(ColdStartManager::new(ColdStartConfig::default())),
        CloudflareIpRanges::default(),
    )
}

async fn downstream(capacity: usize) -> (DuplexStream, Session) {
    let (mut client, stream) = tokio::io::duplex(capacity);
    let mut session = Session::new_h1(Box::new(stream));
    let (written, read) = tokio::join!(
        client.write_all(b"GET / HTTP/1.1\r\nHost: unknown.example\r\n\r\n"),
        session.read_request(),
    );
    written.unwrap();
    assert!(read.unwrap());
    (client, session)
}

fn descriptor() -> OwnedFd {
    // No live socket permission is required. The registry records shutdown attempts even when
    // this deliberate non-socket returns ENOTSOCK; accounting must still await context Drop.
    std::fs::File::open("/dev/null").unwrap().into()
}

#[tokio::test]
async fn direct_response_and_logging_retain_guard_until_context_drops() {
    let temp = tempfile::tempdir().unwrap();
    let proxy = proxy(temp.path().into());
    let (_client, mut session) = downstream(8192).await;
    let mut ctx = proxy.new_ctx();
    assert!(proxy.request_filter(&mut session, &mut ctx).await.unwrap());
    assert_eq!(proxy.proxy_requests.active_count(), 1);
    proxy.logging(&mut session, None, &mut ctx).await;
    // Pingora calls logging BEFORE downstream finish().await, which may still block.
    assert_eq!(proxy.proxy_requests.active_count(), 1);
    proxy.proxy_requests.begin_drain();
    assert!(!proxy.proxy_requests.resume());
    drop(ctx);
    assert_eq!(proxy.proxy_requests.active_count(), 0);
    assert!(proxy.proxy_requests.resume());
}

#[tokio::test]
async fn cancellation_drops_guard_while_direct_response_is_blocked() {
    tokio::time::timeout(Duration::from_secs(2), async {
        let temp = tempfile::tempdir().unwrap();
        let proxy = Arc::new(proxy(temp.path().into()));
        let (_client, mut session) = downstream(1).await;
        let owned = proxy.clone();
        let task = tokio::spawn(async move {
            let mut ctx = owned.new_ctx();
            owned.request_filter(&mut session, &mut ctx).await.unwrap();
        });
        while proxy.proxy_requests.active_count() == 0 {
            tokio::task::yield_now().await;
        }
        assert!(!task.is_finished());
        proxy.proxy_requests.begin_drain();
        assert!(!proxy.proxy_requests.resume());
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert_eq!(proxy.proxy_requests.active_count(), 0);
        assert!(proxy.proxy_requests.resume());
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn either_admission_gate_rejects_without_leaking_context_guard() {
    let temp = tempfile::tempdir().unwrap();
    for flag_gate in [true, false] {
        let proxy = proxy(temp.path().into());
        if flag_gate {
            proxy.draining.store(true, Ordering::Release);
        } else {
            proxy.proxy_requests.begin_drain();
        }
        let (mut client, mut session) = downstream(8192).await;
        let mut ctx = proxy.new_ctx();
        assert!(proxy.request_filter(&mut session, &mut ctx).await.unwrap());
        assert!(ctx.request_guard.is_none());
        assert_eq!(proxy.proxy_requests.active_count(), 0);
        drop(session);
        let mut response = String::new();
        client.read_to_string(&mut response).await.unwrap();
        assert!(response.starts_with("HTTP/1.1 503"));
    }
}

#[tokio::test]
async fn remote_ws_and_sse_are_long_lived_without_a_local_backend() {
    let temp = tempfile::tempdir().unwrap();
    for (status, content_type, long_lived) in [
        (101, None, true),
        (200, Some("text/event-stream"), true),
        (200, Some("Text/Event-Stream; charset=utf-8"), true),
        (200, Some("text/event-streaming"), false),
        (200, Some("application/json"), false),
        (200, None, false),
    ] {
        let proxy = proxy(temp.path().into());
        let (_client, mut session) = downstream(8192).await;
        let mut ctx = proxy.new_ctx();
        ctx.native_remote = Some("fd00::2".parse().unwrap());
        ctx.request_guard = proxy.proxy_requests.register(Some(descriptor()));
        assert!(ctx.backend.is_none());
        let mut response = ResponseHeader::build(status, None).unwrap();
        if let Some(content_type) = content_type {
            response
                .insert_header("Content-Type", content_type)
                .unwrap();
        }
        proxy
            .upstream_response_filter(&mut session, &mut response, &mut ctx)
            .await
            .unwrap();
        proxy.proxy_requests.begin_drain();
        // Already classified streams were shutdown by begin; ordinary HTTP waits for force.
        assert_eq!(proxy.proxy_requests.force_drain(), usize::from(!long_lived));
        assert_eq!(proxy.proxy_requests.active_count(), 1);
        drop(ctx);
        assert!(proxy.proxy_requests.resume());
    }
}

#[tokio::test]
async fn response_classification_after_begin_cannot_escape_shutdown() {
    let temp = tempfile::tempdir().unwrap();
    for status in [101, 200] {
        let proxy = proxy(temp.path().into());
        let (_client, mut session) = downstream(8192).await;
        let mut ctx = proxy.new_ctx();
        ctx.native_remote = Some("fd00::2".parse().unwrap());
        ctx.request_guard = proxy.proxy_requests.register(Some(descriptor()));
        proxy.proxy_requests.begin_drain();
        let mut response = ResponseHeader::build(status, None).unwrap();
        if status == 200 {
            response
                .insert_header("Content-Type", "text/event-stream; charset=utf-8")
                .unwrap();
        }
        proxy
            .upstream_response_filter(&mut session, &mut response, &mut ctx)
            .await
            .unwrap();
        assert_eq!(proxy.proxy_requests.force_drain(), 0);
        assert_eq!(proxy.proxy_requests.active_count(), 1);
        drop(ctx);
        assert!(proxy.proxy_requests.resume());
    }
}

#[tokio::test]
async fn downstream_upgrade_header_marks_guard_before_any_upstream_response() {
    let temp = tempfile::tempdir().unwrap();
    let proxy = proxy(temp.path().into());
    let (_client, mut session) = downstream(8192).await;
    let file = std::fs::File::open("/dev/null").unwrap();
    session
        .as_downstream_mut()
        .digest_mut()
        .unwrap()
        .socket_digest = Some(Arc::new(
        pingora_core::protocols::digest::SocketDigest::from_raw_fd(file.as_raw_fd()),
    ));
    session
        .req_header_mut()
        .insert_header("Upgrade", "websocket")
        .unwrap();
    let mut ctx = proxy.new_ctx();
    assert!(proxy.request_filter(&mut session, &mut ctx).await.unwrap());
    proxy.proxy_requests.begin_drain();
    assert_eq!(proxy.proxy_requests.force_drain(), 0);
    assert_eq!(proxy.proxy_requests.active_count(), 1);
    drop(ctx);
    assert!(proxy.proxy_requests.resume());
}

#[tokio::test]
async fn h2_shared_connection_keeps_each_request_counted_after_driver_exit() {
    tokio::time::timeout(Duration::from_secs(2), async {
        use pingora_core::protocols::http::v2::server::{self, H2Accept, HttpSession};
        use pingora_core::protocols::{Digest, digest::SocketDigest};
        let temp = tempfile::tempdir().unwrap();
        let proxy = proxy(temp.path().into());
        let file = std::fs::File::open("/dev/null").unwrap();
        let socket = Arc::new(SocketDigest::from_raw_fd(file.as_raw_fd()));
        let digest = Arc::new(Digest {
            socket_digest: Some(socket),
            ..Default::default()
        });
        drop(file);
        let (client, io) = tokio::io::duplex(16 * 1024);
        let client_task = tokio::spawn(async move {
            let (mut requests, connection) = h2::client::handshake(client).await.unwrap();
            let send = async move {
                let mut responses = Vec::new();
                for _ in 0..2 {
                    let request = hyper::Request::builder()
                        .uri("https://local/")
                        .body(())
                        .unwrap();
                    responses.push(requests.send_request(request, true).unwrap().0);
                }
                let _ = tokio::join!(responses.remove(0), responses.remove(0));
            };
            let _ = tokio::join!(connection, send);
        });
        let mut driver = server::handshake(Box::new(io), None).await.unwrap();
        let mut sessions = Vec::new();
        for _ in 0..2 {
            let Some(H2Accept::Session(h2)) =
                HttpSession::from_h2_conn(&mut driver, digest.clone())
                    .await
                    .unwrap()
            else {
                panic!("H2 request")
            };
            let (_unused, io) = tokio::io::duplex(1);
            let mut session = Session::new_h1(Box::new(io));
            *session.as_downstream_mut() = pingora_core::protocols::http::server::Session::H2(h2);
            sessions.push(session);
        }
        drop(driver);
        let mut first = proxy.new_ctx();
        let mut second = proxy.new_ctx();
        // Actual H2 Sessions outlive their driver. Safe duplication uses retained ownership.
        first.request_guard = proxy
            .proxy_requests
            .register(native::duplicate_socket(&sessions[0]).unwrap());
        second.request_guard = proxy
            .proxy_requests
            .register(native::duplicate_socket(&sessions[1]).unwrap());
        first.request_guard.as_ref().unwrap().mark_long_lived();
        proxy.proxy_requests.begin_drain();
        assert_eq!(proxy.proxy_requests.active_count(), 2);
        assert_eq!(proxy.proxy_requests.force_drain(), 1);
        assert_eq!(proxy.proxy_requests.force_drain(), 0);
        assert!(!proxy.proxy_requests.resume());
        drop(first);
        assert_eq!(proxy.proxy_requests.active_count(), 1);
        assert!(!proxy.proxy_requests.resume());
        drop(second);
        assert!(proxy.proxy_requests.resume());
        client_task.abort();
        let _ = client_task.await;
    })
    .await
    .unwrap();
}
