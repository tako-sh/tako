//! A one-time import of an app's runs from the per-server SQLite store of
//! earlier releases, `<data dir>/apps/<app>/data/tako/workflows.sqlite`, into
//! its hosted workflow database. Each server imports its own file once, then
//! renames it, so pending work, waits, saved steps and used run IDs carry
//! over the upgrade.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Value, json};

const FILE: &str = "workflows.sqlite";
const IMPORTED: &str = "workflows.sqlite.imported";

/// Where an earlier release kept `app`'s runs on this server.
pub(crate) fn path(data_dir: &Path, app: &str) -> PathBuf {
    data_dir
        .join("apps")
        .join(app)
        .join("data")
        .join("tako")
        .join(FILE)
}

/// The rows of each table of a legacy store, as JSON objects.
#[derive(Debug, Default)]
pub(crate) struct Rows {
    pub(crate) runs: Vec<Value>,
    pub(crate) steps: Vec<Value>,
    pub(crate) schedules: Vec<Value>,
    pub(crate) waiters: Vec<Value>,
    pub(crate) inbox: Vec<Value>,
}

impl Rows {
    pub(crate) fn len(&self) -> usize {
        self.runs.len()
            + self.steps.len()
            + self.schedules.len()
            + self.waiters.len()
            + self.inbox.len()
    }
}

/// Read every row of the legacy store at `path`. Runs that were running
/// lost their worker with the old process: one with attempts left waits
/// for a new claim; one without keeps an expired, ownerless lease, so the
/// next reclaim ends it as dead, as it does for a lost worker.
pub(crate) fn read(path: &Path) -> rusqlite::Result<Rows> {
    use rusqlite::{Connection, OpenFlags};

    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
    conn.busy_timeout(Duration::from_secs(5))?;
    // The renamed file keeps every committed row without its WAL.
    let _ = conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()));
    let tables: BTreeSet<String> = conn
        .prepare("SELECT name FROM sqlite_schema WHERE type = 'table'")?
        .query_map([], |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    let mut rows = Rows::default();
    if tables.contains("runs") {
        let mut statement = conn.prepare(
            "SELECT id, name, payload, status, attempts, max_attempts, run_at, lease_until,
                    worker_id, last_error, created_at, finished_at, result
             FROM runs",
        )?;
        let mut query = statement.query([])?;
        while let Some(row) = query.next()? {
            let status: String = row.get(3)?;
            let attempts: i64 = row.get(4)?;
            let max_attempts: i64 = row.get(5)?;
            let (status, lease_until, worker_id) = match status.as_str() {
                "running" if attempts >= max_attempts => (status, Some(0), None),
                "running" => ("pending".to_string(), None, None),
                _ => (status, row.get::<_, Option<i64>>(7)?, row.get(8)?),
            };
            let worker_id: Option<String> = worker_id;
            rows.runs.push(json!({
                "id": row.get::<_, String>(0)?,
                "name": row.get::<_, String>(1)?,
                "payload": row.get::<_, String>(2)?,
                "status": status,
                "attempts": attempts,
                "max_attempts": max_attempts,
                "run_at": row.get::<_, i64>(6)?,
                "lease_until": lease_until,
                "worker_id": worker_id,
                "last_error": row.get::<_, Option<String>>(9)?,
                "created_at": row.get::<_, i64>(10)?,
                "finished_at": row.get::<_, Option<i64>>(11)?,
                "result": row.get::<_, Option<String>>(12)?,
            }));
        }
    }
    if tables.contains("steps") {
        let mut statement = conn.prepare("SELECT run_id, name, result, completed_at FROM steps")?;
        let mut query = statement.query([])?;
        while let Some(row) = query.next()? {
            rows.steps.push(json!({
                "run_id": row.get::<_, String>(0)?,
                "name": row.get::<_, String>(1)?,
                "result": row.get::<_, String>(2)?,
                "completed_at": row.get::<_, i64>(3)?,
            }));
        }
    }
    if tables.contains("schedules") {
        let mut statement =
            conn.prepare("SELECT name, cron, last_run_at, max_attempts FROM schedules")?;
        let mut query = statement.query([])?;
        while let Some(row) = query.next()? {
            rows.schedules.push(json!({
                "name": row.get::<_, String>(0)?,
                "cron": row.get::<_, String>(1)?,
                "last_run_at": row.get::<_, Option<i64>>(2)?,
                "max_attempts": row.get::<_, Option<i64>>(3)?,
            }));
        }
    }
    if tables.contains("event_waiters") {
        let mut statement =
            conn.prepare("SELECT run_id, step_name, event_name, expires_at FROM event_waiters")?;
        let mut query = statement.query([])?;
        while let Some(row) = query.next()? {
            rows.waiters.push(json!({
                "run_id": row.get::<_, String>(0)?,
                "step_name": row.get::<_, String>(1)?,
                "event_name": row.get::<_, String>(2)?,
                "expires_at": row.get::<_, Option<i64>>(3)?,
            }));
        }
    }
    if tables.contains("run_inbox") {
        let mut statement = conn.prepare(
            "SELECT run_id, event_name, payload, payload_bytes, received_at FROM run_inbox",
        )?;
        let mut query = statement.query([])?;
        while let Some(row) = query.next()? {
            rows.inbox.push(json!({
                "run_id": row.get::<_, String>(0)?,
                "event_name": row.get::<_, String>(1)?,
                "payload": row.get::<_, String>(2)?,
                "payload_bytes": row.get::<_, i64>(3)?,
                "received_at": row.get::<_, i64>(4)?,
            }));
        }
    }
    Ok(rows)
}

/// Rename an imported store so no later start imports it again; the copy
/// stays for inspection until an operator deletes it.
pub(crate) fn retire(path: &Path) -> std::io::Result<()> {
    std::fs::rename(path, path.with_file_name(IMPORTED))?;
    for suffix in ["-wal", "-shm"] {
        let _ = std::fs::remove_file(path.with_file_name(format!("{FILE}{suffix}")));
    }
    Ok(())
}

/// `rows` as JSON arrays of at most about `limit` bytes each, so every
/// import statement stays well inside the database host's request limit.
pub(crate) fn chunks(rows: &[Value], limit: usize) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut current: Vec<String> = Vec::new();
    let mut size = 0;
    for row in rows {
        let text = row.to_string();
        if !current.is_empty() && size + text.len() > limit {
            chunks.push(format!("[{}]", current.join(",")));
            current.clear();
            size = 0;
        }
        size += text.len() + 1;
        current.push(text);
    }
    if !current.is_empty() {
        chunks.push(format!("[{}]", current.join(",")));
    }
    chunks
}
