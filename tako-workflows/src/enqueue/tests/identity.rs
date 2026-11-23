use super::*;

fn identified(id: &str) -> EnqueueOpts {
    EnqueueOpts {
        id: Some(id.into()),
        ..opts()
    }
}

#[test]
fn supplied_id_is_the_run_id() {
    let store = Store::new();
    let db = &store.db;
    let run = db
        .enqueue(
            "pay",
            &serde_json::json!({"amount": 100}),
            &identified("invoice:123"),
        )
        .unwrap();
    assert_eq!(run.id, "invoice:123");
    assert_eq!(
        db.claim("worker", &["pay".into()], 30_000)
            .unwrap()
            .unwrap()
            .id,
        run.id
    );
}

#[test]
fn retained_ids_deduplicate_in_every_status() {
    let store = Store::new();
    let payload = serde_json::json!({"amount": 100});
    for status in ["pending", "running", "succeeded", "cancelled", "dead"] {
        let options = identified(&format!("invoice:{status}"));
        let first = store.db.enqueue("pay", &payload, &options).unwrap();
        store.execute(
            "UPDATE {runs} SET status = ?1 WHERE id = ?2",
            &[status.into(), first.id.as_str().into()],
        );
        let duplicate = store.db.enqueue("pay", &payload, &options).unwrap();
        assert!(duplicate.deduplicated, "status: {status}");
        assert_eq!(duplicate.id, first.id);
        assert_eq!(
            store.db.result("pay", &first.id).unwrap().unwrap().status,
            status
        );
    }
}

#[test]
fn supplied_id_rejects_a_different_payload_or_workflow() {
    let store = Store::new();
    let db = &store.db;
    let options = identified("invoice:123");
    let payload = serde_json::json!({"amount": 100});
    db.enqueue("pay", &payload, &options).unwrap();
    for (name, payload) in [
        ("pay", serde_json::json!({"amount": 200})),
        ("refund", payload),
    ] {
        let error = db.enqueue(name, &payload, &options).unwrap_err();
        assert!(matches!(error, RunsDbError::IdConflict { .. }), "{error}");
        assert!(
            error.to_string().contains("different workflow or payload"),
            "{error}"
        );
    }
}

#[test]
fn payload_object_order_does_not_change_identity() {
    let store = Store::new();
    let db = &store.db;
    let options = identified("invoice:123");
    let first = serde_json::from_str(r#"{"amount":100,"currency":"JPY"}"#).unwrap();
    let reordered = serde_json::from_str(r#"{"currency":"JPY","amount":100}"#).unwrap();
    db.enqueue("pay", &first, &options).unwrap();
    assert!(
        db.enqueue("pay", &reordered, &options)
            .unwrap()
            .deduplicated
    );
}

#[test]
fn pruning_a_finished_run_releases_its_id_and_steps() {
    let store = Store::new();
    let db = &store.db;
    let options = identified("invoice:123");
    let first = db
        .enqueue("pay", &serde_json::json!({"amount": 100}), &options)
        .unwrap();
    db.claim("worker", &["pay".into()], 30_000)
        .unwrap()
        .unwrap();
    db.save_step(&first.id, "worker", "charge", &serde_json::json!("receipt"))
        .unwrap();
    db.complete(&first.id, "worker", &serde_json::Value::Null)
        .unwrap();
    assert_eq!(db.prune_finished_before(now_ms() + 1_000, 100).unwrap(), 1);
    let second = db
        .enqueue("pay", &serde_json::json!({"amount": 200}), &options)
        .unwrap();
    assert_eq!(second.id, "invoice:123");
    assert!(!second.deduplicated);
    let run = db
        .claim("worker", &["pay".into()], 30_000)
        .unwrap()
        .unwrap();
    assert_eq!(run.step_state, serde_json::json!({}));
}

#[test]
fn default_retention_keeps_six_month_old_runs() {
    let store = Store::new();
    let run = store
        .db
        .enqueue("pay", &serde_json::json!({}), &opts())
        .unwrap();
    store
        .db
        .claim("worker", &["pay".into()], 30_000)
        .unwrap()
        .unwrap();
    store
        .db
        .complete(&run.id, "worker", &serde_json::Value::Null)
        .unwrap();
    let now = now_ms();
    store.execute(
        "UPDATE {runs} SET finished_at = ?1 WHERE id = ?2",
        &[
            (now - 183 * 24 * 60 * 60 * 1000).into(),
            run.id.as_str().into(),
        ],
    );
    let db = &store.db;
    assert_eq!(
        db.prune_finished_before(now - DEFAULT_RETENTION_MS, 100)
            .unwrap(),
        0
    );
    assert_eq!(
        db.prune_finished_before(now - DEFAULT_RETENTION_MS + 2 * 24 * 60 * 60 * 1000, 100)
            .unwrap(),
        1
    );
}

/// Eight servers enqueue the same ID at once: exactly one creates the run.
#[test]
fn simultaneous_enqueues_on_several_servers_create_one_run() {
    let fixture = HostedFixture::start();
    let stores: Vec<_> = (0..8).map(|_| fixture.open("app")).collect();
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(stores.len()));
    let threads: Vec<_> = stores
        .into_iter()
        .map(|db| {
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                db.enqueue("pay", &serde_json::json!({}), &identified("invoice:123"))
                    .unwrap()
            })
        })
        .collect();
    let runs: Vec<_> = threads
        .into_iter()
        .map(|thread| thread.join().unwrap())
        .collect();
    assert_eq!(runs.iter().filter(|run| !run.deduplicated).count(), 1);
    assert!(runs.iter().all(|run| run.id == "invoice:123"));
}

