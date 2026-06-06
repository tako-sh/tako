//! Cron ticker + schedule registration + lease reclaim.
//!
//! Workers send `Command::RegisterSchedules` on startup; we persist into the
//! `schedules` table. The ticker task wakes every second, walks the schedules,
//! and enqueues any that are due. The unique key
//! `cron:<name>:<bucket_unix_ms>` prevents a single boundary from enqueuing
//! twice even if the ticker runs twice for the same second or the worker
//! re-registers mid-tick.
//!
//! The same tick also calls `RunsDb::reclaim_expired()` so runs whose worker
//! died holding a lease (SIGKILL, OOM, host crash, server-side drop without
//! graceful drain) come back to `pending` once `lease_until` passes. The
//! callback notifies the dispatcher; the dispatcher wakes a worker only when
//! runnable work exists.

use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use cron::Schedule;
use tako_core::{EnqueueOpts, ScheduleSpec};
use tokio::sync::oneshot;

use super::enqueue::{RunsDb, RunsDbError};

/// Replace the schedules table for this app with the given list.
///
/// Unknown schedules are dropped. Existing schedules keep their `last_run_at`
/// so a re-registration doesn't resurrect already-processed buckets.
pub fn register_schedules(db: &RunsDb, schedules: &[ScheduleSpec]) -> Result<(), RunsDbError> {
    for s in schedules {
        Schedule::from_str(&s.cron).map_err(|e| {
            RunsDbError::Sqlite(rusqlite::Error::ToSqlConversionFailure(Box::new(
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("invalid cron '{}' for '{}': {}", s.cron, s.name, e),
                ),
            )))
        })?;
    }

    db.replace_schedules(schedules)
}

fn list_schedules(db: &RunsDb) -> Result<Vec<super::enqueue::ScheduleRow>, RunsDbError> {
    db.list_schedules()
}

fn set_last_run_at(db: &RunsDb, name: &str, ts: i64) -> Result<(), RunsDbError> {
    db.set_schedule_last_run_at(name, ts)
}

/// Fire any schedules whose next boundary is at or before `now_ms`. Returns
/// the number of tasks enqueued.
pub fn tick_once(db: &RunsDb, now_ms: i64) -> Result<u64, RunsDbError> {
    let schedules = list_schedules(db)?;
    let now = DateTime::<Utc>::from_timestamp_millis(now_ms).unwrap_or_else(Utc::now);
    let mut enqueued = 0u64;

    for row in schedules {
        let schedule = match Schedule::from_str(&row.cron) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(name = %row.name, cron = %row.cron, error = %e, "skip invalid cron");
                continue;
            }
        };

        let after = match row.last_run_at {
            Some(ts) => DateTime::<Utc>::from_timestamp_millis(ts).unwrap_or(now),
            None => now - chrono::Duration::seconds(1),
        };
        // Fast-forward: if the server fell behind (idle, sleep, crash), skip
        // intermediate boundaries and enqueue only the latest that has
        // already passed. Prevents a thundering-herd flood when catching up.
        let mut latest: Option<DateTime<Utc>> = None;
        for t in schedule.after(&after) {
            if t > now {
                break;
            }
            latest = Some(t);
        }
        let Some(next) = latest else {
            continue;
        };

        let bucket_ms = next.timestamp_millis();
        let unique_key = format!("cron:{}:{}", row.name, bucket_ms);
        let opts = EnqueueOpts {
            unique_key: Some(unique_key),
            run_at_ms: Some(bucket_ms),
            max_attempts: None,
        };
        db.enqueue(&row.name, &serde_json::json!({}), &opts)?;
        set_last_run_at(db, &row.name, bucket_ms)?;
        enqueued += 1;
    }

    Ok(enqueued)
}

/// Handle to the running ticker task. Drop stops the loop.
pub struct CronTickerHandle {
    shutdown_tx: Option<oneshot::Sender<()>>,
    join: Option<tokio::task::JoinHandle<()>>,
}

impl CronTickerHandle {
    pub async fn shutdown(mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        if let Some(j) = self.join.take() {
            let _ = j.await;
        }
    }
}

impl Drop for CronTickerHandle {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
    }
}

