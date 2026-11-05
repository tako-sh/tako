use super::*;
use std::cell::RefCell;
use std::sync::{Arc, Barrier, mpsc};
use std::time::Duration;

#[derive(Debug)]
enum SignalProgress {
    WaitingForWriter,
    Finished,
}

struct WriterGate {
    progress: mpsc::Sender<SignalProgress>,
    released: mpsc::Receiver<()>,
}

thread_local! {
    // rusqlite's busy handler takes a function pointer. Keep this gate local to
    // the signal thread so parallel tests cannot consume each other's events.
    static WRITER_GATE: RefCell<Option<WriterGate>> = const { RefCell::new(None) };
}

fn wait_for_writer(_: i32) -> bool {
    WRITER_GATE.with(|gate| {
        let Some(gate) = gate.borrow_mut().take() else {
            return false;
        };
        let _ = gate.progress.send(SignalProgress::WaitingForWriter);
        gate.released.recv_timeout(Duration::from_secs(10)).is_ok()
    })
}

#[test]
fn signal_waits_for_writer_before_reading_waiters() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("workflows.sqlite");
    let signaler = SqliteRunsDb::open(&path).unwrap();
    let writer = SqliteRunsDb::open(&path).unwrap();
    for id in ["committed", "uncommitted"] {
        writer
            .enqueue(
                id,
                &serde_json::json!({}),
                &EnqueueOpts {
                    id: Some(id.into()),
                    ..Default::default()
                },
            )
            .unwrap();
    }
    writer
        .claim("worker", &["committed".into()], 60_000)
        .unwrap();
    writer
        .wait_for_event("committed", "worker", "approval", "event", None)
        .unwrap();

    let mut conn = writer.conn.lock();
    let tx = conn
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .unwrap();
    tx.execute(
        "UPDATE runs SET run_at=?1 WHERE id='uncommitted'",
        [i64::MAX],
    )
    .unwrap();
    tx.execute(
        "INSERT INTO event_waiters (run_id, step_name, event_name)
         VALUES ('uncommitted', 'approval', 'event')",
        (),
    )
    .unwrap();

    let (progress_tx, progress_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let signal = std::thread::spawn(move || {
        WRITER_GATE.with(|gate| {
            *gate.borrow_mut() = Some(WriterGate {
                progress: progress_tx.clone(),
                released: release_rx,
            });
        });
        signaler
            .conn
            .lock()
            .busy_handler(Some(wait_for_writer))
            .unwrap();
        let result = signaler.signal("event", &serde_json::json!({"approved": true}));
        let _ = progress_tx.send(SignalProgress::Finished);
        result
    });

    // A deferred transaction reads the first waiter, then fails its lock upgrade
    // without invoking the busy handler. An immediate transaction waits before
    // taking its snapshot and must also see the second waiter after this commit.
    let progress = progress_rx.recv_timeout(Duration::from_secs(10));
    tx.commit().unwrap();
    drop(conn);
    let _ = release_tx.send(());
    let result = signal.join().unwrap();
    assert!(
        matches!(progress, Ok(SignalProgress::WaitingForWriter)),
        "signal did not wait for the writer: progress={progress:?}, result={result:?}"
    );
    assert_eq!(result.unwrap(), 2);
    assert_eq!(
        writer
            .signal("event", &serde_json::json!("duplicate"))
            .unwrap(),
        0
    );
    for id in ["committed", "uncommitted"] {
        let run = writer
            .claim("resumed", &[id.into()], 60_000)
            .unwrap()
            .unwrap();
        assert_eq!(run.id, id);
        assert_eq!(
            run.step_state["approval"],
            serde_json::json!({"approved": true})
        );
        writer.complete(id, "resumed").unwrap();
    }
    assert_eq!(
        writer.raw_query_values("SELECT COUNT(*) FROM event_waiters", ()),
        vec![Value::Integer(0)]
    );
}

