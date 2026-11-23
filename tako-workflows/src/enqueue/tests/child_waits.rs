//! A run waiting on another run's outcome (a parent on its child): woken by
//! every transition that finishes the child, never missed in a race. Every
//! contract runs against SQLite, the hosted store and, when
//! `TAKO_TEST_POSTGRES_URL` is set, Postgres.

use super::*;
use tako_core::ErrorCode;

type Open<'a> = &'a dyn Fn() -> RunsDb;

macro_rules! every_store {
    ($($name:ident),* $(,)?) => {
        mod sqlite_store {
            use super::RunsDb;
            $(
                #[test]
                fn $name() {
                    let dir = tempfile::tempdir().unwrap();
                    let path = dir.path().join("runs.sqlite");
                    super::$name(&|| RunsDb::open(&path).unwrap());
                }
            )*
        }
        mod hosted_store {
            $(
                #[test]
                fn $name() {
                    let fixture = crate::enqueue::HostedFixture::start();
                    super::$name(&|| fixture.open("app"));
                }
            )*
        }
        mod postgres_store {
            use super::RunsDb;
            $(
                #[test]
                fn $name() {
                    let Ok(url) = std::env::var("TAKO_TEST_POSTGRES_URL") else {
                        return;
                    };
                    let app = format!("child-wait-test/{}", nanoid::nanoid!());
                    super::$name(&|| RunsDb::open_postgres(&url, &app).unwrap());
                }
            )*
        }
    };
}

every_store!(
    a_parent_is_woken_with_the_outcome_when_its_child_succeeds,
    every_finishing_transition_wakes_the_parent,
    a_child_retry_does_not_wake_the_parent,
    a_finished_missing_or_foreign_child_satisfies_the_wait_at_once,
    an_expired_wait_saves_null,
    a_child_finishing_while_the_parent_parks_always_wakes_it,
    a_child_finishing_after_the_deadline_leaves_the_wait_timed_out,
    a_child_that_finished_after_the_deadline_times_out_a_late_registered_wait,
);

const PARENT: &str = "parent";
const CHILD: &str = "child";

fn enqueue(db: &RunsDb, name: &str, id: &str, max_attempts: Option<u32>) -> String {
    let opts = EnqueueOpts {
        id: Some(id.into()),
        max_attempts,
        ..Default::default()
    };
    db.enqueue(name, &serde_json::json!({}), &opts).unwrap().id
}

fn claim(db: &RunsDb, name: &str) -> Option<RunPayload> {
    db.claim("worker", &[name.into()], 60_000).unwrap()
}

/// Enqueue a child and a parent, and park the parent on the child.
fn park_parent(db: &RunsDb, suffix: &str, child_attempts: Option<u32>) -> (String, String) {
    let child = enqueue(db, CHILD, &format!("child-{suffix}"), child_attempts);
    let parent = enqueue(db, PARENT, &format!("parent-{suffix}"), None);
    let run = claim(db, PARENT).unwrap();
    assert!(
        !db.wait_for_run(&parent, &run.worker_id, "outcome", CHILD, &child, None)
            .unwrap()
    );
    assert!(claim(db, PARENT).is_none(), "the parent is parked");
    (parent, child)
}

fn outcome(db: &RunsDb) -> serde_json::Value {
    let run = claim(db, PARENT).expect("the parent is due");
    run.step_state["outcome"].clone()
}

fn a_parent_is_woken_with_the_outcome_when_its_child_succeeds(open: Open) {
    let db = open();
    let (parent, child) = park_parent(&db, "ok", None);
    let waits = db.get_run(&parent).unwrap().unwrap().waits;
    assert_eq!(waits.len(), 1);
    assert_eq!(waits[0].event_name, format!("__run:{child}"));

    let run = claim(&db, CHILD).unwrap();
    db.complete(&child, &run.worker_id, &serde_json::json!({"sent": true}))
        .unwrap();
    assert_eq!(
        outcome(&db),
        serde_json::json!({"status": "succeeded", "result": {"sent": true}, "error": null})
    );
    assert!(db.get_run(&parent).unwrap().unwrap().waits.is_empty());
}

