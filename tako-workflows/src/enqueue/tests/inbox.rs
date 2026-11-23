//! Signals addressed to one run: delivery, buffering, consumption, limits.
//! `open` returns a fresh handle (another server's) to one store.

use super::*;
use tako_core::{
    ErrorCode, SignalRunStatus, WORKFLOW_SIGNAL_INBOX_MAX_BYTES, WORKFLOW_SIGNAL_INBOX_MAX_ENTRIES,
};

fn payload(value: &str) -> serde_json::Value {
    serde_json::json!({ "value": value })
}

fn enqueue(db: &RunsDb, id: &str) -> String {
    let opts = EnqueueOpts {
        id: Some(id.into()),
        ..Default::default()
    };
    db.enqueue("work", &serde_json::json!({}), &opts)
        .unwrap()
        .id
}

fn claim(db: &RunsDb) -> RunPayload {
    db.claim("worker", &["work".into()], 60_000)
        .unwrap()
        .expect("a run is due")
}

fn signals_total(db: &RunsDb, id: &str) -> u64 {
    db.get_run(id).unwrap().unwrap().signals_total
}

#[test]
fn a_signal_sent_before_the_wait_is_consumed_by_it() {
    let store = HostedFixture::start();
    let open = || store.open("app");
    let db = open();
    let id = enqueue(&db, "early");
    assert_eq!(
        db.signal_run(&id, "approval", &payload("yes")).unwrap(),
        SignalRunStatus::Buffered
    );
    let run = claim(&db);
    assert!(run.step_state.get("approval").is_none());

    // The wait consumes the buffered signal instead of parking.
    assert!(
        db.wait_for_event(&id, &run.worker_id, "approval", "approval", None)
            .unwrap()
    );
    assert!(db.has_runnable_work().unwrap(), "the run is due at once");
    let run = claim(&db);
    assert_eq!(run.step_state["approval"], payload("yes"));
    assert_eq!(signals_total(&db, &id), 0);
    assert!(db.get_run(&id).unwrap().unwrap().waits.is_empty());
}

#[test]
fn a_waiting_run_is_woken_at_once() {
    let store = HostedFixture::start();
    let open = || store.open("app");
    let db = open();
    let id = enqueue(&db, "waiting");
    let run = claim(&db);
    assert!(
        !db.wait_for_event(&id, &run.worker_id, "approval", "approval", None)
            .unwrap()
    );
    assert!(!db.has_runnable_work().unwrap());
    assert_eq!(
        db.signal_run(&id, "approval", &payload("yes")).unwrap(),
        SignalRunStatus::Delivered
    );
    let run = claim(&db);
    assert_eq!(run.step_state["approval"], payload("yes"));
    assert_eq!(signals_total(&db, &id), 0);
}

#[test]
fn a_signal_sent_while_the_run_executes_is_not_lost() {
    let store = HostedFixture::start();
    let open = || store.open("app");
    let db = open();
    let id = enqueue(&db, "running");
    // The worker holds step state read at claim time; the signal lands after.
    let run = claim(&db);
    assert_eq!(
        db.signal_run(&id, "approval", &payload("late")).unwrap(),
        SignalRunStatus::Buffered
    );
    assert!(
        db.wait_for_event(
            &id,
            &run.worker_id,
            "approval",
            "approval",
            Some(i64::MAX - 1)
        )
        .unwrap()
    );
    let run = claim(&db);
    assert_eq!(run.step_state["approval"], payload("late"));
}

#[test]
fn the_first_value_for_a_name_wins() {
    let store = HostedFixture::start();
    let open = || store.open("app");
    let db = open();
    let id = enqueue(&db, "dupes");
    assert_eq!(
        db.signal_run(&id, "approval", &payload("first")).unwrap(),
        SignalRunStatus::Buffered
    );
    assert_eq!(
        db.signal_run(&id, "approval", &payload("second")).unwrap(),
        SignalRunStatus::Duplicate
    );
    let run = claim(&db);
    db.wait_for_event(&id, &run.worker_id, "approval", "approval", None)
        .unwrap();
    let run = claim(&db);
    assert_eq!(run.step_state["approval"], payload("first"));
    // A name the run already consumed, or used for a step, is held too.
    assert_eq!(
        db.signal_run(&id, "approval", &payload("third")).unwrap(),
        SignalRunStatus::Duplicate
    );
    db.save_step(&id, &run.worker_id, "charge", &serde_json::json!(1))
        .unwrap();
    assert_eq!(
        db.signal_run(&id, "charge", &payload("forged")).unwrap(),
        SignalRunStatus::Duplicate
    );
    assert_eq!(signals_total(&db, &id), 0);
}

