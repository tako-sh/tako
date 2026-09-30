use super::*;

fn identified(id: &str) -> EnqueueOpts {
    EnqueueOpts {
        id: Some(id.into()),
        ..opts()
    }
}

#[test]
fn supplied_id_is_the_run_id() {
    let db = RunsDb::open_in_memory().unwrap();
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
    for status in ["pending", "running", "succeeded", "cancelled", "dead"] {
        let db = RunsDb::open_in_memory().unwrap();
        let options = identified("invoice:123");
        let payload = serde_json::json!({"amount": 100});
        let first = db.enqueue("pay", &payload, &options).unwrap();
        db.raw_execute(
            "UPDATE runs SET status = ?1 WHERE id = ?2",
            (status, first.id.as_str()),
        );
        let duplicate = db.enqueue("pay", &payload, &options).unwrap();
        assert!(duplicate.deduplicated, "status: {status}");
        assert_eq!(duplicate.id, first.id);
        assert_eq!(
            db.raw_query_values(
                "SELECT status FROM runs WHERE id = ?1",
                (first.id.as_str(),)
            ),
            vec![Value::Text(status.into())]
        );
    }
}

#[test]
fn supplied_id_rejects_a_different_payload_or_workflow() {
    let db = RunsDb::open_in_memory().unwrap();
    let options = identified("invoice:123");
    let payload = serde_json::json!({"amount": 100});
    db.enqueue("pay", &payload, &options).unwrap();
    for (name, payload) in [
        ("pay", serde_json::json!({"amount": 200})),
        ("refund", payload),
    ] {
        let error = db.enqueue(name, &payload, &options).unwrap_err();
        assert!(
            error.to_string().contains("different workflow or payload"),
            "{error}"
        );
    }
}

