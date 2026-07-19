//! Shared turso helpers for tako's SQLite-backed stores.
//!
//! Every store keeps a sync public API and drives turso's async calls with
//! [`block_on`]. Turso futures are waker-driven and runtime-agnostic (no tokio
//! reactor with `default-features = false`), so parking the calling thread is
//! safe even on an async runtime worker — the same blocking profile the
//! rusqlite calls had.

use std::future::Future;
use std::pin::pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll, Wake, Waker};
use std::thread::{self, Thread};
use std::time::{Duration, Instant};

/// How long a connection waits on a locked database before erroring.
pub const BUSY_TIMEOUT: Duration = Duration::from_millis(5000);

/// A poll streak longer than this is either a pathological query or turso's
/// busy-wait (which wakes the waker synchronously without making progress);
/// throttling it caps the CPU burn without slowing ordinary queries.
const THROTTLE_AFTER: Duration = Duration::from_millis(25);
const THROTTLE_PAUSE: Duration = Duration::from_micros(200);

struct ThreadWaker {
    thread: Thread,
    woken: AtomicBool,
}

impl Wake for ThreadWaker {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.woken.store(true, Ordering::Release);
        self.thread.unpark();
    }
}

/// Drive a turso future to completion on the calling thread.
///
/// Turso signals both real progress and its busy-wait by waking the waker
/// synchronously inside `poll`, so a naive executor would hot-spin a full CPU
/// core for the entire busy timeout when a database is contended. This driver
/// re-polls immediately while the future is making progress, throttles poll
/// streaks that outlive [`THROTTLE_AFTER`], and parks the thread whenever the
/// future is genuinely waiting.
pub fn block_on<F: Future>(future: F) -> F::Output {
    let mut future = pin!(future);
    let waker_state = Arc::new(ThreadWaker {
        thread: thread::current(),
        woken: AtomicBool::new(false),
    });
    let waker = Waker::from(waker_state.clone());
    let mut cx = Context::from_waker(&waker);
    let start = Instant::now();
    loop {
        match future.as_mut().poll(&mut cx) {
            Poll::Ready(output) => return output,
            Poll::Pending => {
                if waker_state.woken.swap(false, Ordering::Acquire) {
                    if start.elapsed() > THROTTLE_AFTER {
                        thread::park_timeout(THROTTLE_PAUSE);
                    }
                } else {
                    thread::park();
                }
            }
        }
    }
}

/// Open (or create) a file-backed turso database with tako's standard
/// settings: multiprocess WAL (so the old and new server processes can hold
/// the same DB during a zero-downtime reload), a busy timeout, synchronous
/// NORMAL, and foreign keys on.
pub async fn open_local(path: &str) -> Result<turso::Connection, turso::Error> {
    let db = turso::Builder::new_local(path)
        .experimental_multiprocess_wal(true)
        .build()
        .await?;
    let conn = db.connect()?;
    conn.busy_timeout(BUSY_TIMEOUT)?;
    conn.execute_batch(
        "PRAGMA synchronous = NORMAL;
         PRAGMA foreign_keys = ON;",
    )
    .await?;
    Ok(conn)
}

/// Open an in-memory turso database (single-process; no WAL settings apply).
pub async fn open_in_memory() -> Result<turso::Connection, turso::Error> {
    let db = turso::Builder::new_local(":memory:").build().await?;
    let conn = db.connect()?;
    conn.execute_batch("PRAGMA foreign_keys = ON;").await?;
    Ok(conn)
}

