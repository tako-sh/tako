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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

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
