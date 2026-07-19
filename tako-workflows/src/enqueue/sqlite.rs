use parking_lot::Mutex;
use std::collections::HashMap;
use std::path::Path;
use tako_core::{EnqueueOpts, EnqueueRunResponse, RunPayload, ScheduleSpec};
use turso::{Connection, Value, params_from_iter};

pub(crate) use tako_sqlite::block_on;
use tako_sqlite::commit_or_rollback;

use super::{RunsDbError, ScheduleRow, clamp_lease_ms, now_ms};
use crate::schema;

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
        let path = path
            .to_str()
            .ok_or_else(|| RunsDbError::Storage("non-UTF-8 workflow db path".into()))?;
        let conn = block_on(async {
            let conn = tako_sqlite::open_local(path).await?;
            schema::init(&conn).await?;
            Ok::<_, turso::Error>(conn)
        })?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    #[cfg(test)]
    pub(super) fn open_in_memory() -> Result<Self, RunsDbError> {
        let conn = block_on(async {
            let conn = tako_sqlite::open_in_memory().await?;
            schema::init(&conn).await?;
            Ok::<_, turso::Error>(conn)
        })?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    #[cfg(test)]
    pub(super) fn raw_execute(&self, sql: &str, params: impl turso::IntoParams) {
        let conn = self.conn.lock();
        block_on(conn.execute(sql, params)).expect("raw execute");
    }

    #[cfg(test)]
    pub(super) fn raw_query_values(&self, sql: &str, params: impl turso::IntoParams) -> Vec<Value> {
        let conn = self.conn.lock();
        block_on(async {
            let mut rows = conn.query(sql, params).await.expect("raw query");
            let row = rows
                .next()
                .await
                .expect("raw row")
                .expect("no row returned");
            (0..row.column_count())
                .map(|i| row.get_value(i).expect("column value"))
                .collect()
        })
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
        block_on(async {
            let tx = conn.transaction().await?;
            let result: Result<EnqueueRunResponse, RunsDbError> = async {
                if let Some(key) = unique_key {
                    let mut stmt = tx
                        .prepare_cached(
                            "SELECT id FROM runs WHERE unique_key = ?1 AND status IN ('pending','running') LIMIT 1",
                        )
                        .await?;
                    let mut rows = stmt.query((key,)).await?;
                    if let Some(row) = rows.next().await? {
                        let id: String = row.get(0)?;
                        return Ok(EnqueueRunResponse {
                            id,
                            deduplicated: true,
                        });
                    }
                }

                let mut stmt = tx
                    .prepare_cached(
                        "INSERT INTO runs
                         (id, name, payload, status, attempts, max_attempts, run_at, lease_until, worker_id,
                          last_error, created_at, unique_key)
                         VALUES (?1, ?2, ?3, 'pending', 0, ?4, ?5, NULL, NULL, NULL, ?6, ?7)",
                    )
                    .await?;
                stmt.execute((
                    id.as_str(),
                    name,
                    payload_json.as_str(),
                    max_attempts,
                    run_at,
                    now_ms,
                    unique_key,
                ))
                .await?;

                Ok(EnqueueRunResponse {
                    id: id.clone(),
                    deduplicated: false,
                })
            }
            .await;
            commit_or_rollback(tx, result).await
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

        let conn = self.conn.lock();
        let (claimed, step_rows) = block_on(async {
            let mut stmt = conn.prepare_cached(&sql).await?;
            let mut rows = stmt.query(params_from_iter(params)).await?;
            let claimed = match rows.next().await? {
                Some(row) => (
                    row.get::<String>(0)?,
                    row.get::<String>(1)?,
                    row.get::<String>(2)?,
                    row.get::<String>(3)?,
                    row.get::<i64>(4)? as u32,
                    row.get::<i64>(5)? as u32,
                    row.get::<i64>(6)?,
                ),
                None => return Ok::<_, RunsDbError>((None, Vec::new())),
            };
            drop(rows);

            let mut step_stmt = conn
                .prepare_cached("SELECT name, result FROM steps WHERE run_id = ?1")
                .await?;
            let mut rows = step_stmt.query((claimed.0.as_str(),)).await?;
            let mut step_rows = Vec::new();
            while let Some(row) = rows.next().await? {
                step_rows.push((row.get::<String>(0)?, row.get::<String>(1)?));
            }
            Ok((Some(claimed), step_rows))
        })?;

        let Some(claimed) = claimed else {
            return Ok(None);
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
        let rows = block_on(conn.execute(
            "UPDATE runs SET lease_until = ?1
             WHERE id = ?2 AND worker_id = ?3 AND status='running'",
            (lease_until, id, worker_id),
        ))?;
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
        block_on(async {
            let rows = conn
                .execute(
                    "INSERT OR IGNORE INTO steps (run_id, name, result, completed_at)
                     SELECT ?1, ?2, ?3, ?4
                     FROM runs WHERE id = ?1 AND worker_id = ?5 AND status='running'",
                    (run_id, step_name, r.as_str(), now_ms(), worker_id),
                )
                .await?;
            // rows == 0 can mean "step already saved (IGNORE)" or "stale
            // worker". Distinguish by probing the run's worker_id.
            if rows == 0 {
                let mut probe = conn
                    .query(
                        "SELECT worker_id FROM runs WHERE id = ?1 AND status='running'",
                        (run_id,),
                    )
                    .await?;
                match probe.next().await? {
                    Some(row) => match row.get::<Option<String>>(0)? {
                        Some(wid) if wid == worker_id => {}
                        _ => return Err(RunsDbError::StaleWorker),
                    },
                    None => return Err(RunsDbError::StaleWorker),
                }
            }
            Ok(())
        })
    }

    pub(super) fn complete(&self, id: &str, worker_id: &str) -> Result<(), RunsDbError> {
        let conn = self.conn.lock();
        let rows = block_on(conn.execute(
            "UPDATE runs SET status='succeeded', worker_id=NULL, lease_until=NULL
             WHERE id = ?1 AND worker_id = ?2 AND status='running'",
            (id, worker_id),
        ))?;
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
        let rows = block_on(conn.execute(
            "UPDATE runs SET status='cancelled', last_error=?1, worker_id=NULL, lease_until=NULL
             WHERE id = ?2 AND worker_id = ?3 AND status='running'",
            (reason, id, worker_id),
        ))?;
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
            block_on(conn.execute(
                "UPDATE runs SET status='dead', last_error=?1, worker_id=NULL, lease_until=NULL
                 WHERE id = ?2 AND worker_id = ?3 AND status='running'",
                (error, id, worker_id),
            ))?
        } else {
            let next = next_run_at_ms.ok_or_else(|| {
                RunsDbError::Storage("fail(finalize=false) requires next_run_at_ms".into())
            })?;
            block_on(conn.execute(
                "UPDATE runs SET status='pending', last_error=?1, worker_id=NULL, lease_until=NULL, run_at=?2
                 WHERE id = ?3 AND worker_id = ?4 AND status='running'",
                (error, next, id, worker_id),
            ))?
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
        let rows = block_on(conn.execute(
            "UPDATE runs SET status='pending', worker_id=NULL, lease_until=NULL,
                              run_at=?1, attempts=attempts-1
             WHERE id = ?2 AND worker_id = ?3 AND status='running'",
            (run_at, id, worker_id),
        ))?;
        if rows == 0 {
            return Err(RunsDbError::StaleWorker);
        }
        Ok(())
    }

    pub(super) fn reclaim_expired(&self) -> Result<u64, RunsDbError> {
        let conn = self.conn.lock();
        let changes = block_on(conn.execute(
            "UPDATE runs SET status='pending', worker_id=NULL, lease_until=NULL
             WHERE status='running' AND lease_until IS NOT NULL AND lease_until < ?1",
            (now_ms(),),
        ))?;
        Ok(changes)
    }

    pub(super) fn reclaim_expired_with_workers(&self) -> Result<Vec<String>, RunsDbError> {
        let mut conn = self.conn.lock();
        block_on(async {
            let tx = conn.transaction().await?;
            let result: Result<Vec<String>, RunsDbError> = async {
                let mut stmt = tx
                    .prepare(
                        "SELECT worker_id FROM runs
                         WHERE status='running' AND lease_until IS NOT NULL
                           AND lease_until < ?1 AND worker_id IS NOT NULL",
                    )
                    .await?;
                let mut rows = stmt.query((now_ms(),)).await?;
                let mut workers = Vec::new();
                while let Some(row) = rows.next().await? {
                    workers.push(row.get::<String>(0)?);
                }
                drop(rows);
                tx.execute(
                    "UPDATE runs SET status='pending', worker_id=NULL, lease_until=NULL
                     WHERE status='running' AND lease_until IS NOT NULL AND lease_until < ?1",
                    (now_ms(),),
                )
                .await?;
                Ok(workers)
            }
            .await;
            commit_or_rollback(tx, result).await
        })
    }

    pub(super) fn in_flight_by_worker(&self) -> Result<HashMap<String, u32>, RunsDbError> {
        let conn = self.conn.lock();
        block_on(async {
            let mut rows = conn
                .query(
                    "SELECT worker_id, COUNT(*) FROM runs
                     WHERE status='running' AND worker_id IS NOT NULL
                     GROUP BY worker_id",
                    (),
                )
                .await?;
            let mut out = HashMap::new();
            while let Some(row) = rows.next().await? {
                let worker: String = row.get(0)?;
                let count: i64 = row.get(1)?;
                out.insert(worker, count as u32);
            }
            Ok(out)
        })
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
        block_on(async {
            let tx = conn.transaction().await?;
            let result: Result<(), RunsDbError> = async {
                let rows = tx
                    .execute(
                        "UPDATE runs SET status='pending', worker_id=NULL, lease_until=NULL,
                                          run_at=?1, attempts=attempts-1
                         WHERE id = ?2 AND worker_id = ?3 AND status='running'",
                        (timeout_at_ms.unwrap_or(i64::MAX), run_id, worker_id),
                    )
                    .await?;
                if rows == 0 {
                    return Err(RunsDbError::StaleWorker);
                }
                tx.execute(
                    "INSERT OR REPLACE INTO event_waiters (run_id, step_name, event_name, expires_at)
                     VALUES (?1, ?2, ?3, ?4)",
                    (run_id, step_name, event_name, timeout_at_ms),
                )
                .await?;
                Ok(())
            }
            .await;
            commit_or_rollback(tx, result).await
        })
    }

    pub(super) fn signal(
        &self,
        event_name: &str,
        payload: &serde_json::Value,
    ) -> Result<u64, RunsDbError> {
        let payload_json = serde_json::to_string(payload)?;
        let now = now_ms();
        let mut conn = self.conn.lock();
        block_on(async {
            let tx = conn.transaction().await?;
            let result: Result<u64, RunsDbError> = async {
                // Materialize the event payload as a step result for every waiter.
                // Then wake the runs and clear the waiter rows.
                let mut stmt = tx
                    .prepare("SELECT run_id, step_name FROM event_waiters WHERE event_name = ?1")
                    .await?;
                let mut rows = stmt.query((event_name,)).await?;
                let mut waiters: Vec<(String, String)> = Vec::new();
                while let Some(row) = rows.next().await? {
                    waiters.push((row.get(0)?, row.get(1)?));
                }
                drop(rows);

                let mut woken = 0u64;
                for (run_id, step_name) in &waiters {
                    tx.execute(
                        "INSERT OR IGNORE INTO steps (run_id, name, result, completed_at)
                         VALUES (?1, ?2, ?3, ?4)",
                        (run_id.as_str(), step_name.as_str(), payload_json.as_str(), now),
                    )
                    .await?;
                    tx.execute(
                        "UPDATE runs SET status='pending', run_at=?1 WHERE id = ?2 AND status='pending'",
                        (now, run_id.as_str()),
                    )
                    .await?;
                    tx.execute(
                        "DELETE FROM event_waiters WHERE run_id = ?1 AND step_name = ?2",
                        (run_id.as_str(), step_name.as_str()),
                    )
                    .await?;
                    woken += 1;
                }
                Ok(woken)
            }
            .await;
            commit_or_rollback(tx, result).await
        })
    }

    pub(super) fn pending_count(&self) -> Result<u64, RunsDbError> {
        let conn = self.conn.lock();
        block_on(async {
            let mut rows = conn
                .query("SELECT COUNT(*) FROM runs WHERE status='pending'", ())
                .await?;
            let row = rows
                .next()
                .await?
                .ok_or_else(|| RunsDbError::Storage("count query returned no row".into()))?;
            Ok(row.get::<i64>(0)? as u64)
        })
    }

    pub(super) fn has_runnable_work(&self) -> Result<bool, RunsDbError> {
        let conn = self.conn.lock();
        block_on(async {
            let mut rows = conn
                .query(
                    "SELECT EXISTS(
                        SELECT 1 FROM runs
                        WHERE status='pending' AND run_at <= ?1
                        LIMIT 1
                     )",
                    (now_ms(),),
                )
                .await?;
            let row = rows
                .next()
                .await?
                .ok_or_else(|| RunsDbError::Storage("exists query returned no row".into()))?;
            Ok(row.get::<i64>(0)? != 0)
        })
    }

    pub(super) fn replace_schedules(&self, schedules: &[ScheduleSpec]) -> Result<(), RunsDbError> {
        let mut conn = self.conn.lock();
        block_on(async {
            let tx = conn.transaction().await?;
            let result: Result<(), RunsDbError> = async {
                if schedules.is_empty() {
                    tx.execute("DELETE FROM schedules", ()).await?;
                } else {
                    let placeholders = schedules.iter().map(|_| "?").collect::<Vec<_>>().join(",");
                    let sql = format!("DELETE FROM schedules WHERE name NOT IN ({})", placeholders);
                    let params: Vec<Value> = schedules
                        .iter()
                        .map(|s| Value::Text(s.name.clone()))
                        .collect();
                    tx.execute(&sql, params_from_iter(params)).await?;
                }

                let now_ms = chrono::Utc::now().timestamp_millis();
                for s in schedules {
                    tx.execute(
                        "INSERT INTO schedules (name, cron, last_run_at) VALUES (?1, ?2, ?3)
                         ON CONFLICT(name) DO UPDATE SET cron = excluded.cron",
                        (s.name.as_str(), s.cron.as_str(), now_ms),
                    )
                    .await?;
                }
                Ok(())
            }
            .await;
            commit_or_rollback(tx, result).await
        })
    }

    pub(super) fn list_schedules(&self) -> Result<Vec<ScheduleRow>, RunsDbError> {
        let conn = self.conn.lock();
        block_on(async {
            let mut rows = conn
                .query("SELECT name, cron, last_run_at FROM schedules", ())
                .await?;
            let mut out = Vec::new();
            while let Some(row) = rows.next().await? {
                out.push(ScheduleRow {
                    name: row.get(0)?,
                    cron: row.get(1)?,
                    last_run_at: row.get::<Option<i64>>(2)?,
                });
            }
            Ok(out)
        })
    }

    pub(super) fn set_schedule_last_run_at(&self, name: &str, ts: i64) -> Result<(), RunsDbError> {
        let conn = self.conn.lock();
        block_on(conn.execute(
            "UPDATE schedules SET last_run_at = ?1 WHERE name = ?2",
            (ts, name),
        ))?;
        Ok(())
    }
}
