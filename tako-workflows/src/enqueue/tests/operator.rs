//! Operator list, show, retry, and cancel against every backend.

use super::reclaim::each_store;
use super::*;
use tako_core::WorkflowRunStatus::{Cancelled, Dead, Pending, Running, Succeeded};

fn enqueue(db: &RunsDb, id: &str, name: &str, max_attempts: u32) {
    let options = EnqueueOpts {
        id: Some(id.into()),
        max_attempts: Some(max_attempts),
        ..opts()
    };
    db.enqueue(name, &serde_json::json!({"id": id}), &options)
        .unwrap();
}

fn claim(db: &RunsDb, name: &str, worker: &str) -> tako_core::RunPayload {
    db.claim(worker, &[name.into()], 60_000).unwrap().unwrap()
}

fn run(db: &RunsDb, id: &str) -> WorkflowRunDetail {
    db.get_run(id).unwrap().unwrap()
}

fn ids(runs: &[WorkflowRunSummary]) -> Vec<&str> {
    runs.iter().map(|run| run.id.as_str()).collect()
}

#[test]
fn list_shows_newest_runs_first_with_filters_and_a_limit() {
    each_store("operator_list", |store| {
        let db = &store.db;
        for (id, name) in [("r1", "email"), ("r2", "report"), ("r3", "email")] {
            enqueue(db, id, name, 3);
        }
        let worker = claim(db, "report", "w").worker_id;
        db.fail("r2", &worker, "disk full", None, true).unwrap();

        let (runs, has_more) = db.list_runs(None, None, 50).unwrap();
        assert_eq!((ids(&runs), has_more), (vec!["r3", "r2", "r1"], false));
        let dead = &runs[1];
        assert_eq!((dead.name.as_str(), dead.status), ("report", Dead));
        assert_eq!((dead.attempts, dead.max_attempts), (1, 3));
        assert_eq!(dead.last_error.as_deref(), Some("disk full"));
        assert!(dead.finished_at_ms.is_some() && dead.run_at_ms.is_some());
        assert!(runs[0].finished_at_ms.is_none());

        let (runs, has_more) = db.list_runs(None, None, 2).unwrap();
        assert_eq!((ids(&runs), has_more), (vec!["r3", "r2"], true));
        let (runs, _) = db.list_runs(Some(Pending), None, 50).unwrap();
        assert_eq!(ids(&runs), vec!["r3", "r1"]);
        let (runs, _) = db.list_runs(None, Some("email"), 1).unwrap();
        assert_eq!(ids(&runs), vec!["r3"]);
        let (runs, _) = db.list_runs(Some(Dead), Some("email"), 50).unwrap();
        assert!(runs.is_empty());

        // A shared Postgres store keeps each app's runs to itself.
        if let Some(other) = store.other_app("other") {
            assert!(other.list_runs(None, None, 50).unwrap().0.is_empty());
            assert!(other.get_run("r1").unwrap().is_none());
            assert!(matches!(
                other.cancel_run("r1"),
                Err(RunsDbError::RunNotFound { .. })
            ));
            assert!(matches!(
                other.retry_run("r2"),
                Err(RunsDbError::RunNotFound { .. })
            ));
        }
    });
}

