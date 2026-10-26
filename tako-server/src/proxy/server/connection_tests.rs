use super::public_http_server_options;
use async_trait::async_trait;
use bytes::Bytes;
use pingora_core::apps::{HttpServerApp, HttpServerOptions, ReusedHttpStream, ServerApp};
use pingora_core::protocols::http::ServerSession;
use pingora_core::protocols::l4::stream::Stream as L4Stream;
use pingora_core::server::ShutdownWatch;
use pingora_http::ResponseHeader;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;

struct StreamingApp {
    options: HttpServerOptions,
    finish: Notify,
}

#[async_trait]
impl HttpServerApp for StreamingApp {
    fn server_options(&self) -> Option<&HttpServerOptions> {
        Some(&self.options)
    }

    async fn process_new_http(
        self: &Arc<Self>,
        mut session: ServerSession,
        _shutdown: &ShutdownWatch,
    ) -> Option<ReusedHttpStream> {
        assert!(session.read_request().await.unwrap());
        let mut header = ResponseHeader::build(200, None).unwrap();
        header
            .insert_header("content-type", "text/event-stream")
            .unwrap();
        session
            .write_response_header(Box::new(header))
            .await
            .unwrap();
        self.finish.notified().await;
        session
            .write_response_body(Bytes::from_static(b"data: done\n\n"), true)
            .await
            .unwrap();
        session.finish().await.unwrap();
        None
    }
}

async fn check_h2_idle_timeout(active_stream: bool) {
    let mut options = public_http_server_options();
    assert_eq!(options.h2_idle_timeout, Some(Duration::from_secs(60)));
    // Exercise Pingora's real connection loop with a shorter idle period.
    let idle = Duration::from_millis(100);
    options.h2_idle_timeout = Some(idle);
    options.h2c = true;
    let app = Arc::new(StreamingApp {
        options,
        finish: Notify::new(),
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let client = tokio::net::TcpStream::connect(listener.local_addr().unwrap())
        .await
        .unwrap();
    let (server, _) = listener.accept().await.unwrap();
    let (_shutdown_tx, shutdown) = tokio::sync::watch::channel(false);
    let server_app = app.clone();
    let mut server_task = tokio::spawn(async move {
        server_app
            .process_new(Box::new(L4Stream::from(server)), &shutdown)
            .await
    });
    let (mut requests, connection) = h2::client::handshake(client).await.unwrap();
    let client_task = tokio::spawn(connection);

    if active_stream {
        let request = hyper::Request::builder()
            .uri("http://localhost/events")
            .body(())
            .unwrap();
        let (response, _) = requests.send_request(request, true).unwrap();
        let response = tokio::time::timeout(Duration::from_secs(2), response)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(response.headers()["content-type"], "text/event-stream");
        let mut body = response.into_body();
        assert!(
            tokio::time::timeout(idle * 3, &mut server_task)
                .await
                .is_err(),
            "active SSE stream must survive the connection idle period",
        );
        app.finish.notify_one();
        let data = tokio::time::timeout(Duration::from_secs(2), body.data())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(data, Bytes::from_static(b"data: done\n\n"));
    }

    tokio::time::timeout(Duration::from_secs(3), &mut server_task)
        .await
        .expect("connection must close after its streams become idle")
        .unwrap();
    client_task.abort();
}

#[tokio::test]
async fn h2_idle_connection_closes() {
    check_h2_idle_timeout(false).await;
}

#[tokio::test]
async fn h2_active_sse_survives_until_idle() {
    check_h2_idle_timeout(true).await;
}
