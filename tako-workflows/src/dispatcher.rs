//! Workflow dispatch signaler.
//!
//! Enqueue, signal, cron, and lease-reclaim paths make durable state changes
//! first, then notify this dispatcher. The dispatcher is the small matching
//! layer between "work exists in SQLite" and "a worker should poll now": it
//! coalesces bursts of notifications and periodically scans for due pending
//! runs so delayed retries/sleeps still wake scale-to-zero workers.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

use tokio::sync::{Notify, oneshot};

const DEFAULT_DEBOUNCE: Duration = Duration::from_millis(10);
const DEFAULT_SCAN_INTERVAL: Duration = Duration::from_secs(1);

type WakeFn = Arc<dyn Fn() + Send + Sync>;
type RunnableCheck = Arc<dyn Fn() -> bool + Send + Sync>;

struct DispatcherInner {
    pending_signal: AtomicBool,
    notify: Notify,
}

/// Cheap cloneable handle used by enqueue/signal/cron code after a durable
/// write indicates work may be runnable.
#[derive(Clone)]
pub struct DispatchSignal {
    inner: Arc<DispatcherInner>,
}

impl DispatchSignal {
    pub fn signal(&self) {
        if !self.inner.pending_signal.swap(true, Ordering::AcqRel) {
            self.inner.notify.notify_one();
        }
    }
}

/// Owns the background dispatch task for one app.
pub struct WorkDispatcher {
    signal: DispatchSignal,
    shutdown_tx: Option<oneshot::Sender<()>>,
    join: Option<tokio::task::JoinHandle<()>>,
}

impl WorkDispatcher {
    pub(crate) fn request_shutdown(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
    }

    pub(crate) fn is_finished(&self) -> bool {
        self.join.as_ref().is_none_or(|join| join.is_finished())
    }

    pub fn spawn(wake: WakeFn, has_runnable_work: RunnableCheck) -> Self {
        Self::spawn_with_intervals(
            wake,
            has_runnable_work,
            DEFAULT_DEBOUNCE,
            DEFAULT_SCAN_INTERVAL,
        )
    }

    fn spawn_with_intervals(
        wake: WakeFn,
        has_runnable_work: RunnableCheck,
        debounce: Duration,
        scan_interval: Duration,
    ) -> Self {
        let inner = Arc::new(DispatcherInner {
            pending_signal: AtomicBool::new(false),
            notify: Notify::new(),
        });
        let signal = DispatchSignal {
            inner: inner.clone(),
        };
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let join = tokio::spawn(run_dispatcher(
            inner,
            wake,
            has_runnable_work,
            debounce,
            scan_interval,
            shutdown_rx,
        ));
        Self {
            signal,
            shutdown_tx: Some(shutdown_tx),
            join: Some(join),
        }
    }

    pub fn signaler(&self) -> DispatchSignal {
        self.signal.clone()
    }

    pub async fn shutdown(mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        if let Some(join) = self.join.take() {
            let _ = join.await;
        }
    }
}

impl Drop for WorkDispatcher {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
    }
}

async fn run_dispatcher(
    inner: Arc<DispatcherInner>,
    wake: WakeFn,
    has_runnable_work: RunnableCheck,
    debounce: Duration,
    scan_interval: Duration,
    shutdown_rx: oneshot::Receiver<()>,
) {
    tokio::pin!(shutdown_rx);
    let mut scan = tokio::time::interval(scan_interval);
    scan.tick().await;

    loop {
        tokio::select! {
            _ = &mut shutdown_rx => break,
            _ = inner.notify.notified() => {
                if !debounce.is_zero() {
                    tokio::select! {
                        _ = &mut shutdown_rx => break,
                        _ = tokio::time::sleep(debounce) => {}
                    }
                }
                inner.pending_signal.store(false, Ordering::Release);
                wake_if_runnable(&wake, &has_runnable_work).await;
            }
            _ = scan.tick() => {
                wake_if_runnable(&wake, &has_runnable_work).await;
            }
        }
    }
}