#[test]
fn show_returns_run_fields_steps_and_waits() {
    each_store("operator_show", |store| {
        let db = &store.db;
        assert!(db.get_run("missing").unwrap().is_none());
        enqueue(db, "r1", "work", 3);
        let worker = claim(db, "work", "w").worker_id;
        db.save_step("r1", &worker, "charge", &serde_json::json!({"receipt": 7}))
            .unwrap();
        db.save_step(
            "r1",
            &worker,
            "huge",
            &serde_json::json!("x".repeat(10_000)),
        )
        .unwrap();

        let running = run(db, "r1");
        assert_eq!((running.status, running.attempts), (Running, 1));
        assert_eq!(running.worker_id.as_deref(), Some(worker.as_str()));
        assert!(running.lease_until_ms.is_some());
        assert_eq!(running.payload.value, serde_json::json!({"id": "r1"}));
        assert!(running.result.is_none());
        assert_eq!(running.steps_total, 2);
        let names: Vec<_> = running.steps.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names.len(), 2);
        let charge = running.steps.iter().find(|s| s.name == "charge").unwrap();
        assert_eq!(charge.result.value, serde_json::json!({"receipt": 7}));
        let huge = running.steps.iter().find(|s| s.name == "huge").unwrap();
        assert!(huge.result.truncated);
        assert_eq!(huge.result.bytes, 10_002);
        assert_eq!(
            huge.result.preview.as_ref().unwrap().len(),
            tako_core::WORKFLOW_STEP_VALUE_MAX_BYTES
        );

        db.wait_for_event("r1", &worker, "approval", "approved", None)
            .unwrap();
        let parked = run(db, "r1");
        assert_eq!((parked.status, parked.attempts), (Pending, 0));
        assert_eq!(parked.run_at_ms, None, "parked until a signal");
        assert_eq!(parked.worker_id, None);
        assert_eq!(
            parked.waits,
            vec![tako_core::WorkflowRunWait {
                step_name: "approval".into(),
                event_name: "approved".into(),
                expires_at_ms: None,
            }]
        );
        let (summaries, _) = db.list_runs(None, None, 1).unwrap();
        assert_eq!(summaries[0].run_at_ms, None);

        db.signal("approved", &serde_json::json!(true)).unwrap();
        let worker = claim(db, "work", "w").worker_id;
        db.complete("r1", &worker, &serde_json::json!({"ok": true}))
            .unwrap();
        let done = run(db, "r1");
        assert_eq!(done.status, Succeeded);
        assert_eq!(done.result.unwrap().value, serde_json::json!({"ok": true}));
        assert!(done.waits.is_empty() && done.finished_at_ms.is_some());
        assert_eq!(done.steps_total, 3);
    });
}

#[test]
fn retry_resumes_a_dead_run_after_its_saved_steps_with_a_fresh_budget() {
    each_store("operator_retry", |store| {
        let db = &store.db;
        enqueue(db, "r1", "work", 2);
        // The first attempt saves a step and fails; the second is abandoned
        // by a lost worker on its last attempt after saving another.
        let first = claim(db, "work", "first").worker_id;
        db.save_step("r1", &first, "charge", &serde_json::json!("receipt"))
            .unwrap();
        db.fail("r1", &first, "timeout", Some(0), false).unwrap();
        let second = claim(db, "work", "second").worker_id;
        db.save_step("r1", &second, "ship", &serde_json::json!("label"))
            .unwrap();
        store.expire_lease("r1");
        assert_eq!(store.db.reclaim_expired().unwrap(), 1);
        let db = &store.db;
        assert_eq!(run(db, "r1").status, Dead);

        assert_eq!(db.retry_run("r1").unwrap(), Dead);
        let retried = run(db, "r1");
        assert_eq!((retried.status, retried.attempts), (Pending, 0));
        assert_eq!(retried.max_attempts, 2);
        assert!(retried.run_at_ms.unwrap() <= now_ms());
        assert_eq!(retried.finished_at_ms, None);
        assert_eq!((retried.worker_id, retried.lease_until_ms), (None, None));
        assert_eq!(retried.steps_total, 2);
        assert!(db.has_runnable_work().unwrap());

        // It is claimed like any due run and resumes after its saved steps.
        let third = claim(db, "work", "third");
        assert_eq!((third.id.as_str(), third.attempts), ("r1", 1));
        assert_eq!(third.step_state["charge"], "receipt");
        assert_eq!(third.step_state["ship"], "label");
        // Earlier owners stay fenced.
        assert!(matches!(
            db.complete("r1", &second, &serde_json::Value::Null),
            Err(RunsDbError::StaleWorker)
        ));
        db.complete("r1", &third.worker_id, &serde_json::json!(1))
            .unwrap();

        let error = db.retry_run("r1").unwrap_err();
        assert!(matches!(
            error,
            RunsDbError::RunNotRetryable {
                status: Succeeded,
                ..
            }
        ));
        assert_eq!(
            error.to_string(),
            "Workflow run r1 is succeeded; only dead or cancelled runs can be retried"
        );
        enqueue(db, "r2", "work", 3);
        assert!(matches!(
            db.retry_run("r2"),
            Err(RunsDbError::RunNotRetryable {
                status: Pending,
                ..
            })
        ));
        claim(db, "work", "w");
        assert!(matches!(
            db.retry_run("r2"),
            Err(RunsDbError::RunNotRetryable {
                status: Running,
                ..
            })
        ));
        assert_eq!(
            db.retry_run("missing").unwrap_err().to_string(),
            "Workflow run not found: missing"
        );
    });
}

