use super::*;

/// A store under test. SQLite always runs; Postgres runs when
/// `TAKO_TEST_POSTGRES_URL` is set, in its own schema.
pub(super) struct Store {
    pub(super) db: RunsDb,
    postgres: Option<(postgres::Client, String)>,
}

impl Store {
    fn sqlite() -> Self {
        Self {
            db: RunsDb::open_in_memory().unwrap(),
            postgres: None,
        }
    }

    fn postgres(prefix: &str) -> Option<Self> {
        let url = std::env::var("TAKO_TEST_POSTGRES_URL").ok()?;
        let schema = format!(
            "{prefix}_{}",
            nanoid::nanoid!(12, &['a', 'b', 'c', 'd', 'e', 'f'])
        );
        let db = RunsDb::open_config(WorkflowStoreConfig::Postgres {
            url: url.clone(),
            schema: schema.clone(),
            app_id: "app".into(),
        })
        .unwrap();
        let client = postgres::Client::connect(&url, postgres::NoTls).unwrap();
        Some(Self {
            db,
            postgres: Some((client, schema)),
        })
    }

    /// Another app's view of the same Postgres schema.
    pub(super) fn other_app(&self, app_id: &str) -> Option<RunsDb> {
        let (_, schema) = self.postgres.as_ref()?;
        Some(
            RunsDb::open_config(WorkflowStoreConfig::Postgres {
                url: std::env::var("TAKO_TEST_POSTGRES_URL").unwrap(),
                schema: schema.clone(),
                app_id: app_id.into(),
            })
            .unwrap(),
        )
    }

    /// Expiry is injected into persisted state; no wall-clock sleeps.
    pub(super) fn expire_lease(&mut self, id: &str) {
        match &mut self.postgres {
            None => self
                .db
                .raw_execute("UPDATE runs SET lease_until=0 WHERE id=?1", [id]),
            Some((client, schema)) => {
                client
                    .execute(
                        &format!("UPDATE {schema}.runs SET lease_until=0 WHERE id=$1"),
                        &[&id],
                    )
                    .unwrap();
            }
        }
    }
}

impl Drop for Store {
    fn drop(&mut self) {
        if let Some((client, schema)) = &mut self.postgres {
            let _ = client.batch_execute(&format!("DROP SCHEMA {schema} CASCADE"));
        }
    }
}

/// Run `test` against every available backend.
pub(super) fn each_store(prefix: &str, test: impl Fn(&mut Store)) {
    test(&mut Store::sqlite());
    if let Some(mut store) = Store::postgres(prefix) {
        test(&mut store);
    }
}

fn budget(max_attempts: u32) -> EnqueueOpts {
    EnqueueOpts {
        max_attempts: Some(max_attempts),
        ..opts()
    }
}

fn status(db: &RunsDb, id: &str) -> String {
    db.result("work", id).unwrap().unwrap().status
}

fn claim(db: &RunsDb, worker: &str) -> tako_core::RunPayload {
    db.claim(worker, &["work".into()], 60_000).unwrap().unwrap()
}

#[test]
fn reclaim_kills_a_run_whose_worker_was_lost_on_its_last_attempt() {
    each_store("reclaim_dead", |store| {
        let id = store
            .db
            .enqueue("work", &serde_json::json!({}), &budget(2))
            .unwrap()
            .id;

        // One attempt remains after the first lost worker: requeue as before.
        assert_eq!(claim(&store.db, "first").attempts, 1);
        store
            .db
            .save_step(&id, "first", "charge", &serde_json::json!("receipt"))
            .unwrap();
        store.expire_lease(&id);
        assert_eq!(
            store.db.reclaim_expired_with_workers().unwrap(),
            vec!["first"]
        );
        assert_eq!(status(&store.db, &id), "pending");

        // The second lost worker spent the budget. The run must end here
        // instead of cycling through reclaim and claim forever.
        let second = claim(&store.db, "second");
        assert_eq!(second.attempts, 2);
        assert_eq!(second.step_state["charge"], "receipt");
        store.expire_lease(&id);
        assert_eq!(
            store.db.reclaim_expired_with_workers().unwrap(),
            vec!["second"]
        );
        let outcome = store.db.result("work", &id).unwrap().unwrap();
        assert_eq!(outcome.status, "dead");
        assert_eq!(
            outcome.error.as_deref(),
            Some("worker lost; retry budget exhausted")
        );

        // Dead is terminal: no reclaim, claim, or late write revives it.
        assert_eq!(store.db.reclaim_expired().unwrap(), 0);
        assert!(
            store
                .db
                .claim("third", &["work".into()], 60_000)
                .unwrap()
                .is_none()
        );
        assert_eq!(store.db.pending_count().unwrap(), 0);
        assert!(store.db.in_flight_by_worker().unwrap().is_empty());
        assert!(matches!(
            store.db.heartbeat(&id, "second", 60_000),
            Err(RunsDbError::StaleWorker)
        ));
        assert!(matches!(
            store
                .db
                .save_step(&id, "second", "late", &serde_json::json!(1)),
            Err(RunsDbError::StaleWorker)
        ));
        assert!(matches!(
            store.db.complete(&id, "second", &serde_json::Value::Null),
            Err(RunsDbError::StaleWorker)
        ));
        assert!(matches!(
            store.db.fail(&id, "second", "late", Some(0), false),
            Err(RunsDbError::StaleWorker)
        ));
        assert_eq!(status(&store.db, &id), "dead");

        // It has a completion time, so retention prunes it like other finished runs.
        assert_eq!(
            store
                .db
                .prune_finished_before(now_ms() + 60_000, 100)
                .unwrap(),
            1
        );
    });
}