async fn wake_if_runnable(wake: &WakeFn, has_runnable_work: &RunnableCheck) {
    let check = has_runnable_work.clone();
    match crate::blocking::run(move || check()).await {
        Ok(true) => wake(),
        Ok(false) => {}
        Err(error) => tracing::error!(%error, "Workflow runnable check failed"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test]
    async fn database_check_does_not_block_async_tasks() {
        let (started_tx, mut started_rx) = tokio::sync::mpsc::unbounded_channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let release_rx = std::sync::Mutex::new(release_rx);
        let (result_tx, mut result_rx) = tokio::sync::mpsc::unbounded_channel();
        let check: RunnableCheck = Arc::new(move || {
            started_tx.send(()).unwrap();
            let released = release_rx
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(2))
                .is_ok();
            result_tx.send(released).unwrap();
            false
        });
        let dispatcher = WorkDispatcher::spawn_with_intervals(
            Arc::new(|| {}),
            check,
            Duration::ZERO,
            Duration::from_secs(60),
        );
        dispatcher.signaler().signal();
        started_rx.recv().await.unwrap();
        release_tx.send(()).unwrap();
        assert!(
            result_rx.recv().await.unwrap(),
            "DB check prevented the async task from releasing it"
        );
        dispatcher.shutdown().await;
    }

    fn wake_counter() -> (WakeFn, Arc<AtomicUsize>) {
        let count = Arc::new(AtomicUsize::new(0));
        let counter = count.clone();
        let wake: WakeFn = Arc::new(move || {
            counter.fetch_add(1, Ordering::SeqCst);
        });
        (wake, count)
    }

    #[tokio::test]
    async fn signal_does_not_wake_synchronously() {
        let (wake, count) = wake_counter();
        let has_runnable: RunnableCheck = Arc::new(|| true);
        let dispatcher = WorkDispatcher::spawn_with_intervals(
            wake,
            has_runnable,
            Duration::from_millis(40),
            Duration::from_secs(60),
        );

        dispatcher.signaler().signal();
        assert_eq!(count.load(Ordering::SeqCst), 0);
        tokio::time::sleep(Duration::from_millis(80)).await;
        assert_eq!(count.load(Ordering::SeqCst), 1);

        dispatcher.shutdown().await;
    }

    #[tokio::test]
    async fn signal_coalesces_bursts_into_one_wake() {
        let (wake, count) = wake_counter();
        let has_runnable: RunnableCheck = Arc::new(|| true);
        let dispatcher = WorkDispatcher::spawn_with_intervals(
            wake,
            has_runnable,
            Duration::from_millis(30),
            Duration::from_secs(60),
        );
        let signal = dispatcher.signaler();

        for _ in 0..10 {
            signal.signal();
        }
        tokio::time::sleep(Duration::from_millis(80)).await;
        assert_eq!(count.load(Ordering::SeqCst), 1);

        dispatcher.shutdown().await;
    }

    #[tokio::test]
    async fn signal_skips_wake_when_no_work_is_runnable() {
        let (wake, count) = wake_counter();
        let has_runnable: RunnableCheck = Arc::new(|| false);
        let dispatcher = WorkDispatcher::spawn_with_intervals(
            wake,
            has_runnable,
            Duration::from_millis(20),
            Duration::from_secs(60),
        );

        dispatcher.signaler().signal();
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert_eq!(count.load(Ordering::SeqCst), 0);

        dispatcher.shutdown().await;
    }

    #[tokio::test]
    async fn scan_wakes_runnable_work_without_a_signal() {
        let (wake, count) = wake_counter();
        let runnable = Arc::new(AtomicBool::new(false));
        let check_flag = runnable.clone();
        let has_runnable: RunnableCheck = Arc::new(move || check_flag.load(Ordering::SeqCst));
        let dispatcher = WorkDispatcher::spawn_with_intervals(
            wake,
            has_runnable,
            Duration::from_millis(20),
            Duration::from_millis(30),
        );

        runnable.store(true, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(90)).await;
        assert!(count.load(Ordering::SeqCst) > 0);

        dispatcher.shutdown().await;
    }

    #[tokio::test]
    async fn scan_does_not_wake_when_queue_is_empty() {
        let (wake, count) = wake_counter();
        let has_runnable: RunnableCheck = Arc::new(|| false);
        let dispatcher = WorkDispatcher::spawn_with_intervals(
            wake,
            has_runnable,
            Duration::from_millis(20),
            Duration::from_millis(20),
        );

        tokio::time::sleep(Duration::from_millis(80)).await;
        assert_eq!(count.load(Ordering::SeqCst), 0);

        dispatcher.shutdown().await;
    }
}