/// Fire one tick: enqueue due cron schedules + reclaim any leases that
/// expired since the last tick. Fires `on_enqueue` when either produced
/// work so the dispatcher can wake a worker if a run is due. Extracted
/// from the spawn loop so tests can drive it without waiting on wall-clock
/// time.
///
/// `limiter`, when provided, has its per-worker count decremented once
/// per reclaimed row so dead workers' in-flight budgets drain back to
/// zero without waiting for the SDK to call `complete`/`fail`.
pub fn tick_and_reclaim(
    db: &RunsDb,
    now_ms: i64,
    on_enqueue: &(dyn Fn() + Send + Sync),
    limiter: Option<&crate::in_flight::InFlightLimiter>,
) {
    let cron_enqueued = match tick_once(db, now_ms) {
        Ok(n) => n,
        Err(e) => {
            tracing::warn!(error = %e, "cron tick failed");
            0
        }
    };
    let reclaimed_workers: Vec<String> = if limiter.is_some() {
        match db.reclaim_expired_with_workers() {
            Ok(workers) => workers,
            Err(e) => {
                tracing::warn!(error = %e, "reclaim expired leases failed");
                Vec::new()
            }
        }
    } else {
        match db.reclaim_expired() {
            Ok(n) => {
                // No limiter to update; just report a count-sized vec
                // so the `cron_enqueued || reclaimed` branch below still
                // fires on_enqueue.
                vec![String::new(); n as usize]
            }
            Err(e) => {
                tracing::warn!(error = %e, "reclaim expired leases failed");
                Vec::new()
            }
        }
    };
    if let Some(lim) = limiter {
        for worker in &reclaimed_workers {
            lim.release(worker);
        }
    }
    if cron_enqueued > 0 || !reclaimed_workers.is_empty() {
        on_enqueue();
    }
}

/// Start a cron ticker for an app. `on_enqueue` fires whenever a tick
/// enqueued a scheduled task or reclaimed an expired lease. The manager wires
/// this to the dispatcher so scale-to-zero workers spin up only when work is
/// runnable.
pub fn spawn(db: Arc<RunsDb>, on_enqueue: Arc<dyn Fn() + Send + Sync>) -> CronTickerHandle {
    spawn_inner(db, None, on_enqueue)
}

/// Like [`spawn`] but also drains the in-flight limiter for workers
/// whose leases got reclaimed this tick.
pub fn spawn_with_limiter(
    db: Arc<RunsDb>,
    limiter: Arc<crate::in_flight::InFlightLimiter>,
    on_enqueue: Arc<dyn Fn() + Send + Sync>,
) -> CronTickerHandle {
    spawn_inner(db, Some(limiter), on_enqueue)
}

