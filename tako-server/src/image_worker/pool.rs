use super::process::ImageWorkerProcess;
use super::{
    IMAGE_LOG_SOURCE, IMAGE_WORKER_EXECUTION_TIMEOUT, IMAGE_WORKER_IDLE_TIMEOUT,
    IMAGE_WORKER_MAX_CONCURRENCY, IMAGE_WORKER_MIN_CONCURRENCY, IMAGE_WORKER_QUEUE_CAPACITY,
};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;
use tako_images::ImageError;
use tokio::sync::{Mutex, Semaphore, SemaphorePermit, TryAcquireError};
use tokio::time::timeout;

pub(super) async fn acquire_worker_slot() -> Result<SemaphorePermit<'static>, ImageError> {
    let slots = worker_slots();
    match slots.try_acquire() {
        Ok(permit) => return Ok(permit),
        Err(TryAcquireError::Closed) => return Err(ImageError::TransformFailed),
        Err(TryAcquireError::NoPermits) => {}
    }

    let _queue_slot = reserve_worker_queue_slot()?;
    slots
        .acquire()
        .await
        .map_err(|_| ImageError::TransformFailed)
}

pub(super) async fn run_worker_pool_request(
    app_name: &str,
    input: Vec<u8>,
) -> Result<Vec<u8>, ImageError> {
    start_worker_reaper();
    let mut worker = checkout_worker(app_name).await?;
    let result = timeout(
        IMAGE_WORKER_EXECUTION_TIMEOUT,
        worker.request(app_name, &input),
    )
    .await;

    match result {
        Ok(Ok(output)) => {
            checkin_worker(worker).await;
            Ok(output)
        }
        Ok(Err(error)) => {
            worker.stop().await;
            Err(error)
        }
        Err(_) => {
            tracing::warn!(
                app = %app_name,
                source = IMAGE_LOG_SOURCE,
                timeout_ms = IMAGE_WORKER_EXECUTION_TIMEOUT.as_millis() as u64,
                "Image worker request timed out"
            );
            worker.stop().await;
            Err(ImageError::TransformFailed)
        }
    }
}

async fn checkout_worker(app_name: &str) -> Result<ImageWorkerProcess, ImageError> {
    let mut workers = worker_pool().lock().await;
    while let Some(mut worker) = workers.pop() {
        if worker.is_running(app_name) {
            return Ok(worker);
        }
    }
    drop(workers);
    ImageWorkerProcess::spawn(app_name).await
}

async fn checkin_worker(mut worker: ImageWorkerProcess) {
    worker.idle_since = Instant::now();
    let mut workers = worker_pool().lock().await;
    if workers.len() < worker_concurrency() {
        workers.push(worker);
    } else {
        drop(workers);
        worker.stop().await;
    }
}

fn reserve_worker_queue_slot() -> Result<WorkerQueueSlot, ImageError> {
    let queue_depth = worker_queue_depth();
    let mut current = queue_depth.load(Ordering::Acquire);
    loop {
        if current >= IMAGE_WORKER_QUEUE_CAPACITY {
            return Err(ImageError::TransformQueueFull);
        }
        match queue_depth.compare_exchange_weak(
            current,
            current + 1,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => return Ok(WorkerQueueSlot),
            Err(next) => current = next,
        }
    }
}

struct WorkerQueueSlot;

impl Drop for WorkerQueueSlot {
    fn drop(&mut self) {
        worker_queue_depth().fetch_sub(1, Ordering::AcqRel);
    }
}

fn worker_pool() -> &'static Mutex<Vec<ImageWorkerProcess>> {
    static POOL: OnceLock<Mutex<Vec<ImageWorkerProcess>>> = OnceLock::new();
    POOL.get_or_init(|| Mutex::new(Vec::with_capacity(worker_concurrency())))
}

fn start_worker_reaper() {
    static STARTED: OnceLock<()> = OnceLock::new();
    STARTED.get_or_init(|| {
        tokio::spawn(async {
            loop {
                tokio::time::sleep(IMAGE_WORKER_IDLE_TIMEOUT).await;
                prune_idle_workers().await;
            }
        });
    });
}

