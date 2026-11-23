//! Several servers share one app's store. Each consumable piece of state (a
//! due run, an expired lease, a waiting step, a running attempt) is taken by
//! exactly one of them, however their calls interleave.

use super::*;
use std::sync::{Arc, Barrier};

/// Runs `call` on each handle at once and returns their results in order.
fn at_once<T: Send>(dbs: &[RunsDb], call: impl Fn(&RunsDb) -> T + Sync) -> Vec<T> {
    let barrier = Barrier::new(dbs.len());
    std::thread::scope(|scope| {
        let calls: Vec<_> = dbs
            .iter()
            .map(|db| {
                let (barrier, call) = (&barrier, &call);
                scope.spawn(move || {
                    barrier.wait();
                    call(db)
                })
            })
            .collect();
        calls.into_iter().map(|call| call.join().unwrap()).collect()
    })
}

#[test]
fn a_claim_takes_exactly_one_of_several_due_runs() {
    let store = Store::new();
    for _ in 0..3 {
        store.enqueue("work", &json!({}), &opts()).unwrap();
    }
    for remaining in [2, 1, 0] {
        let run = store
            .claim("worker", &["work".into()], 60_000)
            .unwrap()
            .unwrap();
        assert_eq!(run.attempts, 1);
        assert_eq!(store.pending_count().unwrap(), remaining);
    }
    assert!(
        store
            .claim("worker", &["work".into()], 60_000)
            .unwrap()
            .is_none()
    );

    // Servers claiming at once never take the same run.
    for _ in 0..4 {
        store.enqueue("work", &json!({}), &opts()).unwrap();
    }
    let servers: Vec<_> = (0..4).map(|_| store.other_app("app")).collect();
    let mut claimed: Vec<String> = at_once(&servers, |db| {
        db.claim("worker", &["work".into()], 60_000)
            .unwrap()
            .unwrap()
            .id
    });
    claimed.sort();
    claimed.dedup();
    assert_eq!(claimed.len(), 4);
}

#[test]
fn concurrent_reclaimers_and_signals_consume_state_once() {
    let store = Store::new();
    let servers = [store.other_app("app"), store.other_app("app")];
    for round in 0..10 {
        let id = store.enqueue("work", &json!({}), &opts()).unwrap().id;
        let owner = format!("owner-{round}");
        store.claim(&owner, &["work".into()], 60_000).unwrap();
        store.expire_lease(&id);
        let reclaimed = at_once(&servers, |db| db.reclaim_expired_with_workers().unwrap());
        assert_eq!(reclaimed.concat(), vec![owner], "round {round}");

        let run = store
            .claim("waiter", &["work".into()], 60_000)
            .unwrap()
            .unwrap();
        store
            .wait_for_event(&id, &run.worker_id, "approval", "approval", None)
            .unwrap();
        let woken = at_once(&servers, |db| db.signal("approval", &json!(round)).unwrap());
        assert_eq!(woken.iter().sum::<u64>(), 1, "round {round}");
        let run = store
            .claim("resumed", &["work".into()], 60_000)
            .unwrap()
            .unwrap();
        assert_eq!(run.step_state["approval"], json!(round));
        store.complete(&id, &run.worker_id, &Value::Null).unwrap();
    }
}

/// An operator cancel and the running attempt's completion race on two
/// servers. Exactly one wins; the loser sees a specific refusal and the
/// stored status matches the winner.
#[test]
fn an_operator_cancel_and_a_late_completion_serialize_on_the_run() {
    let store = Store::new();
    let servers = [store.other_app("app"), store.other_app("app")];
    for round in 0..10 {
        let id = store.enqueue("work", &json!({}), &opts()).unwrap().id;
        let owner = store
            .claim("w", &["work".into()], 60_000)
            .unwrap()
            .unwrap()
            .worker_id;
        let [completed, cancelled]: [Result<Option<CancelledRun>, RunsDbError>; 2] =
            at_once(&servers, |db| {
                if std::ptr::eq(db, &servers[0]) {
                    db.complete(&id, &owner, &json!(1)).map(|()| None)
                } else {
                    db.cancel_run(&id).map(Some)
                }
            })
            .try_into()
            .unwrap();
        let status = store.result("work", &id).unwrap().unwrap().status;
        match (completed, cancelled) {
            (Ok(_), Err(RunsDbError::RunAlreadyFinished { status: s, .. })) => {
                assert_eq!(s, WorkflowRunStatus::Succeeded);
                assert_eq!(status, "succeeded");
            }
            (Err(RunsDbError::StaleWorker), Ok(Some(cancelled))) => {
                assert_eq!(cancelled.previous_status, WorkflowRunStatus::Running);
                assert_eq!(status, "cancelled");
            }
            other => panic!("round {round}: both or neither won: {other:?}"),
        }
    }
}

/// Servers run whole workflow lifecycles at once against one store: every
/// run, checkpoint and signal lands exactly once.
#[test]
fn servers_complete_workflows_under_sustained_contention() {
    const SERVERS: usize = 4;
    const RUNS_PER_SERVER: usize = 12;
    const LEASE_MS: u64 = 600_000;
    let store = Store::new();
    let servers: Vec<_> = (0..SERVERS).map(|_| store.other_app("app")).collect();
    let index = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    at_once(&servers, |db| {
        let worker = index.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let name = format!("workflow-{worker}");
        let names = [name.clone()];
        for iteration in 0..RUNS_PER_SERVER {
            let id = format!("run-{worker}-{iteration}");
            let owner = format!("worker-{worker}:{iteration}");
            let resumed_owner = format!("worker-{worker}:{iteration}-resumed");
            let payload = json!({"worker": worker, "iteration": iteration});
            let opts = EnqueueOpts {
                id: Some(id.clone()),
                ..Default::default()
            };
            assert!(!db.enqueue(&name, &payload, &opts).unwrap().deduplicated);
            assert!(db.enqueue(&name, &payload, &opts).unwrap().deduplicated);
            let run = db.claim(&owner, &names, LEASE_MS).unwrap().unwrap();
            assert_eq!((run.id.as_str(), &run.payload), (id.as_str(), &payload));
            db.save_step(&id, &owner, "saved", &payload).unwrap();
            db.heartbeat(&id, &owner, LEASE_MS).unwrap();
            db.wait_for_event(&id, &owner, "approval", &id, None)
                .unwrap();
            assert_eq!(db.signal(&id, &payload).unwrap(), 1);
            assert_eq!(db.signal(&id, &json!("duplicate")).unwrap(), 0);
            let run = db.claim(&resumed_owner, &names, LEASE_MS).unwrap().unwrap();
            assert_eq!(
                run.step_state,
                json!({"saved": payload, "approval": payload})
            );
            assert_eq!(run.attempts, 1);
            db.complete(&id, &resumed_owner, &Value::Null).unwrap();
        }
    });
    let total = (SERVERS * RUNS_PER_SERVER) as i64;
    assert_eq!(
        store.query_row(
            "SELECT COUNT(*), SUM(status = 'succeeded') FROM {runs} WHERE app = 'app'",
            &[]
        ),
        vec![json!(total), json!(total)]
    );
    assert_eq!(
        store.query_row(
            "SELECT COUNT(*) FROM {steps} s JOIN {runs} r ON r.app = s.app AND r.id = s.run_id
             WHERE s.result = r.payload",
            &[]
        ),
        vec![json!(total * 2)]
    );
    assert_eq!(
        store.query_row("SELECT COUNT(*) FROM {event_waiters}", &[]),
        vec![json!(0)]
    );
}