#[test]
fn cancel_fences_a_running_attempt_for_good() {
    each_store("operator_cancel_running", |store| {
        enqueue(&store.db, "r1", "work", 3);
        let worker = claim(&store.db, "work", "w").worker_id;
        assert_eq!(
            store.db.cancel_run("r1").unwrap(),
            CancelledRun {
                previous_status: Running,
                worker_id: Some(worker.clone()),
            }
        );
        let cancelled = run(&store.db, "r1");
        assert_eq!(cancelled.status, Cancelled);
        assert_eq!(
            cancelled.last_error.as_deref(),
            Some(OPERATOR_CANCEL_REASON)
        );
        assert!(cancelled.finished_at_ms.is_some());
        assert_eq!(
            (cancelled.worker_id, cancelled.lease_until_ms),
            (None, None)
        );

        // Every write of the fenced attempt is rejected like a stale owner's.
        let db = &store.db;
        let stale = [
            db.heartbeat("r1", &worker, 60_000),
            db.save_step("r1", &worker, "late", &serde_json::json!(1)),
            db.complete("r1", &worker, &serde_json::Value::Null),
            db.fail("r1", &worker, "boom", Some(0), false),
            db.fail("r1", &worker, "boom", None, true),
            db.defer("r1", &worker, Some(0)),
            db.wait_for_event("r1", &worker, "approval", "approved", None),
            db.cancel("r1", &worker, Some("bail")),
        ];
        for result in stale {
            assert!(
                matches!(result, Err(RunsDbError::StaleWorker)),
                "{result:?}"
            );
        }
        assert_eq!(run(db, "r1").steps_total, 0);
        assert!(db.in_flight_by_worker().unwrap().is_empty());

        // Neither reclaim nor claim resurrects it.
        store.expire_lease("r1");
        assert_eq!(store.db.reclaim_expired().unwrap(), 0);
        let db = &store.db;
        assert!(
            db.claim("next", &["work".into()], 60_000)
                .unwrap()
                .is_none()
        );
        assert_eq!(run(db, "r1").status, Cancelled);

        let error = db.cancel_run("r1").unwrap_err();
        assert!(matches!(
            error,
            RunsDbError::RunAlreadyFinished {
                status: Cancelled,
                ..
            }
        ));
        assert_eq!(
            error.to_string(),
            "Workflow run r1 already finished as cancelled; only pending or running runs can be cancelled"
        );
        assert_eq!(
            db.cancel_run("missing").unwrap_err().to_string(),
            "Workflow run not found: missing"
        );

        // A cancelled run can be retried and then claimed normally.
        assert_eq!(db.retry_run("r1").unwrap(), Cancelled);
        let next = claim(db, "work", "next");
        assert_eq!(next.attempts, 1);
        db.complete("r1", &next.worker_id, &serde_json::Value::Null)
            .unwrap();
        assert!(matches!(
            db.cancel_run("r1"),
            Err(RunsDbError::RunAlreadyFinished {
                status: Succeeded,
                ..
            })
        ));
    });
}