#[test]
fn finished_and_missing_runs_store_nothing() {
    let store = HostedFixture::start();
    let open = || store.open("app");
    let db = open();
    assert_eq!(
        db.signal_run("nope", "approval", &payload("x")).unwrap(),
        SignalRunStatus::Missing
    );
    let id = enqueue(&db, "done");
    let run = claim(&db);
    db.complete(&id, &run.worker_id, &serde_json::Value::Null)
        .unwrap();
    assert_eq!(
        db.signal_run(&id, "approval", &payload("x")).unwrap(),
        SignalRunStatus::Finished
    );
    assert_eq!(signals_total(&db, &id), 0);
}

#[test]
fn a_full_inbox_rejects_instead_of_dropping() {
    let store = HostedFixture::start();
    let open = || store.open("app");
    let db = open();
    let id = enqueue(&db, "full");
    for index in 0..WORKFLOW_SIGNAL_INBOX_MAX_ENTRIES {
        assert_eq!(
            db.signal_run(&id, &format!("item:{index}"), &serde_json::json!(index))
                .unwrap(),
            SignalRunStatus::Buffered
        );
    }
    let error = db
        .signal_run(&id, "one-more", &serde_json::json!(1))
        .unwrap_err();
    assert!(
        matches!(error, RunsDbError::SignalInboxFull { .. }),
        "{error}"
    );
    assert_eq!(error.socket_code(), Some(ErrorCode::SignalInboxFull));
    assert_eq!(
        signals_total(&db, &id),
        WORKFLOW_SIGNAL_INBOX_MAX_ENTRIES as u64
    );
    // Delivery to a waiting step does not need inbox room.
    let run = claim(&db);
    db.wait_for_event(&id, &run.worker_id, "approval", "approval", None)
        .unwrap();
    assert_eq!(
        db.signal_run(&id, "approval", &payload("yes")).unwrap(),
        SignalRunStatus::Delivered
    );
    // Consuming a buffered signal frees room for another.
    let run = claim(&db);
    assert!(
        db.wait_for_event(&id, &run.worker_id, "item:0", "item:0", None)
            .unwrap()
    );
    assert_eq!(
        db.signal_run(&id, "one-more", &serde_json::json!(1))
            .unwrap(),
        SignalRunStatus::Buffered
    );
}

#[test]
fn an_oversized_inbox_rejects_instead_of_dropping() {
    let store = HostedFixture::start();
    let open = || store.open("app");
    let db = open();
    let id = enqueue(&db, "large");
    // JSON text of a string is its contents plus two quotes.
    let half = "x".repeat(WORKFLOW_SIGNAL_INBOX_MAX_BYTES / 2 - 2);
    assert_eq!(
        db.signal_run(&id, "a", &serde_json::json!(half)).unwrap(),
        SignalRunStatus::Buffered
    );
    assert_eq!(
        db.signal_run(&id, "b", &serde_json::json!(half)).unwrap(),
        SignalRunStatus::Buffered
    );
    let error = db.signal_run(&id, "c", &serde_json::json!("")).unwrap_err();
    assert!(
        matches!(error, RunsDbError::SignalInboxFull { .. }),
        "{error}"
    );
    let detail = db.get_run(&id).unwrap().unwrap();
    assert_eq!(
        detail.signals.iter().map(|s| s.bytes).sum::<u64>(),
        WORKFLOW_SIGNAL_INBOX_MAX_BYTES as u64
    );
}