#[test]
fn deferring_and_waiting_do_not_spend_the_budget_that_reclaim_enforces() {
    each_store("reclaim_parked", |store| {
        let id = store
            .db
            .enqueue("work", &serde_json::json!({}), &budget(2))
            .unwrap()
            .id;
        // Sleeps and waits hand the attempt back, however many there are.
        for round in 0..4 {
            assert_eq!(claim(&store.db, "parker").attempts, 1);
            if round % 2 == 0 {
                store.db.defer(&id, "parker", Some(0)).unwrap();
            } else {
                let step = format!("wait-{round}");
                store
                    .db
                    .wait_for_event(&id, "parker", &step, "event", Some(0))
                    .unwrap();
            }
        }

        assert_eq!(claim(&store.db, "lost-1").attempts, 1);
        store.expire_lease(&id);
        assert_eq!(store.db.reclaim_expired().unwrap(), 1);
        assert_eq!(status(&store.db, &id), "pending");

        // A park between two lost workers does not hide the first one.
        assert_eq!(claim(&store.db, "parker").attempts, 2);
        store.db.defer(&id, "parker", Some(0)).unwrap();
        assert_eq!(claim(&store.db, "lost-2").attempts, 2);
        store.expire_lease(&id);
        assert_eq!(store.db.reclaim_expired().unwrap(), 1);
        assert_eq!(status(&store.db, &id), "dead");
    });
}

#[test]
fn one_reclaim_pass_requeues_and_kills_by_remaining_budget() {
    each_store("reclaim_mixed", |store| {
        // Distinct due times fix the order in which the workers claim.
        let enqueue = |max_attempts, run_at_ms| {
            let options = EnqueueOpts {
                run_at_ms: Some(run_at_ms),
                ..budget(max_attempts)
            };
            store
                .db
                .enqueue("work", &serde_json::json!({}), &options)
                .unwrap()
                .id
        };
        let exhausted = enqueue(1, 1);
        let retryable = enqueue(3, 2);
        let healthy = enqueue(1, 3);
        for worker in ["exhausted", "retryable", "healthy"] {
            claim(&store.db, worker);
        }
        store.expire_lease(&exhausted);
        store.expire_lease(&retryable);

        // Both lost workers free their slot, whatever became of their run.
        let mut workers = store.db.reclaim_expired_with_workers().unwrap();
        workers.sort();
        assert_eq!(workers, vec!["exhausted", "retryable"]);
        assert_eq!(status(&store.db, &exhausted), "dead");
        assert_eq!(status(&store.db, &retryable), "pending");
        assert_eq!(status(&store.db, &healthy), "running");
        store.db.heartbeat(&healthy, "healthy", 60_000).unwrap();
        assert_eq!(claim(&store.db, "next").id, retryable);
    });
}

#[test]
fn postgres_reclaim_only_ends_runs_of_its_own_app() {
    let Some(mut store) = Store::postgres("reclaim_scope") else {
        return;
    };
    let other = store.other_app("other").unwrap();
    let options = EnqueueOpts {
        id: Some("shared-id".into()),
        ..budget(1)
    };
    for db in [&store.db, &other] {
        db.enqueue("work", &serde_json::json!({}), &options)
            .unwrap();
        claim(db, "worker");
    }
    // Expires the run with this ID in both apps.
    store.expire_lease("shared-id");

    assert_eq!(store.db.reclaim_expired().unwrap(), 1);
    assert_eq!(status(&store.db, "shared-id"), "dead");
    assert_eq!(status(&other, "shared-id"), "running");
    assert_eq!(
        other.reclaim_expired_with_workers().unwrap(),
        vec!["worker"]
    );
    assert_eq!(status(&other, "shared-id"), "dead");
}
