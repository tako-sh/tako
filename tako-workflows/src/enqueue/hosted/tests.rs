//! Hosted-store behavior beyond the backend-generic contracts in
//! `enqueue::tests`: apps sharing a database, clocks, fencing and errors.

use serde_json::json;
use tako_core::{EnqueueOpts, ErrorCode, ScheduleSpec, SignalRunStatus};

use super::testing::HostedFixture;
use crate::enqueue::{RunsDb, RunsDbError, now_ms};

fn identified(id: &str) -> EnqueueOpts {
    EnqueueOpts {
        id: Some(id.into()),
        ..Default::default()
    }
}

fn claim(db: &RunsDb, worker: &str) -> tako_core::RunPayload {
    db.claim(worker, &["work".into()], 60_000).unwrap().unwrap()
}

fn status(db: &RunsDb, id: &str) -> String {
    db.result("work", id).unwrap().unwrap().status
}

fn schedule(name: &str) -> ScheduleSpec {
    ScheduleSpec {
        name: name.into(),
        cron: "* * * * * *".into(),
        max_attempts: None,
    }
}

fn schedule_names(db: &RunsDb) -> Vec<String> {
    db.list_schedules()
        .unwrap()
        .into_iter()
        .map(|row| row.name)
        .collect()
}

/// A user may point several apps at one database. Each app's runs, waits,
/// signals and schedules stay its own, even under the same run IDs.
#[test]
fn apps_sharing_a_database_never_see_each_others_runs_or_schedules() {
    let fixture = HostedFixture::start();
    let shop = fixture.open("shop/production");
    let blog = fixture.open("blog/production");
    let order = identified("order-1");
    assert!(
        !shop
            .enqueue("work", &json!({"app": "shop"}), &order)
            .unwrap()
            .deduplicated
    );
    // The same ID with another payload is another app's run, not a conflict.
    assert!(
        !blog
            .enqueue("work", &json!({"app": "blog"}), &order)
            .unwrap()
            .deduplicated
    );
    blog.enqueue("other", &json!({}), &identified("blog-only"))
        .unwrap();

    let run = claim(&shop, "shop-worker");
    assert_eq!(run.payload, json!({"app": "shop"}));
    assert_eq!(
        (shop.pending_count().unwrap(), blog.pending_count().unwrap()),
        (0, 2)
    );
    assert!(!shop.has_runnable_work().unwrap());
    assert!(blog.has_runnable_work().unwrap());
    assert!(blog.in_flight_by_worker().unwrap().is_empty());
    assert_eq!(shop.in_flight_by_worker().unwrap().len(), 1);
    shop.save_step("order-1", &run.worker_id, "charge", &json!(1))
        .unwrap();
    // Another app cannot write the run, even with its owner's claim.
    for result in [
        blog.save_step("order-1", &run.worker_id, "forged", &json!(0)),
        blog.heartbeat("order-1", &run.worker_id, 60_000),
        blog.complete("order-1", &run.worker_id, &json!(0)),
        blog.wait_for_event("order-1", &run.worker_id, "approval", "approval", None)
            .map(drop),
    ] {
        assert!(
            matches!(result, Err(RunsDbError::StaleWorker)),
            "{result:?}"
        );
    }
    assert_eq!(blog.get_run("order-1").unwrap().unwrap().steps_total, 0);
    assert_eq!(shop.get_run("order-1").unwrap().unwrap().steps_total, 1);

    // Signals reach only their own app's runs.
    assert_eq!(
        blog.signal_run("order-1", "approval", &json!("blog"))
            .unwrap(),
        SignalRunStatus::Buffered
    );
    assert!(
        !shop
            .wait_for_event("order-1", &run.worker_id, "approval", "approval", None)
            .unwrap(),
        "another app's buffered signal is not consumed"
    );
    assert_eq!(blog.signal("approval", &json!("blog")).unwrap(), 0);
    assert_eq!(shop.signal("approval", &json!("shop")).unwrap(), 1);
    let resumed = claim(&shop, "shop-worker");
    assert_eq!(resumed.step_state["approval"], "shop");
    assert_eq!(blog.get_run("order-1").unwrap().unwrap().signals_total, 1);

    // A wait on another app's run sees it as missing.
    let child = claim(&blog, "blog-worker");
    assert!(
        shop.wait_for_run(
            "order-1",
            &resumed.worker_id,
            "child",
            "other",
            "blog-only",
            None
        )
        .unwrap()
    );
    claim(&shop, "shop-worker");
    let parent = shop.get_run("order-1").unwrap().unwrap();
    let child_step = parent
        .steps
        .iter()
        .find(|step| step.name == "child")
        .unwrap();
    assert_eq!(child_step.result.value["status"], "missing");
    assert_eq!(shop.list_runs(None, None, 10).unwrap().0.len(), 1);
    assert_eq!(blog.list_runs(None, None, 10).unwrap().0.len(), 2);

    // Reclaim and operator actions stay within the app.
    shop.test_execute(
        "UPDATE {runs} SET lease_until = 0 WHERE id = ?1",
        vec!["order-1".into()],
    );
    assert_eq!(
        blog.reclaim_expired_with_workers().unwrap(),
        vec![child.worker_id]
    );
    assert_eq!(status(&shop, "order-1"), "running");
    assert_eq!(
        blog.cancel_run("order-1").unwrap().previous_status,
        tako_core::WorkflowRunStatus::Pending
    );
    assert_eq!(status(&shop, "order-1"), "running");
    assert_eq!(shop.reclaim_expired().unwrap(), 1);
    assert_eq!(status(&shop, "order-1"), "pending");
    let run = claim(&shop, "shop-worker");
    shop.complete("order-1", &run.worker_id, &json!("done"))
        .unwrap();
    assert_eq!(
        blog.prune_finished_before(now_ms() + 1_000, 100).unwrap(),
        1
    );
    assert!(blog.get_run("order-1").unwrap().is_none());
    assert_eq!(
        shop.result("work", "order-1").unwrap().unwrap().result,
        json!("done")
    );
    assert_eq!(shop.get_run("order-1").unwrap().unwrap().steps_total, 3);

    // Schedules: each app replaces only its own set.
    crate::cron::register_schedules(&shop, &[schedule("nightly")]).unwrap();
    crate::cron::register_schedules(&blog, &[schedule("digest")]).unwrap();
    assert_eq!(schedule_names(&shop), ["nightly"]);
    assert_eq!(schedule_names(&blog), ["digest"]);
    let digest_last_run = blog.list_schedules().unwrap()[0].last_run_at;
    assert!(digest_last_run.is_some_and(|at| at > 1));
    shop.set_schedule_last_run_at("digest", 1).unwrap();
    assert_eq!(
        blog.list_schedules().unwrap()[0].last_run_at,
        digest_last_run
    );
    crate::cron::register_schedules(&shop, &[]).unwrap();
    assert!(schedule_names(&shop).is_empty());
    assert_eq!(schedule_names(&blog), ["digest"]);
}

