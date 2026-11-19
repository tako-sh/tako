use parking_lot::Mutex;
use rusqlite::OptionalExtension;
use rusqlite::types::Value;
use rusqlite::{Connection, params, params_from_iter};
use std::collections::HashMap;
use std::path::Path;
use tako_core::{EnqueueOpts, EnqueueRunResponse, RunPayload, ScheduleSpec};

use super::operator::{
    CancelledRun, OPERATOR_CANCEL_REASON, RunRecord, RunRow, SignalRow, StepRow, SummaryRow,
    WaitRow, parse_status,
};
use super::{RunsDbError, ScheduleRow, clamp_lease_ms, now_ms};
use crate::schema;
use tako_core::{SignalRunStatus, WORKFLOW_RUN_MAX_SIGNALS, WorkflowRunStatus};

const DEFAULT_MAX_ATTEMPTS: u32 = 3;

pub(super) struct SqliteRunsDb {
    conn: Mutex<Connection>,
}

impl SqliteRunsDb {
    pub(super) fn open(path: &Path) -> Result<Self, RunsDbError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| RunsDbError::Storage(format!("create workflow dir: {e}")))?;
        }
        let conn = tako_sqlite::open_local(path)?;
        schema::init(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    #[cfg(test)]
    pub(super) fn open_in_memory() -> Result<Self, RunsDbError> {
        let conn = tako_sqlite::open_in_memory()?;
        schema::init(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    #[cfg(test)]
    pub(super) fn raw_execute(&self, sql: &str, params: impl rusqlite::Params) {
        let conn = self.conn.lock();
        conn.execute(sql, params).expect("raw execute");
    }

    #[cfg(test)]
    pub(super) fn raw_query_values(&self, sql: &str, params: impl rusqlite::Params) -> Vec<Value> {
        let conn = self.conn.lock();
        conn.query_row(sql, params, |row| {
            let n = row.as_ref().column_count();
            (0..n).map(|i| row.get(i)).collect()
        })
        .expect("raw query")
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
        let payload_json = serde_json::to_string(payload)?;
        let id = opts.id.clone().unwrap_or_else(|| nanoid::nanoid!());

        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        let inserted = tx.execute(
            "INSERT INTO runs
             (id, name, payload, status, attempts, max_attempts, run_at, lease_until, worker_id,
              last_error, created_at)
             VALUES (?1, ?2, ?3, 'pending', 0, ?4, ?5, NULL, NULL, NULL, ?6)
             ON CONFLICT(id) DO NOTHING",
            (
                id.as_str(),
                name,
                payload_json.as_str(),
                max_attempts,
                run_at,
                now_ms,
            ),
        )?;
        if inserted == 0 {
            let (existing_name, existing_payload): (String, String) = tx.query_row(
                "SELECT name, payload FROM runs WHERE id = ?1",
                [id.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            super::check_existing_run(&id, name, payload, &existing_name, &existing_payload)?;
        }
        tx.commit()?;
        Ok(EnqueueRunResponse {
            id,
            deduplicated: inserted == 0,
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
        let mut params: Vec<Value> = Vec::with_capacity(3 + names.len());
        params.push(Value::Text(worker_id.to_string()));
        params.push(Value::Integer(lease_until));
        params.push(Value::Integer(now));
        for n in names {
            params.push(Value::Text(n.clone()));
        }

        let mut conn = self.conn.lock();
        // Claim, expired-wait materialization, and checkpoint hydration form
        // one transaction, including when another server connection is active.
        let tx = conn.transaction()?;
        let claimed = {
            let mut stmt = tx.prepare_cached(&sql)?;
            let mut rows = stmt.query(params_from_iter(params))?;
            match rows.next()? {
                Some(row) => Some((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, i64>(4)? as u32,
                    row.get::<_, i64>(5)? as u32,
                    row.get::<_, i64>(6)?,
                )),
                None => None,
            }
        };

        let Some(claimed) = claimed else {
            return Ok(None);
        };

        tx.execute(
            "INSERT OR IGNORE INTO steps (run_id, name, result, completed_at)
             SELECT run_id, step_name, 'null', ?1
             FROM event_waiters
             WHERE run_id = ?2
               AND expires_at IS NOT NULL
               AND expires_at <= ?1",
            params![now, claimed.0.as_str()],
        )?;
        tx.execute(
            "DELETE FROM event_waiters
             WHERE run_id = ?1
               AND expires_at IS NOT NULL
               AND expires_at <= ?2",
            params![claimed.0.as_str(), now],
        )?;

        let step_rows = {
            let mut step_stmt =
                tx.prepare_cached("SELECT name, result FROM steps WHERE run_id = ?1")?;
            let rows = step_stmt.query_map([claimed.0.as_str()], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;
            rows.collect::<Result<Vec<_>, _>>()?
        };

        let mut state_map = serde_json::Map::new();
        for (name, result) in step_rows {
            let value = serde_json::from_str(&result).unwrap_or(serde_json::Value::Null);
            state_map.insert(name, value);
        }

        tx.commit()?;
        Ok(Some(RunPayload {
            id: claimed.0,
            worker_id: worker_id.to_owned(),
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
            (lease_until, id, worker_id),
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
            (run_id, step_name, r.as_str(), now_ms(), worker_id),
        )?;
        // rows == 0 can mean "step already saved (IGNORE)" or "stale
        // worker". Distinguish by probing the run's worker_id.
        if rows == 0 {
            match conn.query_row(
                "SELECT worker_id FROM runs WHERE id = ?1 AND status='running'",
                [run_id],
                |row| row.get::<_, Option<String>>(0),
            ) {
                Ok(Some(wid)) if wid == worker_id => {}
                Ok(_) => return Err(RunsDbError::StaleWorker),
                Err(rusqlite::Error::QueryReturnedNoRows) => return Err(RunsDbError::StaleWorker),
                Err(e) => return Err(e.into()),
            }
        }
        Ok(())
    }

    pub(super) fn result(
        &self,
        name: &str,
        id: &str,
    ) -> Result<Option<super::RunResult>, RunsDbError> {
        let conn = self.conn.lock();
        let row = conn
            .query_row(
                "SELECT status, result, last_error FROM runs WHERE id=?1 AND name=?2",
                (id, name),
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, Option<String>>(2)?,
                    ))
                },
            )
            .optional()?;
        row.map(|(status, result, error)| {
            Ok(super::RunResult {
                status,
                result: result
                    .map(|value| serde_json::from_str(&value))
                    .transpose()?
                    .unwrap_or(serde_json::Value::Null),
                error,
            })
        })
        .transpose()
    }

    pub(super) fn complete(
        &self,
        id: &str,
        worker_id: &str,
        result: &serde_json::Value,
    ) -> Result<(), RunsDbError> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        let rows = tx.execute(
            "UPDATE runs SET status='succeeded', result=?4, last_error=NULL, worker_id=NULL, lease_until=NULL, finished_at=?1
             WHERE id = ?2 AND worker_id = ?3 AND status='running'",
            (now_ms(), id, worker_id, serde_json::to_string(result)?),
        )?;
        if rows == 0 {
            return Err(RunsDbError::StaleWorker);
        }
        wake_run_waiters(&tx, id)?;
        tx.commit()?;
        Ok(())
    }

    pub(super) fn cancel(
        &self,
        id: &str,
        worker_id: &str,
        reason: Option<&str>,
    ) -> Result<(), RunsDbError> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        let rows = tx.execute(
            "UPDATE runs SET status='cancelled', last_error=?1, worker_id=NULL, lease_until=NULL,
                             finished_at=?2
             WHERE id = ?3 AND worker_id = ?4 AND status='running'",
            (reason, now_ms(), id, worker_id),
        )?;
        if rows == 0 {
            return Err(RunsDbError::StaleWorker);
        }
        wake_run_waiters(&tx, id)?;
        tx.commit()?;
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
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        let rows = if finalize {
            tx.execute(
                "UPDATE runs SET status='dead', last_error=?1, worker_id=NULL, lease_until=NULL,
                                 finished_at=?2
                 WHERE id = ?3 AND worker_id = ?4 AND status='running'",
                (error, now_ms(), id, worker_id),
            )?
        } else {
            let next = next_run_at_ms.ok_or_else(|| {
                RunsDbError::Storage("fail(finalize=false) requires next_run_at_ms".into())
            })?;
            tx.execute(
                "UPDATE runs SET status='pending', last_error=?1, worker_id=NULL, lease_until=NULL, run_at=?2
                 WHERE id = ?3 AND worker_id = ?4 AND status='running'",
                (error, next, id, worker_id),
            )?
        };
        if rows == 0 {
            return Err(RunsDbError::StaleWorker);
        }
        if finalize {
            wake_run_waiters(&tx, id)?;
        }
        tx.commit()?;
        Ok(())
    }

    pub(super) fn prune_finished_before(
        &self,
        cutoff_ms: i64,
        limit: u32,
    ) -> Result<u64, RunsDbError> {
        let conn = self.conn.lock();
        let deleted = conn.execute(
            "DELETE FROM runs WHERE id IN (
                SELECT id FROM runs
                WHERE status IN ('succeeded', 'cancelled', 'dead')
                  AND finished_at <= ?1
                ORDER BY finished_at
                LIMIT ?2
             )",
            (cutoff_ms, limit),
        )?;
        Ok(deleted as u64)
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
            (run_at, id, worker_id),
        )?;
        if rows == 0 {
            return Err(RunsDbError::StaleWorker);
        }
        Ok(())
    }

    pub(super) fn reclaim_expired(&self) -> Result<super::Reclaimed, RunsDbError> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let now = now_ms();
        let expired = {
            let mut stmt = tx.prepare(
                "SELECT id, worker_id, attempts >= max_attempts FROM runs
                 WHERE status='running' AND lease_until IS NOT NULL AND lease_until < ?1",
            )?;
            let rows = stmt.query_map((now,), |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, bool>(2)?,
                ))
            })?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        // The lost worker spent the attempt that claim counted. A run with
        // none left ends here; requeueing it would repeat the loss forever.
        tx.execute(
            "UPDATE runs SET status='dead', last_error=?2, worker_id=NULL, lease_until=NULL,
                             finished_at=?1
             WHERE status='running' AND lease_until IS NOT NULL AND lease_until < ?1
               AND attempts >= max_attempts",
            (now, super::WORKER_LOST_ERROR),
        )?;
        tx.execute(
            "UPDATE runs SET status='pending', worker_id=NULL, lease_until=NULL
             WHERE status='running' AND lease_until IS NOT NULL AND lease_until < ?1",
            (now,),
        )?;
        for (id, _, exhausted) in &expired {
            if *exhausted {
                wake_run_waiters(&tx, id)?;
            }
        }
        tx.commit()?;
        Ok(super::Reclaimed::from_rows(expired))
    }

    pub(super) fn in_flight_by_worker(&self) -> Result<HashMap<String, u32>, RunsDbError> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT worker_id, COUNT(*) FROM runs
             WHERE status='running' AND worker_id IS NOT NULL
             GROUP BY worker_id",
        )?;
        let rows = stmt.query_map((), |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)? as u32))
        })?;
        Ok(rows.collect::<Result<HashMap<_, _>, _>>()?)
    }

    pub(super) fn wait_for_event(
        &self,
        run_id: &str,
        worker_id: &str,
        step_name: &str,
        event_name: &str,
        timeout_at_ms: Option<i64>,
    ) -> Result<bool, RunsDbError> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        release_for_wait(&tx, run_id, worker_id, timeout_at_ms)?;
        if step_saved(&tx, run_id, step_name)? {
            requeue_now(&tx, run_id, step_name)?;
            tx.commit()?;
            return Ok(true);
        }
        // A signal addressed to this run arrived before the wait. Consume it
        // as the step result here, under the same write lock, so a signal can
        // never land between "nothing buffered" and "parked".
        let buffered: Option<(String, i64)> = tx
            .query_row(
                "SELECT payload, received_at FROM run_inbox WHERE run_id = ?1 AND event_name = ?2",
                (run_id, event_name),
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        if let Some((_, received_at)) = &buffered
            && super::arrived_late(*received_at, timeout_at_ms)
        {
            // The signal arrived after the wait's deadline: the wait timed
            // out. The late signal stays in the inbox, unconsumed.
            satisfy_wait(&tx, run_id, step_name, "null")?;
            tx.commit()?;
            return Ok(true);
        }
        if let Some((payload, _)) = buffered {
            tx.execute(
                "DELETE FROM run_inbox WHERE run_id = ?1 AND event_name = ?2",
                (run_id, event_name),
            )?;
            satisfy_wait(&tx, run_id, step_name, &payload)?;
            tx.commit()?;
            return Ok(true);
        }
        park(&tx, run_id, step_name, event_name, timeout_at_ms)?;
        tx.commit()?;
        Ok(false)
    }

    /// Park a run until `target_id` finishes and save its outcome as the
    /// step result. A finished, missing, or other-workflow target satisfies
    /// the wait at once. Returns true when the run is due again now.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn wait_for_run(
        &self,
        run_id: &str,
        worker_id: &str,
        step_name: &str,
        target_name: &str,
        target_id: &str,
        timeout_at_ms: Option<i64>,
    ) -> Result<bool, RunsDbError> {
        let mut conn = self.conn.lock();
        // The write lock orders this with the transaction that finishes the
        // target, which wakes its waiters: a waiter parked here is either
        // seen by it or sees its outcome.
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        release_for_wait(&tx, run_id, worker_id, timeout_at_ms)?;
        if step_saved(&tx, run_id, step_name)? {
            requeue_now(&tx, run_id, step_name)?;
            tx.commit()?;
            return Ok(true);
        }
        let outcome = match outcome_of(&tx, target_id)? {
            Some((name, outcome)) if name == target_name => parse_status(&outcome.status)?
                .is_terminal()
                .then_some(outcome),
            _ => Some(super::missing_outcome()),
        };
        if let Some(outcome) = outcome {
            let finished_at: Option<i64> = tx
                .query_row(
                    "SELECT finished_at FROM runs WHERE id = ?1",
                    [target_id],
                    |row| row.get(0),
                )
                .optional()?
                .flatten();
            // A target that finished after the wait's deadline leaves it timed out.
            let result = match finished_at {
                Some(finished_at) if super::arrived_late(finished_at, timeout_at_ms) => {
                    "null".to_string()
                }
                _ => serde_json::to_string(&outcome)?,
            };
            satisfy_wait(&tx, run_id, step_name, &result)?;
            tx.commit()?;
            return Ok(true);
        }
        park(
            &tx,
            run_id,
            step_name,
            &super::run_event_name(target_id),
            timeout_at_ms,
        )?;
        tx.commit()?;
        Ok(false)
    }

    /// Deliver `payload` to one run: wake it when it waits for `event_name`,
    /// otherwise keep it in the run's inbox until it does.
    pub(super) fn signal_run(
        &self,
        run_id: &str,
        event_name: &str,
        payload: &serde_json::Value,
    ) -> Result<SignalRunStatus, RunsDbError> {
        let payload_json = serde_json::to_string(payload)?;
        let now = now_ms();
        let mut conn = self.conn.lock();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let status: Option<String> = tx
            .query_row("SELECT status FROM runs WHERE id = ?1", [run_id], |row| {
                row.get(0)
            })
            .optional()?;
        let Some(status) = status else {
            return Ok(SignalRunStatus::Missing);
        };
        if parse_status(&status)?.is_terminal() {
            return Ok(SignalRunStatus::Finished);
        }
        // A wait past its deadline has timed out, even before its run is
        // claimed: it gets `null`, and this signal finds the name taken.
        expire_waits(&tx, event_name, Some(run_id), now)?;

        let waiters = {
            let mut stmt = tx.prepare_cached(
                "SELECT step_name FROM event_waiters WHERE run_id = ?1 AND event_name = ?2",
            )?;
            let rows = stmt.query_map((run_id, event_name), |row| row.get::<_, String>(0))?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        if !waiters.is_empty() {
            for step_name in &waiters {
                tx.execute(
                    "INSERT OR IGNORE INTO steps (run_id, name, result, completed_at)
                     VALUES (?1, ?2, ?3, ?4)",
                    (run_id, step_name.as_str(), payload_json.as_str(), now),
                )?;
                tx.execute(
                    "DELETE FROM event_waiters WHERE run_id = ?1 AND step_name = ?2",
                    (run_id, step_name.as_str()),
                )?;
            }
            tx.execute(
                "UPDATE runs SET run_at = ?1 WHERE id = ?2 AND status = 'pending'",
                (now, run_id),
            )?;
            tx.commit()?;
            return Ok(SignalRunStatus::Delivered);
        }

        let held: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM steps WHERE run_id = ?1 AND name = ?2)
                 OR EXISTS(SELECT 1 FROM run_inbox WHERE run_id = ?1 AND event_name = ?2)",
            (run_id, event_name),
            |row| row.get(0),
        )?;
        if held {
            return Ok(SignalRunStatus::Duplicate);
        }
        let (entries, bytes): (i64, i64) = tx.query_row(
            "SELECT COUNT(*), COALESCE(SUM(payload_bytes), 0) FROM run_inbox WHERE run_id = ?1",
            [run_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        super::check_inbox_room(run_id, entries, bytes, payload_json.len())?;
        tx.execute(
            "INSERT INTO run_inbox (run_id, event_name, payload, payload_bytes, received_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            (
                run_id,
                event_name,
                payload_json.as_str(),
                payload_json.len() as i64,
                now,
            ),
        )?;
        tx.commit()?;
        Ok(SignalRunStatus::Buffered)
    }

    pub(super) fn signal(
        &self,
        event_name: &str,
        payload: &serde_json::Value,
    ) -> Result<u64, RunsDbError> {
        let payload_json = serde_json::to_string(payload)?;
        let now = now_ms();
        let mut conn = self.conn.lock();
        // Acquire the writer lock before reading waiters. A deferred transaction
        // cannot wait on a competing writer when upgrading an existing snapshot.
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        expire_waits(&tx, event_name, None, now)?;
        let waiters = {
            let mut stmt =
                tx.prepare("SELECT run_id, step_name FROM event_waiters WHERE event_name = ?1")?;
            let rows = stmt.query_map((event_name,), |row| Ok((row.get(0)?, row.get(1)?)))?;
            rows.collect::<Result<Vec<(String, String)>, _>>()?
        };

        let mut woken = 0u64;
        for (run_id, step_name) in &waiters {
            tx.execute(
                "INSERT OR IGNORE INTO steps (run_id, name, result, completed_at)
                 VALUES (?1, ?2, ?3, ?4)",
                (
                    run_id.as_str(),
                    step_name.as_str(),
                    payload_json.as_str(),
                    now,
                ),
            )?;
            tx.execute(
                "UPDATE runs SET status='pending', run_at=?1 WHERE id = ?2 AND status='pending'",
                (now, run_id.as_str()),
            )?;
            tx.execute(
                "DELETE FROM event_waiters WHERE run_id = ?1 AND step_name = ?2",
                (run_id.as_str(), step_name.as_str()),
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
            (),
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
            (now_ms(),),
            |row| row.get(0),
        )?;
        Ok(exists != 0)
    }

    pub(super) fn list_runs(
        &self,
        status: Option<&str>,
        name: Option<&str>,
        limit: u32,
    ) -> Result<Vec<SummaryRow>, RunsDbError> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare_cached(
            "SELECT id, name, status, attempts, max_attempts, run_at, created_at, finished_at,
                    last_error
             FROM runs
             WHERE (?1 IS NULL OR status = ?1) AND (?2 IS NULL OR name = ?2)
             ORDER BY created_at DESC, id DESC
             LIMIT ?3",
        )?;
        let rows = stmt.query_map((status, name, limit), summary_row)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    pub(super) fn get_run(
        &self,
        id: &str,
        step_limit: i64,
    ) -> Result<Option<RunRecord>, RunsDbError> {
        let mut conn = self.conn.lock();
        // One snapshot, so the steps belong to the run state shown with them.
        let tx = conn.transaction()?;
        let run = tx
            .query_row(
                "SELECT id, name, status, attempts, max_attempts, run_at, created_at, finished_at,
                        last_error, payload, result, lease_until, worker_id
                 FROM runs WHERE id = ?1",
                [id],
                |row| {
                    Ok(RunRow {
                        summary: summary_row(row)?,
                        payload: row.get(9)?,
                        result: row.get(10)?,
                        lease_until: row.get(11)?,
                        worker_id: row.get(12)?,
                    })
                },
            )
            .optional()?;
        let Some(run) = run else {
            return Ok(None);
        };
        let steps = {
            let mut stmt = tx.prepare_cached(
                "SELECT name, result, completed_at FROM steps WHERE run_id = ?1
                 ORDER BY completed_at DESC, name DESC LIMIT ?2",
            )?;
            let rows = stmt.query_map((id, step_limit), |row| {
                Ok(StepRow {
                    name: row.get(0)?,
                    result: row.get(1)?,
                    completed_at: row.get(2)?,
                })
            })?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        let steps_total: i64 = tx.query_row(
            "SELECT COUNT(*) FROM steps WHERE run_id = ?1",
            [id],
            |row| row.get(0),
        )?;
        let waits = {
            let mut stmt = tx.prepare_cached(
                "SELECT step_name, event_name, expires_at FROM event_waiters WHERE run_id = ?1
                 ORDER BY step_name LIMIT ?2",
            )?;
            let rows = stmt.query_map((id, step_limit), |row| {
                Ok(WaitRow {
                    step_name: row.get(0)?,
                    event_name: row.get(1)?,
                    expires_at: row.get(2)?,
                })
            })?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        let signals = {
            let mut stmt = tx.prepare_cached(
                "SELECT event_name, received_at, payload_bytes FROM run_inbox WHERE run_id = ?1
                 ORDER BY received_at, event_name LIMIT ?2",
            )?;
            let rows = stmt.query_map((id, WORKFLOW_RUN_MAX_SIGNALS as i64), |row| {
                Ok(SignalRow {
                    event_name: row.get(0)?,
                    received_at: row.get(1)?,
                    bytes: row.get(2)?,
                })
            })?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        let signals_total: i64 = tx.query_row(
            "SELECT COUNT(*) FROM run_inbox WHERE run_id = ?1",
            [id],
            |row| row.get(0),
        )?;
        tx.commit()?;
        Ok(Some(RunRecord {
            run,
            steps,
            steps_total: steps_total as u64,
            waits,
            signals,
            signals_total: signals_total as u64,
        }))
    }

    pub(super) fn retry_run(&self, id: &str) -> Result<WorkflowRunStatus, RunsDbError> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let (status, _) = run_status(&tx, id)?;
        if !matches!(
            status,
            WorkflowRunStatus::Dead | WorkflowRunStatus::Cancelled
        ) {
            return Err(RunsDbError::RunNotRetryable {
                id: id.to_string(),
                status,
            });
        }
        tx.execute(
            "UPDATE runs SET status='pending', attempts=0, run_at=?1, lease_until=NULL,
                             worker_id=NULL, finished_at=NULL, result=NULL
             WHERE id = ?2",
            (now_ms(), id),
        )?;
        tx.execute("DELETE FROM event_waiters WHERE run_id = ?1", [id])?;
        tx.commit()?;
        Ok(status)
    }

    pub(super) fn cancel_run(&self, id: &str) -> Result<CancelledRun, RunsDbError> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let (status, worker_id) = run_status(&tx, id)?;
        if !matches!(
            status,
            WorkflowRunStatus::Pending | WorkflowRunStatus::Running
        ) {
            return Err(RunsDbError::RunAlreadyFinished {
                id: id.to_string(),
                status,
            });
        }
        tx.execute(
            "UPDATE runs SET status='cancelled', last_error=?1, worker_id=NULL, lease_until=NULL,
                             finished_at=?2
             WHERE id = ?3",
            (OPERATOR_CANCEL_REASON, now_ms(), id),
        )?;
        tx.execute("DELETE FROM event_waiters WHERE run_id = ?1", [id])?;
        wake_run_waiters(&tx, id)?;
        tx.commit()?;
        Ok(CancelledRun {
            previous_status: status,
            worker_id: worker_id.filter(|_| status == WorkflowRunStatus::Running),
        })
    }

    pub(super) fn replace_schedules(&self, schedules: &[ScheduleSpec]) -> Result<(), RunsDbError> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        if schedules.is_empty() {
            tx.execute("DELETE FROM schedules", ())?;
        } else {
            let placeholders = schedules.iter().map(|_| "?").collect::<Vec<_>>().join(",");
            let sql = format!("DELETE FROM schedules WHERE name NOT IN ({})", placeholders);
            let params: Vec<Value> = schedules
                .iter()
                .map(|s| Value::Text(s.name.clone()))
                .collect();
            tx.execute(&sql, params_from_iter(params))?;
        }

        let now_ms = chrono::Utc::now().timestamp_millis();
        for s in schedules {
            tx.execute(
                "INSERT INTO schedules (name, cron, last_run_at, max_attempts)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(name) DO UPDATE
                 SET cron = excluded.cron, max_attempts = excluded.max_attempts",
                (s.name.as_str(), s.cron.as_str(), now_ms, s.max_attempts),
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub(super) fn list_schedules(&self) -> Result<Vec<ScheduleRow>, RunsDbError> {
        let conn = self.conn.lock();
        let mut stmt =
            conn.prepare("SELECT name, cron, last_run_at, max_attempts FROM schedules")?;
        let rows = stmt.query_map((), |row| {
            Ok(ScheduleRow {
                name: row.get(0)?,
                cron: row.get(1)?,
                last_run_at: row.get::<_, Option<i64>>(2)?,
                max_attempts: row.get::<_, Option<u32>>(3)?,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    pub(super) fn set_schedule_last_run_at(&self, name: &str, ts: i64) -> Result<(), RunsDbError> {
        let conn = self.conn.lock();
        conn.execute(
            "UPDATE schedules SET last_run_at = ?1 WHERE name = ?2",
            (ts, name),
        )?;
        Ok(())
    }
}

fn summary_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<SummaryRow> {
    Ok(SummaryRow {
        id: row.get(0)?,
        name: row.get(1)?,
        status: row.get(2)?,
        attempts: row.get(3)?,
        max_attempts: row.get(4)?,
        run_at: row.get(5)?,
        created_at: row.get(6)?,
        finished_at: row.get(7)?,
        last_error: row.get(8)?,
    })
}

/// Release the claim of a run that is about to wait on a step. The run
/// returns to `pending`, due at the wait's timeout, and hands back the
/// attempt its claim counted.
fn release_for_wait(
    conn: &Connection,
    run_id: &str,
    worker_id: &str,
    timeout_at_ms: Option<i64>,
) -> Result<(), RunsDbError> {
    let rows = conn.execute(
        "UPDATE runs SET status='pending', worker_id=NULL, lease_until=NULL,
                          run_at=?1, attempts=attempts-1
         WHERE id = ?2 AND worker_id = ?3 AND status='running'",
        (timeout_at_ms.unwrap_or(i64::MAX), run_id, worker_id),
    )?;
    if rows == 0 {
        return Err(RunsDbError::StaleWorker);
    }
    Ok(())
}

/// Whether the step already has a saved result. A wait whose result the
/// worker did not see is never parked: the run is made due instead.
fn step_saved(conn: &Connection, run_id: &str, step_name: &str) -> Result<bool, RunsDbError> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM steps WHERE run_id = ?1 AND name = ?2)",
        (run_id, step_name),
        |row| row.get(0),
    )?)
}

/// Drop any wait for the step and make the run due now.
fn requeue_now(conn: &Connection, run_id: &str, step_name: &str) -> Result<(), RunsDbError> {
    conn.execute(
        "DELETE FROM event_waiters WHERE run_id = ?1 AND step_name = ?2",
        (run_id, step_name),
    )?;
    conn.execute(
        "UPDATE runs SET run_at = ?1 WHERE id = ?2",
        (now_ms(), run_id),
    )?;
    Ok(())
}

/// Save `result` (JSON text) as the step and make the run due now.
fn satisfy_wait(
    conn: &Connection,
    run_id: &str,
    step_name: &str,
    result: &str,
) -> Result<(), RunsDbError> {
    conn.execute(
        "INSERT OR IGNORE INTO steps (run_id, name, result, completed_at)
         VALUES (?1, ?2, ?3, ?4)",
        (run_id, step_name, result, now_ms()),
    )?;
    requeue_now(conn, run_id, step_name)
}

fn park(
    conn: &Connection,
    run_id: &str,
    step_name: &str,
    event_name: &str,
    timeout_at_ms: Option<i64>,
) -> Result<(), RunsDbError> {
    conn.execute(
        "INSERT OR REPLACE INTO event_waiters (run_id, step_name, event_name, expires_at)
         VALUES (?1, ?2, ?3, ?4)",
        (run_id, step_name, event_name, timeout_at_ms),
    )?;
    Ok(())
}

/// A run's workflow name and its outcome as `GetRunResult` reports it.
fn outcome_of(
    conn: &Connection,
    id: &str,
) -> Result<Option<(String, super::RunResult)>, RunsDbError> {
    let row = conn
        .query_row(
            "SELECT name, status, result, last_error FROM runs WHERE id = ?1",
            [id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                ))
            },
        )
        .optional()?;
    row.map(|(name, status, result, error)| {
        Ok((
            name,
            super::RunResult {
                status,
                result: result
                    .map(|value| serde_json::from_str(&value))
                    .transpose()?
                    .unwrap_or(serde_json::Value::Null),
                error,
            },
        ))
    })
    .transpose()
}

/// Time out every wait on `event_name` (of `run_id` only, when given) whose
/// deadline has passed: save `null` as its step, as a claim would. A wake
/// that arrives after the deadline but before the run is claimed must not
/// turn a timed-out wait into a delivered one; the outcome may not depend
/// on how soon a worker claims the run. The run is already due (its
/// `run_at` is the deadline).
fn expire_waits(
    conn: &Connection,
    event_name: &str,
    run_id: Option<&str>,
    now: i64,
) -> Result<(), RunsDbError> {
    conn.execute(
        "INSERT OR IGNORE INTO steps (run_id, name, result, completed_at)
         SELECT run_id, step_name, 'null', ?3 FROM event_waiters
         WHERE event_name = ?1 AND (?2 IS NULL OR run_id = ?2)
           AND expires_at IS NOT NULL AND expires_at <= ?3",
        (event_name, run_id, now),
    )?;
    conn.execute(
        "DELETE FROM event_waiters
         WHERE event_name = ?1 AND (?2 IS NULL OR run_id = ?2)
           AND expires_at IS NOT NULL AND expires_at <= ?3",
        (event_name, run_id, now),
    )?;
    Ok(())
}

/// Wake every run waiting on `id`'s outcome with it. Called inside the
/// transaction that finished `id`, so a parent never misses its child.
fn wake_run_waiters(conn: &Connection, id: &str) -> Result<(), RunsDbError> {
    expire_waits(conn, &super::run_event_name(id), None, now_ms())?;
    let waiters = {
        let mut stmt = conn
            .prepare_cached("SELECT run_id, step_name FROM event_waiters WHERE event_name = ?1")?;
        let rows = stmt.query_map([super::run_event_name(id)], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        rows.collect::<Result<Vec<_>, _>>()?
    };
    if waiters.is_empty() {
        return Ok(());
    }
    let outcome = outcome_of(conn, id)?
        .map(|(_, outcome)| outcome)
        .unwrap_or_else(super::missing_outcome);
    let outcome = serde_json::to_string(&outcome)?;
    for (run_id, step_name) in &waiters {
        conn.execute(
            "INSERT OR IGNORE INTO steps (run_id, name, result, completed_at)
             VALUES (?1, ?2, ?3, ?4)",
            (run_id, step_name, outcome.as_str(), now_ms()),
        )?;
        conn.execute(
            "DELETE FROM event_waiters WHERE run_id = ?1 AND step_name = ?2",
            (run_id, step_name),
        )?;
        conn.execute(
            "UPDATE runs SET run_at = ?1 WHERE id = ?2 AND status = 'pending'",
            (now_ms(), run_id),
        )?;
    }
    Ok(())
}

fn run_status(
    conn: &Connection,
    id: &str,
) -> Result<(WorkflowRunStatus, Option<String>), RunsDbError> {
    let row = conn
        .query_row(
            "SELECT status, worker_id FROM runs WHERE id = ?1",
            [id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)),
        )
        .optional()?;
    let (status, worker_id) = row.ok_or_else(|| RunsDbError::RunNotFound { id: id.to_string() })?;
    Ok((parse_status(&status)?, worker_id))
}

#[cfg(test)]
mod tests;
