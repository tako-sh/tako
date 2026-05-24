use super::*;

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

    let conn = db.conn.lock();
    let (run_at, max_attempts): (i64, i64) = conn
        .query_row(
            "SELECT run_at, max_attempts FROM runs WHERE id = ?1",
            params![r.id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(run_at, future);
    assert_eq!(max_attempts, 7);
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

    {
        let conn = db.conn.lock();
        conn.execute(
            "UPDATE runs SET status='succeeded' WHERE id = ?1",
            params![r1.id],
        )
        .unwrap();
    }

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

    let conn = db.conn.lock();
    let status: String = conn
        .query_row(
            "SELECT status FROM runs WHERE id = ?1",
            params![r.id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(status, "succeeded");
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM steps WHERE run_id = ?1",
            params![r.id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, 1);
}

#[test]
fn cancel_marks_cancelled_with_reason() {
    let db = RunsDb::open_in_memory().unwrap();
    let r = db.enqueue("w", &serde_json::json!({}), &opts()).unwrap();
    db.claim("w1", &["w".into()], 30_000).unwrap();
    db.cancel(&r.id, "w1", Some("user cancelled")).unwrap();

    let conn = db.conn.lock();
    let (status, last_error): (String, Option<String>) = conn
        .query_row(
            "SELECT status, last_error FROM runs WHERE id = ?1",
            params![r.id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(status, "cancelled");
    assert_eq!(last_error, Some("user cancelled".into()));
}

#[test]
fn defer_sets_run_at_and_decrements_attempts() {
    let db = RunsDb::open_in_memory().unwrap();
    let r = db.enqueue("w", &serde_json::json!({}), &opts()).unwrap();
    let claimed = db.claim("w1", &["w".into()], 30_000).unwrap().unwrap();
    assert_eq!(claimed.attempts, 1);

    let wake = now_ms() + 60_000;
    db.defer(&r.id, "w1", Some(wake)).unwrap();

    let conn = db.conn.lock();
    let (status, run_at, attempts): (String, i64, i64) = conn
        .query_row(
            "SELECT status, run_at, attempts FROM runs WHERE id = ?1",
            params![r.id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(status, "pending");
    assert_eq!(run_at, wake);
    // defer rolls attempts back so it doesn't consume retry budget
    assert_eq!(attempts, 0);
}

#[test]
fn defer_with_none_parks_indefinitely() {
    let db = RunsDb::open_in_memory().unwrap();
    let r = db.enqueue("w", &serde_json::json!({}), &opts()).unwrap();
    db.claim("w1", &["w".into()], 30_000).unwrap();
    db.defer(&r.id, "w1", None).unwrap();

    let conn = db.conn.lock();
    let run_at: i64 = conn
        .query_row(
            "SELECT run_at FROM runs WHERE id = ?1",
            params![r.id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(run_at, i64::MAX);
}

#[test]
fn reclaim_expired_moves_past_due_leases_back_to_pending() {
    let db = RunsDb::open_in_memory().unwrap();
    let r = db.enqueue("w", &serde_json::json!({}), &opts()).unwrap();
    // Claim then rewrite lease_until into the past to simulate a worker
    // that died mid-run and never completed / heartbeated.
    db.claim("w1", &["w".into()], 30_000).unwrap();
    {
        let conn = db.conn.lock();
        conn.execute(
            "UPDATE runs SET lease_until = ?1 WHERE id = ?2",
            params![now_ms() - 1_000, r.id],
        )
        .unwrap();
    }

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
fn reclaim_expired_ignores_terminal_runs() {
    // A succeeded / dead / cancelled row has lease_until=NULL, but we
    // still want to be explicit: only status='running' is reclaimed.
    let db = RunsDb::open_in_memory().unwrap();
    let r = db.enqueue("w", &serde_json::json!({}), &opts()).unwrap();
    db.claim("w1", &["w".into()], 30_000).unwrap();
    db.complete(&r.id, "w1").unwrap();

    assert_eq!(db.reclaim_expired().unwrap(), 0);
}