async fn prune_idle_workers() {
    let now = Instant::now();
    let mut expired = Vec::new();
    let mut workers = worker_pool().lock().await;
    let mut index = 0;
    while index < workers.len() {
        if now.saturating_duration_since(workers[index].idle_since) >= IMAGE_WORKER_IDLE_TIMEOUT {
            expired.push(workers.swap_remove(index));
        } else {
            index += 1;
        }
    }
    drop(workers);

    for worker in expired {
        worker.stop().await;
    }
}

fn worker_slots() -> &'static Semaphore {
    static SLOTS: OnceLock<Semaphore> = OnceLock::new();
    SLOTS.get_or_init(|| Semaphore::new(worker_concurrency()))
}

fn worker_concurrency() -> usize {
    let parallelism = std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(IMAGE_WORKER_MIN_CONCURRENCY);
    if parallelism <= 2 {
        IMAGE_WORKER_MIN_CONCURRENCY
    } else {
        (parallelism / 4).clamp(IMAGE_WORKER_MIN_CONCURRENCY, IMAGE_WORKER_MAX_CONCURRENCY)
    }
}

fn worker_queue_depth() -> &'static AtomicUsize {
    static DEPTH: OnceLock<AtomicUsize> = OnceLock::new();
    DEPTH.get_or_init(|| AtomicUsize::new(0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn worker_concurrency_scales_with_host_without_exceeding_limit() {
        let concurrency = worker_concurrency();

        assert!(
            (IMAGE_WORKER_MIN_CONCURRENCY..=IMAGE_WORKER_MAX_CONCURRENCY).contains(&concurrency)
        );
    }

    #[tokio::test]
    async fn worker_slot_queue_waits_until_permit_is_available() {
        let _lock = acquire_worker_slot_test_lock().await;
        let permits = acquire_all_worker_slots().await;
        let mut queued = tokio::spawn(acquire_worker_slot());

        if timeout(Duration::from_millis(25), &mut queued)
            .await
            .is_ok()
        {
            panic!("queued worker slot returned early");
        }

        drop(permits);

        let _queued_permit = timeout(Duration::from_secs(1), queued)
            .await
            .expect("queued worker slot should acquire after release")
            .expect("queued worker task")
            .expect("queued worker slot");
    }

    #[tokio::test]
    async fn worker_slot_queue_rejects_when_capacity_is_reached() {
        let _lock = acquire_worker_slot_test_lock().await;
        assert_eq!(worker_queue_depth().load(Ordering::Acquire), 0);
        let permits = acquire_all_worker_slots().await;
        let mut queued = Vec::with_capacity(IMAGE_WORKER_QUEUE_CAPACITY);
        for _ in 0..IMAGE_WORKER_QUEUE_CAPACITY {
            queued.push(tokio::spawn(acquire_worker_slot()));
        }
        wait_for_worker_queue_depth(IMAGE_WORKER_QUEUE_CAPACITY).await;

        let result = acquire_worker_slot().await;

        assert!(matches!(result, Err(ImageError::TransformQueueFull)));
        drop(permits);
        for task in queued {
            let permit = timeout(Duration::from_secs(1), task)
                .await
                .expect("queued worker slot should acquire after release")
                .expect("queued worker task")
                .expect("queued worker slot");
            drop(permit);
        }
        assert_eq!(worker_queue_depth().load(Ordering::Acquire), 0);
    }

    async fn acquire_all_worker_slots() -> Vec<SemaphorePermit<'static>> {
        let mut permits = Vec::with_capacity(worker_concurrency());
        for _ in 0..worker_concurrency() {
            permits.push(acquire_worker_slot().await.expect("worker slot"));
        }
        permits
    }

    async fn acquire_worker_slot_test_lock() -> tokio::sync::MutexGuard<'static, ()> {
        static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
            .lock()
            .await
    }

    async fn wait_for_worker_queue_depth(expected: usize) {
        for _ in 0..100 {
            if worker_queue_depth().load(Ordering::Acquire) == expected {
                return;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        panic!(
            "worker queue depth did not reach {expected}; current={}",
            worker_queue_depth().load(Ordering::Acquire)
        );
    }
}
