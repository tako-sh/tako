use super::*;
use serde_json::{Value, json};

mod child_waits;
mod concurrency;
mod histories;
mod identity;
mod inbox;
mod operator;
mod reclaim;
mod schedules;

/// One app's store (`"app"`) in its own hosted database, served while this
/// lives. Dereferences to the app's handle.
pub(super) struct Store {
    pub(super) db: RunsDb,
    fixture: HostedFixture,
}

impl Store {
    pub(super) fn new() -> Self {
        let fixture = HostedFixture::start();
        Self {
            db: fixture.open("app"),
            fixture,
        }
    }

    /// Another app's view of the same database, or a second server's handle
    /// when `app_id` is this store's own app (`"app"`).
    pub(super) fn other_app(&self, app_id: &str) -> RunsDb {
        self.fixture.open(app_id)
    }

    /// Raw SQL against every app in the database; see
    /// [`RunsDb::test_execute`].
    pub(super) fn execute(&self, sql: &str, args: &[Value]) {
        self.db.test_execute(sql, args.to_vec());
    }

    /// One row of a raw query; see [`RunsDb::test_query_row`].
    pub(super) fn query_row(&self, sql: &str, args: &[Value]) -> Vec<Value> {
        self.db.test_query_row(sql, args.to_vec())
    }

    /// Expiry is injected into persisted state; no wall-clock sleeps.
    pub(super) fn expire_lease(&self, id: &str) {
        self.execute(
            "UPDATE {runs} SET lease_until = 0 WHERE id = ?1",
            &[id.into()],
        );
    }
}

impl std::ops::Deref for Store {
    type Target = RunsDb;

    fn deref(&self) -> &RunsDb {
        &self.db
    }
}

fn opts() -> EnqueueOpts {
    EnqueueOpts::default()
}

#[test]
fn enqueue_inserts_a_pending_row() {
    let db = Store::new();
    let result = db
        .enqueue("send-email", &json!({"to":"a@b.c"}), &opts())
        .unwrap();
    assert!(!result.deduplicated);
    assert!(!result.id.is_empty());
    assert_eq!(db.pending_count().unwrap(), 1);
}

#[test]
fn workflow_results_survive_restart_and_are_scoped_and_fenced() {
    let store = Store::new();
    let db = &store.db;
    let run = db.enqueue("report", &Value::Null, &opts()).unwrap();
    assert_eq!(
        db.result("report", &run.id).unwrap().unwrap().status,
        "pending"
    );
    assert!(db.result("other", &run.id).unwrap().is_none());
    db.claim("owner", &["report".into()], 30_000)
        .unwrap()
        .unwrap();
    let value = json!({"url":"report.pdf","count":42});
    assert!(matches!(
        db.complete(&run.id, "stale", &value),
        Err(RunsDbError::StaleWorker)
    ));
    assert_eq!(
        db.result("report", &run.id).unwrap().unwrap().status,
        "running"
    );
    db.complete(&run.id, "owner", &value).unwrap();
    assert!(matches!(
        db.complete(&run.id, "owner", &Value::Null),
        Err(RunsDbError::StaleWorker)
    ));
    // A restarted server opens the store again.
    let db = store.other_app("app");
    let result = db.result("report", &run.id).unwrap().unwrap();
    assert_eq!(result.status, "succeeded");
    assert_eq!(result.result, value);
    assert_eq!(result.error, None);
    store.execute(
        "UPDATE {runs} SET finished_at = 0 WHERE id = ?1",
        &[run.id.as_str().into()],
    );
    db.prune_finished_before(1, 100).unwrap();
    assert!(db.result("report", &run.id).unwrap().is_none());
}

fn count(store: &Store, table: &str, column: &str, id: &str) -> Value {
    store
        .query_row(
            &format!("SELECT COUNT(*) FROM {{{table}}} WHERE {column} = ?1"),
            &[id.into()],
        )
        .remove(0)
}