#[test]
fn names_and_ids_are_validated() {
    let store = HostedFixture::start();
    let open = || store.open("app");
    let db = open();
    let id = enqueue(&db, "names");
    for name in ["", "__run:x", "a\0b", "a\nb", "\u{1b}[2J", &"n".repeat(256)] {
        let error = db.signal_run(&id, name, &payload("x")).unwrap_err();
        assert_eq!(
            error.socket_code(),
            Some(ErrorCode::InvalidRequest),
            "{name:?}"
        );
    }
    assert_eq!(
        db.signal_run(&id, &"n".repeat(255), &payload("x")).unwrap(),
        SignalRunStatus::Buffered
    );
    for run_id in ["", "a\0b", &"r".repeat(256)] {
        let error = db
            .signal_run(run_id, "approval", &payload("x"))
            .unwrap_err();
        assert_eq!(
            error.socket_code(),
            Some(ErrorCode::InvalidRequest),
            "{run_id:?}"
        );
    }
    // Broadcasts keep accepting any name a wait can use, except the reserved prefix.
    let error = db.signal("__run:x", &payload("x")).unwrap_err();
    assert_eq!(error.socket_code(), Some(ErrorCode::InvalidRequest));
    assert_eq!(db.signal(&"n".repeat(300), &payload("x")).unwrap(), 0);
}

#[test]
fn broadcast_signals_are_never_buffered() {
    let store = HostedFixture::start();
    let open = || store.open("app");
    let db = open();
    let id = enqueue(&db, "broadcast");
    assert_eq!(db.signal("approval", &payload("x")).unwrap(), 0);
    assert_eq!(signals_total(&db, &id), 0);
    let run = claim(&db);
    assert!(
        !db.wait_for_event(&id, &run.worker_id, "approval", "approval", None)
            .unwrap()
    );
}