fn spawn_inner(
    db: Arc<RunsDb>,
    limiter: Option<Arc<crate::in_flight::InFlightLimiter>>,
    on_enqueue: Arc<dyn Fn() + Send + Sync>,
) -> CronTickerHandle {
    let (tx, mut rx) = oneshot::channel::<()>();
    let join = tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = &mut rx => break,
                _ = tokio::time::sleep(Duration::from_secs(1)) => {
                    let now_ms = chrono::Utc::now().timestamp_millis();
                    tick_and_reclaim(&db, now_ms, &*on_enqueue, limiter.as_deref());
                }
            }
        }
    });
    CronTickerHandle {
        shutdown_tx: Some(tx),
        join: Some(join),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Arc<RunsDb> {
        Arc::new(RunsDb::open_in_memory().unwrap())
    }

    #[test]
    fn register_schedules_inserts_rows() {
        let db = db();
        register_schedules(
            &db,
            &[
                ScheduleSpec {
                    name: "a".into(),
                    cron: "0 */5 * * * *".into(),
                },
                ScheduleSpec {
                    name: "b".into(),
                    cron: "0 0 * * * *".into(),
                },
            ],
        )
        .unwrap();

        let schedules = list_schedules(&db).unwrap();
        assert_eq!(schedules.len(), 2);
    }

    #[test]
    fn register_schedules_is_idempotent_on_repeat_call() {
        let db = db();
        let s = ScheduleSpec {
            name: "a".into(),
            cron: "0 */5 * * * *".into(),
        };
        register_schedules(&db, std::slice::from_ref(&s)).unwrap();
        register_schedules(&db, std::slice::from_ref(&s)).unwrap();
        assert_eq!(list_schedules(&db).unwrap().len(), 1);
    }

    #[test]
    fn register_schedules_removes_schedules_not_in_new_list() {
        let db = db();
        register_schedules(
            &db,
            &[
                ScheduleSpec {
                    name: "keep".into(),
                    cron: "0 */5 * * * *".into(),
                },
                ScheduleSpec {
                    name: "drop".into(),
                    cron: "0 0 * * * *".into(),
                },
            ],
        )
        .unwrap();
        register_schedules(
            &db,
            &[ScheduleSpec {
                name: "keep".into(),
                cron: "0 */5 * * * *".into(),
            }],
        )
        .unwrap();
        let rows = list_schedules(&db).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "keep");
    }

    #[test]
    fn register_schedules_rejects_invalid_cron() {
        let db = db();
        let err = register_schedules(
            &db,
            &[ScheduleSpec {
                name: "a".into(),
                cron: "not a cron".into(),
            }],
        )
        .unwrap_err();
        assert!(format!("{err}").to_lowercase().contains("invalid"));
    }

    #[test]
    fn tick_enqueues_due_schedules() {
        let db = db();
        register_schedules(
            &db,
            &[ScheduleSpec {
                name: "every-sec".into(),
                cron: "* * * * * *".into(),
            }],
        )
        .unwrap();

        let now_ms = chrono::Utc::now().timestamp_millis() + 60_000;
        let count = tick_once(&db, now_ms).unwrap();
        assert_eq!(count, 1);
        assert_eq!(db.pending_count().unwrap(), 1);
    }

    #[test]
    fn tick_is_idempotent_within_same_bucket() {
        let db = db();
        register_schedules(
            &db,
            &[ScheduleSpec {
                name: "min".into(),
                cron: "0 * * * * *".into(), // every minute boundary
            }],
        )
        .unwrap();

        let now_ms = chrono::Utc::now().timestamp_millis() + 120_000;
        let first = tick_once(&db, now_ms).unwrap();
        let second = tick_once(&db, now_ms).unwrap();
        assert!(first >= 1);
        // Second tick shouldn't enqueue again for the same bucket — the
        // unique_key dedup catches it and last_run_at was advanced.
        assert_eq!(second, 0);
    }

    #[test]
    fn tick_and_reclaim_recovers_orphan_runs_and_wakes_supervisor() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tako_core::EnqueueOpts;

        let db = db();
        let r = db
            .enqueue("w", &serde_json::json!({}), &EnqueueOpts::default())
            .unwrap();
        // Simulate a worker that claimed the run and then died — lease is
        // in the past, run is still `running`.
        db.claim("dead-worker", &["w".into()], 30_000).unwrap();
        {
            let conn = db.lock_conn();
            conn.execute(
                "UPDATE runs SET lease_until = ?1 WHERE id = ?2",
                rusqlite::params![chrono::Utc::now().timestamp_millis() - 1_000, r.id],
            )
            .unwrap();
        }

        let wakes = Arc::new(AtomicUsize::new(0));
        let w = wakes.clone();
        let on_enqueue = move || {
            w.fetch_add(1, Ordering::SeqCst);
        };

        tick_and_reclaim(
            &db,
            chrono::Utc::now().timestamp_millis(),
            &on_enqueue,
            None,
        );

        assert_eq!(
            wakes.load(Ordering::SeqCst),
            1,
            "wake should fire on reclaim"
        );
        let next = db.claim("new-worker", &["w".into()], 30_000).unwrap();
        assert_eq!(
            next.map(|r| r.id),
            Some(r.id),
            "reclaimed run should be claimable"
        );
    }

    #[test]
    fn tick_and_reclaim_does_not_wake_when_nothing_to_do() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let db = db();
        let wakes = Arc::new(AtomicUsize::new(0));
        let w = wakes.clone();
        let on_enqueue = move || {
            w.fetch_add(1, Ordering::SeqCst);
        };

        tick_and_reclaim(
            &db,
            chrono::Utc::now().timestamp_millis(),
            &on_enqueue,
            None,
        );

        assert_eq!(wakes.load(Ordering::SeqCst), 0);
    }
}
