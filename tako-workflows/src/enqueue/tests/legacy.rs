//! The one-time import of an earlier release's per-server SQLite store.

use super::*;

/// The tables of the per-server store earlier releases created.
const LEGACY_SCHEMA: &str = "
    CREATE TABLE runs (id TEXT PRIMARY KEY, name TEXT NOT NULL, payload TEXT NOT NULL,
        status TEXT NOT NULL, attempts INTEGER NOT NULL DEFAULT 0, max_attempts INTEGER NOT NULL,
        run_at INTEGER NOT NULL, lease_until INTEGER, worker_id TEXT, last_error TEXT,
        created_at INTEGER NOT NULL, finished_at INTEGER, result TEXT);
    CREATE TABLE steps (run_id TEXT NOT NULL, name TEXT NOT NULL, result TEXT NOT NULL,
        completed_at INTEGER NOT NULL, PRIMARY KEY (run_id, name));
    CREATE TABLE schedules (name TEXT PRIMARY KEY, cron TEXT NOT NULL, last_run_at INTEGER,
        max_attempts INTEGER);
    CREATE TABLE event_waiters (run_id TEXT NOT NULL, step_name TEXT NOT NULL,
        event_name TEXT NOT NULL, expires_at INTEGER, PRIMARY KEY (run_id, step_name));
    CREATE TABLE run_inbox (run_id TEXT NOT NULL, event_name TEXT NOT NULL, payload TEXT NOT NULL,
        payload_bytes INTEGER NOT NULL, received_at INTEGER NOT NULL,
        PRIMARY KEY (run_id, event_name));
    INSERT INTO runs VALUES
        ('waiting', 'work', '{\"n\":1}', 'pending', 0, 3, 9000000000000, NULL, NULL, NULL, 10, NULL, NULL),
        ('retry', 'work', '{}', 'running', 1, 3, 10, 9999999999999, 'old-claim', NULL, 11, NULL, NULL),
        ('spent', 'work', '{}', 'running', 3, 3, 10, 9999999999999, 'old-claim', NULL, 12, NULL, NULL),
        ('done', 'work', '{}', 'succeeded', 1, 3, 10, NULL, NULL, NULL, 13, 20, '{\"ok\":true}'),
        ('taken', 'work', '{\"old\":true}', 'pending', 0, 3, 10, NULL, NULL, NULL, 14, NULL, NULL);
    INSERT INTO steps VALUES ('retry', 'fetch', '{\"value\":7}', 15);
    INSERT INTO schedules VALUES ('nightly', '0 0 0 * * *', 1234, NULL);
    INSERT INTO event_waiters VALUES ('waiting', 'approval', 'approved', NULL);
    INSERT INTO run_inbox VALUES ('waiting', 'early', '{}', 2, 16);
";

fn legacy_store(dir: &std::path::Path) -> std::path::PathBuf {
    let path = dir.join("workflows.sqlite");
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch("PRAGMA journal_mode = WAL;").unwrap();
    conn.execute_batch(LEGACY_SCHEMA).unwrap();
    path
}

#[test]
fn an_earlier_per_server_store_is_imported_once_and_retired() {
    let dir = tempfile::tempdir().unwrap();
    let path = legacy_store(dir.path());
    let store = Store::new();
    // Rows already in the hosted store win over the legacy copy.
    store
        .enqueue(
            "work",
            &json!({"new": true}),
            &EnqueueOpts {
                id: Some("taken".into()),
                ..Default::default()
            },
        )
        .unwrap();

    store.import_legacy_store(&path);
    assert!(!path.exists());
    assert!(dir.path().join("workflows.sqlite.imported").is_file());

    let status = |id: &str| {
        store.query_row(
            "SELECT status, worker_id, lease_until FROM {runs} WHERE app = 'app' AND id = ?1",
            &[id.into()],
        )
    };
    assert_eq!(
        status("waiting"),
        vec![json!("pending"), Value::Null, Value::Null]
    );
    // A lost worker's run is claimable again, or ended by the next reclaim
    // once it has no attempts left.
    assert_eq!(
        status("retry"),
        vec![json!("pending"), Value::Null, Value::Null]
    );
    assert_eq!(
        status("spent"),
        vec![json!("running"), Value::Null, json!(0)]
    );
    assert_eq!(status("done")[0], json!("succeeded"));
    let taken = store.get_run("taken").unwrap().unwrap();
    assert_eq!(taken.payload.value, json!({"new": true}));

    let done = store.get_run("done").unwrap().unwrap();
    assert_eq!(done.result.unwrap().value, json!({"ok": true}));
    assert_eq!(
        store.query_row(
            "SELECT result FROM {steps} WHERE app = 'app' AND run_id = 'retry' AND name = 'fetch'",
            &[],
        ),
        vec![json!("{\"value\":7}")]
    );
    assert_eq!(
        store.query_row(
            "SELECT event_name FROM {event_waiters} WHERE app = 'app' AND run_id = 'waiting'",
            &[],
        ),
        vec![json!("approved")]
    );
    assert_eq!(
        store.query_row(
            "SELECT payload_bytes FROM {run_inbox} WHERE app = 'app' AND run_id = 'waiting'",
            &[],
        ),
        vec![json!(2)]
    );
    // Schedules keep their last run, so the upgrade does not repeat it.
    assert_eq!(
        store.query_row(
            "SELECT last_run_at FROM {schedules} WHERE app = 'app' AND name = 'nightly'",
            &[],
        ),
        vec![json!(1234)]
    );
    // Used run IDs keep deduplicating enqueues.
    let again = store
        .enqueue(
            "work",
            &json!({}),
            &EnqueueOpts {
                id: Some("done".into()),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(again.id, "done");
    assert_eq!(status("done")[0], json!("succeeded"));
    // Other apps sharing the database see none of it.
    let other = store.other_app("other");
    assert!(other.get_run("waiting").unwrap().is_none());
}

#[test]
fn an_unreadable_legacy_store_is_left_for_the_next_start() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("workflows.sqlite");
    std::fs::write(&path, b"not a database").unwrap();
    let store = Store::new();
    store.import_legacy_store(&path);
    assert!(path.is_file());
}

#[test]
fn import_chunks_stay_under_their_size_limit() {
    let rows: Vec<Value> = (0..100)
        .map(|i| json!({"id": i, "pad": "x".repeat(100)}))
        .collect();
    let chunks = crate::enqueue::legacy::chunks(&rows, 1000);
    assert!(chunks.len() > 1);
    let total: usize = chunks
        .iter()
        .map(|chunk| serde_json::from_str::<Vec<Value>>(chunk).unwrap().len())
        .sum();
    assert_eq!(total, rows.len());
    assert!(chunks.iter().all(|chunk| chunk.len() <= 1000 + 200));
}
