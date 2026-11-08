//! Bound synchronous storage operations before dispatching them off the runtime.

use tokio::sync::Semaphore;

// A database outage must not consume Tokio's entire blocking-thread pool.
static STORAGE_OPERATIONS: Semaphore = Semaphore::const_new(32);
// A network invocation can call back into the recipient's storage socket.
// Never hold a storage permit while waiting for that round trip.
static NETWORK_OPERATIONS: Semaphore = Semaphore::const_new(64);

pub(crate) async fn run<T: Send + 'static>(
    operation: impl FnOnce() -> T + Send + 'static,
) -> Result<T, tokio::task::JoinError> {
    run_with(&STORAGE_OPERATIONS, operation).await
}

pub(crate) async fn run_network<T: Send + 'static>(
    operation: impl FnOnce() -> T + Send + 'static,
) -> Result<T, tokio::task::JoinError> {
    run_with(&NETWORK_OPERATIONS, operation).await
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
