use parking_lot::{Mutex, MutexGuard};
use postgres::{Client, Config, NoTls};
use std::collections::HashMap;
use tako_core::{EnqueueOpts, EnqueueRunResponse, RunPayload, ScheduleSpec};

use super::blocking::ReleaseOffRuntime;
use super::enqueue::operator::{
    CancelledRun, OPERATOR_CANCEL_REASON, RunRecord, RunRow, StepRow, SummaryRow, WaitRow,
    parse_status,
};
use super::enqueue::{
    Reclaimed, RunsDbError, ScheduleRow, WORKER_LOST_ERROR, clamp_lease_ms, now_ms,
};
use tako_core::WorkflowRunStatus;

const DEFAULT_MAX_ATTEMPTS: u32 = 3;

/// The database clock in Unix milliseconds. Leases are written and compared
/// with it, never with a server's own clock. Servers sharing a store can
/// disagree about the time: a lease judged by a clock ahead of the one that
/// wrote it is reclaimed while its worker still runs, duplicating steps.
/// `statement_timestamp()` is stable within a statement, so the expiry
/// comparison can still use the running-lease index.
const DB_NOW_MS: &str = "(EXTRACT(EPOCH FROM statement_timestamp()) * 1000)::BIGINT";

pub(super) struct PostgresRunsDb {
    client: Mutex<ReleaseOffRuntime<Client>>,
    config: Config,
    schema: String,
    app_id: String,
    /// This server's clock, in Unix milliseconds, for scheduling and history
    /// timestamps. Leases use [`DB_NOW_MS`] instead. Tests replace it to
    /// stand in for a server whose clock disagrees with the database.
    clock: fn() -> i64,
}

impl PostgresRunsDb {
    pub(super) fn open(url: &str, schema: &str, app_id: &str) -> Result<Self, RunsDbError> {
        Self::open_with_clock(url, schema, app_id, now_ms)
    }

    fn open_with_clock(
        url: &str,
        schema: &str,
        app_id: &str,
        clock: fn() -> i64,
    ) -> Result<Self, RunsDbError> {
        validate_identifier(schema)?;
        let mut config: Config = url.parse()?;
        if config.get_connect_timeout().is_none() {
            config.connect_timeout(std::time::Duration::from_secs(10));
        }
        let mut client = config.connect(NoTls)?;
        init_schema(&mut client, schema)?;
        Ok(Self {
            client: Mutex::new(ReleaseOffRuntime::new(client)),
            config,
            schema: schema.to_string(),
            app_id: app_id.to_string(),
            clock,
        })
    }

