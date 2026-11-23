//! Tables of the hosted workflow store. Every name starts with
//! `tako_workflow_` and every table is keyed by `app`, so several apps, and
//! the app's own SQL, can share one database. Mirrors the Postgres schema.

use tako_db::client::Statement;

/// Recorded in `tako_workflow_meta`. Raise it with a migration whenever a
/// table changes; a server refuses a store written by a newer schema.
pub(super) const VERSION: i64 = 1;

const TABLES: &[&str] = &[
    // `schema_version`, and the counter that orders schedule registrations
    // across servers (see `next_schedule_revision`).
    "CREATE TABLE IF NOT EXISTS tako_workflow_meta (
        name TEXT PRIMARY KEY,
        value INTEGER NOT NULL
    ) STRICT, WITHOUT ROWID",
    "CREATE TABLE IF NOT EXISTS tako_workflow_runs (
        app TEXT NOT NULL,
        id TEXT NOT NULL,
        name TEXT NOT NULL,
        payload TEXT NOT NULL,
        status TEXT NOT NULL,
        attempts INTEGER NOT NULL,
        max_attempts INTEGER NOT NULL,
        run_at INTEGER NOT NULL,
        lease_until INTEGER,
        worker_id TEXT,
        last_error TEXT,
        created_at INTEGER NOT NULL,
        finished_at INTEGER,
        result TEXT,
        PRIMARY KEY (app, id)
    ) STRICT",
    // Claims walk due runs in order. The rowid ending every index entry
    // makes the order total (first enqueued first among equal due times), so
    // every statement of a claim picks the same candidate.
    "CREATE INDEX IF NOT EXISTS tako_workflow_runs_claim
        ON tako_workflow_runs (app, run_at) WHERE status = 'pending'",
    // Only running runs can expire.
    "CREATE INDEX IF NOT EXISTS tako_workflow_runs_lease
        ON tako_workflow_runs (app, lease_until) WHERE status = 'running'",
    // Operator listings show the newest runs first.
    "CREATE INDEX IF NOT EXISTS tako_workflow_runs_created
        ON tako_workflow_runs (app, created_at)",
    "CREATE INDEX IF NOT EXISTS tako_workflow_runs_finished
        ON tako_workflow_runs (app, finished_at, id)
        WHERE status IN ('succeeded', 'cancelled', 'dead')",
    "CREATE TABLE IF NOT EXISTS tako_workflow_steps (
        app TEXT NOT NULL,
        run_id TEXT NOT NULL,
        name TEXT NOT NULL,
        result TEXT NOT NULL,
        completed_at INTEGER NOT NULL,
        PRIMARY KEY (app, run_id, name)
    ) STRICT",
    "CREATE TABLE IF NOT EXISTS tako_workflow_schedules (
        app TEXT NOT NULL,
        name TEXT NOT NULL,
        cron TEXT NOT NULL,
        last_run_at INTEGER,
        max_attempts INTEGER,
        PRIMARY KEY (app, name)
    ) STRICT",
    // The schedule set registered last, by the release with the highest
    // revision. An older release cannot replace it.
    "CREATE TABLE IF NOT EXISTS tako_workflow_schedule_releases (
        app TEXT PRIMARY KEY,
        build TEXT NOT NULL,
        revision INTEGER NOT NULL
    ) STRICT, WITHOUT ROWID",
    "CREATE TABLE IF NOT EXISTS tako_workflow_event_waiters (
        app TEXT NOT NULL,
        run_id TEXT NOT NULL,
        step_name TEXT NOT NULL,
        event_name TEXT NOT NULL,
        expires_at INTEGER,
        PRIMARY KEY (app, run_id, step_name)
    ) STRICT",
    "CREATE INDEX IF NOT EXISTS tako_workflow_event_waiters_event
        ON tako_workflow_event_waiters (app, event_name)",
    "CREATE INDEX IF NOT EXISTS tako_workflow_event_waiters_expiry
        ON tako_workflow_event_waiters (app, expires_at) WHERE expires_at IS NOT NULL",
    // Signals addressed to one run that arrived before it waited for them;
    // see the SQLite schema. Pruned with their run.
    "CREATE TABLE IF NOT EXISTS tako_workflow_run_inbox (
        app TEXT NOT NULL,
        run_id TEXT NOT NULL,
        event_name TEXT NOT NULL,
        payload TEXT NOT NULL,
        payload_bytes INTEGER NOT NULL,
        received_at INTEGER NOT NULL,
        PRIMARY KEY (app, run_id, event_name)
    ) STRICT",
];

/// Idempotent: creates missing tables and records the schema version.
pub(super) fn init() -> Vec<Statement> {
    let mut statements: Vec<_> = TABLES
        .iter()
        .map(|sql| Statement::new(*sql, vec![]))
        .collect();
    statements.push(Statement::new(
        "INSERT INTO tako_workflow_meta (name, value)
         VALUES ('schema_version', ?1), ('schedule_revision', 0)
         ON CONFLICT (name) DO NOTHING",
        vec![VERSION.into()],
    ));
    statements
}