const SKEW_MS: i64 = 60 * 60 * 1000;
const LEASE_MS: u64 = 60_000;

fn fast_clock() -> i64 {
    now_ms() + SKEW_MS
}

fn slow_clock() -> i64 {
    now_ms() - SKEW_MS
}

/// Milliseconds left on a run's lease, measured by the database clock.
fn remaining_lease(db: &RunsDb, id: &str) -> i64 {
    db.test_query_row(
        "SELECT lease_until - CAST(unixepoch('subsec') * 1000 AS INTEGER)
         FROM {runs} WHERE app = 'app' AND id = ?1",
        vec![id.into()],
    )[0]
    .as_i64()
    .unwrap()
}

/// Three servers share one store. One clock runs an hour ahead of the
/// database and one an hour behind. Leases written and judged by those
/// clocks would be reclaimed an hour early (duplicating steps) or an hour
/// late (stalling runs whose worker died). Due times stay on the server
/// clock that scheduled them.
#[test]
fn leases_follow_the_database_clock_on_servers_with_skewed_clocks() {
    let fixture = HostedFixture::start();
    let accurate = fixture.open("app");
    let fast = fixture.open_with_clock("app", fast_clock);
    let slow = fixture.open_with_clock("app", slow_clock);
    let full_lease = (LEASE_MS as i64 - 5_000)..=LEASE_MS as i64;

    // Scheduling uses the enqueuing server's clock.
    let early = fast
        .enqueue("later", &json!({}), &EnqueueOpts::default())
        .unwrap()
        .id;
    assert!(!accurate.has_runnable_work().unwrap());
    assert!(fast.has_runnable_work().unwrap());
    accurate.cancel_run(&early).unwrap();

    // Due on every server's own clock: scheduling is not what is tested.
    let due = EnqueueOpts {
        run_at_ms: Some(0),
        ..Default::default()
    };
    for (name, server) in [("fast", &fast), ("slow", &slow)] {
        let id = accurate.enqueue("work", &json!({}), &due).unwrap().id;
        let run = server
            .claim(name, &["work".into()], LEASE_MS)
            .unwrap()
            .unwrap();
        assert_eq!(run.id, id);
        let remaining = remaining_lease(&accurate, &id);
        assert!(
            full_lease.contains(&remaining),
            "{name} server claimed with {remaining}ms of lease"
        );
        // No server takes the run from a worker that still holds its lease.
        for reclaimer in [&accurate, &fast, &slow] {
            assert_eq!(reclaimer.reclaim_expired().unwrap(), 0, "{name}");
        }

        // Shorten the lease, then renew it through the skewed server.
        accurate.test_execute(
            "UPDATE {runs} SET lease_until = lease_until - 50000 WHERE id = ?1",
            vec![id.as_str().into()],
        );
        server.heartbeat(&id, name, LEASE_MS).unwrap();
        let remaining = remaining_lease(&accurate, &id);
        assert!(
            full_lease.contains(&remaining),
            "{name} server renewed to {remaining}ms of lease"
        );
        for reclaimer in [&accurate, &fast, &slow] {
            assert_eq!(reclaimer.reclaim_expired().unwrap(), 0, "{name}");
        }

        // Once the database clock passes the lease, every server reclaims it.
        accurate.test_execute(
            "UPDATE {runs} SET lease_until = CAST(unixepoch('subsec') * 1000 AS INTEGER) - 1
             WHERE id = ?1",
            vec![id.as_str().into()],
        );
        assert_eq!(server.reclaim_expired_with_workers().unwrap(), vec![name]);
        assert!(matches!(
            accurate.heartbeat(&id, name, LEASE_MS),
            Err(RunsDbError::StaleWorker)
        ));
        let run = accurate
            .claim("next", &["work".into()], LEASE_MS)
            .unwrap()
            .unwrap();
        assert_eq!((run.id, run.attempts), (id.clone(), 2));
        accurate.complete(&id, "next", &json!(null)).unwrap();
    }
}

