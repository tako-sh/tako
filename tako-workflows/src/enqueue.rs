//! Run enqueue + per-step persistence + lifecycle transitions.
//!
//! All run state lives in two tables:
//!   - `runs` — one row per run; tracks status, attempts, lease.
//!   - `steps` — append-only memoization of completed step results.
//!
//! Operations are synchronous; call from `tokio::task::spawn_blocking` when
//! invoking from async contexts.

use parking_lot::Mutex;
use rusqlite::{Connection, OptionalExtension, params};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use tako_core::{EnqueueOpts, EnqueueRunResponse, RunPayload};

use super::schema;

const DEFAULT_MAX_ATTEMPTS: u32 = 3;
pub const POSTGRES_WORKFLOWS_SCHEMA: &str = "tako_workflows";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkflowStoreConfig {
    Sqlite {
        path: PathBuf,
    },
    Postgres {
        url: String,
        schema: String,
        app_id: String,
    },
}

impl WorkflowStoreConfig {
    pub fn sqlite(path: impl Into<PathBuf>) -> Self {
        Self::Sqlite { path: path.into() }
    }

    pub fn postgres(url: impl Into<String>, app_id: impl Into<String>) -> Self {
        Self::Postgres {
            url: url.into(),
            schema: POSTGRES_WORKFLOWS_SCHEMA.to_string(),
            app_id: app_id.into(),
        }
    }
}

/// Cap for client-supplied `lease_ms`. One week is far longer than any
/// legitimate workflow step; anything larger is a misuse or hostile input
/// that would wrap `i64` arithmetic below if passed through directly.
const MAX_LEASE_MS: u64 = 7 * 24 * 60 * 60 * 1000;

/// Saturating cast of client-supplied lease length (in ms) to the signed
/// millisecond value we store in sqlite. Prevents `u64::MAX as i64 → -1`
/// from producing a negative `lease_until` that would be reclaimed
/// instantly.
fn clamp_lease_ms(lease_ms: u64) -> i64 {
    lease_ms.min(MAX_LEASE_MS) as i64
}

#[derive(thiserror::Error, Debug)]
pub enum RunsDbError {
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("{0}")]
    UnsupportedBackend(String),
    /// The run is no longer owned by the caller (lease expired and was
    /// reclaimed by another worker, or the run already terminated).
    #[error("stale worker: run is no longer owned by this worker")]
    StaleWorker,
}

pub struct RunsDb {
    backend: RunsDbBackend,
}

enum RunsDbBackend {
    Sqlite(SqliteRunsDb),
}

struct SqliteRunsDb {
    conn: Mutex<Connection>,
}

impl RunsDb {
    pub fn open_config(config: WorkflowStoreConfig) -> Result<Self, RunsDbError> {
        match config {
            WorkflowStoreConfig::Sqlite { path } => Self::open_sqlite(&path),
            WorkflowStoreConfig::Postgres { schema, .. } => Err(RunsDbError::UnsupportedBackend(
                format!("postgres workflow storage is not implemented yet (schema {schema})"),
            )),
        }
    }

    pub fn open(path: &Path) -> Result<Self, RunsDbError> {
        Self::open_sqlite(path)
    }