    fn client(&self) -> Result<MutexGuard<'_, ReleaseOffRuntime<Client>>, RunsDbError> {
        let mut client = self.client.lock();
        if client.is_closed() {
            *client = ReleaseOffRuntime::new(self.config.connect(NoTls)?);
        }
        Ok(client)
    }

    fn now(&self) -> i64 {
        (self.clock)()
    }

    pub(super) fn enqueue(
        &self,
        name: &str,
        payload: &serde_json::Value,
        opts: &EnqueueOpts,
    ) -> Result<EnqueueRunResponse, RunsDbError> {
        let now = self.now();
        let run_at = opts.run_at_ms.unwrap_or(now);
        let max_attempts = opts.max_attempts.unwrap_or(DEFAULT_MAX_ATTEMPTS) as i64;
        let payload_json = serde_json::to_string(payload)?;
        let id = opts.id.clone().unwrap_or_else(|| nanoid::nanoid!());

        let mut client = self.client()?;
        let mut tx = client.transaction()?;
        let inserted = tx.execute(
            &format!(
                "INSERT INTO {}.runs
                 (app_id, id, name, payload, status, attempts, max_attempts, run_at, lease_until,
                  worker_id, last_error, created_at)
                 VALUES ($1, $2, $3, $4, 'pending', 0, $5, $6, NULL, NULL, NULL, $7)
                 ON CONFLICT(app_id, id) DO NOTHING",
                self.schema
            ),
            &[
                &self.app_id,
                &id,
                &name,
                &payload_json,
                &max_attempts,
                &run_at,
                &now,
            ],
        )?;
        if inserted == 0 {
            let existing = tx.query_one(
                &format!(
                    "SELECT name, payload FROM {}.runs WHERE app_id=$1 AND id=$2",
                    self.schema
                ),
                &[&self.app_id, &id],
            )?;
            super::enqueue::check_existing_run(
                &id,
                name,
                payload,
                existing.get(0),
                existing.get(1),
            )?;
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
        let now = self.now();
        let lease_ms = clamp_lease_ms(lease_ms);
        let mut client = self.client()?;
        let mut tx = client.transaction()?;
        // The candidate is a CTE so it is chosen once. As a subquery in FROM
        // the planner may rescan it per row of `runs`; each rescan skips the
        // row this statement just claimed and locks the next one, so a single
        // claim would take several runs.
        let row = tx.query_opt(
            &format!(
                "WITH candidate AS (
                    SELECT id FROM {schema}.runs
                    WHERE app_id=$1 AND status='pending' AND run_at <= $4 AND name = ANY($5)
                    ORDER BY run_at
                    LIMIT 1
                    FOR UPDATE SKIP LOCKED
                 )
                 UPDATE {schema}.runs r
                 SET status='running', worker_id=$2, lease_until={DB_NOW_MS} + $3,
                     attempts=attempts+1
                 FROM candidate
                 WHERE r.app_id=$1 AND r.id=candidate.id
                 RETURNING r.id, r.name, r.payload, r.status, r.attempts, r.max_attempts, r.run_at",
                schema = self.schema
            ),
            &[&self.app_id, &worker_id, &lease_ms, &now, &names],
        )?;
        let Some(row) = row else {
            tx.commit()?;
            return Ok(None);
        };
        let claimed = (
            row.get::<_, String>(0),
            row.get::<_, String>(1),
            row.get::<_, String>(2),
            row.get::<_, String>(3),
            row.get::<_, i64>(4) as u32,
            row.get::<_, i64>(5) as u32,
            row.get::<_, i64>(6),
        );
        tx.execute(
            &format!(
                "INSERT INTO {schema}.steps (app_id, run_id, name, result, completed_at)
                 SELECT $1, run_id, step_name, 'null', $3
                 FROM {schema}.event_waiters
                 WHERE app_id=$1 AND run_id=$2
                   AND expires_at IS NOT NULL
                   AND expires_at <= $3
                 ON CONFLICT DO NOTHING",
                schema = self.schema
            ),
            &[&self.app_id, &claimed.0, &now],
        )?;
        tx.execute(
            &format!(
                "DELETE FROM {}.event_waiters
                 WHERE app_id=$1 AND run_id=$2
                   AND expires_at IS NOT NULL
                   AND expires_at <= $3",
                self.schema
            ),
            &[&self.app_id, &claimed.0, &now],
        )?;
        let steps = tx.query(
            &format!(
                "SELECT name, result FROM {}.steps WHERE app_id=$1 AND run_id=$2",
                self.schema
            ),
            &[&self.app_id, &claimed.0],
        )?;
        tx.commit()?;

        let mut state_map = serde_json::Map::new();
        for row in steps {
            let name: String = row.get(0);
            let result: String = row.get(1);
            state_map.insert(
                name,
                serde_json::from_str(&result).unwrap_or(serde_json::Value::Null),
            );
        }
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
        self.update_running(
            &format!("lease_until = {DB_NOW_MS} + $4"),
            &[&self.app_id, &id, &worker_id, &clamp_lease_ms(lease_ms)],
        )
    }

    pub(super) fn save_step(
        &self,
        run_id: &str,
        worker_id: &str,
        step_name: &str,
        result: &serde_json::Value,
    ) -> Result<(), RunsDbError> {
        let result = serde_json::to_string(result)?;
        let mut client = self.client()?;
        let mut tx = client.transaction()?;
        // Serialize the ownership check and checkpoint with lease takeover.
        // An INSERT ... SELECT alone can keep reading the previous owner from
        // its MVCC snapshot while another connection reclaims the run.
        let owned = tx.query_opt(
            &format!(
                "SELECT id FROM {}.runs
                 WHERE app_id=$1 AND id=$2 AND worker_id=$3 AND status='running'
                 FOR UPDATE",
                self.schema
            ),
            &[&self.app_id, &run_id, &worker_id],
        )?;
        if owned.is_none() {
            return Err(RunsDbError::StaleWorker);
        }
        tx.execute(
            &format!(
                "INSERT INTO {}.steps (app_id, run_id, name, result, completed_at)
                 VALUES ($1, $2, $3, $4, $5)
                 ON CONFLICT(app_id, run_id, name) DO NOTHING",
                self.schema
            ),
            &[&self.app_id, &run_id, &step_name, &result, &self.now()],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub(super) fn result(
        &self,
        name: &str,
        id: &str,
    ) -> Result<Option<crate::enqueue::RunResult>, RunsDbError> {
        let mut client = self.client()?;
        let row = client.query_opt(
            &format!("SELECT status, result, last_error FROM {}.runs WHERE app_id=$1 AND id=$2 AND name=$3", self.schema),
            &[&self.app_id, &id, &name],
        )?;
        row.map(|row| {
            Ok(crate::enqueue::RunResult {
                status: row.get(0),
                result: row
                    .get::<_, Option<String>>(1)
                    .map(|value| serde_json::from_str(&value))
                    .transpose()?
                    .unwrap_or(serde_json::Value::Null),
                error: row.get(2),
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
        self.update_running(
            "status='succeeded', result=$5, last_error=NULL, worker_id=NULL, lease_until=NULL, finished_at=$4",
            &[&self.app_id, &id, &worker_id, &self.now(), &serde_json::to_string(result)?],
        )
    }

    pub(super) fn cancel(
        &self,
        id: &str,
        worker_id: &str,
        reason: Option<&str>,
    ) -> Result<(), RunsDbError> {
        self.update_running(
            "status='cancelled', last_error=$4, worker_id=NULL, lease_until=NULL, finished_at=$5",
            &[&self.app_id, &id, &worker_id, &reason, &self.now()],
        )
    }

    pub(super) fn fail(
        &self,
        id: &str,
        worker_id: &str,
        error: &str,
        next_run_at_ms: Option<i64>,
        finalize: bool,
    ) -> Result<(), RunsDbError> {
        if finalize {
            return self.update_running(
                "status='dead', last_error=$4, worker_id=NULL, lease_until=NULL, finished_at=$5",
                &[&self.app_id, &id, &worker_id, &error, &self.now()],
            );
        }
        let next = next_run_at_ms.ok_or_else(|| {
            RunsDbError::UnsupportedBackend("fail(finalize=false) requires next_run_at_ms".into())
        })?;
        self.update_running(
            "status='pending', last_error=$4, worker_id=NULL, lease_until=NULL, run_at=$5",
            &[&self.app_id, &id, &worker_id, &error, &next],
        )
    }

    pub(super) fn prune_finished_before(
        &self,
        cutoff_ms: i64,
        limit: u32,
    ) -> Result<u64, RunsDbError> {
        let mut client = self.client()?;
        let mut tx = client.transaction()?;
        let rows = tx.query(
            &format!(
                "SELECT id FROM {}.runs
                 WHERE app_id=$1 AND status IN ('succeeded', 'cancelled', 'dead')
                   AND finished_at <= $2
                 ORDER BY finished_at
                 LIMIT $3 FOR UPDATE SKIP LOCKED",
                self.schema
            ),
            &[&self.app_id, &cutoff_ms, &(limit as i64)],
        )?;
        let ids: Vec<String> = rows.into_iter().map(|row| row.get(0)).collect();
        if ids.is_empty() {
            tx.commit()?;
            return Ok(0);
        }
        for table in ["steps", "event_waiters", "runs"] {
            tx.execute(
                &format!(
                    "DELETE FROM {}.{} WHERE app_id=$1 AND {} = ANY($2)",
                    self.schema,
                    table,
                    if table == "runs" { "id" } else { "run_id" }
                ),
                &[&self.app_id, &ids],
            )?;
        }
        tx.commit()?;
        Ok(ids.len() as u64)
    }

    pub(super) fn defer(
        &self,
        id: &str,
        worker_id: &str,
        wake_at_ms: Option<i64>,
    ) -> Result<(), RunsDbError> {
        let run_at = wake_at_ms.unwrap_or(i64::MAX);
        self.update_running(
            "status='pending', worker_id=NULL, lease_until=NULL, run_at=$4, attempts=attempts-1",
            &[&self.app_id, &id, &worker_id, &run_at],
        )
    }

    pub(super) fn reclaim_expired(&self) -> Result<Reclaimed, RunsDbError> {
        let mut client = self.client()?;
        // Return exactly the owners whose rows this statement reclaims.
        // Row locks prevent a heartbeat or another reclaimer from changing
        // ownership between selection and accounting.
        //
        // The lost worker spent the attempt that claim counted. A run with
        // none left ends here; requeueing it would repeat the loss forever.
        let expired = client
            .query(
                &format!(
                    "WITH expired AS (
                    SELECT id, worker_id, attempts >= max_attempts AS exhausted
                    FROM {schema}.runs
                    WHERE app_id=$1 AND status='running' AND lease_until < {DB_NOW_MS}
                    FOR UPDATE
                 )
                 UPDATE {schema}.runs r SET
                    status=CASE WHEN expired.exhausted THEN 'dead' ELSE 'pending' END,
                    last_error=CASE WHEN expired.exhausted THEN $3 ELSE r.last_error END,
                    finished_at=CASE WHEN expired.exhausted THEN $2 ELSE r.finished_at END,
                    worker_id=NULL, lease_until=NULL
                 FROM expired WHERE r.app_id=$1 AND r.id=expired.id
                 RETURNING expired.id, expired.worker_id, expired.exhausted",
                    schema = self.schema
                ),
                &[&self.app_id, &self.now(), &WORKER_LOST_ERROR],
            )?
            .into_iter()
            .map(|row| (row.get(0), row.get(1), row.get(2)))
            .collect();
        Ok(Reclaimed::from_rows(expired))
    }

    pub(super) fn in_flight_by_worker(&self) -> Result<HashMap<String, u32>, RunsDbError> {
        let mut client = self.client()?;
        let mut out = HashMap::new();
        for row in client.query(
            &format!(
                "SELECT worker_id, COUNT(*) FROM {}.runs
                 WHERE app_id=$1 AND status='running' AND worker_id IS NOT NULL
                 GROUP BY worker_id",
                self.schema
            ),
            &[&self.app_id],
        )? {
            out.insert(row.get::<_, String>(0), row.get::<_, i64>(1) as u32);
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
        let mut client = self.client()?;
        let mut tx = client.transaction()?;
        let run_at = timeout_at_ms.unwrap_or(i64::MAX);
        let rows = tx.execute(
            &format!(
                "UPDATE {}.runs
                 SET status='pending', worker_id=NULL, lease_until=NULL, run_at=$4, attempts=attempts-1
                 WHERE app_id=$1 AND id=$2 AND worker_id=$3 AND status='running'",
                self.schema
            ),
            &[&self.app_id, &run_id, &worker_id, &run_at],
        )?;
        if rows == 0 {
            return Err(RunsDbError::StaleWorker);
        }
        tx.execute(
            &format!(
                "INSERT INTO {}.event_waiters (app_id, run_id, step_name, event_name, expires_at)
                 VALUES ($1, $2, $3, $4, $5)
                 ON CONFLICT(app_id, run_id, step_name) DO UPDATE
                 SET event_name=excluded.event_name, expires_at=excluded.expires_at",
                self.schema
            ),
            &[
                &self.app_id,
                &run_id,
                &step_name,
                &event_name,
                &timeout_at_ms,
            ],
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
        let now = self.now();
        let mut client = self.client()?;
        let mut tx = client.transaction()?;
        let waiters = tx.query(
            &format!(
                "SELECT run_id, step_name FROM {}.event_waiters
                 WHERE app_id=$1 AND event_name=$2 ORDER BY run_id, step_name",
                self.schema
            ),
            &[&self.app_id, &event_name],
        )?;
        let mut woken = 0;
        for row in waiters {
            let run_id: String = row.get(0);
            let step_name: String = row.get(1);
            tx.query_opt(
                &format!(
                    "SELECT id FROM {}.runs WHERE app_id=$1 AND id=$2 FOR UPDATE",
                    self.schema
                ),
                &[&self.app_id, &run_id],
            )?;
            // A claim or competing signal may have consumed this waiter while
            // we waited for its run lock. Only its consumer can materialize it.
            let consumed = tx.execute(
                &format!("DELETE FROM {}.event_waiters WHERE app_id=$1 AND run_id=$2 AND step_name=$3 AND event_name=$4", self.schema),
                &[&self.app_id, &run_id, &step_name, &event_name],
            )?;
            if consumed == 0 {
                continue;
            }
            tx.execute(
                &format!(
                    "INSERT INTO {}.steps (app_id, run_id, name, result, completed_at)
                     VALUES ($1, $2, $3, $4, $5)
                     ON CONFLICT(app_id, run_id, name) DO NOTHING",
                    self.schema
                ),
                &[&self.app_id, &run_id, &step_name, &payload_json, &now],
            )?;
            tx.execute(
                &format!(
                    "UPDATE {}.runs SET status='pending', run_at=$3
                     WHERE app_id=$1 AND id=$2 AND status='pending'",
                    self.schema
                ),
                &[&self.app_id, &run_id, &now],
            )?;
            woken += 1;
        }
        tx.commit()?;
        Ok(woken)
    }

    pub(super) fn pending_count(&self) -> Result<u64, RunsDbError> {
        let mut client = self.client()?;
        let count: i64 = client
            .query_one(
                &format!(
                    "SELECT COUNT(*) FROM {}.runs WHERE app_id=$1 AND status='pending'",
                    self.schema
                ),
                &[&self.app_id],
            )?
            .get(0);
        Ok(count as u64)
    }

    pub(super) fn has_runnable_work(&self) -> Result<bool, RunsDbError> {
        let mut client = self.client()?;
        let exists: bool = client
            .query_one(
                &format!(
                    "SELECT EXISTS(
                        SELECT 1 FROM {}.runs
                        WHERE app_id=$1 AND status='pending' AND run_at <= $2 LIMIT 1
                     )",
                    self.schema
                ),
                &[&self.app_id, &self.now()],
            )?
            .get(0);
        Ok(exists)
    }

    pub(super) fn replace_schedules(&self, schedules: &[ScheduleSpec]) -> Result<(), RunsDbError> {
        let mut client = self.client()?;
        let mut tx = client.transaction()?;
        let names = schedules
            .iter()
            .map(|s| s.name.clone())
            .collect::<Vec<String>>();
        if names.is_empty() {
            tx.execute(
                &format!("DELETE FROM {}.schedules WHERE app_id=$1", self.schema),
                &[&self.app_id],
            )?;
        } else {
            tx.execute(
                &format!(
                    "DELETE FROM {}.schedules WHERE app_id=$1 AND NOT (name = ANY($2))",
                    self.schema
                ),
                &[&self.app_id, &names],
            )?;
        }
        let now = self.now();
        for schedule in schedules {
            tx.execute(
                &format!(
                    "INSERT INTO {}.schedules (app_id, name, cron, last_run_at, max_attempts)
                     VALUES ($1, $2, $3, $4, $5)
                     ON CONFLICT(app_id, name)
                     DO UPDATE SET cron=excluded.cron, max_attempts=excluded.max_attempts",
                    self.schema
                ),
                &[
                    &self.app_id,
                    &schedule.name,
                    &schedule.cron,
                    &now,
                    &schedule.max_attempts.map(i64::from),
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub(super) fn list_schedules(&self) -> Result<Vec<ScheduleRow>, RunsDbError> {
        let mut client = self.client()?;
        let rows = client.query(
            &format!(
                "SELECT name, cron, last_run_at, max_attempts FROM {}.schedules WHERE app_id=$1",
                self.schema
            ),
            &[&self.app_id],
        )?;
        Ok(rows
            .into_iter()
            .map(|row| ScheduleRow {
                name: row.get(0),
                cron: row.get(1),
                last_run_at: row.get(2),
                max_attempts: row
                    .get::<_, Option<i64>>(3)
                    .map(|budget| budget.clamp(0, u32::MAX as i64) as u32),
            })
            .collect())
    }

    pub(super) fn set_schedule_last_run_at(&self, name: &str, ts: i64) -> Result<(), RunsDbError> {
        let mut client = self.client()?;
        client.execute(
            &format!(
                "UPDATE {}.schedules SET last_run_at=$3 WHERE app_id=$1 AND name=$2",
                self.schema
            ),
            &[&self.app_id, &name, &ts],
        )?;
        Ok(())
    }

    pub(super) fn list_runs(
        &self,
        status: Option<&str>,
        name: Option<&str>,
        limit: u32,
    ) -> Result<Vec<SummaryRow>, RunsDbError> {
        let mut client = self.client()?;
        let rows = client.query(
            &format!(
                "SELECT id, name, status, attempts, max_attempts, run_at, created_at, finished_at,
                        last_error
                 FROM {}.runs
                 WHERE app_id=$1 AND ($2::TEXT IS NULL OR status=$2)
                   AND ($3::TEXT IS NULL OR name=$3)
                 ORDER BY created_at DESC, id DESC
                 LIMIT $4",
                self.schema
            ),
            &[&self.app_id, &status, &name, &(limit as i64)],
        )?;
        Ok(rows.iter().map(summary_row).collect())
    }

    pub(super) fn get_run(
        &self,
        id: &str,
        step_limit: i64,
    ) -> Result<Option<RunRecord>, RunsDbError> {
        let mut client = self.client()?;
        // One snapshot, so the steps belong to the run state shown with them.
        let mut tx = client
            .build_transaction()
            .isolation_level(postgres::IsolationLevel::RepeatableRead)
            .read_only(true)
            .start()?;
        let Some(row) = tx.query_opt(
            &format!(
                "SELECT id, name, status, attempts, max_attempts, run_at, created_at, finished_at,
                        last_error, payload, result, lease_until, worker_id
                 FROM {}.runs WHERE app_id=$1 AND id=$2",
                self.schema
            ),
            &[&self.app_id, &id],
        )?
        else {
            tx.commit()?;
            return Ok(None);
        };
        let run = RunRow {
            summary: summary_row(&row),
            payload: row.get(9),
            result: row.get(10),
            lease_until: row.get(11),
            worker_id: row.get(12),
        };
        let steps = tx
            .query(
                &format!(
                    "SELECT name, result, completed_at FROM {}.steps WHERE app_id=$1 AND run_id=$2
                     ORDER BY completed_at DESC, name DESC LIMIT $3",
                    self.schema
                ),
                &[&self.app_id, &id, &step_limit],
            )?
            .into_iter()
            .map(|row| StepRow {
                name: row.get(0),
                result: row.get(1),
                completed_at: row.get(2),
            })
            .collect();
        let steps_total: i64 = tx
            .query_one(
                &format!(
                    "SELECT COUNT(*) FROM {}.steps WHERE app_id=$1 AND run_id=$2",
                    self.schema
                ),
                &[&self.app_id, &id],
            )?
            .get(0);
        let waits = tx
            .query(
                &format!(
                    "SELECT step_name, event_name, expires_at FROM {}.event_waiters
                     WHERE app_id=$1 AND run_id=$2 ORDER BY step_name LIMIT $3",
                    self.schema
                ),
                &[&self.app_id, &id, &step_limit],
            )?
            .into_iter()
            .map(|row| WaitRow {
                step_name: row.get(0),
                event_name: row.get(1),
                expires_at: row.get(2),
            })
            .collect();
        tx.commit()?;
        Ok(Some(RunRecord {
            run,
            steps,
            steps_total: steps_total as u64,
            waits,
        }))
    }

    pub(super) fn retry_run(&self, id: &str) -> Result<WorkflowRunStatus, RunsDbError> {
        let mut client = self.client()?;
        let mut tx = client.transaction()?;
        let (status, _) = self.lock_run_status(&mut tx, id)?;
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
            &format!(
                "UPDATE {}.runs SET status='pending', attempts=0, run_at=$3, lease_until=NULL,
                                    worker_id=NULL, finished_at=NULL, result=NULL
                 WHERE app_id=$1 AND id=$2",
                self.schema
            ),
            &[&self.app_id, &id, &self.now()],
        )?;
        self.delete_waiters(&mut tx, id)?;
        tx.commit()?;
        Ok(status)
    }

    pub(super) fn cancel_run(&self, id: &str) -> Result<CancelledRun, RunsDbError> {
        let mut client = self.client()?;
        let mut tx = client.transaction()?;
        // The row lock orders this with checkpoints, which lock the run
        // before checking ownership, and with guarded lifecycle updates,
        // which re-check their owner after waiting for it.
        let (status, worker_id) = self.lock_run_status(&mut tx, id)?;
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
            &format!(
                "UPDATE {}.runs SET status='cancelled', last_error=$3, worker_id=NULL,
                                    lease_until=NULL, finished_at=$4
                 WHERE app_id=$1 AND id=$2",
                self.schema
            ),
            &[&self.app_id, &id, &OPERATOR_CANCEL_REASON, &self.now()],
        )?;
        self.delete_waiters(&mut tx, id)?;
        tx.commit()?;
        Ok(CancelledRun {
            previous_status: status,
            worker_id: worker_id.filter(|_| status == WorkflowRunStatus::Running),
        })
    }

    fn lock_run_status(
        &self,
        tx: &mut postgres::Transaction<'_>,
        id: &str,
    ) -> Result<(WorkflowRunStatus, Option<String>), RunsDbError> {
        let row = tx
            .query_opt(
                &format!(
                    "SELECT status, worker_id FROM {}.runs WHERE app_id=$1 AND id=$2 FOR UPDATE",
                    self.schema
                ),
                &[&self.app_id, &id],
            )?
            .ok_or_else(|| RunsDbError::RunNotFound { id: id.to_string() })?;
        Ok((parse_status(row.get(0))?, row.get(1)))
    }

    fn delete_waiters(
        &self,
        tx: &mut postgres::Transaction<'_>,
        id: &str,
    ) -> Result<(), RunsDbError> {
        tx.execute(
            &format!(
                "DELETE FROM {}.event_waiters WHERE app_id=$1 AND run_id=$2",
                self.schema
            ),
            &[&self.app_id, &id],
        )?;
        Ok(())
    }

    fn update_running(
        &self,
        set_clause: &str,
        params: &[&(dyn postgres::types::ToSql + Sync)],
    ) -> Result<(), RunsDbError> {
        let mut client = self.client()?;
        let rows = client.execute(
            &format!(
                "UPDATE {}.runs SET {set_clause}
                 WHERE app_id=$1 AND id=$2 AND worker_id=$3 AND status='running'",
                self.schema
            ),
            params,
        )?;
        if rows == 0 {
            return Err(RunsDbError::StaleWorker);
        }
        Ok(())
    }
}

fn summary_row(row: &postgres::Row) -> SummaryRow {
    SummaryRow {
        id: row.get(0),
        name: row.get(1),
        status: row.get(2),
        attempts: row.get(3),
        max_attempts: row.get(4),
        run_at: row.get(5),
        created_at: row.get(6),
        finished_at: row.get(7),
        last_error: row.get(8),
    }
}

fn init_schema(client: &mut Client, schema: &str) -> Result<(), RunsDbError> {
    let mut tx = client.transaction()?;
    // PostgreSQL's IF NOT EXISTS does not serialize concurrent schema creation.
    tx.query_one("SELECT pg_advisory_xact_lock(hashtext($1))", &[&schema])?;
    tx.batch_execute(&format!(
        "CREATE SCHEMA IF NOT EXISTS {schema};
         CREATE TABLE IF NOT EXISTS {schema}.runs (
             app_id TEXT NOT NULL,
             id TEXT NOT NULL,
             name TEXT NOT NULL,
             payload TEXT NOT NULL,
             status TEXT NOT NULL,
             attempts BIGINT NOT NULL,
             max_attempts BIGINT NOT NULL,
             run_at BIGINT NOT NULL,
             lease_until BIGINT,
             worker_id TEXT,
             last_error TEXT,
             created_at BIGINT NOT NULL,
             finished_at BIGINT,
             PRIMARY KEY(app_id, id)
         );
         ALTER TABLE {schema}.runs ADD COLUMN IF NOT EXISTS finished_at BIGINT;
         ALTER TABLE {schema}.runs ADD COLUMN IF NOT EXISTS result TEXT;
         DO $$ BEGIN
             IF EXISTS (
                 SELECT 1 FROM pg_constraint
                 WHERE conrelid = '{schema}.runs'::regclass AND contype = 'p' AND cardinality(conkey) = 1
             ) THEN
                 ALTER TABLE {schema}.runs DROP CONSTRAINT runs_pkey, ADD PRIMARY KEY(app_id, id);
             END IF;
         END $$;
         UPDATE {schema}.runs SET finished_at = (EXTRACT(EPOCH FROM clock_timestamp()) * 1000)::BIGINT
           WHERE finished_at IS NULL AND status IN ('succeeded', 'cancelled', 'dead');
         CREATE INDEX IF NOT EXISTS idx_runs_app_finished
           ON {schema}.runs(app_id, finished_at)
           WHERE status IN ('succeeded', 'cancelled', 'dead');
         CREATE INDEX IF NOT EXISTS idx_runs_app_status_run_at
           ON {schema}.runs(app_id, status, run_at);
         -- Operator listings show the newest runs first.
         CREATE INDEX IF NOT EXISTS idx_runs_app_created
           ON {schema}.runs(app_id, created_at);
         -- Only running runs can expire. Keep range lookups without indexing
         -- pending jobs or the much larger retained terminal history.
         CREATE INDEX IF NOT EXISTS idx_runs_app_running_lease
           ON {schema}.runs(app_id, lease_until) WHERE status='running';
         DROP INDEX IF EXISTS {schema}.idx_runs_app_lease;

         CREATE TABLE IF NOT EXISTS {schema}.steps (
             app_id TEXT NOT NULL,
             run_id TEXT NOT NULL,
             name TEXT NOT NULL,
             result TEXT NOT NULL,
             completed_at BIGINT NOT NULL,
             PRIMARY KEY(app_id, run_id, name)
         );

         CREATE TABLE IF NOT EXISTS {schema}.schedules (
             app_id TEXT NOT NULL,
             name TEXT NOT NULL,
             cron TEXT NOT NULL,
             last_run_at BIGINT,
             max_attempts BIGINT,
             PRIMARY KEY(app_id, name)
         );
         ALTER TABLE {schema}.schedules ADD COLUMN IF NOT EXISTS max_attempts BIGINT;

         CREATE TABLE IF NOT EXISTS {schema}.event_waiters (
             app_id TEXT NOT NULL,
             run_id TEXT NOT NULL,
             step_name TEXT NOT NULL,
             event_name TEXT NOT NULL,
             expires_at BIGINT,
             PRIMARY KEY(app_id, run_id, step_name)
         );
         CREATE INDEX IF NOT EXISTS idx_event_waiters_app_event
           ON {schema}.event_waiters(app_id, event_name);
         CREATE INDEX IF NOT EXISTS idx_event_waiters_app_expiry
           ON {schema}.event_waiters(app_id, expires_at)
           WHERE expires_at IS NOT NULL;"
    ))?;
    tx.commit()?;
    Ok(())
}

fn validate_identifier(identifier: &str) -> Result<(), RunsDbError> {
    let mut chars = identifier.chars();
    let Some(first) = chars.next() else {
        return Err(RunsDbError::UnsupportedBackend(
            "postgres schema name cannot be empty".into(),
        ));
    };
    if !(first == '_' || first.is_ascii_alphabetic())
        || !chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
    {
        return Err(RunsDbError::UnsupportedBackend(format!(
            "invalid postgres schema name '{identifier}'"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn postgres_reconnects_before_the_next_workflow_operation() {
        let Ok(url) = std::env::var("TAKO_TEST_POSTGRES_URL") else {
            return;
        };
        let schema = format!(
            "reconnect_{}",
            nanoid::nanoid!(12, &['a', 'b', 'c', 'd', 'e', 'f'])
        );
        let db = PostgresRunsDb::open(&url, &schema, "app").unwrap();
        let enqueued = db
            .enqueue("work", &serde_json::json!(42), &EnqueueOpts::default())
            .unwrap();
        let pid: i32 = db
            .client
            .lock()
            .query_one("SELECT pg_backend_pid()", &[])
            .unwrap()
            .get(0);
        let mut admin = Client::connect(&url, NoTls).unwrap();
        assert!(
            admin
                .query_one("SELECT pg_terminate_backend($1, 5000)", &[&pid])
                .unwrap()
                .get::<_, bool>(0)
        );
        // The first request observes the broken socket. A write is never replayed implicitly.
        assert!(db.claim("worker", &["work".into()], 60_000).is_err());

        let run = db
            .claim("worker", &["work".into()], 60_000)
            .unwrap()
            .unwrap();
        assert_eq!(run.id, enqueued.id);
        assert_eq!(run.attempts, 1);
        admin
            .batch_execute(&format!("DROP SCHEMA {schema} CASCADE"))
            .unwrap();
    }

    #[test]
    fn claim_takes_exactly_one_of_several_due_runs() {
        let Ok(url) = std::env::var("TAKO_TEST_POSTGRES_URL") else {
            return;
        };
        let schema = format!(
            "claim_one_{}",
            nanoid::nanoid!(12, &['a', 'b', 'c', 'd', 'e', 'f'])
        );
        let db = PostgresRunsDb::open(&url, &schema, "app").unwrap();
        for _ in 0..3 {
            db.enqueue("work", &serde_json::json!({}), &EnqueueOpts::default())
                .unwrap();
        }
        // A fresh, never-analyzed table is where the planner is most likely to
        // re-evaluate a candidate subquery once per scanned row.
        for remaining in [2, 1, 0] {
            let run = db
                .claim("worker", &["work".into()], 60_000)
                .unwrap()
                .unwrap();
            assert_eq!(run.attempts, 1);
            assert_eq!(db.pending_count().unwrap(), remaining);
        }
        assert!(
            db.claim("worker", &["work".into()], 60_000)
                .unwrap()
                .is_none()
        );
        db.client
            .lock()
            .batch_execute(&format!("DROP SCHEMA {schema} CASCADE"))
            .unwrap();
    }

    const SKEW_MS: i64 = 60 * 60 * 1000;
    const LEASE_MS: u64 = 60_000;

    fn fast_clock() -> i64 {
        now_ms() + SKEW_MS
    }

    fn slow_clock() -> i64 {
        now_ms() - SKEW_MS
    }

    /// Milliseconds left on a run's lease, measured by the database clock.
    fn remaining_lease(gate: &mut Client, schema: &str, id: &str) -> i64 {
        gate.query_one(
            &format!(
                "SELECT lease_until - (EXTRACT(EPOCH FROM clock_timestamp()) * 1000)::BIGINT
                 FROM {schema}.runs WHERE app_id='app' AND id=$1"
            ),
            &[&id],
        )
        .unwrap()
        .get(0)
    }

    /// Three servers share one store. One clock runs an hour ahead of the
    /// database and one an hour behind. Leases written and judged by those
    /// clocks would be reclaimed an hour early (duplicating steps) or an
    /// hour late (stalling runs whose worker died).
    #[test]
    fn leases_follow_the_database_clock_on_servers_with_skewed_clocks() {
        let Ok(url) = std::env::var("TAKO_TEST_POSTGRES_URL") else {
            return;
        };
        let schema = format!(
            "lease_clock_{}",
            nanoid::nanoid!(12, &['a', 'b', 'c', 'd', 'e', 'f'])
        );
        let accurate = PostgresRunsDb::open(&url, &schema, "app").unwrap();
        let fast = PostgresRunsDb::open_with_clock(&url, &schema, "app", fast_clock).unwrap();
        let slow = PostgresRunsDb::open_with_clock(&url, &schema, "app", slow_clock).unwrap();
        let mut gate = Client::connect(&url, NoTls).unwrap();
        let full_lease = (LEASE_MS as i64 - 5_000)..=LEASE_MS as i64;
        // Due on every server's own clock: scheduling is not what is tested.
        let due = EnqueueOpts {
            run_at_ms: Some(0),
            ..Default::default()
        };

        for (name, server) in [("fast", &fast), ("slow", &slow)] {
            let id = accurate
                .enqueue("work", &serde_json::json!({}), &due)
                .unwrap()
                .id;
            let run = server
                .claim(name, &["work".into()], LEASE_MS)
                .unwrap()
                .unwrap();
            assert_eq!(run.id, id);
            let remaining = remaining_lease(&mut gate, &schema, &id);
            assert!(
                full_lease.contains(&remaining),
                "{name} server claimed with {remaining}ms of lease"
            );
            // No server takes the run from a worker that still holds its lease.
            for reclaimer in [&accurate, &fast, &slow] {
                assert_eq!(reclaimer.reclaim_expired().unwrap().count, 0, "{name}");
            }

            // Shorten the lease, then renew it through the skewed server.
            gate.execute(
                &format!(
                    "UPDATE {schema}.runs SET lease_until = lease_until - 50000
                     WHERE app_id='app' AND id=$1"
                ),
                &[&id],
            )
            .unwrap();
            server.heartbeat(&id, name, LEASE_MS).unwrap();
            let remaining = remaining_lease(&mut gate, &schema, &id);
            assert!(
                full_lease.contains(&remaining),
                "{name} server renewed to {remaining}ms of lease"
            );
            for reclaimer in [&accurate, &fast, &slow] {
                assert_eq!(reclaimer.reclaim_expired().unwrap().count, 0, "{name}");
            }

            // Once the database clock passes the lease, every server reclaims it.
            gate.execute(
                &format!(
                    "UPDATE {schema}.runs
                     SET lease_until = (EXTRACT(EPOCH FROM clock_timestamp()) * 1000)::BIGINT - 1
                     WHERE app_id='app' AND id=$1"
                ),
                &[&id],
            )
            .unwrap();
            assert_eq!(server.reclaim_expired().unwrap().workers, vec![name]);
            assert!(matches!(
                accurate.heartbeat(&id, name, LEASE_MS),
                Err(RunsDbError::StaleWorker)
            ));
            let run = accurate
                .claim("next", &["work".into()], LEASE_MS)
                .unwrap()
                .unwrap();
            assert_eq!((run.id, run.attempts), (id.clone(), 2));
            accurate
                .complete(&id, "next", &serde_json::Value::Null)
                .unwrap();
        }
        gate.batch_execute(&format!("DROP SCHEMA {schema} CASCADE"))
            .unwrap();
    }

    #[test]
    fn concurrent_reclaimers_and_signals_consume_state_once() {
        let Ok(url) = std::env::var("TAKO_TEST_POSTGRES_URL") else {
            return;
        };
        for signal in [false, true] {
            let schema = format!(
                "consume_{}",
                nanoid::nanoid!(12, &['a', 'b', 'c', 'd', 'e', 'f'])
            );
            let first = PostgresRunsDb::open(&url, &schema, "app").unwrap();
            let second = PostgresRunsDb::open(&url, &schema, "app").unwrap();
            let id = first
                .enqueue("work", &serde_json::json!({}), &EnqueueOpts::default())
                .unwrap()
                .id;
            first
                .claim("owner", &["work".into()], 60_000)
                .unwrap()
                .unwrap();
            if signal {
                first
                    .wait_for_event(&id, "owner", "wait", "event", None)
                    .unwrap();
            }
            let pids: Vec<i32> = [&first, &second]
                .iter()
                .map(|db| {
                    db.client
                        .lock()
                        .query_one("SELECT pg_backend_pid()", &[])
                        .unwrap()
                        .get(0)
                })
                .collect();
            let mut gate = Client::connect(&url, NoTls).unwrap();
            gate.execute(
                &format!("UPDATE {schema}.runs SET lease_until=0 WHERE app_id='app' AND id=$1"),
                &[&id],
            )
            .unwrap();
            let mut tx = gate.transaction().unwrap();
            tx.query_one(
                &format!("SELECT id FROM {schema}.runs WHERE app_id='app' AND id=$1 FOR UPDATE"),
                &[&id],
            )
            .unwrap();
            let calls: Vec<_> = [first, second]
                .into_iter()
                .map(|db| {
                    std::thread::spawn(move || {
                        if signal {
                            db.signal("event", &serde_json::json!(42)).unwrap()
                        } else {
                            db.reclaim_expired().unwrap().workers.len() as u64
                        }
                    })
                })
                .collect();
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                let blocked: i64 = tx.query_one(
                    "SELECT COUNT(*) FROM pg_stat_activity WHERE pid=ANY($1) AND cardinality(pg_blocking_pids(pid)) > 0",
                    &[&pids],
                ).unwrap().get(0);
                if blocked == 2 {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "consumers never reached their lock gates"
                );
                std::thread::sleep(Duration::from_millis(5));
            }
            tx.commit().unwrap();
            assert_eq!(
                calls
                    .into_iter()
                    .map(|call| call.join().unwrap())
                    .sum::<u64>(),
                1
            );
            gate.batch_execute(&format!("DROP SCHEMA {schema} CASCADE"))
                .unwrap();
        }
    }

    #[test]
    fn checkpoint_holds_ownership_until_commit() {
        let Ok(url) = std::env::var("TAKO_TEST_POSTGRES_URL") else {
            return;
        };
        let schema = format!(
            "checkpoint_{}",
            nanoid::nanoid!(12, &['a', 'b', 'c', 'd', 'e', 'f'])
        );
        let db = PostgresRunsDb::open(&url, &schema, "app").unwrap();
        let id = db
            .enqueue("work", &serde_json::json!({}), &EnqueueOpts::default())
            .unwrap()
            .id;
        db.claim("old", &["work".into()], 60_000).unwrap().unwrap();
        let mut gate = Client::connect(&url, NoTls).unwrap();
        let key: i64 = gate
            .query_one("SELECT hashtext($1)::bigint", &[&schema])
            .unwrap()
            .get(0);
        gate.batch_execute(&format!(
            "CREATE FUNCTION {schema}.block_checkpoint() RETURNS trigger LANGUAGE plpgsql AS $$
             BEGIN PERFORM pg_advisory_xact_lock({key}); RETURN NEW; END $$;
             CREATE TRIGGER block_checkpoint BEFORE INSERT ON {schema}.steps
             FOR EACH ROW EXECUTE FUNCTION {schema}.block_checkpoint();"
        ))
        .unwrap();
        gate.query_one("SELECT pg_advisory_lock($1)", &[&key])
            .unwrap();
        let gate_pid: i32 = gate
            .query_one("SELECT pg_backend_pid()", &[])
            .unwrap()
            .get(0);
        let save_id = id.clone();
        let save = std::thread::spawn(move || {
            let result = db.save_step(&save_id, "old", "checkpoint", &serde_json::json!(42));
            (db, result)
        });
        // Observe the checkpoint inside its INSERT trigger before attempting
        // takeover. No assumption about thread scheduling or query speed.
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let blocked: bool = gate.query_one(
                "SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE $1 = ANY(pg_blocking_pids(pid)))",
                &[&gate_pid],
            ).unwrap().get(0);
            if blocked {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "checkpoint never reached its gate"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        let mut takeover = Client::connect(&url, NoTls).unwrap();
        takeover.batch_execute("SET lock_timeout='250ms'").unwrap();
        let transfer =
            format!("UPDATE {schema}.runs SET worker_id='new' WHERE app_id='app' AND id=$1");
        let blocked_transfer = takeover.execute(&transfer, &[&id]);
        gate.query_one("SELECT pg_advisory_unlock($1)", &[&key])
            .unwrap();
        let (db, result) = save.join().unwrap();
        result.unwrap();
        assert_eq!(
            blocked_transfer.unwrap_err().code(),
            Some(&postgres::error::SqlState::LOCK_NOT_AVAILABLE)
        );
        assert_eq!(takeover.execute(&transfer, &[&id]).unwrap(), 1);
        assert!(matches!(
            db.save_step(&id, "old", "late", &serde_json::json!(0)),
            Err(RunsDbError::StaleWorker)
        ));
        assert_eq!(
            gate.query_one(&format!("SELECT COUNT(*) FROM {schema}.steps"), &[])
                .unwrap()
                .get::<_, i64>(0),
            1
        );
        drop(db);
        gate.batch_execute(&format!("DROP SCHEMA {schema} CASCADE"))
            .unwrap();
    }

    /// An operator cancel and the running attempt's completion race on two
    /// servers. Exactly one wins; the loser sees a specific refusal and the
    /// stored status matches the winner.
    #[test]
    fn operator_cancel_and_a_late_completion_serialize_on_the_run() {
        let Ok(url) = std::env::var("TAKO_TEST_POSTGRES_URL") else {
            return;
        };
        let schema = format!(
            "cancel_race_{}",
            nanoid::nanoid!(12, &['a', 'b', 'c', 'd', 'e', 'f'])
        );
        let worker_side = PostgresRunsDb::open(&url, &schema, "app").unwrap();
        let operator_side = PostgresRunsDb::open(&url, &schema, "app").unwrap();
        let id = worker_side
            .enqueue("work", &serde_json::json!({}), &EnqueueOpts::default())
            .unwrap()
            .id;
        let owner = worker_side
            .claim("w", &["work".into()], 60_000)
            .unwrap()
            .unwrap()
            .worker_id;
        let pids: Vec<i32> = [&worker_side, &operator_side]
            .iter()
            .map(|db| {
                db.client
                    .lock()
                    .query_one("SELECT pg_backend_pid()", &[])
                    .unwrap()
                    .get(0)
            })
            .collect();
        let mut gate = Client::connect(&url, NoTls).unwrap();
        let mut tx = gate.transaction().unwrap();
        tx.query_one(
            &format!("SELECT id FROM {schema}.runs WHERE app_id='app' AND id=$1 FOR UPDATE"),
            &[&id],
        )
        .unwrap();
        let complete = {
            let id = id.clone();
            std::thread::spawn(move || {
                let result = worker_side.complete(&id, &owner, &serde_json::json!(1));
                (worker_side, result)
            })
        };
        let cancel = {
            let id = id.clone();
            std::thread::spawn(move || {
                let result = operator_side.cancel_run(&id);
                (operator_side, result)
            })
        };
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let blocked: i64 = tx.query_one(
                "SELECT COUNT(*) FROM pg_stat_activity WHERE pid=ANY($1) AND cardinality(pg_blocking_pids(pid)) > 0",
                &[&pids],
            ).unwrap().get(0);
            if blocked == 2 {
                break;
            }
            assert!(Instant::now() < deadline, "writers never reached the gate");
            std::thread::sleep(Duration::from_millis(5));
        }
        tx.commit().unwrap();
        let (db, completed) = complete.join().unwrap();
        let (_, cancelled) = cancel.join().unwrap();
        let status = db.result("work", &id).unwrap().unwrap().status;
        match (completed, cancelled) {
            (Ok(()), Err(RunsDbError::RunAlreadyFinished { status: s, .. })) => {
                assert_eq!(s, WorkflowRunStatus::Succeeded);
                assert_eq!(status, "succeeded");
            }
            (Err(RunsDbError::StaleWorker), Ok(cancelled)) => {
                assert_eq!(cancelled.previous_status, WorkflowRunStatus::Running);
                assert_eq!(status, "cancelled");
            }
            other => panic!("both or neither won: {other:?}"),
        }
        gate.batch_execute(&format!("DROP SCHEMA {schema} CASCADE"))
            .unwrap();
    }
}