/// A worker whose lease expired and whose run another server reclaimed (and
/// another worker claimed) is fenced: none of its writes land.
#[test]
fn a_worker_whose_run_was_reclaimed_can_no_longer_write_it() {
    let fixture = HostedFixture::start();
    let first = fixture.open("app");
    let second = fixture.open("app");
    let id = first
        .enqueue("work", &json!({}), &identified("run-1"))
        .unwrap()
        .id;
    let old = claim(&first, "old");
    first
        .save_step(&id, &old.worker_id, "checkpoint", &json!(1))
        .unwrap();
    first.test_execute(
        "UPDATE {runs} SET lease_until = 0 WHERE id = ?1",
        vec![id.as_str().into()],
    );
    assert_eq!(
        second.reclaim_expired_with_workers().unwrap(),
        vec![old.worker_id.clone()]
    );

    let stale = |db: &RunsDb| {
        [
            db.heartbeat(&id, &old.worker_id, 60_000),
            db.save_step(&id, &old.worker_id, "late", &json!(2)),
            db.complete(&id, &old.worker_id, &json!(null)),
            db.fail(&id, &old.worker_id, "late", Some(0), false),
            db.fail(&id, &old.worker_id, "late", None, true),
            db.cancel(&id, &old.worker_id, Some("late")),
            db.defer(&id, &old.worker_id, Some(0)),
            db.wait_for_event(&id, &old.worker_id, "approval", "approval", None)
                .map(drop),
            db.wait_for_run(&id, &old.worker_id, "child", "work", "other", None)
                .map(drop),
        ]
    };
    // Reclaimed but not yet claimed again, then owned by a new worker.
    for result in stale(&first) {
        assert!(
            matches!(result, Err(RunsDbError::StaleWorker)),
            "{result:?}"
        );
    }
    let new = claim(&second, "new");
    assert_eq!(new.attempts, 2);
    assert_eq!(new.step_state, json!({"checkpoint": 1}));
    for result in stale(&first) {
        assert!(
            matches!(result, Err(RunsDbError::StaleWorker)),
            "{result:?}"
        );
    }
    let detail = first.get_run(&id).unwrap().unwrap();
    assert_eq!(detail.steps_total, 1);
    assert!(detail.waits.is_empty());
    assert_eq!(detail.worker_id.as_deref(), Some(new.worker_id.as_str()));
    second.heartbeat(&id, &new.worker_id, 60_000).unwrap();
    second.complete(&id, &new.worker_id, &json!("ok")).unwrap();
    assert_eq!(status(&first, &id), "succeeded");
}

