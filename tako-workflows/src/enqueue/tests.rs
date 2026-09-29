use super::*;
use rusqlite::types::Value;

fn opts() -> EnqueueOpts {
    EnqueueOpts::default()
}

#[test]
fn enqueue_inserts_a_pending_row() {
    let db = RunsDb::open_in_memory().unwrap();
    let result = db
        .enqueue("send-email", &serde_json::json!({"to":"a@b.c"}), &opts())
        .unwrap();
    assert!(!result.deduplicated);
    assert!(!result.id.is_empty());
    assert_eq!(db.pending_count().unwrap(), 1);
}

#[test]
fn pruning_removes_only_expired_finished_runs_and_their_steps() {
    let db = RunsDb::open_in_memory().unwrap();
    let old = db.enqueue("work", &serde_json::json!({}), &opts()).unwrap();
    assert_eq!(
        db.claim("worker", &["work".into()], 30_000)
            .unwrap()
            .unwrap()
            .id,
        old.id
    );
    db.save_step(
        &old.id,
        "worker",
        "step",
        &serde_json::json!({"large": true}),
    )
    .unwrap();
    db.complete(&old.id, "worker").unwrap();
    let recent = db.enqueue("work", &serde_json::json!({}), &opts()).unwrap();
    assert_eq!(
        db.claim("worker", &["work".into()], 30_000)
            .unwrap()
            .unwrap()
            .id,
        recent.id
    );
    db.save_step(
        &recent.id,
        "worker",
        "step",
        &serde_json::json!({"large": true}),
    )
    .unwrap();
    db.complete(&recent.id, "worker").unwrap();
    let pending = db.enqueue("work", &serde_json::json!({}), &opts()).unwrap();
    let now = now_ms();
    db.raw_execute(
        "UPDATE runs SET finished_at = ?1 WHERE id = ?2",
        (now - 8 * 24 * 60 * 60 * 1000, old.id.as_str()),
    );

    assert_eq!(
        db.prune_finished_before(now - 7 * 24 * 60 * 60 * 1000, 1)
            .unwrap(),
        1
    );
    assert_eq!(
        db.prune_finished_before(now - 7 * 24 * 60 * 60 * 1000, 1)
            .unwrap(),
        0
    );
    assert_eq!(
        db.raw_query_values(
            "SELECT COUNT(*) FROM runs WHERE id = ?1",
            (old.id.as_str(),)
        ),
        vec![Value::Integer(0)]
    );
    assert_eq!(
        db.raw_query_values(
            "SELECT COUNT(*) FROM steps WHERE run_id = ?1",
            (old.id.as_str(),)
        ),
        vec![Value::Integer(0)]
    );
    for id in [&recent.id, &pending.id] {
        assert_eq!(
            db.raw_query_values("SELECT COUNT(*) FROM runs WHERE id = ?1", (id.as_str(),)),
            vec![Value::Integer(1)]
        );
    }
}

#[test]
fn pruning_never_removes_old_pending_or_running_runs() {
    let db = RunsDb::open_in_memory().unwrap();
    let running = db.enqueue("work", &serde_json::json!({}), &opts()).unwrap();
    let pending = db.enqueue("work", &serde_json::json!({}), &opts()).unwrap();
    db.claim("worker", &["work".into()], 30_000).unwrap();
    db.raw_execute("UPDATE runs SET created_at = 1, finished_at = 1", ());

    assert_eq!(db.prune_finished_before(now_ms(), 100).unwrap(), 0);
    assert_eq!(db.pending_count().unwrap(), 1);
    assert_eq!(
        db.raw_query_values(
            "SELECT COUNT(*) FROM runs WHERE id = ?1",
            (running.id.as_str(),)
        ),
        vec![Value::Integer(1)]
    );
    assert_eq!(
        db.raw_query_values(
            "SELECT COUNT(*) FROM runs WHERE id = ?1",
            (pending.id.as_str(),)
        ),
        vec![Value::Integer(1)]
    );
}

#[test]
fn cancelled_and_dead_runs_get_a_completion_time() {
    let db = RunsDb::open_in_memory().unwrap();
    let cancelled = db.enqueue("work", &serde_json::json!({}), &opts()).unwrap();
    db.claim("worker", &["work".into()], 30_000).unwrap();
    db.cancel(&cancelled.id, "worker", Some("no longer needed"))
        .unwrap();

    let dead = db.enqueue("work", &serde_json::json!({}), &opts()).unwrap();
    db.claim("worker", &["work".into()], 30_000).unwrap();
    db.fail(&dead.id, "worker", "permanent error", None, true)
        .unwrap();

    assert_eq!(db.prune_finished_before(now_ms() + 1_000, 100).unwrap(), 2);
}

