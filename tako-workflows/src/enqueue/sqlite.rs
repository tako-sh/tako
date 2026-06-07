use parking_lot::Mutex;
use rusqlite::{Connection, OptionalExtension, params};
use std::collections::HashMap;
use std::path::Path;
use tako_core::{EnqueueOpts, EnqueueRunResponse, RunPayload, ScheduleSpec};

use super::{RunsDbError, ScheduleRow, clamp_lease_ms, now_ms};
use crate::schema;

const DEFAULT_MAX_ATTEMPTS: u32 = 3;

pub(super) struct SqliteRunsDb {
    conn: Mutex<Connection>,
}

impl SqliteRunsDb {
    pub(super) fn open(path: &Path) -> Result<Self, RunsDbError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                RunsDbError::Sqlite(rusqlite::Error::ToSqlConversionFailure(Box::new(e)))
            })?;
        }
        let conn = Connection::open(path)?;
        schema::init(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    #[cfg(test)]
    pub(super) fn open_in_memory() -> Result<Self, RunsDbError> {
        let conn = Connection::open_in_memory()?;
        schema::init(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    #[cfg(test)]
    pub(super) fn lock_conn(&self) -> parking_lot::MutexGuard<'_, Connection> {
        self.conn.lock()
    }

    pub(super) fn enqueue(
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

        let mut conn = self.conn.lock();
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

    pub(super) fn claim(
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
            let conn = self.conn.lock();
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

    pub(super) fn heartbeat(
        &self,
        id: &str,
        worker_id: &str,
        lease_ms: u64,
    ) -> Result<(), RunsDbError> {
        let lease_until = now_ms().saturating_add(clamp_lease_ms(lease_ms));
        let conn = self.conn.lock();
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

    pub(super) fn save_step(
        &self,
        run_id: &str,
        worker_id: &str,
        step_name: &str,
        result: &serde_json::Value,
    ) -> Result<(), RunsDbError> {
        let r = serde_json::to_string(result)?;
        let conn = self.conn.lock();
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
                Some(Some(wid)) if wid == worker_id => {}
                _ => return Err(RunsDbError::StaleWorker),
            }
        }
        Ok(())
    }

    pub(super) fn complete(&self, id: &str, worker_id: &str) -> Result<(), RunsDbError> {
        let conn = self.conn.lock();
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

    pub(super) fn cancel(
        &self,
        id: &str,
        worker_id: &str,
        reason: Option<&str>,
    ) -> Result<(), RunsDbError> {
        let conn = self.conn.lock();
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

    pub(super) fn fail(
        &self,
        id: &str,
        worker_id: &str,
        error: &str,
        next_run_at_ms: Option<i64>,
        finalize: bool,
    ) -> Result<(), RunsDbError> {
        let conn = self.conn.lock();
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

    pub(super) fn defer(
        &self,
        id: &str,
        worker_id: &str,
        wake_at_ms: Option<i64>,
    ) -> Result<(), RunsDbError> {
        let conn = self.conn.lock();
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

    pub(super) fn reclaim_expired(&self) -> Result<u64, RunsDbError> {
        let conn = self.conn.lock();
        let changes = conn.execute(
            "UPDATE runs SET status='pending', worker_id=NULL, lease_until=NULL
             WHERE status='running' AND lease_until IS NOT NULL AND lease_until < ?1",
            params![now_ms()],
        )?;
        Ok(changes as u64)
    }

    pub(super) fn reclaim_expired_with_workers(&self) -> Result<Vec<String>, RunsDbError> {
        let mut conn = self.conn.lock();
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

    pub(super) fn in_flight_by_worker(&self) -> Result<HashMap<String, u32>, RunsDbError> {
        let conn = self.conn.lock();
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
        let mut out = HashMap::new();
        for r in rows {
            let (worker, count) = r?;
            out.insert(worker, count);
        }
        Ok(out)
    }

    pub(super) fn wait_for_event(
        &self,
        run_id: &str,
        worker_id: &str,
        step_name: &str,
        event_name: &str,
        timeout_at_ms: Option<i64>,
    ) -> Result<(), RunsDbError> {
        let mut conn = self.conn.lock();
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

    pub(super) fn signal(
        &self,
        event_name: &str,
        payload: &serde_json::Value,
    ) -> Result<u64, RunsDbError> {
        let payload_json = serde_json::to_string(payload)?;
        let now = now_ms();
        let mut conn = self.conn.lock();
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

    pub(super) fn pending_count(&self) -> Result<u64, RunsDbError> {
        let conn = self.conn.lock();
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM runs WHERE status='pending'",
            [],
            |row| row.get(0),
        )?;
        Ok(count as u64)
    }

    pub(super) fn has_runnable_work(&self) -> Result<bool, RunsDbError> {
        let conn = self.conn.lock();
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

    pub(super) fn replace_schedules(&self, schedules: &[ScheduleSpec]) -> Result<(), RunsDbError> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;

        let names: Vec<String> = schedules.iter().map(|s| s.name.clone()).collect();
        if names.is_empty() {
            tx.execute("DELETE FROM schedules", [])?;
        } else {
            let placeholders = names.iter().map(|_| "?").collect::<Vec<_>>().join(",");
            let sql = format!("DELETE FROM schedules WHERE name NOT IN ({})", placeholders);
            let params: Vec<&dyn rusqlite::ToSql> =
                names.iter().map(|n| n as &dyn rusqlite::ToSql).collect();
            tx.execute(&sql, &params[..])?;
        }

        let now_ms = chrono::Utc::now().timestamp_millis();
        for s in schedules {
            tx.execute(
                "INSERT INTO schedules (name, cron, last_run_at) VALUES (?1, ?2, ?3)
                 ON CONFLICT(name) DO UPDATE SET cron = excluded.cron",
                params![s.name, s.cron, now_ms],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub(super) fn list_schedules(&self) -> Result<Vec<ScheduleRow>, RunsDbError> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare("SELECT name, cron, last_run_at FROM schedules")?;
        let rows = stmt
            .query_map([], |row| {
                Ok(ScheduleRow {
                    name: row.get(0)?,
                    cron: row.get(1)?,
                    last_run_at: row.get(2)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub(super) fn set_schedule_last_run_at(&self, name: &str, ts: i64) -> Result<(), RunsDbError> {
        let conn = self.conn.lock();
        conn.execute(
            "UPDATE schedules SET last_run_at = ?1 WHERE name = ?2",
            params![ts, name],
        )?;
        Ok(())
    }
}