#[test]
fn supplied_ids_must_fit_the_storage_contract() {
    let store = Store::new();
    let db = &store.db;
    for id in [String::new(), "x".repeat(256), "bad\0id".into()] {
        assert!(matches!(
            db.enqueue("pay", &serde_json::json!({}), &identified(&id)),
            Err(RunsDbError::InvalidId)
        ));
    }
    let longest = "x".repeat(255);
    assert_eq!(
        db.enqueue("pay", &serde_json::json!({}), &identified(&longest))
            .unwrap()
            .id,
        longest
    );
}

/// Apps sharing a database each keep their own run IDs: the same ID is
/// retained, deduplicated, conflicting and released per app.
#[test]
fn apps_sharing_a_database_keep_their_own_ids() {
    let store = Store::new();
    let other = store.other_app("other");
    let options = identified("invoice:123");
    let payload = serde_json::json!({"amount": 100});
    assert_eq!(
        store.enqueue("pay", &payload, &options).unwrap().id,
        "invoice:123"
    );
    assert!(
        !other
            .enqueue("refund", &serde_json::json!({}), &options)
            .unwrap()
            .deduplicated
    );
    assert!(matches!(
        store.enqueue("refund", &serde_json::json!({}), &options),
        Err(RunsDbError::IdConflict { .. })
    ));
    let run = store
        .claim("worker", &["pay".into()], 30_000)
        .unwrap()
        .unwrap();
    store
        .save_step(&run.id, "worker", "charge", &serde_json::json!("receipt"))
        .unwrap();
    store
        .complete(&run.id, "worker", &serde_json::Value::Null)
        .unwrap();
    assert_eq!(
        store.prune_finished_before(now_ms() + 1_000, 100).unwrap(),
        1
    );
    // The pruned ID is free in its app only.
    assert!(
        !store
            .enqueue("pay", &serde_json::json!({"amount": 200}), &options)
            .unwrap()
            .deduplicated
    );
    assert_eq!(
        store
            .claim("worker", &["pay".into()], 30_000)
            .unwrap()
            .unwrap()
            .step_state,
        serde_json::json!({})
    );
    assert!(
        other
            .enqueue("refund", &serde_json::json!({}), &options)
            .unwrap()
            .deduplicated
    );
}

/// Servers opening a fresh database at once each create the tables, and
/// exactly one run.
#[test]
fn servers_opening_a_new_store_at_once_create_it_once() {
    let fixture = HostedFixture::start();
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
    let runs: Vec<_> = std::thread::scope(|scope| {
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let barrier = barrier.clone();
                let fixture = &fixture;
                scope.spawn(move || {
                    barrier.wait();
                    fixture
                        .open("app")
                        .enqueue("pay", &serde_json::json!({}), &identified("invoice:123"))
                        .unwrap()
                })
            })
            .collect();
        threads
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .collect()
    });
    assert_eq!(runs.iter().filter(|run| !run.deduplicated).count(), 1);
}