#[test]
fn workflow_store_config_names_postgres_schema() {
    assert_eq!(POSTGRES_WORKFLOWS_SCHEMA, "tako_workflows");
    assert_eq!(
        WorkflowStoreConfig::postgres("postgres://example", "workflow-app/production").clone(),
        WorkflowStoreConfig::Postgres {
            url: "postgres://example".to_string(),
            schema: "tako_workflows".to_string(),
            app_id: "workflow-app/production".to_string(),
        },
    );
}

#[test]
fn postgres_workflow_store_round_trips_when_url_is_set() {
    let Ok(url) = std::env::var("TAKO_TEST_POSTGRES_URL") else {
        return;
    };
    let app_id = format!(
        "workflow-test/{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let db = RunsDb::open_postgres(&url, &app_id).unwrap();
    let result = db
        .enqueue(
            "send-email",
            &serde_json::json!({"to":"a@b.c"}),
            &EnqueueOpts {
                unique_key: Some("email-1".to_string()),
                ..Default::default()
            },
        )
        .unwrap();
    let duplicate = db
        .enqueue(
            "send-email",
            &serde_json::json!({"to":"a@b.c"}),
            &EnqueueOpts {
                unique_key: Some("email-1".to_string()),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(duplicate.id, result.id);
    assert!(duplicate.deduplicated);

    let run = db
        .claim("worker-1", &["send-email".to_string()], 30_000)
        .unwrap()
        .unwrap();
    assert_eq!(run.id, result.id);
    db.save_step(
        &run.id,
        "worker-1",
        "step-1",
        &serde_json::json!({"ok": true}),
    )
    .unwrap();
    db.complete(&run.id, "worker-1").unwrap();
    assert_eq!(db.pending_count().unwrap(), 0);
    let other_app_id = format!("{app_id}-other");
    let other = RunsDb::open_postgres(&url, &other_app_id).unwrap();
    let other_run = other
        .enqueue("send-email", &serde_json::json!({}), &opts())
        .unwrap();
    other
        .claim("worker-2", &["send-email".into()], 30_000)
        .unwrap();
    other.complete(&other_run.id, "worker-2").unwrap();

    assert_eq!(db.prune_finished_before(now_ms() + 1_000, 100).unwrap(), 1);
    let mut client = postgres::Client::connect(&url, postgres::NoTls).unwrap();
    let remaining: i64 = client
        .query_one(
            "SELECT COUNT(*) FROM tako_workflows.steps WHERE app_id=$1 AND run_id=$2",
            &[&app_id, &result.id],
        )
        .unwrap()
        .get(0);
    assert_eq!(remaining, 0);
    let other_remaining: i64 = client
        .query_one(
            "SELECT COUNT(*) FROM tako_workflows.runs WHERE app_id=$1 AND id=$2",
            &[&other_app_id, &other_run.id],
        )
        .unwrap()
        .get(0);
    assert_eq!(other_remaining, 1);
    assert_eq!(
        other.prune_finished_before(now_ms() + 1_000, 100).unwrap(),
        1
    );
}

#[test]
fn opening_old_sqlite_store_gives_finished_runs_a_grace_period() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("workflows.sqlite");
    {
        let conn = tako_sqlite::open_local(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE runs (
                id TEXT PRIMARY KEY, name TEXT NOT NULL, payload TEXT NOT NULL,
                status TEXT NOT NULL, attempts INTEGER NOT NULL, max_attempts INTEGER NOT NULL,
                run_at INTEGER NOT NULL, lease_until INTEGER, worker_id TEXT, last_error TEXT,
                created_at INTEGER NOT NULL, unique_key TEXT
             );
             INSERT INTO runs (id, name, payload, status, attempts, max_attempts, run_at, created_at)
             VALUES ('old', 'work', '{}', 'succeeded', 1, 3, 1, 1);",
        )
        .unwrap();
    }
    let db = RunsDb::open(&path).unwrap();
    assert_eq!(
        db.prune_finished_before(now_ms() - DEFAULT_RETENTION_MS, 100)
            .unwrap(),
        0
    );
    assert_eq!(
        db.raw_query_values("SELECT COUNT(*) FROM runs WHERE id='old'", ()),
        vec![Value::Integer(1)]
    );
}

#[test]
fn enqueue_deduplicates_on_unique_key() {
    let db = RunsDb::open_in_memory().unwrap();
    let key = Some("cron:5m:0".into());
    let first = db
        .enqueue(
            "w",
            &serde_json::json!({}),
            &EnqueueOpts {
                unique_key: key.clone(),
                ..opts()
            },
        )
        .unwrap();
    let second = db
        .enqueue(
            "w",
            &serde_json::json!({}),
            &EnqueueOpts {
                unique_key: key,
                ..opts()
            },
        )
        .unwrap();

    assert_eq!(first.id, second.id);
    assert!(!first.deduplicated);
    assert!(second.deduplicated);
    assert_eq!(db.pending_count().unwrap(), 1);
}

#[test]
fn enqueue_different_unique_keys_do_not_collide() {
    let db = RunsDb::open_in_memory().unwrap();
    db.enqueue(
        "w",
        &serde_json::json!({}),
        &EnqueueOpts {
            unique_key: Some("k1".into()),
            ..opts()
        },
    )
    .unwrap();
    db.enqueue(
        "w",
        &serde_json::json!({}),
        &EnqueueOpts {
            unique_key: Some("k2".into()),
            ..opts()
        },
    )
    .unwrap();
    assert_eq!(db.pending_count().unwrap(), 2);
}

#[test]
fn enqueue_without_unique_key_always_inserts() {
    let db = RunsDb::open_in_memory().unwrap();
    db.enqueue("w", &serde_json::json!({}), &opts()).unwrap();
    db.enqueue("w", &serde_json::json!({}), &opts()).unwrap();
    assert_eq!(db.pending_count().unwrap(), 2);
}

#[test]
fn enqueue_honors_custom_max_attempts_and_run_at() {
    let db = RunsDb::open_in_memory().unwrap();
    let future = now_ms() + 60_000;
    let r = db
        .enqueue(
            "w",
            &serde_json::json!({}),
            &EnqueueOpts {
                run_at_ms: Some(future),
                max_attempts: Some(7),
                unique_key: None,
            },
        )
        .unwrap();

    let row = db.raw_query_values(
        "SELECT run_at, max_attempts FROM runs WHERE id = ?1",
        (r.id.as_str(),),
    );
    assert_eq!(row, vec![Value::Integer(future), Value::Integer(7)]);
}

#[test]
fn has_runnable_work_detects_due_pending_runs_only() {
    let db = RunsDb::open_in_memory().unwrap();
    let due = now_ms() - 1;
    let future = now_ms() + 60_000;

    assert!(!db.has_runnable_work().unwrap());

    let future_run = db
        .enqueue(
            "w",
            &serde_json::json!({}),
            &EnqueueOpts {
                run_at_ms: Some(future),
                ..opts()
            },
        )
        .unwrap();
    assert!(!db.has_runnable_work().unwrap());

    db.enqueue(
        "w",
        &serde_json::json!({}),
        &EnqueueOpts {
            run_at_ms: Some(due),
            ..opts()
        },
    )
    .unwrap();
    assert!(db.has_runnable_work().unwrap());

    db.claim("w1", &["w".into()], 30_000).unwrap();
    assert!(!db.has_runnable_work().unwrap());

    db.raw_execute(
        "UPDATE runs SET run_at = ?1 WHERE id = ?2",
        (due, future_run.id.as_str()),
    );
    assert!(db.has_runnable_work().unwrap());
}

#[test]
fn open_creates_parent_directory() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp
        .path()
        .join("nested")
        .join("dir")
        .join("workflows.sqlite");
    let db = RunsDb::open(&path).unwrap();
    db.enqueue("w", &serde_json::json!({}), &opts()).unwrap();
    assert!(path.exists());
}

#[test]
fn deduplication_frees_slot_once_original_is_terminal() {
    let db = RunsDb::open_in_memory().unwrap();
    let r1 = db
        .enqueue(
            "w",
            &serde_json::json!({}),
            &EnqueueOpts {
                unique_key: Some("k".into()),
                ..opts()
            },
        )
        .unwrap();

    db.raw_execute(
        "UPDATE runs SET status='succeeded' WHERE id = ?1",
        (r1.id.as_str(),),
    );

    let r2 = db
        .enqueue(
            "w",
            &serde_json::json!({}),
            &EnqueueOpts {
                unique_key: Some("k".into()),
                ..opts()
            },
        )
        .unwrap();
    assert_ne!(r1.id, r2.id);
    assert!(!r2.deduplicated);
}

#[test]
fn save_step_persists_to_steps_table_and_claim_hydrates_state() {
    let db = RunsDb::open_in_memory().unwrap();
    let r = db.enqueue("w", &serde_json::json!({}), &opts()).unwrap();
    let claimed = db.claim("w1", &["w".into()], 30_000).unwrap().unwrap();
    assert_eq!(claimed.id, r.id);
    assert_eq!(claimed.step_state, serde_json::json!({}));

    db.save_step(&r.id, "w1", "fetch-user", &serde_json::json!({"id":"u1"}))
        .unwrap();
    db.save_step(&r.id, "w1", "send", &serde_json::json!(true))
        .unwrap();
    // Bounce the run back to pending so we can claim it again.
    db.fail(&r.id, "w1", "boom", Some(now_ms()), false).unwrap();

    let claimed2 = db.claim("w2", &["w".into()], 30_000).unwrap().unwrap();
    assert_eq!(claimed2.id, r.id);
    let state = claimed2.step_state.as_object().unwrap();
    assert_eq!(
        state.get("fetch-user"),
        Some(&serde_json::json!({"id":"u1"}))
    );
    assert_eq!(state.get("send"), Some(&serde_json::json!(true)));
}

#[test]
fn save_step_is_idempotent_first_wins() {
    let db = RunsDb::open_in_memory().unwrap();
    let r = db.enqueue("w", &serde_json::json!({}), &opts()).unwrap();
    db.claim("w1", &["w".into()], 30_000).unwrap();
    db.save_step(&r.id, "w1", "fetch", &serde_json::json!("first"))
        .unwrap();
    // Same step name written again — INSERT OR IGNORE keeps the first.
    db.save_step(&r.id, "w1", "fetch", &serde_json::json!("second"))
        .unwrap();

    db.fail(&r.id, "w1", "x", Some(now_ms()), false).unwrap();
    let claimed = db.claim("w2", &["w".into()], 30_000).unwrap().unwrap();
    assert_eq!(
        claimed.step_state.as_object().unwrap().get("fetch"),
        Some(&serde_json::json!("first"))
    );
}

#[test]
fn complete_marks_succeeded_and_keeps_steps() {
    let db = RunsDb::open_in_memory().unwrap();
    let r = db.enqueue("w", &serde_json::json!({}), &opts()).unwrap();
    db.claim("w1", &["w".into()], 30_000).unwrap();
    db.save_step(&r.id, "w1", "s", &serde_json::json!("v"))
        .unwrap();
    db.complete(&r.id, "w1").unwrap();

    let status = db.raw_query_values("SELECT status FROM runs WHERE id = ?1", (r.id.as_str(),));
    assert_eq!(status, vec![Value::Text("succeeded".into())]);
    let count = db.raw_query_values(
        "SELECT COUNT(*) FROM steps WHERE run_id = ?1",
        (r.id.as_str(),),
    );
    assert_eq!(count, vec![Value::Integer(1)]);
}

#[test]
fn cancel_marks_cancelled_with_reason() {
    let db = RunsDb::open_in_memory().unwrap();
    let r = db.enqueue("w", &serde_json::json!({}), &opts()).unwrap();
    db.claim("w1", &["w".into()], 30_000).unwrap();
    db.cancel(&r.id, "w1", Some("user cancelled")).unwrap();

    let row = db.raw_query_values(
        "SELECT status, last_error FROM runs WHERE id = ?1",
        (r.id.as_str(),),
    );
    assert_eq!(
        row,
        vec![
            Value::Text("cancelled".into()),
            Value::Text("user cancelled".into())
        ]
    );
}

#[test]
fn defer_sets_run_at_and_decrements_attempts() {
    let db = RunsDb::open_in_memory().unwrap();
    let r = db.enqueue("w", &serde_json::json!({}), &opts()).unwrap();
    let claimed = db.claim("w1", &["w".into()], 30_000).unwrap().unwrap();
    assert_eq!(claimed.attempts, 1);

    let wake = now_ms() + 60_000;
    db.defer(&r.id, "w1", Some(wake)).unwrap();

    let row = db.raw_query_values(
        "SELECT status, run_at, attempts FROM runs WHERE id = ?1",
        (r.id.as_str(),),
    );
    // defer rolls attempts back so it doesn't consume retry budget
    assert_eq!(
        row,
        vec![
            Value::Text("pending".into()),
            Value::Integer(wake),
            Value::Integer(0)
        ]
    );
}

#[test]
fn defer_with_none_parks_indefinitely() {
    let db = RunsDb::open_in_memory().unwrap();
    let r = db.enqueue("w", &serde_json::json!({}), &opts()).unwrap();
    db.claim("w1", &["w".into()], 30_000).unwrap();
    db.defer(&r.id, "w1", None).unwrap();

    let row = db.raw_query_values("SELECT run_at FROM runs WHERE id = ?1", (r.id.as_str(),));
    assert_eq!(row, vec![Value::Integer(i64::MAX)]);
}

#[test]
fn reclaim_expired_moves_past_due_leases_back_to_pending() {
    let db = RunsDb::open_in_memory().unwrap();
    let r = db.enqueue("w", &serde_json::json!({}), &opts()).unwrap();
    // Claim then rewrite lease_until into the past to simulate a worker
    // that died mid-run and never completed / heartbeated.
    db.claim("w1", &["w".into()], 30_000).unwrap();
    db.raw_execute(
        "UPDATE runs SET lease_until = ?1 WHERE id = ?2",
        (now_ms() - 1_000, r.id.as_str()),
    );

    let reclaimed = db.reclaim_expired().unwrap();
    assert_eq!(reclaimed, 1);

    // The row is pending again, with no lease owner — and claimable.
    let next = db.claim("w2", &["w".into()], 30_000).unwrap().unwrap();
    assert_eq!(next.id, r.id);
}

#[test]
fn reclaim_expired_leaves_runs_with_valid_lease_alone() {
    let db = RunsDb::open_in_memory().unwrap();
    db.enqueue("w", &serde_json::json!({}), &opts()).unwrap();
    // Claim with a long lease; it must not be reclaimed.
    db.claim("w1", &["w".into()], 60_000).unwrap();

    assert_eq!(db.reclaim_expired().unwrap(), 0);
    // Still held by w1 — a fresh claim finds nothing.
    assert!(db.claim("w2", &["w".into()], 30_000).unwrap().is_none());
}

#[test]
fn wait_for_event_timeout_materializes_null_step() {
    let db = RunsDb::open_in_memory().unwrap();
    db.enqueue("w", &serde_json::json!({}), &opts()).unwrap();
    let run = db.claim("w1", &["w".into()], 30_000).unwrap().unwrap();
    let past = now_ms() - 1;
    db.wait_for_event(&run.id, "w1", "approval", "approval", Some(past))
        .unwrap();

    let claimed = db.claim("w1", &["w".into()], 30_000).unwrap().unwrap();
    assert_eq!(claimed.step_state["approval"], serde_json::Value::Null);
    assert_eq!(
        db.signal("approval", &serde_json::json!({"ok": true}))
            .unwrap(),
        0
    );
}

#[test]
fn wait_for_event_signal_writes_payload_before_timeout() {
    let db = RunsDb::open_in_memory().unwrap();
    db.enqueue("w", &serde_json::json!({}), &opts()).unwrap();
    let run = db.claim("w1", &["w".into()], 30_000).unwrap().unwrap();
    let future = now_ms() + 60_000;
    db.wait_for_event(&run.id, "w1", "approval", "approval", Some(future))
        .unwrap();
    assert_eq!(
        db.signal("approval", &serde_json::json!({"ok": true}))
            .unwrap(),
        1
    );

    let claimed = db.claim("w1", &["w".into()], 30_000).unwrap().unwrap();
    assert_eq!(
        claimed.step_state["approval"],
        serde_json::json!({"ok": true})
    );
}

#[test]
fn reclaim_expired_ignores_terminal_runs() {
    // A succeeded / dead / cancelled row has lease_until=NULL, but we
    // still want to be explicit: only status='running' is reclaimed.
    let db = RunsDb::open_in_memory().unwrap();
    let r = db.enqueue("w", &serde_json::json!({}), &opts()).unwrap();
    db.claim("w1", &["w".into()], 30_000).unwrap();
    db.complete(&r.id, "w1").unwrap();

    assert_eq!(db.reclaim_expired().unwrap(), 0);
}