fn every_finishing_transition_wakes_the_parent(open: Open) {
    let db = open();

    let (_, child) = park_parent(&db, "bail", None);
    let run = claim(&db, CHILD).unwrap();
    db.cancel(&child, &run.worker_id, Some("not needed"))
        .unwrap();
    let woken = outcome(&db);
    assert_eq!(woken["status"], "cancelled");
    assert_eq!(woken["error"], "not needed");

    let (_, child) = park_parent(&db, "dead", None);
    let run = claim(&db, CHILD).unwrap();
    db.fail(&child, &run.worker_id, "boom", None, true).unwrap();
    let woken = outcome(&db);
    assert_eq!(
        (woken["status"].as_str(), woken["error"].as_str()),
        (Some("dead"), Some("boom"))
    );

    let (_, child) = park_parent(&db, "operator", None);
    db.cancel_run(&child).unwrap();
    let woken = outcome(&db);
    assert_eq!(woken["status"], "cancelled");
    assert_eq!(woken["error"], OPERATOR_CANCEL_REASON);

    // A child whose worker is lost on its last attempt ends dead at reclaim.
    park_parent(&db, "lost", Some(1));
    db.claim("worker", &[CHILD.into()], 1).unwrap().unwrap();
    std::thread::sleep(std::time::Duration::from_millis(20));
    db.reclaim_expired().unwrap();
    let woken = outcome(&db);
    assert_eq!(woken["status"], "dead");
    assert_eq!(woken["error"], WORKER_LOST_ERROR);
}

/// The parent claimed by `outcome` is still running; give back its owner.
fn claim_owner(db: &RunsDb, id: &str) -> String {
    db.get_run(id)
        .unwrap()
        .unwrap()
        .worker_id
        .unwrap_or_default()
}

fn a_child_retry_does_not_wake_the_parent(open: Open) {
    let db = open();
    let (parent, child) = park_parent(&db, "retry", None);
    let run = claim(&db, CHILD).unwrap();
    db.fail(
        &child,
        &run.worker_id,
        "flaky",
        Some(now_ms() + 60_000),
        false,
    )
    .unwrap();
    assert!(claim(&db, PARENT).is_none());
    assert_eq!(db.get_run(&parent).unwrap().unwrap().waits.len(), 1);
}

fn a_finished_missing_or_foreign_child_satisfies_the_wait_at_once(open: Open) {
    let db = open();
    let child = enqueue(&db, CHILD, "done-child", None);
    let run = claim(&db, CHILD).unwrap();
    db.complete(&child, &run.worker_id, &serde_json::json!(7))
        .unwrap();

    for (index, (name, target)) in [
        (CHILD, child.as_str()),
        (CHILD, "no-such-run"),
        ("other-workflow", child.as_str()),
    ]
    .into_iter()
    .enumerate()
    {
        let parent = enqueue(&db, PARENT, &format!("instant-{index}"), None);
        let run = claim(&db, PARENT).unwrap();
        assert!(
            db.wait_for_run(&parent, &run.worker_id, "outcome", name, target, None)
                .unwrap()
        );
        let woken = outcome(&db);
        if index == 0 {
            assert_eq!(
                woken,
                serde_json::json!({"status": "succeeded", "result": 7, "error": null})
            );
        } else {
            assert_eq!(woken["status"], "missing", "{name} {target}");
        }
        db.complete(
            &parent,
            &claim_owner(&db, &parent),
            &serde_json::Value::Null,
        )
        .unwrap();
    }

    let parent = enqueue(&db, PARENT, "bad-target", None);
    let run = claim(&db, PARENT).unwrap();
    let error = db
        .wait_for_run(&parent, &run.worker_id, "outcome", CHILD, "", None)
        .unwrap_err();
    assert_eq!(error.socket_code(), Some(ErrorCode::InvalidRequest));
}

fn an_expired_wait_saves_null(open: Open) {
    let db = open();
    let child = enqueue(&db, CHILD, "slow-child", None);
    let parent = enqueue(&db, PARENT, "impatient", None);
    let run = claim(&db, PARENT).unwrap();
    assert!(
        !db.wait_for_run(
            &parent,
            &run.worker_id,
            "outcome",
            CHILD,
            &child,
            Some(now_ms() - 1)
        )
        .unwrap()
    );
    assert_eq!(outcome(&db), serde_json::Value::Null);
    // The child finishing later finds no waiter and changes nothing.
    let run = claim(&db, CHILD).unwrap();
    db.complete(&child, &run.worker_id, &serde_json::Value::Null)
        .unwrap();
}

