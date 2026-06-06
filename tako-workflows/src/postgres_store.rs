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
        let unique_key = opts.unique_key.as_deref();
        let payload_json = serde_json::to_string(payload)?;
        let id = nanoid::nanoid!();

        let mut client = self.client.lock();
        let mut tx = client.transaction()?;

        if let Some(key) = unique_key
            && let Some(existing) = self.select_existing_unique(&mut tx, key)?
        {
            tx.commit()?;
            return Ok(EnqueueRunResponse {
                id: existing,
                deduplicated: true,
            });
        }

        let inserted = tx.execute(
            &format!(
                "INSERT INTO {}.runs
                 (app_id, id, name, payload, status, attempts, max_attempts, run_at, lease_until,
                  worker_id, last_error, created_at, unique_key)
                 VALUES ($1, $2, $3, $4, 'pending', 0, $5, $6, NULL, NULL, NULL, $7, $8)
                 ON CONFLICT DO NOTHING",
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
                &unique_key,
            ],
        );
        let inserted = inserted?;
        if inserted == 0 {
            if let Some(unique_key) = unique_key {
                let existing = self
                    .select_existing_unique(&mut tx, unique_key)?
                    .ok_or_else(|| {
                        RunsDbError::UnsupportedBackend("workflow id collision".into())
                    })?;
                tx.commit()?;
                return Ok(EnqueueRunResponse {
                    id: existing,
                    deduplicated: true,
                });
            }
            return Err(RunsDbError::UnsupportedBackend(
                "workflow id collision".into(),
            ));
        }

        tx.commit()?;
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
        let rows = client.execute(
            &format!(
                "INSERT INTO {}.steps (app_id, run_id, name, result, completed_at)
                 SELECT $1, $2, $3, $4, $5
                 FROM {}.runs
                 WHERE app_id=$1 AND id=$2 AND worker_id=$6 AND status='running'
                 ON CONFLICT(app_id, run_id, name) DO NOTHING",
                self.schema, self.schema
            ),
            &[
                &self.app_id,
                &run_id,
                &step_name,
                &result,
                &now_ms(),
                &worker_id,
            ],
        )?;
        if rows == 0 && !self.running_owned_by(&mut client, run_id, worker_id)? {
            return Err(RunsDbError::StaleWorker);
        }
        Ok(())
    }

    pub(super) fn complete(&self, id: &str, worker_id: &str) -> Result<(), RunsDbError> {
        self.update_running(
            "status='succeeded', worker_id=NULL, lease_until=NULL",
            &[&self.app_id, &id, &worker_id],
        )
    }

    pub(super) fn cancel(
        &self,
        id: &str,
        worker_id: &str,
        reason: Option<&str>,
    ) -> Result<(), RunsDbError> {
        self.update_running(
            "status='cancelled', last_error=$4, worker_id=NULL, lease_until=NULL",
            &[&self.app_id, &id, &worker_id, &reason],
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
                "status='dead', last_error=$4, worker_id=NULL, lease_until=NULL",
                &[&self.app_id, &id, &worker_id, &error],
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
        let mut tx = client.transaction()?;
        let workers = tx
            .query(
                &format!(
                    "SELECT worker_id FROM {}.runs
                     WHERE app_id=$1 AND status='running' AND lease_until IS NOT NULL
                       AND lease_until < $2 AND worker_id IS NOT NULL",
                    self.schema
                ),
                &[&self.app_id, &now_ms()],
            )?
            .into_iter()
            .map(|row| row.get(0))
            .collect::<Vec<String>>();
        tx.execute(
            &format!(
                "UPDATE {}.runs SET status='pending', worker_id=NULL, lease_until=NULL
                 WHERE app_id=$1 AND status='running' AND lease_until IS NOT NULL AND lease_until < $2",
                self.schema
            ),
            &[&self.app_id, &now_ms()],
        )?;
        tx.commit()?;
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
                 WHERE app_id=$1 AND event_name=$2",
                self.schema
            ),
            &[&self.app_id, &event_name],
        )?;
        let mut woken = 0;
        for row in waiters {
            let run_id: String = row.get(0);
            let step_name: String = row.get(1);
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
            tx.execute(
                &format!(
                    "DELETE FROM {}.event_waiters
                     WHERE app_id=$1 AND run_id=$2 AND step_name=$3",
                    self.schema
                ),
                &[&self.app_id, &run_id, &step_name],
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

    fn running_owned_by(
        &self,
        client: &mut Client,
        run_id: &str,
        worker_id: &str,
    ) -> Result<bool, RunsDbError> {
        let owner: Option<String> = client
            .query_opt(
                &format!(
                    "SELECT worker_id FROM {}.runs
                     WHERE app_id=$1 AND id=$2 AND status='running'",
                    self.schema
                ),
                &[&self.app_id, &run_id],
            )?
            .and_then(|row| row.get(0));
        Ok(owner.as_deref() == Some(worker_id))
    }

    fn select_existing_unique(
        &self,
        tx: &mut postgres::Transaction<'_>,
        unique_key: &str,
    ) -> Result<Option<String>, RunsDbError> {
        Ok(tx
            .query_opt(
                &format!(
                    "SELECT id FROM {}.runs
                     WHERE app_id=$1 AND unique_key=$2 AND status IN ('pending','running')
                     LIMIT 1",
                    self.schema
                ),
                &[&self.app_id, &unique_key],
            )?
            .map(|row| row.get(0)))
    }
}

fn init_schema(client: &mut Client, schema: &str) -> Result<(), RunsDbError> {
    client.batch_execute(&format!(
        "CREATE SCHEMA IF NOT EXISTS {schema};
         CREATE TABLE IF NOT EXISTS {schema}.runs (
             app_id TEXT NOT NULL,
             id TEXT PRIMARY KEY,
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
             unique_key TEXT
         );
         CREATE INDEX IF NOT EXISTS idx_runs_app_status_run_at
           ON {schema}.runs(app_id, status, run_at);
         CREATE INDEX IF NOT EXISTS idx_runs_app_lease
           ON {schema}.runs(app_id, lease_until);
         CREATE UNIQUE INDEX IF NOT EXISTS idx_runs_app_unique_active
           ON {schema}.runs(app_id, unique_key)
           WHERE unique_key IS NOT NULL AND status IN ('pending','running');

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
           ON {schema}.event_waiters(app_id, event_name);"
    ))?;
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
