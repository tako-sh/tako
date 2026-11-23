use super::*;
use crate::WorkerSpec;
use crate::enqueue::HostedFixture;
use std::time::Duration;
use tako_core::EnqueueOpts;

#[tokio::test(flavor = "multi_thread")]
async fn scrape_exports_claims_without_database_io_and_removes_stopped_apps() {
    let dir = tempfile::tempdir().unwrap();
    let store = HostedFixture::start();
    let manager = Arc::new(WorkflowManager::new(dir.path()));
    manager.set_store_resolver(store.resolver());
    let registry = Registry::new();
    register(&manager, "test", &registry).unwrap();
    for app in ["a", "b"] {
        let spec = WorkerSpec {
            app: app.into(),
            workers: 0,
            concurrency: 1,
            idle_timeout_ms: 0,
            command: vec!["sleep".into(), "10".into()],
            cwd: dir.path().into(),
            env: Default::default(),
            secrets: Default::default(),
            storages: Default::default(),
            databases: Default::default(),
            kv: Default::default(),
            log_sink: None,
            isolation: None,
            lanes: Vec::new(),
            external: false,
        };
        manager.ensure(app, None, spec).await.unwrap();
    }
    registry.gather(); // Enables in-memory timers.
    let stores = manager.monitored_stores();
    let db = &stores.iter().find(|(app, _)| app == "a").unwrap().1;
    tokio::task::block_in_place(|| {
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
    });
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
    // Only claim timings: no gauge is sampled from the store.
    assert!(families.iter().all(|f| f.name() == CLAIM_NAME));
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

#[test]
fn claims_record_nothing_until_monitoring_observes_the_store() {
    let store = HostedFixture::start();
    let db = store.open("app");
    db.enqueue("work", &serde_json::json!({}), &EnqueueOpts::default())
        .unwrap();
    db.claim("worker", &["work".into()], 60_000).unwrap();
    assert!(db.monitoring.claims.get().is_none());
    db.monitoring.enable("test", "app");
    // A failing claim is timed as an error.
    drop(store);
    assert!(db.claim("worker", &["work".into()], 60_000).is_err());
    assert_eq!(
        db.monitoring.claims.get().unwrap().outcomes[2].get_sample_count(),
        1
    );
}
