//! Bound synchronous storage operations before dispatching them off the runtime.

use tokio::sync::Semaphore;

// A database outage must not consume Tokio's entire blocking-thread pool.
static STORAGE_OPERATIONS: Semaphore = Semaphore::const_new(32);
// A network invocation can call back into the recipient's storage socket.
// Never hold a storage permit while waiting for that round trip.
static NETWORK_OPERATIONS: Semaphore = Semaphore::const_new(64);
// KV calls never call back into a socket, so they wait for a permit rather
// than fail. Their own pool keeps a busy or unreachable store from starving
// network calls and storage operations, and the reverse.
static KV_OPERATIONS: Semaphore = Semaphore::const_new(256);

pub(crate) async fn run<T: Send + 'static>(
    operation: impl FnOnce() -> T + Send + 'static,
) -> Result<T, tokio::task::JoinError> {
    run_with(&STORAGE_OPERATIONS, operation).await
}

pub(crate) async fn run_kv<T: Send + 'static>(
    operation: impl FnOnce() -> T + Send + 'static,
) -> Result<T, tokio::task::JoinError> {
    run_with(&KV_OPERATIONS, operation).await
}

pub(crate) async fn run_network<T: Send + 'static>(
    operation: impl FnOnce() -> T + Send + 'static,
) -> Result<T, tako_core::SocketError> {
    // A handler may recursively call another handler through this pool.
    // Waiting for a permit can deadlock every ancestor; reject overload now.
    let permit = NETWORK_OPERATIONS.try_acquire().map_err(|_| {
        tako_core::SocketError::new(
            tako_core::ErrorCode::Unavailable,
            "Network call capacity exceeded",
        )
    })?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        operation()
    })
    .await
    .map_err(|error| tako_core::SocketError::unclassified(error.to_string()))
}

async fn run_with<T: Send + 'static>(
    limit: &'static Semaphore,
    operation: impl FnOnce() -> T + Send + 'static,
) -> Result<T, tokio::task::JoinError> {
    let permit = limit
        .acquire()
        .await
        .expect("operation semaphore is never closed");
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        operation()
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn network_overload_fails_without_blocking_recipient_storage() {
        let permits = NETWORK_OPERATIONS.acquire_many(64).await.unwrap();
        let refused = run_network(|| 1).await.unwrap_err();
        assert!(refused.message.contains("capacity"));
        assert_eq!(refused.code, Some(tako_core::ErrorCode::Unavailable));
        assert_eq!(run(|| 2).await.unwrap(), 2);
        drop(permits);
        assert_eq!(run_network(|| 3).await.unwrap(), 3);
    }
}
