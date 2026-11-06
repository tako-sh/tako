use std::{
    future::Future,
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

/// Own the serving future so cancellation drops it before the caller closes the session.
pub(super) async fn until_draining<T>(
    draining: &AtomicBool,
    serve: impl Future<Output = T>,
) -> Option<T> {
    let drain = async {
        loop {
            if draining.load(Ordering::Acquire) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    };
    tokio::select! {
        biased;
        _ = drain => None,
        result = serve => Some(result),
    }
}

/// Disable reuse even if a transport shutdown stalls; returning drops the downstream session.
pub(super) async fn close(session: &mut pingora_proxy::Session) {
    session.set_keepalive(None);
    let _ = tokio::time::timeout(
        Duration::from_secs(1),
        session.as_downstream_mut().shutdown(),
    )
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn drain_shuts_down_actual_pingora_downstream_without_sse_event() {
        let flag = Arc::new(AtomicBool::new(false));
        let task_flag = flag.clone();
        let (server, mut client) = tokio::io::duplex(4096);
        let (started, receive) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            let mut session = pingora_proxy::Session::new_h1(Box::new(server));
            session.read_request().await.unwrap();
            let result = until_draining(&task_flag, async {
                let mut response = pingora_http::ResponseHeader::build(200, None).unwrap();
                response
                    .insert_header("content-type", "text/event-stream")
                    .unwrap();
                session
                    .write_response_header(Box::new(response), false)
                    .await
                    .unwrap();
                started.send(()).unwrap();
                session.read_request_body().await.unwrap();
                std::future::pending::<()>().await;
            })
            .await;
            assert!(result.is_none());
            close(&mut session).await;
            // Keep the Session alive to prove shutdown, rather than its destructor, causes EOF.
            tokio::time::sleep(Duration::from_millis(200)).await;
        });
        client
            .write_all(b"GET / HTTP/1.1\r\nHost: local\r\n\r\n")
            .await
            .unwrap();
        receive.await.unwrap();
        flag.store(true, Ordering::Release);
        let mut bytes = Vec::new();
        tokio::time::timeout(Duration::from_secs(1), client.read_to_end(&mut bytes))
            .await
            .unwrap()
            .unwrap();
        assert!(bytes.starts_with(b"HTTP/1.1 200"));
        let body = bytes.windows(4).position(|b| b == b"\r\n\r\n").unwrap() + 4;
        assert!(bytes[body..].is_empty());
        task.await.unwrap();
    }

    #[tokio::test]
    async fn drain_drops_active_stream_without_application_message() {
        let flag = Arc::new(AtomicBool::new(false));
        let task_flag = flag.clone();
        let (mut server, mut client) = tokio::io::duplex(64);
        let task = tokio::spawn(async move {
            until_draining(&task_flag, async move {
                server.write_all(b"ready").await.unwrap();
                std::future::pending::<()>().await;
                drop(server);
            })
            .await
        });
        let mut ready = [0; 5];
        client.read_exact(&mut ready).await.unwrap();
        assert_eq!(&ready, b"ready");
        flag.store(true, Ordering::Release);
        let mut tail = Vec::new();
        tokio::time::timeout(Duration::from_secs(1), client.read_to_end(&mut tail))
            .await
            .unwrap()
            .unwrap();
        assert!(tail.is_empty());
        assert!(task.await.unwrap().is_none());
    }

    #[tokio::test]
    async fn already_draining_never_polls_handler() {
        let flag = AtomicBool::new(true);
        assert!(
            until_draining(&flag, async { panic!("handler polled during drain") })
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn completed_handler_retains_its_result() {
        assert_eq!(
            until_draining(&AtomicBool::new(false), async { 42 }).await,
            Some(42)
        );
    }
}
