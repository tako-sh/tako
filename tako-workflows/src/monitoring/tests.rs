use super::*;
use crate::{RunsDb, WorkerSpec, WorkflowStoreConfig};
use tako_core::EnqueueOpts;

#[test]
fn postgres_samples_due_work_and_pinned_snapshots_and_recovers_from_failure() {
    let Ok(url) = std::env::var("TAKO_TEST_POSTGRES_URL") else {
        return;
    };
    let schema = format!(
        "monitor_test_{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let open = |app: &str| {
        RunsDb::open_config(WorkflowStoreConfig::Postgres {
            url: url.clone(),
            schema: schema.clone(),
            app_id: app.into(),
        })
        .unwrap()
    };
    let db = open("a");
    let other = open("b");
    assert!(
        db.monitoring
            .reader
            .as_ref()
            .unwrap()
            .lock()
            .client
            .is_none()
    );
    assert!(db.monitoring.claims.get().is_none());
    db.monitoring.enable("test", "a");
    let now = crate::enqueue::now_ms();
    for due in [now - 60_000, now + 60_000, i64::MAX] {
        db.enqueue(
            "work",
            &serde_json::json!({}),
            &EnqueueOpts {
                run_at_ms: Some(due),
                ..Default::default()
            },
        )
        .unwrap();
    }
    other
        .enqueue(
            "work",
            &serde_json::json!({}),
            &EnqueueOpts {
                run_at_ms: Some(now - 120_000),
                ..Default::default()
            },
        )
        .unwrap();
    let mut holder = Client::connect(&url, NoTls).unwrap();
    holder
        .batch_execute("BEGIN ISOLATION LEVEL REPEATABLE READ; SELECT pg_current_snapshot()")
        .unwrap();
    // Advance transaction IDs after the snapshot so xmin age is observable.
    other
        .enqueue("work", &serde_json::json!({}), &EnqueueOpts::default())
        .unwrap();
    db.monitoring.sample().unwrap();
    let first = db.monitoring.health.read();
    let values = first.values.as_ref().unwrap();
    assert!(first.success);
    assert_eq!(values[0], 1.0);
    assert!(values[1] >= 60.0 && values[1] < 90.0);
    assert!(values[4] > 0.0 && values[5] > 0.0);
    assert!(values[7] > 0.0); // Visible transaction age, excluding sampler.
    assert!(values[8] > 0.0); // Pinned backend xmin age, measured in transactions.
    let timestamp = first.timestamp;
    drop(first);
    holder.batch_execute("ROLLBACK; BEGIN").unwrap();
    holder
        .batch_execute(&format!(
            "LOCK TABLE {schema}.runs IN ACCESS EXCLUSIVE MODE"
        ))
        .unwrap();
    assert!(db.monitoring.sample().is_err()); // Bounded lock timeout, not a hang.
    assert!(!db.monitoring.health.read().success);
    assert_eq!(db.monitoring.health.read().timestamp, timestamp);
    assert_eq!(db.monitoring.health.read().values.as_ref().unwrap()[0], 1.0);
    holder.batch_execute("ROLLBACK").unwrap();
    db.claim("worker", &["work".into()], 60_000)
        .unwrap()
        .unwrap();
    db.monitoring.sample().unwrap();
    assert!(db.monitoring.health.read().success);
    assert_eq!(db.monitoring.health.read().values.as_ref().unwrap()[0], 0.0);

    let mut reader = db.monitoring.reader.as_ref().unwrap().lock();
    let client = reader.client.as_mut().unwrap();
    let read_only: String = client
        .query_one("SHOW default_transaction_read_only", &[])
        .unwrap()
        .get(0);
    assert_eq!(read_only, "on");
    drop(reader);
    holder
        .batch_execute(&format!("DROP SCHEMA {schema} CASCADE"))
        .unwrap();
    assert!(db.claim("worker", &["work".into()], 60_000).is_err());
    assert_eq!(
        db.monitoring.claims.get().unwrap().outcomes[2].get_sample_count(),
        1
    );
}

#[tokio::test]
async fn scrape_exports_claims_without_database_io_and_removes_stopped_apps() {
    let dir = tempfile::tempdir().unwrap();
    let manager = Arc::new(WorkflowManager::new(dir.path()));
    let registry = Registry::new();
    register(
        &manager,
        "test",
        &registry,
        &tokio::runtime::Handle::current(),
    )
    .unwrap();
    for app in ["a", "b"] {
        manager
            .ensure(app, None, |_| WorkerSpec {
                app: app.into(),
                workers: 0,
                concurrency: 1,
                idle_timeout_ms: 0,
                command: vec!["sleep".into(), "10".into()],
                cwd: dir.path().into(),
                env: Default::default(),
                secrets: Default::default(),
                storages: Default::default(),
                log_sink: None,
                isolation: None,
                lanes: Vec::new(),
            })
            .await
            .unwrap();
    }
    registry.gather(); // Enables in-memory timers, no health connection.
    let stores = manager.monitored_stores();
    let db = &stores.iter().find(|(app, _)| app == "a").unwrap().1;
    db.enqueue("work", &serde_json::json!({}), &EnqueueOpts::default())
        .unwrap();
    db.claim("worker", &["work".into()], 60_000)
        .unwrap()
        .unwrap();
    assert!(
        db.claim("worker", &["work".into()], 60_000)
            .unwrap()
            .is_none()
    );
    let families = registry.gather();
    let claims = families.iter().find(|f| f.name() == CLAIM_NAME).unwrap();
    for outcome in ["claimed", "empty"] {
        let sample = claims
            .get_metric()
            .iter()
            .find(|m| {
                m.get_label()
                    .iter()
                    .any(|l| l.name() == "app" && l.value() == "a")
                    && m.get_label()
                        .iter()
                        .any(|l| l.name() == "outcome" && l.value() == outcome)
            })
            .unwrap();
        assert_eq!(sample.get_histogram().get_sample_count(), 1);
    }
    assert!(
        !families
            .iter()
            .any(|f| f.name() == "tako_workflow_pg_runs_dead_tuples")
    );
    manager.stop("a", Duration::ZERO).await;
    assert!(
        registry
            .gather()
            .iter()
            .flat_map(|f| f.get_metric())
            .all(|m| {
                !m.get_label()
                    .iter()
                    .any(|l| l.name() == "app" && l.value() == "a")
            })
    );
    manager.shutdown_all(Duration::ZERO).await;
    assert!(registry.gather().is_empty());
}