    pub fn open_sqlite(path: &Path) -> Result<Self, RunsDbError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                RunsDbError::Sqlite(rusqlite::Error::ToSqlConversionFailure(Box::new(e)))
            })?;
        }
        let conn = Connection::open(path)?;
        schema::init(&conn)?;
        Ok(Self {
            backend: RunsDbBackend::Sqlite(SqliteRunsDb {
                conn: Mutex::new(conn),
            }),
        })
    }

    pub fn open_postgres(url: &str, app_id: &str) -> Result<Self, RunsDbError> {
        Self::open_config(WorkflowStoreConfig::postgres(url, app_id))
    }

    #[cfg(test)]
    pub fn open_in_memory() -> Result<Self, RunsDbError> {
        let conn = Connection::open_in_memory()?;
        schema::init(&conn)?;
        Ok(Self {
            backend: RunsDbBackend::Sqlite(SqliteRunsDb {
                conn: Mutex::new(conn),
            }),
        })
    }

    fn sqlite(&self) -> &SqliteRunsDb {
        match &self.backend {
            RunsDbBackend::Sqlite(db) => db,
        }
    }

    /// Insert a new run, or return the id of an existing non-terminal run
    /// with the same `unique_key` if one exists.
    pub fn enqueue(
        &self,
        name: &str,
        payload: &serde_json::Value,
        opts: &EnqueueOpts,
    ) -> Result<EnqueueRunResponse, RunsDbError> {
        let now_ms = now_ms();
        let run_at = opts.run_at_ms.unwrap_or(now_ms);
        let max_attempts = opts.max_attempts.unwrap_or(DEFAULT_MAX_ATTEMPTS) as i64;
        let unique_key = opts.unique_key.as_deref();
        let payload_json = serde_json::to_string(payload)?;
        let id = nanoid::nanoid!();

        let mut conn = self.sqlite().conn.lock();
        let tx = conn.transaction()?;

        if let Some(key) = unique_key {
            let existing: Option<String> = {
                let mut stmt = tx.prepare_cached(
                    "SELECT id FROM runs WHERE unique_key = ?1 AND status IN ('pending','running') LIMIT 1",
                )?;
                stmt.query_row(params![key], |row| row.get(0)).optional()?
            };
            if let Some(id) = existing {
                tx.commit()?;
                drop(conn);
                return Ok(EnqueueRunResponse {
                    id,
                    deduplicated: true,
                });
            }
        }

        {
            let mut stmt = tx.prepare_cached(
                "INSERT INTO runs
                 (id, name, payload, status, attempts, max_attempts, run_at, lease_until, worker_id,
                  last_error, created_at, unique_key)
                 VALUES (?1, ?2, ?3, 'pending', 0, ?4, ?5, NULL, NULL, NULL, ?6, ?7)",
            )?;
            stmt.execute(params![
                id,
                name,
                payload_json,
                max_attempts,
                run_at,
                now_ms,
                unique_key
            ])?;
        }
        tx.commit()?;
        drop(conn);

        Ok(EnqueueRunResponse {
            id,
            deduplicated: false,
        })
    }

    pub(crate) fn lock_conn(&self) -> parking_lot::MutexGuard<'_, Connection> {
        self.sqlite().conn.lock()
    }

    /// Atomically claim the oldest eligible run for one of `names`. Bumps
    /// `attempts`. Returns `None` when nothing is due. Loads any persisted
    /// step results into `step_state`.
    pub fn claim(
        &self,
        worker_id: &str,
        names: &[String],
        lease_ms: u64,
    ) -> Result<Option<RunPayload>, RunsDbError> {
        if names.is_empty() {
            return Ok(None);
        }
        let now = now_ms();
        let lease_until = now.saturating_add(clamp_lease_ms(lease_ms));
        let placeholders = names.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let sql = format!(
            "UPDATE runs
             SET status='running', worker_id=?, lease_until=?, attempts=attempts+1
             WHERE id = (
                 SELECT id FROM runs
                 WHERE status='pending' AND run_at <= ? AND name IN ({})
                 ORDER BY run_at
                 LIMIT 1
             )
             RETURNING id, name, payload, status, attempts, max_attempts, run_at",
            placeholders
        );
        let mut params: Vec<Box<dyn rusqlite::ToSql>> = Vec::with_capacity(3 + names.len());
        params.push(Box::new(worker_id.to_string()));
        params.push(Box::new(lease_until));
        params.push(Box::new(now));
        for n in names {
            params.push(Box::new(n.clone()));
        }
        let refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(|b| b.as_ref()).collect();

        let (claimed, step_rows) = {
            let conn = self.sqlite().conn.lock();
            let mut stmt = conn.prepare_cached(&sql)?;
            let row_opt = stmt
                .query_row(&refs[..], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, i64>(4)? as u32,
                        row.get::<_, i64>(5)? as u32,
                        row.get::<_, i64>(6)?,
                    ))
                })
                .optional()?;
            drop(stmt);

            let Some(claimed) = row_opt else {
                return Ok(None);
            };

            let mut step_stmt =
                conn.prepare_cached("SELECT name, result FROM steps WHERE run_id = ?1")?;
            let rows = step_stmt.query_map(params![claimed.0.as_str()], |row| {
                let name: String = row.get(0)?;
                let result: String = row.get(1)?;
                Ok((name, result))
            })?;
            let step_rows = rows.collect::<rusqlite::Result<Vec<_>>>()?;
            (claimed, step_rows)
        };

        let mut state_map = serde_json::Map::new();
        for (name, result) in step_rows {
            let value = serde_json::from_str(&result).unwrap_or(serde_json::Value::Null);
            state_map.insert(name, value);
        }

        Ok(Some(RunPayload {
            id: claimed.0,
            name: claimed.1,
            payload: serde_json::from_str(&claimed.2).unwrap_or(serde_json::Value::Null),
            status: claimed.3,
            attempts: claimed.4,
            max_attempts: claimed.5,
            run_at_ms: claimed.6,
            step_state: serde_json::Value::Object(state_map),
        }))
    }

    /// All of these lifecycle writes are guarded by `worker_id = ?1 AND
    /// status = 'running'`. If a worker's lease expired and another worker
    /// already reclaimed the run, the UPDATE affects 0 rows and we return
    /// `StaleWorker` so the SDK can log/raise rather than silently
    /// marking the run in a state the new worker didn't intend.
    pub fn heartbeat(&self, id: &str, worker_id: &str, lease_ms: u64) -> Result<(), RunsDbError> {
        let lease_until = now_ms().saturating_add(clamp_lease_ms(lease_ms));
        let conn = self.sqlite().conn.lock();
        let rows = conn.execute(
            "UPDATE runs SET lease_until = ?1
             WHERE id = ?2 AND worker_id = ?3 AND status='running'",
            params![lease_until, id, worker_id],
        )?;
        if rows == 0 {
            return Err(RunsDbError::StaleWorker);
        }
        Ok(())
    }

    /// Persist a single completed step result. Guarded by the run's
    /// current `worker_id` so a stale worker that lost its lease can't
    /// write step results into a different worker's in-flight run.
    /// First-write-wins on `(run_id, name)` — a duplicate save after a
    /// failed RPC is silently deduped.
    pub fn save_step(
        &self,
        run_id: &str,
        worker_id: &str,
        step_name: &str,
        result: &serde_json::Value,
    ) -> Result<(), RunsDbError> {
        let r = serde_json::to_string(result)?;
        let conn = self.sqlite().conn.lock();
        let rows = conn.execute(
            "INSERT OR IGNORE INTO steps (run_id, name, result, completed_at)
             SELECT ?1, ?2, ?3, ?4
             FROM runs WHERE id = ?1 AND worker_id = ?5 AND status='running'",
            params![run_id, step_name, r, now_ms(), worker_id],
        )?;
        // rows == 0 can mean "step already saved (IGNORE)" or "stale
        // worker". Distinguish by probing the run's worker_id.
        if rows == 0 {
            let current: Option<Option<String>> = conn
                .query_row(
                    "SELECT worker_id FROM runs WHERE id = ?1 AND status='running'",
                    params![run_id],
                    |row| row.get(0),
                )
                .optional()?;
            match current {
                Some(Some(wid)) if wid == worker_id => { /* duplicate save, fine */ }
                _ => return Err(RunsDbError::StaleWorker),
            }
        }
        Ok(())
    }

    pub fn complete(&self, id: &str, worker_id: &str) -> Result<(), RunsDbError> {
        let conn = self.sqlite().conn.lock();
        let rows = conn.execute(
            "UPDATE runs SET status='succeeded', worker_id=NULL, lease_until=NULL
             WHERE id = ?1 AND worker_id = ?2 AND status='running'",
            params![id, worker_id],
        )?;
        if rows == 0 {
            return Err(RunsDbError::StaleWorker);
        }
        Ok(())
    }

    pub fn cancel(
        &self,
        id: &str,
        worker_id: &str,
        reason: Option<&str>,
    ) -> Result<(), RunsDbError> {
        let conn = self.sqlite().conn.lock();
        let rows = conn.execute(
            "UPDATE runs SET status='cancelled', last_error=?1, worker_id=NULL, lease_until=NULL
             WHERE id = ?2 AND worker_id = ?3 AND status='running'",
            params![reason, id, worker_id],
        )?;
        if rows == 0 {
            return Err(RunsDbError::StaleWorker);
        }
        Ok(())
    }

    pub fn fail(
        &self,
        id: &str,
        worker_id: &str,
        error: &str,
        next_run_at_ms: Option<i64>,
        finalize: bool,
    ) -> Result<(), RunsDbError> {
        let conn = self.sqlite().conn.lock();
        let rows = if finalize {
            conn.execute(
                "UPDATE runs SET status='dead', last_error=?1, worker_id=NULL, lease_until=NULL
                 WHERE id = ?2 AND worker_id = ?3 AND status='running'",
                params![error, id, worker_id],
            )?
        } else {
            let next = next_run_at_ms.ok_or_else(|| {
                RunsDbError::Sqlite(rusqlite::Error::ToSqlConversionFailure(Box::new(
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        "fail(finalize=false) requires next_run_at_ms",
                    ),
                )))
            })?;
            conn.execute(
                "UPDATE runs SET status='pending', last_error=?1, worker_id=NULL, lease_until=NULL, run_at=?2
                 WHERE id = ?3 AND worker_id = ?4 AND status='running'",
                params![error, next, id, worker_id],
            )?
        };
        if rows == 0 {
            return Err(RunsDbError::StaleWorker);
        }
        Ok(())
    }

    /// Reschedule a run for later without bumping attempts (for durable
    /// `ctx.sleep` and `ctx.waitFor` parking). When `wake_at_ms` is None
    /// the run is parked indefinitely (waiting for an event).
    pub fn defer(
        &self,
        id: &str,
        worker_id: &str,
        wake_at_ms: Option<i64>,
    ) -> Result<(), RunsDbError> {
        let conn = self.sqlite().conn.lock();
        let run_at = wake_at_ms.unwrap_or(i64::MAX);
        let rows = conn.execute(
            "UPDATE runs SET status='pending', worker_id=NULL, lease_until=NULL,
                              run_at=?1, attempts=attempts-1
             WHERE id = ?2 AND worker_id = ?3 AND status='running'",
            params![run_at, id, worker_id],
        )?;
        if rows == 0 {
            return Err(RunsDbError::StaleWorker);
        }
        Ok(())
    }

    pub fn reclaim_expired(&self) -> Result<u64, RunsDbError> {
        let conn = self.sqlite().conn.lock();
        let changes = conn.execute(
            "UPDATE runs SET status='pending', worker_id=NULL, lease_until=NULL
             WHERE status='running' AND lease_until IS NOT NULL AND lease_until < ?1",
            params![now_ms()],
        )?;
        Ok(changes as u64)
    }

    /// Atomically reclaim expired leases and return the list of
    /// `worker_id`s whose runs were reclaimed, one entry per reclaimed
    /// row (so callers can decrement per-worker in-flight counters).
    pub fn reclaim_expired_with_workers(&self) -> Result<Vec<String>, RunsDbError> {
        let mut conn = self.sqlite().conn.lock();
        let tx = conn.transaction()?;
        let workers: Vec<String> = {
            let mut stmt = tx.prepare(
                "SELECT worker_id FROM runs
                 WHERE status='running' AND lease_until IS NOT NULL
                   AND lease_until < ?1 AND worker_id IS NOT NULL",
            )?;
            let rows = stmt.query_map(params![now_ms()], |row| row.get::<_, String>(0))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        tx.execute(
            "UPDATE runs SET status='pending', worker_id=NULL, lease_until=NULL
             WHERE status='running' AND lease_until IS NOT NULL AND lease_until < ?1",
            params![now_ms()],
        )?;
        tx.commit()?;
        Ok(workers)
    }

    /// Snapshot of `worker_id -> in-flight count` over currently-running
    /// rows. Used to rehydrate [`InFlightLimiter`] on startup so cached
    /// counts match reality before the socket starts serving claims.
    pub fn in_flight_by_worker(
        &self,
    ) -> Result<std::collections::HashMap<String, u32>, RunsDbError> {
        let conn = self.sqlite().conn.lock();
        let mut stmt = conn.prepare(
            "SELECT worker_id, COUNT(*) FROM runs
             WHERE status='running' AND worker_id IS NOT NULL
             GROUP BY worker_id",
        )?;
        let rows = stmt.query_map([], |row| {
            let worker: String = row.get(0)?;
            let count: i64 = row.get(1)?;
            Ok((worker, count as u32))
        })?;
        let mut out = std::collections::HashMap::new();
        for r in rows {
            let (worker, count) = r?;
            out.insert(worker, count);
        }
        Ok(out)
    }

    /// Park a run waiting for a named event. Stores the waiter and defers
    /// the run. Wake happens via `signal` (or via run_at if a timeout was
    /// set and it elapses).
    pub fn wait_for_event(
        &self,
        run_id: &str,
        worker_id: &str,
        step_name: &str,
        event_name: &str,
        timeout_at_ms: Option<i64>,
    ) -> Result<(), RunsDbError> {
        let mut conn = self.sqlite().conn.lock();
        let tx = conn.transaction()?;
        let rows = tx.execute(
            "UPDATE runs SET status='pending', worker_id=NULL, lease_until=NULL,
                              run_at=?1, attempts=attempts-1
             WHERE id = ?2 AND worker_id = ?3 AND status='running'",
            params![timeout_at_ms.unwrap_or(i64::MAX), run_id, worker_id],
        )?;
        if rows == 0 {
            return Err(RunsDbError::StaleWorker);
        }
        tx.execute(
            "INSERT OR REPLACE INTO event_waiters (run_id, step_name, event_name, expires_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![run_id, step_name, event_name, timeout_at_ms],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Deliver an event payload. Wakes every parked waiter with matching
    /// `event_name`: the payload is stored as the waiter's step result,
    /// the waiter row is removed, and the run is set to pending. Returns
    /// the number of runs woken.
    pub fn signal(
        &self,
        event_name: &str,
        payload: &serde_json::Value,
    ) -> Result<u64, RunsDbError> {
        let payload_json = serde_json::to_string(payload)?;
        let now = now_ms();
        let mut conn = self.sqlite().conn.lock();
        let tx = conn.transaction()?;

        // Materialize the event payload as a step result for every waiter.
        // Then wake the runs and clear the waiter rows.
        let mut stmt =
            tx.prepare("SELECT run_id, step_name FROM event_waiters WHERE event_name = ?1")?;
        let waiters: Vec<(String, String)> = stmt
            .query_map(params![event_name], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<Result<Vec<_>, _>>()?;
        drop(stmt);

        let mut woken = 0u64;
        for (run_id, step_name) in &waiters {
            tx.execute(
                "INSERT OR IGNORE INTO steps (run_id, name, result, completed_at)
                 VALUES (?1, ?2, ?3, ?4)",
                params![run_id, step_name, payload_json, now],
            )?;
            tx.execute(
                "UPDATE runs SET status='pending', run_at=?1 WHERE id = ?2 AND status='pending'",
                params![now, run_id],
            )?;
            tx.execute(
                "DELETE FROM event_waiters WHERE run_id = ?1 AND step_name = ?2",
                params![run_id, step_name],
            )?;
            woken += 1;
        }
        tx.commit()?;
        Ok(woken)
    }

    pub fn pending_count(&self) -> Result<u64, RunsDbError> {
        let conn = self.sqlite().conn.lock();
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM runs WHERE status='pending'",
            [],
            |row| row.get(0),
        )?;
        Ok(count as u64)
    }

    /// Returns true when at least one pending run is due for workers to
    /// claim now. Future `run_at` rows stay durable without waking a
    /// scale-to-zero worker until the dispatcher scan sees them become due.
    pub fn has_runnable_work(&self) -> Result<bool, RunsDbError> {
        let conn = self.sqlite().conn.lock();
        let exists: i64 = conn.query_row(
            "SELECT EXISTS(
                SELECT 1 FROM runs
                WHERE status='pending' AND run_at <= ?1
                LIMIT 1
             )",
            params![now_ms()],
            |row| row.get(0),
        )?;
        Ok(exists != 0)
    }
}

pub(crate) fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

#[cfg(test)]
mod tests;
