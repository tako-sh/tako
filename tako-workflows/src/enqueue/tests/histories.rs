use super::*;

#[path = "../../../../test-support/history.rs"]
mod history;

fn lifecycle_history(seed: u64) -> Vec<String> {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("workflows.sqlite");
    let mut db = RunsDb::open(&path).unwrap();
    let mut random = history::Seed(seed);
    let mut trace = Vec::new();
    for turn in 0..12 {
        let id = format!("run-{turn}");
        let options = EnqueueOpts {
            id: Some(id.clone()),
            ..Default::default()
        };
        let payload = serde_json::json!({"turn": turn});
        assert!(!db.enqueue("work", &payload, &options).unwrap().deduplicated);
        // Retry an enqueue whose successful response was lost.
        assert!(db.enqueue("work", &payload, &options).unwrap().deduplicated);
        assert!(matches!(
            db.enqueue("other", &payload, &options),
            Err(RunsDbError::IdConflict { .. })
        ));
        let run = db.claim("old", &["work".into()], 60_000).unwrap().unwrap();
        assert_eq!(run.id, id);
        assert!(
            db.claim("competitor", &["work".into()], 60_000)
                .unwrap()
                .is_none()
        );
        db.save_step(&id, "old", "saved", &payload).unwrap();
        db.save_step(
            &id,
            "old",
            "saved",
            &serde_json::json!("duplicate response"),
        )
        .unwrap();

        let transition = random.choose(3);
        match transition {
            0 => {
                // Expiry is injected into persisted state; no wall-clock sleeps.
                db.raw_execute("UPDATE runs SET lease_until=0 WHERE id=?1", [&id]);
                assert_eq!(db.reclaim_expired().unwrap(), 1);
            }
            1 => db.fail(&id, "old", "retry", Some(0), false).unwrap(),
            _ => db.defer(&id, "old", Some(0)).unwrap(),
        }
        drop(db);
        db = RunsDb::open(&path).unwrap();
        assert_eq!(
            db.raw_query_values("PRAGMA synchronous", ()),
            vec![Value::Integer(2)]
        );
        let run = db.claim("new", &["work".into()], 60_000).unwrap().unwrap();
        assert_eq!(run.id, id);
        assert_eq!(run.step_state["saved"], payload);
        assert!(matches!(
            db.heartbeat(&id, "old", 60_000),
            Err(RunsDbError::StaleWorker)
        ));
        assert!(matches!(
            db.save_step(&id, "old", "late", &payload),
            Err(RunsDbError::StaleWorker)
        ));
        assert!(matches!(
            db.complete(&id, "old"),
            Err(RunsDbError::StaleWorker)
        ));
        assert!(matches!(
            db.wait_for_event(&id, "old", "late-wait", "event", None),
            Err(RunsDbError::StaleWorker)
        ));

        let timeout = random.choose(2) == 0;
        db.wait_for_event(&id, "new", "approval", "event", timeout.then_some(0))
            .unwrap();
        if !timeout {
            assert!(
                db.claim("early", &["work".into()], 60_000)
                    .unwrap()
                    .is_none()
            );
            assert_eq!(db.signal("unrelated", &payload).unwrap(), 0);
            assert_eq!(db.signal("event", &payload).unwrap(), 1);
            assert_eq!(db.signal("event", &serde_json::json!("retry")).unwrap(), 0);
        }
        let resumed = db
            .claim("resumed", &["work".into()], 60_000)
            .unwrap()
            .unwrap();
        assert_eq!(resumed.step_state["saved"], payload);
        assert_eq!(
            resumed.step_state["approval"],
            if timeout {
                serde_json::Value::Null
            } else {
                payload
            }
        );
        assert!(resumed.step_state.get("late").is_none());
        db.complete(&id, "resumed").unwrap();
        assert!(
            db.claim("finished", &["work".into()], 60_000)
                .unwrap()
                .is_none()
        );
        assert_eq!(db.reclaim_expired().unwrap(), 0);
        trace.push(format!(
            "{id}: transition={transition} timeout={timeout} steps={}",
            resumed.step_state
        ));
    }
    trace
}

#[test]
fn workflow_lifecycle_histories_are_reproducible() {
    for seed in history::seeds() {
        let first = std::panic::catch_unwind(|| lifecycle_history(seed)).unwrap_or_else(|error| {
            panic!("workflow history failed: seed={seed}, error={error:?}")
        });
        assert_eq!(
            first,
            lifecycle_history(seed),
            "seed={seed}: logical trace changed"
        );
    }
}

#[test]
fn workflow_crash_recovery() {
    const CHILD_PATH: &str = "TAKO_TEST_WORKFLOW_CRASH_PATH";
    if let Some(path) = std::env::var_os(CHILD_PATH) {
        let db = RunsDb::open(std::path::Path::new(&path)).unwrap();
        db.enqueue(
            "work",
            &serde_json::json!({}),
            &EnqueueOpts {
                id: Some("acknowledged".into()),
                ..Default::default()
            },
        )
        .unwrap();
        db.claim("exited", &["work".into()], 60_000)
            .unwrap()
            .unwrap();
        db.save_step(
            "acknowledged",
            "exited",
            "checkpoint",
            &serde_json::json!(42),
        )
        .unwrap();
        // Exit without Rust destructors or a SQLite close/checkpoint. This
        // verifies process-crash recovery, not physical power-loss behavior.
        std::process::exit(23);
    }
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("workflows.sqlite");
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "enqueue::tests::histories::workflow_crash_recovery",
            "--nocapture",
        ])
        .env(CHILD_PATH, &path)
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(23));
    let db = RunsDb::open(&path).unwrap();
    assert_eq!(
        db.raw_query_values("PRAGMA synchronous", ()),
        vec![Value::Integer(2)]
    );
    db.raw_execute("UPDATE runs SET lease_until=0", ());
    assert_eq!(db.reclaim_expired().unwrap(), 1);
    let run = db
        .claim("replacement", &["work".into()], 60_000)
        .unwrap()
        .unwrap();
    assert_eq!(run.id, "acknowledged");
    assert_eq!(run.step_state["checkpoint"], 42);
    assert!(matches!(
        db.complete(&run.id, "exited"),
        Err(RunsDbError::StaleWorker)
    ));
}