#[test]
fn cancel_removes_parked_waits_and_retry_restarts_parked_runs() {
    each_store("operator_cancel_parked", |store| {
        let db = &store.db;
        // Pending, never claimed.
        enqueue(db, "fresh", "work", 3);
        assert_eq!(
            db.cancel_run("fresh").unwrap(),
            CancelledRun {
                previous_status: Pending,
                worker_id: None,
            }
        );
        assert!(db.claim("w", &["work".into()], 60_000).unwrap().is_none());

        // Parked on an event: the waiter goes with the cancel, so a later
        // signal neither wakes the run nor writes its step.
        enqueue(db, "waiting", "work", 3);
        let worker = claim(db, "work", "w").worker_id;
        db.save_step("waiting", &worker, "before", &serde_json::json!(1))
            .unwrap();
        db.wait_for_event(
            "waiting",
            &worker,
            "approval",
            "approved",
            Some(now_ms() + 60_000),
        )
        .unwrap();
        db.cancel_run("waiting").unwrap();
        assert!(run(db, "waiting").waits.is_empty());
        assert_eq!(db.signal("approved", &serde_json::json!(true)).unwrap(), 0);
        let cancelled = run(db, "waiting");
        assert_eq!((cancelled.status, cancelled.steps_total), (Cancelled, 1));

        // Parked in a durable sleep: retry makes it due now; the SDK's
        // sleep marker decides whether it parks again.
        enqueue(db, "sleeping", "work", 3);
        let worker = claim(db, "work", "w").worker_id;
        let wake_at = now_ms() + 3_600_000;
        db.save_step(
            "sleeping",
            &worker,
            "__sleep:nap",
            &serde_json::json!({"wakeAt": wake_at}),
        )
        .unwrap();
        db.defer("sleeping", &worker, Some(wake_at)).unwrap();
        assert!(db.claim("w", &["work".into()], 60_000).unwrap().is_none());
        db.cancel_run("sleeping").unwrap();

        db.retry_run("waiting").unwrap();
        db.retry_run("sleeping").unwrap();
        let mut resumed = [claim(db, "work", "w"), claim(db, "work", "w")];
        resumed.sort_by(|a, b| a.id.cmp(&b.id));
        let [sleeping, waiting] = resumed;
        assert_eq!(sleeping.id, "sleeping");
        assert_eq!(sleeping.step_state["__sleep:nap"]["wakeAt"], wake_at);
        assert_eq!(waiting.id, "waiting");
        assert_eq!(waiting.step_state, serde_json::json!({"before": 1}));
        // The resumed wait parks again with a fresh waiter.
        db.wait_for_event("waiting", &waiting.worker_id, "approval", "approved", None)
            .unwrap();
        assert_eq!(db.signal("approved", &serde_json::json!(true)).unwrap(), 1);
    });
}

/// JSON escapes a control character to six bytes, so byte caps alone do not
/// bound a reply. Lists and runs stay inside the 1 MiB frame anyway.
#[test]
fn replies_fit_the_frame_with_escape_heavy_errors_and_long_names() {
    each_store("operator_budget", |store| {
        let db = &store.db;
        let control = |bytes: usize| "\u{1}".repeat(bytes);
        let (name, worker) = (control(4096), control(4096));
        for index in 0..200 {
            enqueue(db, &format!("r{index}"), &name, 1);
        }
        for _ in 0..200 {
            let claimed = claim(db, &name, &worker);
            db.fail(&claimed.id, &claimed.worker_id, &control(4096), None, true)
                .unwrap();
        }
        let (runs, has_more) = db.list_runs(None, None, 200).unwrap();
        let size = serde_json::to_vec(&runs).unwrap().len();
        assert!(size <= 768 * 1024, "{size}");
        assert!(has_more && !runs.is_empty() && runs.len() < 200);
        assert!(runs.iter().all(|run| run.name.len() == 256));
        assert!(runs.iter().all(|run| run.last_error_truncated));

        enqueue(db, "parked", &name, 3);
        let claimed = claim(db, &name, &worker);
        let running = run(db, "parked");
        assert_eq!(running.name.len(), 256);
        assert_eq!(running.worker_id.unwrap().len(), 256);
        db.wait_for_event(
            "parked",
            &claimed.worker_id,
            &control(4096),
            &control(4096),
            None,
        )
        .unwrap();
        let parked = run(db, "parked");
        assert_eq!(parked.waits.len(), 1);
        assert_eq!(parked.waits[0].step_name.len(), 256);
        assert_eq!(parked.waits[0].event_name.len(), 256);
    });
}