#[test]
fn operators_see_buffered_signals() {
    let store = HostedFixture::start();
    let open = || store.open("app");
    let db = open();
    let id = enqueue(&db, "visible");
    db.signal_run(&id, "shipped", &serde_json::json!({"carrier": "ups"}))
        .unwrap();
    db.signal_run(&id, "delivered", &serde_json::json!(true))
        .unwrap();
    let detail = db.get_run(&id).unwrap().unwrap();
    assert_eq!(detail.signals_total, 2);
    let mut names: Vec<_> = detail
        .signals
        .iter()
        .map(|s| s.event_name.as_str())
        .collect();
    names.sort();
    assert_eq!(names, ["delivered", "shipped"]);
    let shipped = detail
        .signals
        .iter()
        .find(|s| s.event_name == "shipped")
        .unwrap();
    assert_eq!(shipped.bytes, r#"{"carrier":"ups"}"#.len() as u64);
}

#[test]
fn pruning_a_run_drops_its_inbox_before_its_id_is_reused() {
    let store = HostedFixture::start();
    let open = || store.open("app");
    let db = open();
    let id = enqueue(&db, "order-1");
    db.signal_run(&id, "approval", &payload("stale")).unwrap();
    let run = claim(&db);
    db.complete(&id, &run.worker_id, &serde_json::Value::Null)
        .unwrap();
    assert_eq!(
        signals_total(&db, &id),
        1,
        "kept for inspection until pruned"
    );
    assert_eq!(db.prune_finished_before(i64::MAX, 100).unwrap(), 1);

    let id = enqueue(&db, "order-1");
    assert_eq!(signals_total(&db, &id), 0);
    let run = claim(&db);
    assert!(
        !db.wait_for_event(&id, &run.worker_id, "approval", "approval", None)
            .unwrap(),
        "a pruned run's signal must not reach the next run with its ID"
    );
}

/// A wait whose step result is already saved (the worker's step state was
/// read before it was written) makes the run due instead of parking it.
#[test]
fn a_wait_whose_result_is_already_saved_never_parks() {
    let store = HostedFixture::start();
    let open = || store.open("app");
    let db = open();
    let id = enqueue(&db, "saved");
    let run = claim(&db);
    db.save_step(&id, &run.worker_id, "approval", &payload("saved"))
        .unwrap();
    assert!(
        db.wait_for_event(&id, &run.worker_id, "approval", "approval", None)
            .unwrap()
    );
    let run = claim(&db);
    assert_eq!(run.step_state["approval"], payload("saved"));
    assert!(db.get_run(&id).unwrap().unwrap().waits.is_empty());
}

/// A wait and a signal for the same run on separate connections, in either
/// order: the payload must end up as the step result and the run due.
#[test]
fn a_signal_racing_a_wait_always_reaches_the_run() {
    let store = HostedFixture::start();
    let open = || store.open("app");
    let waiter = open();
    let sender = open();
    for round in 0..40 {
        let id = enqueue(&waiter, &format!("race-{round}"));
        let run = claim(&waiter);
        let owner = run.worker_id.clone();
        std::thread::scope(|scope| {
            let wait = scope.spawn(|| {
                waiter
                    .wait_for_event(&id, &owner, "approval", "approval", None)
                    .unwrap()
            });
            let send = scope.spawn(|| {
                sender
                    .signal_run(&id, "approval", &payload(&format!("round {round}")))
                    .unwrap()
            });
            let delivered_at_wait = wait.join().unwrap();
            let status = send.join().unwrap();
            assert_eq!(
                delivered_at_wait,
                status == SignalRunStatus::Buffered,
                "round {round}: {status:?}"
            );
        });
        let run = claim(&waiter);
        assert_eq!(run.id, id, "round {round}: the run must be due");
        assert_eq!(
            run.step_state["approval"],
            payload(&format!("round {round}"))
        );
        waiter
            .complete(&id, &run.worker_id, &serde_json::Value::Null)
            .unwrap();
    }
}

/// A signal that arrives after a wait's deadline, before its run is
/// claimed, leaves the wait timed out (`null`), whether addressed to the
/// run or broadcast.
#[test]
fn signals_after_a_wait_deadline_leave_it_timed_out() {
    let store = HostedFixture::start();
    let open = || store.open("app");
    let db = open();
    let id = enqueue(&db, "late-run");
    let run = claim(&db);
    db.wait_for_event(
        &id,
        &run.worker_id,
        "approval",
        "approval",
        Some(now_ms() + 20),
    )
    .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(40));
    assert_eq!(
        db.signal_run(&id, "approval", &payload("late")).unwrap(),
        SignalRunStatus::Duplicate
    );
    assert_eq!(claim(&db).step_state["approval"], serde_json::Value::Null);
    assert_eq!(signals_total(&db, &id), 0);

    let id = enqueue(&db, "late-broadcast");
    let run = claim(&db);
    db.wait_for_event(
        &id,
        &run.worker_id,
        "late-event",
        "late-event",
        Some(now_ms() + 20),
    )
    .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(40));
    assert_eq!(db.signal("late-event", &payload("late")).unwrap(), 0);
    assert_eq!(claim(&db).step_state["late-event"], serde_json::Value::Null);
}

/// A signal buffered after a wait's deadline, before the run registers that
/// wait (a worker still finishing sibling steps), leaves the wait timed out.
/// One buffered before the deadline is consumed even when the run registers
/// the wait late.
#[test]
fn a_signal_buffered_after_the_deadline_is_not_consumed() {
    let store = HostedFixture::start();
    let open = || store.open("app");
    let db = open();
    let id = enqueue(&db, "slow-register");
    let run = claim(&db);
    let deadline = now_ms() - 1_000;
    db.signal_run(&id, "approval", &payload("late")).unwrap();
    assert!(
        db.wait_for_event(&id, &run.worker_id, "approval", "approval", Some(deadline))
            .unwrap()
    );
    assert_eq!(claim(&db).step_state["approval"], serde_json::Value::Null);
    assert_eq!(
        signals_total(&db, &id),
        1,
        "the late signal is kept, unconsumed"
    );

    let id = enqueue(&db, "slow-register-in-time");
    let run = claim(&db);
    db.signal_run(&id, "approval", &payload("on time")).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(5));
    let deadline = now_ms();
    std::thread::sleep(std::time::Duration::from_millis(5));
    assert!(
        db.wait_for_event(&id, &run.worker_id, "approval", "approval", Some(deadline))
            .unwrap()
    );
    assert_eq!(claim(&db).step_state["approval"], payload("on time"));
}