#[test]
fn payload_object_order_does_not_change_identity() {
    let db = RunsDb::open_in_memory().unwrap();
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
    let db = RunsDb::open_in_memory().unwrap();
    let options = identified("invoice:123");
    let first = db
        .enqueue("pay", &serde_json::json!({"amount": 100}), &options)
        .unwrap();
    db.claim("worker", &["pay".into()], 30_000)
        .unwrap()
        .unwrap();
    db.save_step(&first.id, "worker", "charge", &serde_json::json!("receipt"))
        .unwrap();
    db.complete(&first.id, "worker").unwrap();
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
    let db = RunsDb::open_in_memory().unwrap();
    let run = db.enqueue("pay", &serde_json::json!({}), &opts()).unwrap();
    db.claim("worker", &["pay".into()], 30_000)
        .unwrap()
        .unwrap();
    db.complete(&run.id, "worker").unwrap();
    let now = now_ms();
    db.raw_execute(
        "UPDATE runs SET finished_at = ?1 WHERE id = ?2",
        (now - 183 * 24 * 60 * 60 * 1000, run.id.as_str()),
    );
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

#[test]
fn simultaneous_enqueues_create_one_run() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("workflows.sqlite");
    let stores: Vec<_> = (0..8).map(|_| RunsDb::open(&path).unwrap()).collect();
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
    let db = RunsDb::open_in_memory().unwrap();
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

#[test]
fn postgres_ids_are_retained_scoped_and_reusable_after_pruning() {
    let Ok(url) = std::env::var("TAKO_TEST_POSTGRES_URL") else {
        return;
    };
    let app_id = format!("identity-test/{}", nanoid::nanoid!());
    let db = RunsDb::open_postgres(&url, &app_id).unwrap();
    let other = RunsDb::open_postgres(&url, &format!("{app_id}-other")).unwrap();
    let options = identified("invoice:123");
    let payload = serde_json::json!({"amount":100});
    assert_eq!(
        db.enqueue("pay", &payload, &options).unwrap().id,
        "invoice:123"
    );
    assert_eq!(
        other
            .enqueue("refund", &serde_json::json!({}), &options)
            .unwrap()
            .id,
        "invoice:123"
    );
    let mut client = postgres::Client::connect(&url, postgres::NoTls).unwrap();
    for status in ["pending", "running", "succeeded", "cancelled", "dead"] {
        client
            .execute(
                "UPDATE tako_workflows.runs SET status=$1 WHERE app_id=$2 AND id=$3",
                &[&status, &app_id, &"invoice:123"],
            )
            .unwrap();
        assert!(
            db.enqueue("pay", &payload, &options).unwrap().deduplicated,
            "{status}"
        );
    }
    assert!(matches!(
        db.enqueue("pay", &serde_json::json!({"amount":200}), &options),
        Err(RunsDbError::IdConflict { .. })
    ));
    assert!(matches!(
        db.enqueue("refund", &payload, &options),
        Err(RunsDbError::IdConflict { .. })
    ));
    client
        .execute(
            "UPDATE tako_workflows.runs SET status='pending' WHERE app_id=$1",
            &[&app_id],
        )
        .unwrap();
    db.claim("worker", &["pay".into()], 30_000)
        .unwrap()
        .unwrap();
    db.save_step(
        "invoice:123",
        "worker",
        "charge",
        &serde_json::json!("receipt"),
    )
    .unwrap();
    db.complete("invoice:123", "worker").unwrap();
    assert_eq!(db.prune_finished_before(now_ms() + 1_000, 100).unwrap(), 1);
    assert!(
        !db.enqueue("pay", &serde_json::json!({"amount":200}), &options)
            .unwrap()
            .deduplicated
    );
    assert_eq!(
        db.claim("worker", &["pay".into()], 30_000)
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
    client
        .execute(
            "DELETE FROM tako_workflows.runs WHERE app_id=$1 OR app_id=$2",
            &[&app_id, &format!("{app_id}-other")],
        )
        .unwrap();
}

#[test]
fn simultaneous_postgres_enqueues_create_one_run() {
    let Ok(url) = std::env::var("TAKO_TEST_POSTGRES_URL") else {
        return;
    };
    let app_id = format!("concurrent-identity-test/{}", nanoid::nanoid!());
    let stores: Vec<_> = (0..8)
        .map(|_| RunsDb::open_postgres(&url, &app_id).unwrap())
        .collect();
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
    let mut client = postgres::Client::connect(&url, postgres::NoTls).unwrap();
    client
        .execute(
            "DELETE FROM tako_workflows.runs WHERE app_id=$1",
            &[&app_id],
        )
        .unwrap();
}

#[test]
fn postgres_existing_stores_allow_the_same_id_in_different_apps() {
    let Ok(url) = std::env::var("TAKO_TEST_POSTGRES_URL") else {
        return;
    };
    let schema = format!(
        "identity_migration_{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let mut client = postgres::Client::connect(&url, postgres::NoTls).unwrap();
    client.batch_execute(&format!(
        "CREATE SCHEMA {schema};
         CREATE TABLE {schema}.runs (
             app_id TEXT NOT NULL, id TEXT PRIMARY KEY, name TEXT NOT NULL,
             payload TEXT NOT NULL, status TEXT NOT NULL, attempts BIGINT NOT NULL,
             max_attempts BIGINT NOT NULL, run_at BIGINT NOT NULL, lease_until BIGINT,
             worker_id TEXT, last_error TEXT, created_at BIGINT NOT NULL, finished_at BIGINT
         );
         INSERT INTO {schema}.runs (app_id,id,name,payload,status,attempts,max_attempts,run_at,created_at,finished_at)
         VALUES ('a','invoice:123','pay','{{}}','succeeded',1,3,1,1,1);"
    )).unwrap();
    for app_id in ["a", "b"] {
        let db = RunsDb::open_config(WorkflowStoreConfig::Postgres {
            url: url.clone(),
            schema: schema.clone(),
            app_id: app_id.into(),
        })
        .unwrap();
        let run = db
            .enqueue("pay", &serde_json::json!({}), &identified("invoice:123"))
            .unwrap();
        assert_eq!(run.id, "invoice:123");
        assert_eq!(run.deduplicated, app_id == "a");
    }
    client
        .batch_execute(&format!("DROP SCHEMA {schema} CASCADE"))
        .unwrap();
}

#[test]
fn postgres_store_initialization_is_safe_across_servers() {
    let Ok(url) = std::env::var("TAKO_TEST_POSTGRES_URL") else {
        return;
    };
    let schema = format!(
        "identity_concurrent_init_{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
    let threads: Vec<_> = (0..8)
        .map(|_| {
            let barrier = barrier.clone();
            let url = url.clone();
            let schema = schema.clone();
            std::thread::spawn(move || {
                barrier.wait();
                let db = RunsDb::open_config(WorkflowStoreConfig::Postgres {
                    url,
                    schema,
                    app_id: "a".into(),
                })
                .unwrap();
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
    let mut client = postgres::Client::connect(&url, postgres::NoTls).unwrap();
    client
        .batch_execute(&format!("DROP SCHEMA {schema} CASCADE"))
        .unwrap();
}
