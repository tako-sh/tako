use parking_lot::Mutex;
use postgres::{Client, NoTls};
use std::collections::HashMap;
use tako_core::{EnqueueOpts, EnqueueRunResponse, RunPayload, ScheduleSpec};

use super::enqueue::{RunsDbError, ScheduleRow, clamp_lease_ms, now_ms};

const DEFAULT_MAX_ATTEMPTS: u32 = 3;

pub(super) struct PostgresRunsDb {
    client: Mutex<Client>,
    schema: String,
    app_id: String,
}

impl PostgresRunsDb {
    pub(super) fn open(url: &str, schema: &str, app_id: &str) -> Result<Self, RunsDbError> {
        validate_identifier(schema)?;
        let mut client = Client::connect(url, NoTls)?;
        init_schema(&mut client, schema)?;
        Ok(Self {
            client: Mutex::new(client),
            schema: schema.to_string(),
            app_id: app_id.to_string(),
        })
    }

    pub(super) fn enqueue(
        &self,
        name: &str,
        payload: &serde_json::Value,
        opts: &EnqueueOpts,
    ) -> Result<EnqueueRunResponse, RunsDbError> {
        let now = now_ms();
        let run_at = opts.run_at_ms.unwrap_or(now);
        let max_attempts = opts.max_attempts.unwrap_or(DEFAULT_MAX_ATTEMPTS) as i64;
        let payload_json = serde_json::to_string(payload)?;
        let id = opts.id.clone().unwrap_or_else(|| nanoid::nanoid!());

        let mut client = self.client.lock();
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
        let now = now_ms();
        let lease_until = now.saturating_add(clamp_lease_ms(lease_ms));
        let mut client = self.client.lock();
        let mut tx = client.transaction()?;
        let row = tx.query_opt(
            &format!(
                "UPDATE {}.runs r
                 SET status='running', worker_id=$2, lease_until=$3, attempts=attempts+1
                 FROM (
                    SELECT id FROM {}.runs
                    WHERE app_id=$1 AND status='pending' AND run_at <= $4 AND name = ANY($5)
                    ORDER BY run_at
                    LIMIT 1
                    FOR UPDATE SKIP LOCKED
                 ) candidate
                 WHERE r.app_id=$1 AND r.id=candidate.id
                 RETURNING r.id, r.name, r.payload, r.status, r.attempts, r.max_attempts, r.run_at",
                self.schema, self.schema
            ),
            &[&self.app_id, &worker_id, &lease_until, &now, &names],
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
        let lease_until = now_ms().saturating_add(clamp_lease_ms(lease_ms));
        self.update_running(
            "lease_until = $4",
            &[&self.app_id, &id, &worker_id, &lease_until],
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
        let mut client = self.client.lock();
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
            &[&self.app_id, &run_id, &step_name, &result, &now_ms()],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub(super) fn result(
        &self,
        name: &str,
        id: &str,
    ) -> Result<Option<crate::enqueue::RunResult>, RunsDbError> {
        let mut client = self.client.lock();
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
            &[&self.app_id, &id, &worker_id, &now_ms(), &serde_json::to_string(result)?],
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
            &[&self.app_id, &id, &worker_id, &reason, &now_ms()],
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
                &[&self.app_id, &id, &worker_id, &error, &now_ms()],
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
        let mut client = self.client.lock();
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

    pub(super) fn reclaim_expired(&self) -> Result<u64, RunsDbError> {
        let mut client = self.client.lock();
        let rows = client.execute(
            &format!(
                "UPDATE {}.runs SET status='pending', worker_id=NULL, lease_until=NULL
                 WHERE app_id=$1 AND status='running' AND lease_until IS NOT NULL AND lease_until < $2",
                self.schema
            ),
            &[&self.app_id, &now_ms()],
        )?;
        Ok(rows)
    }

    pub(super) fn reclaim_expired_with_workers(&self) -> Result<Vec<String>, RunsDbError> {
        let mut client = self.client.lock();
        // Return exactly the owners whose rows this statement reclaims.
        // Row locks prevent a heartbeat or another reclaimer from changing
        // ownership between selection and accounting.
        let workers = client
            .query(
                &format!(
                    "WITH expired AS (
                    SELECT id, worker_id FROM {schema}.runs
                    WHERE app_id=$1 AND status='running' AND lease_until < $2
                    FOR UPDATE
                 )
                 UPDATE {schema}.runs r SET status='pending', worker_id=NULL, lease_until=NULL
                 FROM expired WHERE r.app_id=$1 AND r.id=expired.id
                 RETURNING expired.worker_id",
                    schema = self.schema
                ),
                &[&self.app_id, &now_ms()],
            )?
            .into_iter()
            .filter_map(|row| row.get::<_, Option<String>>(0))
            .collect();
        Ok(workers)
    }

    pub(super) fn in_flight_by_worker(&self) -> Result<HashMap<String, u32>, RunsDbError> {
        let mut client = self.client.lock();
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
        let mut client = self.client.lock();
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
        let now = now_ms();
        let mut client = self.client.lock();
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
        let mut client = self.client.lock();
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
        let mut client = self.client.lock();
        let exists: bool = client
            .query_one(
                &format!(
                    "SELECT EXISTS(
                        SELECT 1 FROM {}.runs
                        WHERE app_id=$1 AND status='pending' AND run_at <= $2 LIMIT 1
                     )",
                    self.schema
                ),
                &[&self.app_id, &now_ms()],
            )?
            .get(0);
        Ok(exists)
    }

    pub(super) fn replace_schedules(&self, schedules: &[ScheduleSpec]) -> Result<(), RunsDbError> {
        let mut client = self.client.lock();
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
        let now = now_ms();
        for schedule in schedules {
            tx.execute(
                &format!(
                    "INSERT INTO {}.schedules (app_id, name, cron, last_run_at)
                     VALUES ($1, $2, $3, $4)
                     ON CONFLICT(app_id, name) DO UPDATE SET cron=excluded.cron",
                    self.schema
                ),
                &[&self.app_id, &schedule.name, &schedule.cron, &now],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub(super) fn list_schedules(&self) -> Result<Vec<ScheduleRow>, RunsDbError> {
        let mut client = self.client.lock();
        let rows = client.query(
            &format!(
                "SELECT name, cron, last_run_at FROM {}.schedules WHERE app_id=$1",
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
            })
            .collect())
    }

    pub(super) fn set_schedule_last_run_at(&self, name: &str, ts: i64) -> Result<(), RunsDbError> {
        let mut client = self.client.lock();
        client.execute(
            &format!(
                "UPDATE {}.schedules SET last_run_at=$3 WHERE app_id=$1 AND name=$2",
                self.schema
            ),
            &[&self.app_id, &name, &ts],
        )?;
        Ok(())
    }

    fn update_running(
        &self,
        set_clause: &str,
        params: &[&(dyn postgres::types::ToSql + Sync)],
    ) -> Result<(), RunsDbError> {
        let mut client = self.client.lock();
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
             PRIMARY KEY(app_id, name)
         );

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
                            db.reclaim_expired_with_workers().unwrap().len() as u64
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
}
