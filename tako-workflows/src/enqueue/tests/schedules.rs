//! Cron schedules carry the workflow's retry budget into the runs they create.

use super::reclaim::each_store;
use super::*;
use crate::cron::{register_schedules, tick_once};

fn schedule(name: &str, max_attempts: Option<u32>) -> ScheduleSpec {
    ScheduleSpec {
        name: name.into(),
        cron: "* * * * * *".into(),
        max_attempts,
    }
}

/// Budgets of a workflow's runs, smallest first.
fn budgets(db: &RunsDb, name: &str) -> Vec<u32> {
    let (runs, _) = db.list_runs(None, Some(name), 200).unwrap();
    let mut budgets: Vec<_> = runs.iter().map(|run| run.max_attempts).collect();
    budgets.sort();
    budgets
}

#[test]
fn scheduled_runs_use_the_registered_retry_budget() {
    each_store("schedule_budget", |store| {
        let db = &store.db;
        register_schedules(db, &[schedule("custom", Some(5)), schedule("plain", None)]).unwrap();
        let now = now_ms() + 60_000;
        assert_eq!(tick_once(db, now).unwrap(), 2);
        assert_eq!(budgets(db, "custom"), vec![5]);
        assert_eq!(budgets(db, "plain"), vec![3], "the server default");

        // A new release re-registers its schedules with the current budget.
        register_schedules(db, &[schedule("custom", Some(1)), schedule("plain", None)]).unwrap();
        assert_eq!(tick_once(db, now + 60_000).unwrap(), 2);
        assert_eq!(budgets(db, "custom"), vec![1, 5]);
        assert_eq!(budgets(db, "plain"), vec![3, 3]);
    });
}

#[test]
fn retired_workers_and_recovered_servers_cannot_replace_the_active_schedule_set() {
    use crate::manager::ScheduleActivation::{Deploy, Recover};
    each_store("schedule_release", |store| {
        store
            .db
            .prepare_schedule_registration(Some("v1".into()), Deploy)
            .unwrap();
        let register = |db: &RunsDb, build: &str, name: &str| {
            db.register_worker_schedules(Some(build), &[schedule(name, None)])
                .unwrap();
        };
        let names = || {
            store
                .db
                .list_schedules()
                .unwrap()
                .into_iter()
                .map(|row| row.name)
                .collect::<Vec<_>>()
        };
        register(&store.db, "retired", "retired");
        assert!(store.db.needs_schedule_registration());
        assert!(names().is_empty());
        let invalid = ScheduleSpec {
            name: "bad".into(),
            cron: "invalid".into(),
            max_attempts: None,
        };
        assert!(
            store
                .db
                .register_worker_schedules(Some("v1"), &[invalid])
                .is_err()
        );
        assert!(store.db.needs_schedule_registration());
        register(&store.db, "v1", "original");
        assert_eq!(names(), ["original"]);
        assert!(!store.db.needs_schedule_registration());

        let Some(mut next) = store.other_app("app") else {
            return;
        };
        let original = store.db.schedule_revision();
        next.prepare_schedule_registration(Some("v2".into()), Deploy)
            .unwrap();
        let next_revision = next.schedule_revision();
        assert!(next_revision > original);

        // Crash after the local release committed, before the worker registered.
        drop(next);
        let mut recovered = store.other_app("app").unwrap();
        recovered
            .prepare_schedule_registration(Some("v2".into()), Recover(next_revision))
            .unwrap();
        register(&recovered, "v2", "replacement");
        assert_eq!(names(), ["replacement"]);

        for old_revision in [original, None] {
            let mut stale = store.other_app("app").unwrap();
            stale
                .prepare_schedule_registration(Some("v1".into()), Recover(old_revision))
                .unwrap();
            register(&stale, "v1", "obsolete");
            assert_eq!(names(), ["replacement"]);
        }
        register(&store.db, "v1", "obsolete");
        assert_eq!(names(), ["replacement"]);

        // A deliberate rollback gets a fresh revision and can remove every cron.
        let mut rollback = store.other_app("app").unwrap();
        rollback
            .prepare_schedule_registration(Some("v1".into()), Deploy)
            .unwrap();
        rollback.register_worker_schedules(Some("v1"), &[]).unwrap();
        assert!(names().is_empty());
    });
}

/// Schedules registered before budgets were stored keep firing.
fn assert_old_schedule_row_keeps_working(db: &RunsDb) {
    assert_eq!(tick_once(db, now_ms() + 60_000).unwrap(), 1);
    assert_eq!(budgets(db, "old"), vec![3]);
    register_schedules(db, &[schedule("old", Some(4))]).unwrap();
    assert_eq!(tick_once(db, now_ms() + 120_000).unwrap(), 1);
    assert_eq!(budgets(db, "old"), vec![3, 4]);
}

#[test]
fn sqlite_schedules_from_an_older_store_keep_working() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("workflows.sqlite");
    {
        let conn = tako_sqlite::open_local(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE schedules (name TEXT PRIMARY KEY, cron TEXT NOT NULL, last_run_at INTEGER);
             INSERT INTO schedules (name, cron, last_run_at) VALUES ('old', '* * * * * *', NULL);",
        )
        .unwrap();
    }
    assert_old_schedule_row_keeps_working(&RunsDb::open(&path).unwrap());
}

#[test]
fn postgres_schedules_from_an_older_store_keep_working() {
    let Ok(url) = std::env::var("TAKO_TEST_POSTGRES_URL") else {
        return;
    };
    let schema = format!(
        "old_schedules_{}",
        nanoid::nanoid!(12, &['a', 'b', 'c', 'd', 'e', 'f'])
    );
    let mut client = postgres::Client::connect(&url, postgres::NoTls).unwrap();
    client
        .batch_execute(&format!(
            "CREATE SCHEMA {schema};
             CREATE TABLE {schema}.schedules (
                 app_id TEXT NOT NULL, name TEXT NOT NULL, cron TEXT NOT NULL,
                 last_run_at BIGINT, PRIMARY KEY(app_id, name)
             );
             INSERT INTO {schema}.schedules VALUES ('app', 'old', '* * * * * *', NULL);"
        ))
        .unwrap();
    let db = RunsDb::open_config(WorkflowStoreConfig::Postgres {
        url,
        schema: schema.clone(),
        app_id: "app".into(),
    })
    .unwrap();
    assert_old_schedule_row_keeps_working(&db);
    drop(db);
    client
        .batch_execute(&format!("DROP SCHEMA {schema} CASCADE"))
        .unwrap();
}
