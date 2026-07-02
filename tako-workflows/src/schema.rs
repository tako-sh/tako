//! SQLite schema for the workflow engine. Kept in sync with the JS/Go SDKs.

use rusqlite::Connection;

pub(crate) const SCHEMA_SQL: &str = r#"
PRAGMA journal_mode = WAL;
PRAGMA synchronous = NORMAL;
PRAGMA busy_timeout = 5000;
PRAGMA foreign_keys = ON;

CREATE TABLE IF NOT EXISTS runs (
  id            TEXT PRIMARY KEY,
  name          TEXT NOT NULL,
  payload       TEXT NOT NULL,
  status        TEXT NOT NULL,                 -- pending | running | succeeded | cancelled | dead
  attempts      INTEGER NOT NULL DEFAULT 0,
  max_attempts  INTEGER NOT NULL,
  run_at        INTEGER NOT NULL,              -- unix ms
  lease_until   INTEGER,                       -- unix ms
  worker_id     TEXT,
  last_error    TEXT,
  created_at    INTEGER NOT NULL,
  unique_key    TEXT
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_runs_unique_live
  ON runs(unique_key)
  WHERE unique_key IS NOT NULL AND status IN ('pending','running');

CREATE INDEX IF NOT EXISTS idx_runs_claim
  ON runs(run_at)
  WHERE status='pending';

CREATE INDEX IF NOT EXISTS idx_runs_lease
  ON runs(lease_until)
  WHERE status='running';

-- Per-step memoization. One row per completed step within a run; the result
-- column holds the JSON-encoded return value of the step body. CASCADE on
-- run delete keeps things tidy.
CREATE TABLE IF NOT EXISTS steps (
  run_id        TEXT NOT NULL,
  name          TEXT NOT NULL,
  result        TEXT NOT NULL,
  completed_at  INTEGER NOT NULL,
  PRIMARY KEY (run_id, name),
  FOREIGN KEY (run_id) REFERENCES runs(id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS schedules (
  name         TEXT PRIMARY KEY,
  cron         TEXT NOT NULL,
  last_run_at  INTEGER
);

-- Parked runs waiting on an event. When `workflowsEngine.signal(name, payload)`
-- fires, every waiter with a matching event_name is re-scheduled (run_at=now)
-- and the payload is materialized as a step result on resume.
CREATE TABLE IF NOT EXISTS event_waiters (
  run_id      TEXT NOT NULL,
  step_name   TEXT NOT NULL,
  event_name  TEXT NOT NULL,
  expires_at  INTEGER,                          -- unix ms; null = no timeout
  PRIMARY KEY (run_id, step_name),
  FOREIGN KEY (run_id) REFERENCES runs(id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_event_waiters_name
  ON event_waiters(event_name);

CREATE INDEX IF NOT EXISTS idx_event_waiters_expiry
  ON event_waiters(expires_at)
  WHERE expires_at IS NOT NULL;
"#;

pub(crate) fn init(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(SCHEMA_SQL)
}