/// Commit `tx` when `result` is Ok, roll it back when it is Err.
///
/// Turso's `Transaction` does NOT roll back in `Drop` — it only marks the
/// transaction dangling, and the actual ROLLBACK runs at the next
/// execute/query on the same connection. An early-error return that just
/// drops the transaction therefore keeps the WAL write lock held, starving
/// other processes sharing the DB. Every transactional store operation must
/// funnel its result through this helper (or call `tx.rollback()` itself).
pub async fn commit_or_rollback<T, E>(
    tx: turso::transaction::Transaction<'_>,
    result: Result<T, E>,
) -> Result<T, E>
where
    E: From<turso::Error>,
{
    match result {
        Ok(value) => {
            tx.commit().await?;
            Ok(value)
        }
        Err(error) => {
            let _ = tx.rollback().await;
            Err(error)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_on_returns_ready_value() {
        assert_eq!(block_on(async { 42 }), 42);
    }

    #[test]
    fn block_on_wakes_from_another_thread() {
        struct CrossThread {
            done: Arc<AtomicBool>,
            spawned: bool,
        }
        impl Future for CrossThread {
            type Output = &'static str;
            fn poll(
                mut self: std::pin::Pin<&mut Self>,
                cx: &mut Context<'_>,
            ) -> Poll<Self::Output> {
                if self.done.load(Ordering::Acquire) {
                    return Poll::Ready("woken");
                }
                if !self.spawned {
                    self.spawned = true;
                    let done = self.done.clone();
                    let waker = cx.waker().clone();
                    thread::spawn(move || {
                        thread::sleep(Duration::from_millis(20));
                        done.store(true, Ordering::Release);
                        waker.wake();
                    });
                }
                Poll::Pending
            }
        }
        let result = block_on(CrossThread {
            done: Arc::new(AtomicBool::new(false)),
            spawned: false,
        });
        assert_eq!(result, "woken");
    }

    #[test]
    fn block_on_completes_self_waking_spin_future() {
        // Models turso's busy-wait: every poll wakes the waker and yields.
        struct Spinner {
            remaining: u32,
        }
        impl Future for Spinner {
            type Output = u32;
            fn poll(
                mut self: std::pin::Pin<&mut Self>,
                cx: &mut Context<'_>,
            ) -> Poll<Self::Output> {
                if self.remaining == 0 {
                    return Poll::Ready(0);
                }
                self.remaining -= 1;
                cx.waker().wake_by_ref();
                Poll::Pending
            }
        }
        assert_eq!(block_on(Spinner { remaining: 10_000 }), 0);
    }

    #[test]
    fn open_local_round_trips_and_reopens() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.db");
        let path = path.to_str().unwrap();

        block_on(async {
            let conn = open_local(path).await.unwrap();
            conn.execute("CREATE TABLE t (x INTEGER)", ())
                .await
                .unwrap();
            conn.execute("INSERT INTO t (x) VALUES (7)", ())
                .await
                .unwrap();
        });

        block_on(async {
            let conn = open_local(path).await.unwrap();
            let mut rows = conn.query("SELECT x FROM t", ()).await.unwrap();
            let row = rows.next().await.unwrap().unwrap();
            assert_eq!(row.get::<i64>(0).unwrap(), 7);
        });
    }

    #[test]
    fn commit_or_rollback_commits_on_ok() {
        block_on(async {
            let mut conn = open_in_memory().await.unwrap();
            conn.execute("CREATE TABLE t (x INTEGER)", ())
                .await
                .unwrap();
            let tx = conn.transaction().await.unwrap();
            let result: Result<(), turso::Error> = async {
                tx.execute("INSERT INTO t (x) VALUES (1)", ()).await?;
                Ok(())
            }
            .await;
            commit_or_rollback(tx, result).await.unwrap();

            let mut rows = conn.query("SELECT COUNT(*) FROM t", ()).await.unwrap();
            let count: i64 = rows.next().await.unwrap().unwrap().get(0).unwrap();
            assert_eq!(count, 1);
        });
    }

    #[test]
    fn rollback_on_error_releases_write_lock_for_other_connections() {
        // Regression guard: a dropped-but-not-rolled-back turso transaction
        // keeps the WAL write lock until the next op on that connection. The
        // helper must roll back eagerly so other connections can write
        // immediately.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("locks.db");
        let path = path.to_str().unwrap();

        block_on(async {
            let mut writer = open_local(path).await.unwrap();
            writer
                .execute("CREATE TABLE t (x INTEGER)", ())
                .await
                .unwrap();

            let tx = writer.transaction().await.unwrap();
            let result: Result<(), turso::Error> = async {
                tx.execute("INSERT INTO t (x) VALUES (1)", ()).await?;
                Err(turso::Error::Error("simulated".into()))
            }
            .await;
            assert!(commit_or_rollback(tx, result).await.is_err());
            // `writer` is now idle and must NOT hold the write lock.

            let other = open_local(path).await.unwrap();
            let start = Instant::now();
            other
                .execute("INSERT INTO t (x) VALUES (2)", ())
                .await
                .expect("second connection must write immediately");
            assert!(
                start.elapsed() < Duration::from_millis(1000),
                "write had to wait on a dangling lock"
            );

            let mut rows = other.query("SELECT COUNT(*) FROM t", ()).await.unwrap();
            let count: i64 = rows.next().await.unwrap().unwrap().get(0).unwrap();
            assert_eq!(count, 1, "rolled-back row must be gone, new row present");
        });
    }

    #[test]
    fn open_in_memory_enforces_foreign_keys() {
        block_on(async {
            let conn = open_in_memory().await.unwrap();
            conn.execute_batch(
                "CREATE TABLE parent (id INTEGER PRIMARY KEY);
                 CREATE TABLE child (
                     pid INTEGER NOT NULL,
                     FOREIGN KEY (pid) REFERENCES parent(id)
                 );",
            )
            .await
            .unwrap();
            let orphan = conn
                .execute("INSERT INTO child (pid) VALUES (99)", ())
                .await;
            assert!(orphan.is_err(), "orphan insert must violate FK");
        });
    }
}