#[test]
fn independent_workflow_connections_complete_under_sustained_contention() {
    const WRITERS: usize = 6;
    const RUNS_PER_WRITER: usize = 48;
    const LEASE_MS: u64 = 600_000;
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("workflows.sqlite");
    let stores: Vec<_> = (0..WRITERS)
        .map(|_| SqliteRunsDb::open(&path).unwrap())
        .collect();
    for store in &stores {
        assert_eq!(
            store.raw_query_values("PRAGMA journal_mode", ()),
            vec![Value::Text("wal".into())]
        );
        assert_eq!(
            store.raw_query_values("PRAGMA synchronous", ()),
            vec![Value::Integer(2)]
        );
        assert_eq!(
            store.raw_query_values("PRAGMA busy_timeout", ()),
            vec![Value::Integer(5000)]
        );
    }
    let barrier = Arc::new(Barrier::new(WRITERS));
    let writers: Vec<_> = stores
        .into_iter()
        .enumerate()
        .map(|(worker, db)| {
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                let name = format!("workflow-{worker}");
                let names = [name.clone()];
                barrier.wait();
                for iteration in 0..RUNS_PER_WRITER {
                    let id = format!("run-{worker}-{iteration}");
                    let owner = format!("worker-{worker}:{iteration}");
                    let resumed_owner = format!("worker-{worker}:{iteration}-resumed");
                    let retry_owner = format!("worker-{worker}:{iteration}-retry");
                    let payload = serde_json::json!({"worker": worker, "iteration": iteration});
                    let opts = EnqueueOpts {
                        id: Some(id.clone()),
                        ..Default::default()
                    };
                    assert!(!db.enqueue(&name, &payload, &opts).unwrap().deduplicated);
                    assert!(db.enqueue(&name, &payload, &opts).unwrap().deduplicated);
                    let run = db.claim(&owner, &names, LEASE_MS).unwrap().unwrap();
                    assert_eq!(run.id, id);
                    assert_eq!(run.payload, payload);
                    db.save_step(&id, &owner, "saved", &payload).unwrap();
                    db.heartbeat(&id, &owner, LEASE_MS).unwrap();
                    assert!(db.reclaim_expired_with_workers().unwrap().is_empty());
                    db.wait_for_event(&id, &owner, "approval", &id, None)
                        .unwrap();
                    assert_eq!(db.signal(&id, &payload).unwrap(), 1);
                    assert_eq!(db.signal(&id, &serde_json::json!("duplicate")).unwrap(), 0);
                    let run = db.claim(&resumed_owner, &names, LEASE_MS).unwrap().unwrap();
                    assert_eq!(run.id, id);
                    assert_eq!(
                        run.step_state,
                        serde_json::json!({"saved": payload, "approval": payload})
                    );
                    assert_eq!(run.attempts, 1);
                    if iteration % 3 == 0 {
                        db.fail(&id, &resumed_owner, "retry", Some(0), false)
                            .unwrap();
                        let run = db.claim(&retry_owner, &names, LEASE_MS).unwrap().unwrap();
                        assert_eq!(run.id, id);
                        assert_eq!(run.attempts, 2);
                        assert_eq!(
                            run.step_state,
                            serde_json::json!({"saved": payload, "approval": payload})
                        );
                        db.complete(&id, &retry_owner).unwrap();
                    } else {
                        db.complete(&id, &resumed_owner).unwrap();
                    }
                    std::thread::yield_now();
                }
            })
        })
        .collect();
    for writer in writers {
        writer.join().unwrap();
    }

    // Reopen after every writer has closed, and verify committed terminal state,
    // checkpoint payloads, and waiter cleanup across all connections.
    let db = SqliteRunsDb::open(&path).unwrap();
    assert_eq!(
        db.raw_query_values("SELECT COUNT(*), SUM(status='succeeded') FROM runs", ()),
        vec![Value::Integer((WRITERS * RUNS_PER_WRITER) as i64); 2]
    );
    assert_eq!(
        db.raw_query_values("SELECT COUNT(*) FROM steps JOIN runs ON runs.id=steps.run_id WHERE steps.result=runs.payload", ()),
        vec![Value::Integer((WRITERS * RUNS_PER_WRITER * 2) as i64)]
    );
    assert_eq!(
        db.raw_query_values("SELECT COUNT(*) FROM event_waiters", ()),
        vec![Value::Integer(0)]
    );
    assert_eq!(
        db.raw_query_values("PRAGMA integrity_check", ()),
        vec![Value::Text("ok".into())]
    );
}