/// A failing statement rolls back its whole operation, and errors carry
/// SQLite's error name, never SQL or app data. An unreachable database is
/// retryable.
#[test]
fn failures_roll_back_whole_operations_without_leaking_app_data() {
    let fixture = HostedFixture::start();
    let db = fixture.open("app");
    let id = db
        .enqueue("work", &json!({}), &EnqueueOpts::default())
        .unwrap()
        .id;
    let run = claim(&db, "first");
    db.wait_for_event(&id, &run.worker_id, "approval", "event", Some(0))
        .unwrap();
    db.test_execute(
        "CREATE TRIGGER reject_checkpoint BEFORE INSERT ON {steps}
         BEGIN SELECT RAISE(ABORT, 'secret-app-data'); END",
        vec![],
    );
    // The claim's timed-out wait cannot be saved: the claim itself is undone.
    let error = db.claim("failed", &["work".into()], 60_000).unwrap_err();
    assert!(matches!(error, RunsDbError::Hosted(_)), "{error:?}");
    assert!(!error.to_string().contains("secret"), "{error}");
    assert_eq!(error.socket_code(), None);
    assert_eq!(db.pending_count().unwrap(), 1);
    assert_eq!(
        db.test_query_row(
            "SELECT attempts, worker_id FROM {runs} WHERE id = ?1",
            vec![id.as_str().into()]
        ),
        vec![json!(0), json!(null)]
    );
    db.test_execute("DROP TRIGGER reject_checkpoint", vec![]);
    let run = claim(&db, "next");
    assert_eq!(run.attempts, 1);
    assert_eq!(run.step_state["approval"], json!(null));
    assert!(db.get_run(&id).unwrap().unwrap().waits.is_empty());

    drop(fixture);
    let error = db.heartbeat(&id, &run.worker_id, 60_000).unwrap_err();
    assert_eq!(error.socket_code(), Some(ErrorCode::Unavailable), "{error}");
}

#[test]
fn a_store_written_by_a_newer_schema_is_refused() {
    let fixture = HostedFixture::start();
    let db = fixture.open("app");
    db.test_execute(
        "UPDATE tako_workflow_meta SET value = 99 WHERE name = 'schema_version'",
        vec![],
    );
    match RunsDb::open_config(fixture.config("app")) {
        Err(RunsDbError::UnsupportedBackend(message)) => assert!(message.contains("99")),
        Err(error) => panic!("{error}"),
        Ok(_) => panic!("opened a newer store"),
    }
}

#[test]
fn config_debug_output_leaves_out_credentials() {
    let config = crate::WorkflowStoreConfig::hosted(
        "http://127.0.0.1:9845",
        "f".repeat(64),
        "shop/production",
    );
    let debug = format!("{config:?}");
    assert!(
        !debug.contains("ffff") && !debug.contains("9845"),
        "{debug}"
    );
    assert!(debug.contains("shop/production"), "{debug}");
}