/// A parent parking and its child finishing on separate connections, in
/// either order: the parent must end up due with the child's outcome.
fn a_child_finishing_while_the_parent_parks_always_wakes_it(open: Open) {
    let parent_db = open();
    let child_db = open();
    for round in 0..40 {
        let child = enqueue(&child_db, CHILD, &format!("race-child-{round}"), None);
        let parent = enqueue(&parent_db, PARENT, &format!("race-parent-{round}"), None);
        let parent_run = claim(&parent_db, PARENT).unwrap();
        let child_run = claim(&child_db, CHILD).unwrap();
        std::thread::scope(|scope| {
            let wait = scope.spawn(|| {
                parent_db
                    .wait_for_run(
                        &parent,
                        &parent_run.worker_id,
                        "outcome",
                        CHILD,
                        &child,
                        None,
                    )
                    .unwrap()
            });
            let finish = scope.spawn(|| {
                child_db
                    .complete(&child, &child_run.worker_id, &serde_json::json!(round))
                    .unwrap()
            });
            wait.join().unwrap();
            finish.join().unwrap();
        });
        let run =
            claim(&parent_db, PARENT).unwrap_or_else(|| panic!("round {round}: parent not due"));
        assert_eq!(run.id, parent);
        assert_eq!(
            run.step_state["outcome"]["result"],
            serde_json::json!(round)
        );
        parent_db
            .complete(&parent, &run.worker_id, &serde_json::Value::Null)
            .unwrap();
    }
}

/// A wait past its deadline times out even when the child finishes before
/// the parent is claimed: the outcome may not depend on claim latency.
fn a_child_finishing_after_the_deadline_leaves_the_wait_timed_out(open: Open) {
    let db = open();
    let child = enqueue(&db, CHILD, "late-child", None);
    let parent = enqueue(&db, PARENT, "late-parent", None);
    let run = claim(&db, PARENT).unwrap();
    db.wait_for_run(
        &parent,
        &run.worker_id,
        "outcome",
        CHILD,
        &child,
        Some(now_ms() + 20),
    )
    .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(40));
    let run = claim(&db, CHILD).unwrap();
    db.complete(&child, &run.worker_id, &serde_json::json!("late"))
        .unwrap();
    assert_eq!(outcome(&db), serde_json::Value::Null);
}

/// The parent registers its wait after its deadline (a worker still
/// finishing sibling steps): a child that finished in time gives its
/// outcome, one that finished after the deadline leaves the wait timed out.
fn a_child_that_finished_after_the_deadline_times_out_a_late_registered_wait(open: Open) {
    let db = open();
    let late = enqueue(&db, CHILD, "finished-late", None);
    let run = claim(&db, CHILD).unwrap();
    let deadline = now_ms() - 1_000;
    db.complete(&late, &run.worker_id, &serde_json::json!("late"))
        .unwrap();
    let parent = enqueue(&db, PARENT, "registers-late", None);
    let run = claim(&db, PARENT).unwrap();
    assert!(
        db.wait_for_run(
            &parent,
            &run.worker_id,
            "outcome",
            CHILD,
            &late,
            Some(deadline)
        )
        .unwrap()
    );
    assert_eq!(outcome(&db), serde_json::Value::Null);

    let in_time = enqueue(&db, CHILD, "finished-in-time", None);
    let run = claim(&db, CHILD).unwrap();
    db.complete(&in_time, &run.worker_id, &serde_json::json!("ok"))
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(5));
    let deadline = now_ms();
    std::thread::sleep(std::time::Duration::from_millis(5));
    let parent = enqueue(&db, PARENT, "registers-late-2", None);
    let run = claim(&db, PARENT).unwrap();
    assert!(
        db.wait_for_run(
            &parent,
            &run.worker_id,
            "outcome",
            CHILD,
            &in_time,
            Some(deadline)
        )
        .unwrap()
    );
    assert_eq!(outcome(&db)["result"], "ok");
}