#[test]
fn pruning_removes_only_expired_finished_runs_and_their_steps() {
    let db = Store::new();
    let old = db.enqueue("work", &json!({}), &opts()).unwrap();
    assert_eq!(
        db.claim("worker", &["work".into()], 30_000)
            .unwrap()
            .unwrap()
            .id,
        old.id
    );
    db.save_step(&old.id, "worker", "step", &json!({"large": true}))
        .unwrap();
    db.complete(&old.id, "worker", &Value::Null).unwrap();
    let recent = db.enqueue("work", &json!({}), &opts()).unwrap();
    assert_eq!(
        db.claim("worker", &["work".into()], 30_000)
            .unwrap()
            .unwrap()
            .id,
        recent.id
    );
    db.save_step(&recent.id, "worker", "step", &json!({"large": true}))
        .unwrap();
    db.complete(&recent.id, "worker", &Value::Null).unwrap();
    let pending = db.enqueue("work", &json!({}), &opts()).unwrap();
    let now = now_ms();
    db.execute(
        "UPDATE {runs} SET finished_at = ?1 WHERE id = ?2",
        &[
            (now - 8 * 24 * 60 * 60 * 1000).into(),
            old.id.as_str().into(),
        ],
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
    assert_eq!(count(&db, "runs", "id", &old.id), 0);
    assert_eq!(count(&db, "steps", "run_id", &old.id), 0);
    for id in [&recent.id, &pending.id] {
        assert_eq!(count(&db, "runs", "id", id), 1);
    }
}

#[test]
fn pruning_never_removes_old_pending_or_running_runs() {
    let db = Store::new();
    let running = db.enqueue("work", &json!({}), &opts()).unwrap();
    let pending = db.enqueue("work", &json!({}), &opts()).unwrap();
    db.claim("worker", &["work".into()], 30_000).unwrap();
    db.execute("UPDATE {runs} SET created_at = 1, finished_at = 1", &[]);

    assert_eq!(db.prune_finished_before(now_ms(), 100).unwrap(), 0);
    assert_eq!(db.pending_count().unwrap(), 1);
    assert_eq!(count(&db, "runs", "id", &running.id), 1);
    assert_eq!(count(&db, "runs", "id", &pending.id), 1);
}

#[test]
fn cancelled_and_dead_runs_get_a_completion_time() {
    let db = Store::new();
    let cancelled = db.enqueue("work", &json!({}), &opts()).unwrap();
    db.claim("worker", &["work".into()], 30_000).unwrap();
    db.cancel(&cancelled.id, "worker", Some("no longer needed"))
        .unwrap();

    let dead = db.enqueue("work", &json!({}), &opts()).unwrap();
    db.claim("worker", &["work".into()], 30_000).unwrap();
    db.fail(&dead.id, "worker", "permanent error", None, true)
        .unwrap();

    assert_eq!(db.prune_finished_before(now_ms() + 1_000, 100).unwrap(), 2);
}

#[test]
fn enqueue_deduplicates_on_id() {
    let db = Store::new();
    let key = Some("cron:5m:0".into());
    let first = db
        .enqueue(
            "w",
            &json!({}),
            &EnqueueOpts {
                id: key.clone(),
                ..opts()
            },
        )
        .unwrap();
    let second = db
        .enqueue("w", &json!({}), &EnqueueOpts { id: key, ..opts() })
        .unwrap();

    assert_eq!(first.id, second.id);
    assert!(!first.deduplicated);
    assert!(second.deduplicated);
    assert_eq!(db.pending_count().unwrap(), 1);
}

#[test]
fn enqueue_different_ids_do_not_collide() {
    let db = Store::new();
    for id in ["k1", "k2"] {
        db.enqueue(
            "w",
            &json!({}),
            &EnqueueOpts {
                id: Some(id.into()),
                ..opts()
            },
        )
        .unwrap();
    }
    assert_eq!(db.pending_count().unwrap(), 2);
}

#[test]
fn enqueue_without_id_always_inserts() {
    let db = Store::new();
    db.enqueue("w", &json!({}), &opts()).unwrap();
    db.enqueue("w", &json!({}), &opts()).unwrap();
    assert_eq!(db.pending_count().unwrap(), 2);
}

#[test]
fn enqueue_honors_custom_max_attempts_and_run_at() {
    let db = Store::new();
    let future = now_ms() + 60_000;
    let r = db
        .enqueue(
            "w",
            &json!({}),
            &EnqueueOpts {
                run_at_ms: Some(future),
                max_attempts: Some(7),
                id: None,
            },
        )
        .unwrap();

    let row = db.query_row(
        "SELECT run_at, max_attempts FROM {runs} WHERE id = ?1",
        &[r.id.as_str().into()],
    );
    assert_eq!(row, vec![json!(future), json!(7)]);
}

#[test]
fn has_runnable_work_detects_due_pending_runs_only() {
    let db = Store::new();
    let due = now_ms() - 1;
    let future = now_ms() + 60_000;

    assert!(!db.has_runnable_work().unwrap());

    let future_run = db
        .enqueue(
            "w",
            &json!({}),
            &EnqueueOpts {
                run_at_ms: Some(future),
                ..opts()
            },
        )
        .unwrap();
    assert!(!db.has_runnable_work().unwrap());

    db.enqueue(
        "w",
        &json!({}),
        &EnqueueOpts {
            run_at_ms: Some(due),
            ..opts()
        },
    )
    .unwrap();
    assert!(db.has_runnable_work().unwrap());

    db.claim("w1", &["w".into()], 30_000).unwrap();
    assert!(!db.has_runnable_work().unwrap());

    db.execute(
        "UPDATE {runs} SET run_at = ?1 WHERE id = ?2",
        &[due.into(), future_run.id.as_str().into()],
    );
    assert!(db.has_runnable_work().unwrap());
}

#[test]
fn deduplication_reuses_a_completed_run() {
    let db = Store::new();
    let options = EnqueueOpts {
        id: Some("k".into()),
        ..opts()
    };
    let r1 = db.enqueue("w", &json!({}), &options).unwrap();

    db.execute(
        "UPDATE {runs} SET status = 'succeeded' WHERE id = ?1",
        &[r1.id.as_str().into()],
    );

    let r2 = db.enqueue("w", &json!({}), &options).unwrap();
    assert_eq!(r1.id, r2.id);
    assert!(r2.deduplicated);
}

#[test]
fn save_step_persists_to_steps_table_and_claim_hydrates_state() {
    let db = Store::new();
    let r = db.enqueue("w", &json!({}), &opts()).unwrap();
    let claimed = db.claim("w1", &["w".into()], 30_000).unwrap().unwrap();
    assert_eq!(claimed.id, r.id);
    assert_eq!(claimed.step_state, json!({}));

    db.save_step(&r.id, "w1", "fetch-user", &json!({"id":"u1"}))
        .unwrap();
    db.save_step(&r.id, "w1", "send", &json!(true)).unwrap();
    // Bounce the run back to pending so we can claim it again.
    db.fail(&r.id, "w1", "boom", Some(now_ms()), false).unwrap();

    let claimed2 = db.claim("w2", &["w".into()], 30_000).unwrap().unwrap();
    assert_eq!(claimed2.id, r.id);
    let state = claimed2.step_state.as_object().unwrap();
    assert_eq!(state.get("fetch-user"), Some(&json!({"id":"u1"})));
    assert_eq!(state.get("send"), Some(&json!(true)));
}

#[test]
fn save_step_is_idempotent_first_wins() {
    let db = Store::new();
    let r = db.enqueue("w", &json!({}), &opts()).unwrap();
    db.claim("w1", &["w".into()], 30_000).unwrap();
    db.save_step(&r.id, "w1", "fetch", &json!("first")).unwrap();
    // Same step name written again: the first value is kept.
    db.save_step(&r.id, "w1", "fetch", &json!("second"))
        .unwrap();

    db.fail(&r.id, "w1", "x", Some(now_ms()), false).unwrap();
    let claimed = db.claim("w2", &["w".into()], 30_000).unwrap().unwrap();
    assert_eq!(
        claimed.step_state.as_object().unwrap().get("fetch"),
        Some(&json!("first"))
    );
}

#[test]
fn complete_marks_succeeded_and_keeps_steps() {
    let db = Store::new();
    let r = db.enqueue("w", &json!({}), &opts()).unwrap();
    db.claim("w1", &["w".into()], 30_000).unwrap();
    db.save_step(&r.id, "w1", "s", &json!("v")).unwrap();
    db.complete(&r.id, "w1", &Value::Null).unwrap();

    let status = db.query_row(
        "SELECT status FROM {runs} WHERE id = ?1",
        &[r.id.as_str().into()],
    );
    assert_eq!(status, vec![json!("succeeded")]);
    assert_eq!(count(&db, "steps", "run_id", &r.id), 1);
}

#[test]
fn cancel_marks_cancelled_with_reason() {
    let db = Store::new();
    let r = db.enqueue("w", &json!({}), &opts()).unwrap();
    db.claim("w1", &["w".into()], 30_000).unwrap();
    db.cancel(&r.id, "w1", Some("user cancelled")).unwrap();

    let row = db.query_row(
        "SELECT status, last_error FROM {runs} WHERE id = ?1",
        &[r.id.as_str().into()],
    );
    assert_eq!(row, vec![json!("cancelled"), json!("user cancelled")]);
}

#[test]
fn defer_sets_run_at_and_decrements_attempts() {
    let db = Store::new();
    let r = db.enqueue("w", &json!({}), &opts()).unwrap();
    let claimed = db.claim("w1", &["w".into()], 30_000).unwrap().unwrap();
    assert_eq!(claimed.attempts, 1);

    let wake = now_ms() + 60_000;
    db.defer(&r.id, "w1", Some(wake)).unwrap();

    let row = db.query_row(
        "SELECT status, run_at, attempts FROM {runs} WHERE id = ?1",
        &[r.id.as_str().into()],
    );
    // defer rolls attempts back so it doesn't consume retry budget
    assert_eq!(row, vec![json!("pending"), json!(wake), json!(0)]);
}

#[test]
fn defer_with_none_parks_indefinitely() {
    let db = Store::new();
    let r = db.enqueue("w", &json!({}), &opts()).unwrap();
    db.claim("w1", &["w".into()], 30_000).unwrap();
    db.defer(&r.id, "w1", None).unwrap();

    let row = db.query_row(
        "SELECT run_at FROM {runs} WHERE id = ?1",
        &[r.id.as_str().into()],
    );
    assert_eq!(row, vec![json!(i64::MAX)]);
}

#[test]
fn reclaim_expired_moves_past_due_leases_back_to_pending() {
    let db = Store::new();
    let r = db.enqueue("w", &json!({}), &opts()).unwrap();
    // Claim then rewrite lease_until into the past to simulate a worker
    // that died mid-run and never completed / heartbeated.
    db.claim("w1", &["w".into()], 30_000).unwrap();
    db.expire_lease(&r.id);

    let reclaimed = db.reclaim_expired().unwrap();
    assert_eq!(reclaimed, 1);

    // The row is pending again, with no lease owner — and claimable.
    let next = db.claim("w2", &["w".into()], 30_000).unwrap().unwrap();
    assert_eq!(next.id, r.id);
}

#[test]
fn reclaim_expired_leaves_runs_with_valid_lease_alone() {
    let db = Store::new();
    db.enqueue("w", &json!({}), &opts()).unwrap();
    // Claim with a long lease; it must not be reclaimed.
    db.claim("w1", &["w".into()], 60_000).unwrap();

    assert_eq!(db.reclaim_expired().unwrap(), 0);
    // Still held by w1 — a fresh claim finds nothing.
    assert!(db.claim("w2", &["w".into()], 30_000).unwrap().is_none());
}

#[test]
fn wait_for_event_timeout_materializes_null_step() {
    let db = Store::new();
    db.enqueue("w", &json!({}), &opts()).unwrap();
    let run = db.claim("w1", &["w".into()], 30_000).unwrap().unwrap();
    let past = now_ms() - 1;
    db.wait_for_event(&run.id, "w1", "approval", "approval", Some(past))
        .unwrap();

    let claimed = db.claim("w1", &["w".into()], 30_000).unwrap().unwrap();
    assert_eq!(claimed.step_state["approval"], Value::Null);
    assert_eq!(db.signal("approval", &json!({"ok": true})).unwrap(), 0);
}

#[test]
fn wait_for_event_signal_writes_payload_before_timeout() {
    let db = Store::new();
    db.enqueue("w", &json!({}), &opts()).unwrap();
    let run = db.claim("w1", &["w".into()], 30_000).unwrap().unwrap();
    let future = now_ms() + 60_000;
    db.wait_for_event(&run.id, "w1", "approval", "approval", Some(future))
        .unwrap();
    assert_eq!(db.signal("approval", &json!({"ok": true})).unwrap(), 1);

    let claimed = db.claim("w1", &["w".into()], 30_000).unwrap().unwrap();
    assert_eq!(claimed.step_state["approval"], json!({"ok": true}));
}

#[test]
fn reclaim_expired_ignores_terminal_runs() {
    // A succeeded / dead / cancelled row has no lease, but be explicit:
    // only running runs are reclaimed.
    let db = Store::new();
    let r = db.enqueue("w", &json!({}), &opts()).unwrap();
    db.claim("w1", &["w".into()], 30_000).unwrap();
    db.complete(&r.id, "w1", &Value::Null).unwrap();

    assert_eq!(db.reclaim_expired().unwrap(), 0);
}
