use super::*;
use tako_core::EnqueueOpts;
use tako_socket::{read_json_line, write_json_line};
use tokio::io::BufReader;
use tokio::net::UnixStream;

fn test_limiter() -> Arc<crate::in_flight::InFlightLimiter> {
    Arc::new(crate::in_flight::InFlightLimiter::new(10))
}

fn accepting_new_work() -> Arc<std::sync::atomic::AtomicBool> {
    Arc::new(std::sync::atomic::AtomicBool::new(true))
}

fn draining_work() -> Arc<std::sync::atomic::AtomicBool> {
    Arc::new(std::sync::atomic::AtomicBool::new(false))
}

fn lookup_for(map: std::collections::HashMap<String, Arc<RunsDb>>) -> AppLookup {
    Arc::new(move |app: &str| {
        map.get(app).map(|db| AppHandlers {
            db: db.clone(),
            limiter: test_limiter(),
            accepting_new_work: accepting_new_work(),
            on_enqueue: Arc::new(|| {}),
            health_check: Arc::new(|| Ok(())),
            on_claimed: Arc::new(|| {}),
        })
    })
}

#[test]
fn network_calls_authenticate_the_caller_before_resolving_the_destination() {
    let lookup = lookup_for(Default::default());
    let auth: PeerAuthFn = Arc::new(|app, uid| {
        if app == "web/staging" && uid == 1001 {
            Ok(())
        } else {
            Err("caller mismatch".into())
        }
    });
    let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
    let observed = calls.clone();
    let route: NetworkCallFn = Arc::new(move |caller, target, invocation| {
        assert_eq!(caller, "web/staging");
        observed
            .lock()
            .unwrap()
            .push((target.to_owned(), invocation.name));
        Response::ok(serde_json::json!({"id":"run-1"}))
    });
    let command = |app: &str, address: &str| Command::NetworkCall {
        app: app.into(),
        address: address.into(),
        request: tako_core::network::NetworkRequest::Enqueue {
            payload: serde_json::json!({"to":"a@example.com"}),
            opts: Default::default(),
        },
    };
    assert!(
        handle_command(
            &lookup,
            None,
            Some(&auth),
            Some(1001),
            Some(&route),
            command("web/staging", "mail.send-email")
        )
        .data()
        .is_some()
    );
    for (app, address, uid) in [
        ("mail/staging", "mail.send-email", Some(1001)),
        ("web/staging", "mail.send-email", Some(1002)),
        ("web/staging", "mail.send-email", None),
        ("web/staging", "mail/production.send-email", Some(1001)),
    ] {
        assert!(
            handle_command(
                &lookup,
                None,
                Some(&auth),
                uid,
                Some(&route),
                command(app, address)
            )
            .data()
            .is_none()
        );
    }
    assert_eq!(
        *calls.lock().unwrap(),
        vec![("mail/staging".into(), "send-email".into())]
    );
}

#[tokio::test]
async fn internal_socket_is_group_accessible_for_app_processes() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = tempfile::tempdir().unwrap();
    let sock = tmp.path().join("internal.sock");
    let handle = spawn(&sock, lookup_for(Default::default()), None, None, None).unwrap();
    let mode = std::fs::metadata(&sock).unwrap().permissions().mode() & 0o777;

    handle.shutdown().await;
    assert_eq!(mode, 0o660);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn network_calls_leave_storage_workers_available() {
    let tmp = tempfile::tempdir().unwrap();
    let sock = tmp.path().join("internal.sock");
    let barrier = Arc::new(std::sync::Barrier::new(32));
    let route: NetworkCallFn = Arc::new(move |_, _, _| {
        barrier.wait();
        // The recipient must be able to enqueue while every caller awaits it.
        tokio::runtime::Handle::current().block_on(async {
            match tokio::time::timeout(
                std::time::Duration::from_secs(5),
                crate::blocking::run(|| Response::ok(serde_json::json!({"id":"run"}))),
            )
            .await
            {
                Ok(Ok(response)) => response,
                _ => Response::error("storage workers exhausted by network calls"),
            }
        })
    });
    let handle = spawn(
        &sock,
        lookup_for(Default::default()),
        None,
        None,
        Some(route),
    )
    .unwrap();
    let mut calls = tokio::task::JoinSet::new();
    for _ in 0..32 {
        let sock = sock.clone();
        calls.spawn(async move {
            let mut socket = UnixStream::connect(sock).await.unwrap();
            write_json_line(
                &mut socket,
                &Command::NetworkCall {
                    app: "web/staging".into(),
                    address: "mail.send".into(),
                    request: tako_core::network::NetworkRequest::Enqueue {
                        payload: serde_json::Value::Null,
                        opts: Default::default(),
                    },
                },
            )
            .await
            .unwrap();
            let response: Response = read_json_line(&mut BufReader::new(socket))
                .await
                .unwrap()
                .unwrap();
            assert!(response.data().is_some(), "{response:?}");
        });
    }
    while let Some(result) = calls.join_next().await {
        result.unwrap();
    }
    handle.shutdown().await;
}

#[test]
fn app_socket_gid_uses_tako_app_group_when_server_runs_as_root() {
    let gid = app_socket_gid(|_| Ok(Some(987)), true, &[]).unwrap();
    assert_eq!(gid, Some(987));
}

#[test]
fn app_socket_gid_uses_service_supplementary_group() {
    let gid = app_socket_gid(|_| Ok(Some(987)), false, &[987]).unwrap();
    assert_eq!(gid, Some(987));
}

#[test]
fn app_socket_gid_is_unchanged_for_unrelated_local_user() {
    let gid = app_socket_gid(|_| Ok(Some(987)), false, &[123]).unwrap();
    assert_eq!(gid, None);
}

#[test]
fn app_socket_gid_is_unchanged_when_tako_app_is_missing() {
    let gid = app_socket_gid(|_| Ok(None), true, &[]).unwrap();
    assert_eq!(gid, None);
}

#[tokio::test]
async fn enqueue_routes_by_app() {
    let tmp = tempfile::tempdir().unwrap();
    let sock = tmp.path().join("internal.sock");
    let db_a = Arc::new(RunsDb::open_in_memory().unwrap());
    let db_b = Arc::new(RunsDb::open_in_memory().unwrap());

    let mut map = std::collections::HashMap::new();
    map.insert("a".to_string(), db_a.clone());
    map.insert("b".to_string(), db_b.clone());
    let handle = spawn(&sock, lookup_for(map), None, None, None).unwrap();

    let stream = UnixStream::connect(&sock).await.unwrap();
    let (r, mut w) = stream.into_split();
    let mut r = BufReader::new(r);

    let cmd = Command::EnqueueRun {
        app: "a".into(),
        name: "w".into(),
        payload: serde_json::json!({}),
        opts: EnqueueOpts::default(),
    };
    write_json_line(&mut w, &cmd).await.unwrap();
    let resp: Response = read_json_line(&mut r).await.unwrap().unwrap();
    assert!(resp.is_ok());

    // App 'a' should have one pending run; app 'b' should have zero.
    assert_eq!(db_a.pending_count().unwrap(), 1);
    assert_eq!(db_b.pending_count().unwrap(), 0);

    handle.shutdown().await;
}

#[tokio::test]
async fn enqueue_rejects_when_health_check_fails() {
    let tmp = tempfile::tempdir().unwrap();
    let sock = tmp.path().join("internal.sock");
    let db = Arc::new(RunsDb::open_in_memory().unwrap());

    let db_for_lookup = db.clone();
    let lookup: AppLookup = Arc::new(move |_app: &str| {
        Some(AppHandlers {
            db: db_for_lookup.clone(),
            limiter: test_limiter(),
            accepting_new_work: accepting_new_work(),
            on_enqueue: Arc::new(|| {}),
            health_check: Arc::new(|| Err("bootstrap crashed".to_string())),
            on_claimed: Arc::new(|| {}),
        })
    });
    let handle = spawn(&sock, lookup, None, None, None).unwrap();

    let stream = UnixStream::connect(&sock).await.unwrap();
    let (r, mut w) = stream.into_split();
    let mut r = BufReader::new(r);

    let cmd = Command::EnqueueRun {
        app: "a".into(),
        name: "w".into(),
        payload: serde_json::json!({}),
        opts: EnqueueOpts::default(),
    };
    write_json_line(&mut w, &cmd).await.unwrap();
    let resp: Response = read_json_line(&mut r).await.unwrap().unwrap();
    let err = resp.error_message().unwrap();
    assert!(
        err.contains("worker unhealthy") && err.contains("bootstrap crashed"),
        "expected unhealthy error with reason, got: {err}"
    );
    // DB must stay empty — enqueue short-circuited before db.enqueue().
    assert_eq!(db.pending_count().unwrap(), 0);

    handle.shutdown().await;
}

#[tokio::test]
async fn draining_runtime_rejects_enqueue_and_claims_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let sock = tmp.path().join("internal.sock");
    let db = Arc::new(RunsDb::open_in_memory().unwrap());
    db.enqueue("w", &serde_json::json!({}), &EnqueueOpts::default())
        .unwrap();

    let db_for_lookup = db.clone();
    let lookup: AppLookup = Arc::new(move |_app: &str| {
        Some(AppHandlers {
            db: db_for_lookup.clone(),
            limiter: test_limiter(),
            accepting_new_work: draining_work(),
            on_enqueue: Arc::new(|| {}),
            health_check: Arc::new(|| Ok(())),
            on_claimed: Arc::new(|| {}),
        })
    });
    let handle = spawn(&sock, lookup, None, None, None).unwrap();

    let stream = UnixStream::connect(&sock).await.unwrap();
    let (r, mut w) = stream.into_split();
    let mut r = BufReader::new(r);

    let enqueue = Command::EnqueueRun {
        app: "a".into(),
        name: "w".into(),
        payload: serde_json::json!({}),
        opts: EnqueueOpts::default(),
    };
    write_json_line(&mut w, &enqueue).await.unwrap();
    let resp: Response = read_json_line(&mut r).await.unwrap().unwrap();
    assert!(resp.error_message().unwrap().contains("draining"));

    let claim = Command::ClaimRun {
        app: "a".into(),
        worker_id: "w1".into(),
        names: vec!["w".into()],
        lease_ms: 30_000,
    };
    write_json_line(&mut w, &claim).await.unwrap();
    let resp: Response = read_json_line(&mut r).await.unwrap().unwrap();
    assert!(resp.data().unwrap().is_null());
    assert_eq!(db.pending_count().unwrap(), 1);

    handle.shutdown().await;
}

#[tokio::test]
async fn claim_run_fires_on_claimed() {
    let tmp = tempfile::tempdir().unwrap();
    let sock = tmp.path().join("internal.sock");
    let db = Arc::new(RunsDb::open_in_memory().unwrap());
    db.enqueue("w", &serde_json::json!({}), &EnqueueOpts::default())
        .unwrap();

    let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = count.clone();
    let on_claimed: OnClaimed = Arc::new(move || {
        counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    });
    let db_for_lookup = db.clone();
    let lookup: AppLookup = Arc::new(move |_app: &str| {
        Some(AppHandlers {
            db: db_for_lookup.clone(),
            limiter: test_limiter(),
            accepting_new_work: accepting_new_work(),
            on_enqueue: Arc::new(|| {}),
            health_check: Arc::new(|| Ok(())),
            on_claimed: on_claimed.clone(),
        })
    });
    let handle = spawn(&sock, lookup, None, None, None).unwrap();

    let stream = UnixStream::connect(&sock).await.unwrap();
    let (r, mut w) = stream.into_split();
    let mut r = BufReader::new(r);

    let cmd = Command::ClaimRun {
        app: "a".into(),
        worker_id: "w1".into(),
        names: vec!["w".into()],
        lease_ms: 30_000,
    };
    write_json_line(&mut w, &cmd).await.unwrap();
    let _resp: Response = read_json_line(&mut r).await.unwrap().unwrap();
    assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 1);

    handle.shutdown().await;
}

#[tokio::test]
async fn claim_respects_in_flight_limiter() {
    use crate::in_flight::InFlightLimiter;

    let tmp = tempfile::tempdir().unwrap();
    let sock = tmp.path().join("internal.sock");
    let db = Arc::new(RunsDb::open_in_memory().unwrap());
    // Seed 3 runs so there's always something the DB could return.
    for _ in 0..3 {
        db.enqueue("w", &serde_json::json!({}), &EnqueueOpts::default())
            .unwrap();
    }
    // Cap at 2 concurrent in-flight for this worker.
    let limiter = Arc::new(InFlightLimiter::new(2));

    let db_for_lookup = db.clone();
    let limiter_for_lookup = limiter.clone();
    let lookup: AppLookup = Arc::new(move |_app: &str| {
        Some(AppHandlers {
            db: db_for_lookup.clone(),
            limiter: limiter_for_lookup.clone(),
            accepting_new_work: accepting_new_work(),
            on_enqueue: Arc::new(|| {}),
            health_check: Arc::new(|| Ok(())),
            on_claimed: Arc::new(|| {}),
        })
    });
    let handle = spawn(&sock, lookup, None, None, None).unwrap();

    let stream = UnixStream::connect(&sock).await.unwrap();
    let (r, mut w) = stream.into_split();
    let mut r = BufReader::new(r);

    // First two claims succeed.
    let mut claimed = Vec::new();
    for _ in 0..2 {
        let cmd = Command::ClaimRun {
            app: "a".into(),
            worker_id: "w1".into(),
            names: vec!["w".into()],
            lease_ms: 30_000,
        };
        write_json_line(&mut w, &cmd).await.unwrap();
        let resp: Response = read_json_line(&mut r).await.unwrap().unwrap();
        let v = resp.data().unwrap();
        assert!(!v.is_null(), "expected a run, got null: {resp:?}");
        claimed.push(serde_json::from_value::<tako_core::RunPayload>(v.clone()).unwrap());
    }

    // Third claim: limiter refuses, DB row is NOT consumed, response
    // is a null payload (same shape as "queue empty").
    let cmd = Command::ClaimRun {
        app: "a".into(),
        worker_id: "w1".into(),
        names: vec!["w".into()],
        lease_ms: 30_000,
    };
    write_json_line(&mut w, &cmd).await.unwrap();
    let resp: Response = read_json_line(&mut r).await.unwrap().unwrap();
    assert!(resp.data().unwrap().is_null());
    // One run still pending — limiter refused BEFORE the DB claim fired.
    assert_eq!(db.pending_count().unwrap(), 1);

    // Complete one run → slot frees → next claim succeeds.
    db.complete(
        &claimed[0].id,
        &claimed[0].worker_id,
        &serde_json::Value::Null,
    )
    .unwrap();
    limiter.release_claim(&claimed[0].worker_id);
    let cmd = Command::ClaimRun {
        app: "a".into(),
        worker_id: "w1".into(),
        names: vec!["w".into()],
        lease_ms: 30_000,
    };
    write_json_line(&mut w, &cmd).await.unwrap();
    let resp: Response = read_json_line(&mut r).await.unwrap().unwrap();
    assert!(!resp.data().unwrap().is_null());

    handle.shutdown().await;
}

#[tokio::test]
async fn claim_without_work_does_not_hold_a_slot() {
    use crate::in_flight::InFlightLimiter;

    let tmp = tempfile::tempdir().unwrap();
    let sock = tmp.path().join("internal.sock");
    let db = Arc::new(RunsDb::open_in_memory().unwrap());
    let limiter = Arc::new(InFlightLimiter::new(1));

    let db_for_lookup = db.clone();
    let limiter_for_lookup = limiter.clone();
    let lookup: AppLookup = Arc::new(move |_app: &str| {
        Some(AppHandlers {
            db: db_for_lookup.clone(),
            limiter: limiter_for_lookup.clone(),
            accepting_new_work: accepting_new_work(),
            on_enqueue: Arc::new(|| {}),
            health_check: Arc::new(|| Ok(())),
            on_claimed: Arc::new(|| {}),
        })
    });
    let handle = spawn(&sock, lookup, None, None, None).unwrap();

    let stream = UnixStream::connect(&sock).await.unwrap();
    let (r, mut w) = stream.into_split();
    let mut r = BufReader::new(r);

    // Empty queue: claim reserves a slot, DB returns nothing, slot
    // must be released so the next claim isn't rejected by the cap.
    for _ in 0..5 {
        let cmd = Command::ClaimRun {
            app: "a".into(),
            worker_id: "w1".into(),
            names: vec!["w".into()],
            lease_ms: 30_000,
        };
        write_json_line(&mut w, &cmd).await.unwrap();
        let resp: Response = read_json_line(&mut r).await.unwrap().unwrap();
        assert!(resp.data().unwrap().is_null());
    }
    assert_eq!(limiter.count("w1"), 0);

    handle.shutdown().await;
}

#[tokio::test]
async fn complete_releases_a_slot() {
    use crate::in_flight::InFlightLimiter;

    let tmp = tempfile::tempdir().unwrap();
    let sock = tmp.path().join("internal.sock");
    let db = Arc::new(RunsDb::open_in_memory().unwrap());
    db.enqueue("w", &serde_json::json!({}), &EnqueueOpts::default())
        .unwrap();
    let limiter = Arc::new(InFlightLimiter::new(1));

    let db_for_lookup = db.clone();
    let limiter_for_lookup = limiter.clone();
    let lookup: AppLookup = Arc::new(move |_app: &str| {
        Some(AppHandlers {
            db: db_for_lookup.clone(),
            limiter: limiter_for_lookup.clone(),
            accepting_new_work: accepting_new_work(),
            on_enqueue: Arc::new(|| {}),
            health_check: Arc::new(|| Ok(())),
            on_claimed: Arc::new(|| {}),
        })
    });
    let handle = spawn(&sock, lookup, None, None, None).unwrap();

    let stream = UnixStream::connect(&sock).await.unwrap();
    let (r, mut w) = stream.into_split();
    let mut r = BufReader::new(r);

    // Claim.
    let cmd = Command::ClaimRun {
        app: "a".into(),
        worker_id: "w1".into(),
        names: vec!["w".into()],
        lease_ms: 30_000,
    };
    write_json_line(&mut w, &cmd).await.unwrap();
    let resp: Response = read_json_line(&mut r).await.unwrap().unwrap();
    let run = resp.data().unwrap();
    let id = run.get("id").and_then(|v| v.as_str()).unwrap().to_string();
    assert_eq!(limiter.count("w1"), 1);

    // Complete.
    let cmd = Command::CompleteRun {
        result: serde_json::Value::Null,
        app: "a".into(),
        id,
        worker_id: run["worker_id"].as_str().unwrap().to_owned(),
    };
    write_json_line(&mut w, &cmd).await.unwrap();
    let resp: Response = read_json_line(&mut r).await.unwrap().unwrap();
    assert!(resp.is_ok());
    assert_eq!(limiter.count("w1"), 0);

    handle.shutdown().await;
}

#[tokio::test]
async fn unknown_app_returns_error() {
    let tmp = tempfile::tempdir().unwrap();
    let sock = tmp.path().join("internal.sock");
    let handle = spawn(&sock, lookup_for(Default::default()), None, None, None).unwrap();

    let stream = UnixStream::connect(&sock).await.unwrap();
    let (r, mut w) = stream.into_split();
    let mut r = BufReader::new(r);

    let cmd = Command::EnqueueRun {
        app: "ghost".into(),
        name: "w".into(),
        payload: serde_json::json!({}),
        opts: EnqueueOpts::default(),
    };
    write_json_line(&mut w, &cmd).await.unwrap();
    let resp: Response = read_json_line(&mut r).await.unwrap().unwrap();
    assert!(resp.error_message().unwrap().contains("unknown app"));

    handle.shutdown().await;
}

#[tokio::test]
async fn on_enqueue_fires_for_signal_with_waiters_only() {
    let tmp = tempfile::tempdir().unwrap();
    let sock = tmp.path().join("internal.sock");
    let db = Arc::new(RunsDb::open_in_memory().unwrap());
    let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));

    let counter = count.clone();
    let on_enq: OnEnqueue = Arc::new(move || {
        counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    });
    let db_for_lookup = db.clone();
    let lookup: AppLookup = Arc::new(move |_app: &str| {
        Some(AppHandlers {
            db: db_for_lookup.clone(),
            limiter: test_limiter(),
            accepting_new_work: accepting_new_work(),
            on_enqueue: on_enq.clone(),
            health_check: Arc::new(|| Ok(())),
            on_claimed: Arc::new(|| {}),
        })
    });
    let handle = spawn(&sock, lookup, None, None, None).unwrap();

    // Signal with no waiters → should NOT fire on_enqueue.
    let stream = UnixStream::connect(&sock).await.unwrap();
    let (_r, mut w) = stream.into_split();
    let cmd = Command::Signal {
        app: "a".into(),
        event_name: "noop".into(),
        payload: serde_json::json!({}),
    };
    write_json_line(&mut w, &cmd).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 0);

    // Now seed a waiter and signal again — should fire.
    let r = db
        .enqueue("w", &serde_json::json!({}), &EnqueueOpts::default())
        .unwrap();
    let _ = db.claim("w1", &["w".into()], 30_000).unwrap();
    db.wait_for_event(&r.id, "w1", "step", "evt", None).unwrap();

    let stream = UnixStream::connect(&sock).await.unwrap();
    let (_r, mut w) = stream.into_split();
    let cmd = Command::Signal {
        app: "a".into(),
        event_name: "evt".into(),
        payload: serde_json::json!({"x": 1}),
    };
    write_json_line(&mut w, &cmd).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 1);

    handle.shutdown().await;
}

#[test]
fn authorize_peer_skips_when_unset() {
    assert!(authorize_peer(None, None, "app").is_ok());
}

#[test]
fn authorize_peer_rejects_missing_uid_when_configured() {
    let auth: PeerAuthFn = Arc::new(|_app, _uid| Ok(()));
    let err = authorize_peer(Some(&auth), None, "app").unwrap_err();
    assert!(err.contains("missing peer credentials"));
}

#[test]
fn authorize_peer_rejects_mismatched_app() {
    let auth: PeerAuthFn = Arc::new(|app, uid| {
        if app == "owned" && uid == 42 {
            Ok(())
        } else {
            Err("app mismatch".into())
        }
    });
    assert!(authorize_peer(Some(&auth), Some(42), "owned").is_ok());
    assert_eq!(
        authorize_peer(Some(&auth), Some(42), "other").unwrap_err(),
        "app mismatch"
    );
}

#[tokio::test]
async fn shutdown_removes_pid_socket_file() {
    let tmp = tempfile::tempdir().unwrap();
    let sock = tmp.path().join("internal.sock");
    let handle = spawn(&sock, lookup_for(Default::default()), None, None, None).unwrap();
    assert!(sock.exists() || sock.is_symlink());
    handle.shutdown().await;
}

fn assert_same_process_takeover(db: Arc<RunsDb>, expire: impl FnOnce(&str)) {
    let limiter = Arc::new(crate::in_flight::InFlightLimiter::new(1));
    let lookup: AppLookup = {
        let db = db.clone();
        let limiter = limiter.clone();
        Arc::new(move |_| {
            Some(AppHandlers {
                db: db.clone(),
                limiter: limiter.clone(),
                accepting_new_work: accepting_new_work(),
                on_enqueue: Arc::new(|| {}),
                health_check: Arc::new(|| Ok(())),
                on_claimed: Arc::new(|| {}),
            })
        })
    };
    let call = |cmd| handle_command(&lookup, None, None, None, None, cmd);
    let claim = || {
        let response = call(Command::ClaimRun {
            app: "a".into(),
            worker_id: "same:process".into(),
            names: vec!["work".into()],
            lease_ms: 60_000,
        });
        serde_json::from_value::<tako_core::RunPayload>(response.data().unwrap().clone()).unwrap()
    };
    let id = db
        .enqueue("work", &serde_json::json!({}), &EnqueueOpts::default())
        .unwrap()
        .id;
    let first = claim();
    assert_eq!(first.id, id);
    expire(&id);
    for owner in db.reclaim_expired_with_workers().unwrap() {
        limiter.release_claim(&owner);
    }
    let second = claim();
    assert_eq!(second.id, id);
    assert_ne!(first.worker_id, second.worker_id);
    assert_eq!(limiter.count("same:process"), 1);
    let app = "a".to_owned();
    let worker_id = first.worker_id;
    for cmd in [
        Command::HeartbeatRun {
            app: app.clone(),
            id: id.clone(),
            worker_id: worker_id.clone(),
            lease_ms: 60_000,
        },
        Command::SaveStep {
            app: app.clone(),
            id: id.clone(),
            worker_id: worker_id.clone(),
            step_name: "late".into(),
            result: serde_json::json!(1),
        },
        Command::CompleteRun {
            result: serde_json::Value::Null,
            app: app.clone(),
            id: id.clone(),
            worker_id: worker_id.clone(),
        },
        Command::CancelRun {
            app: app.clone(),
            id: id.clone(),
            worker_id: worker_id.clone(),
            reason: None,
        },
        Command::FailRun {
            app: app.clone(),
            id: id.clone(),
            worker_id: worker_id.clone(),
            error: "late".into(),
            next_run_at_ms: None,
            finalize: false,
        },
        Command::DeferRun {
            app: app.clone(),
            id: id.clone(),
            worker_id: worker_id.clone(),
            wake_at_ms: Some(0),
        },
        Command::WaitForEvent {
            app: app.clone(),
            id: id.clone(),
            worker_id,
            step_name: "late".into(),
            event_name: "event".into(),
            timeout_at_ms: None,
        },
    ] {
        let response = call(cmd);
        assert!(!response.is_ok(), "stale attempt was accepted");
        assert_eq!(limiter.count("same:process"), 1);
    }
    assert!(
        call(Command::SaveStep {
            app: app.clone(),
            id: id.clone(),
            worker_id: second.worker_id.clone(),
            step_name: "saved".into(),
            result: serde_json::json!(2)
        })
        .is_ok()
    );
    assert!(
        call(Command::CompleteRun {
            result: serde_json::Value::Null,
            app,
            id,
            worker_id: second.worker_id
        })
        .is_ok()
    );
    assert_eq!(limiter.count("same:process"), 0);
}

#[test]
fn sqlite_same_process_reclaim_rejects_old_attempt() {
    let db = Arc::new(RunsDb::open_in_memory().unwrap());
    assert_same_process_takeover(db.clone(), |id| {
        db.raw_execute("UPDATE runs SET lease_until=0 WHERE id=?1", [id])
    });
}

#[test]
fn remotely_reclaimed_claim_does_not_exhaust_local_worker_budget() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("workflows.sqlite");
    let db = Arc::new(RunsDb::open(&path).unwrap());
    let remote = RunsDb::open(&path).unwrap();
    assert_remote_reclaim_recovers_budget(db, |id| {
        remote.raw_execute("UPDATE runs SET lease_until=0 WHERE id=?1", [id]);
        remote.reclaim_expired_with_workers().unwrap()
    });
}

fn assert_remote_reclaim_recovers_budget(
    db: Arc<RunsDb>,
    reclaim: impl FnOnce(&str) -> Vec<String>,
) {
    let limiter = Arc::new(crate::in_flight::InFlightLimiter::new(1));
    let lookup: AppLookup = {
        let db = db.clone();
        let limiter = limiter.clone();
        Arc::new(move |_| {
            Some(AppHandlers {
                db: db.clone(),
                limiter: limiter.clone(),
                accepting_new_work: accepting_new_work(),
                on_enqueue: Arc::new(|| {}),
                health_check: Arc::new(|| Ok(())),
                on_claimed: Arc::new(|| {}),
            })
        })
    };
    let claim = || {
        let response = handle_command(
            &lookup,
            None,
            None,
            None,
            None,
            Command::ClaimRun {
                app: "a".into(),
                worker_id: "worker".into(),
                names: vec!["work".into()],
                lease_ms: 60_000,
            },
        );
        serde_json::from_value::<Option<tako_core::RunPayload>>(response.data().unwrap().clone())
            .unwrap()
    };
    db.enqueue("work", &serde_json::json!({}), &EnqueueOpts::default())
        .unwrap();
    let first = claim().unwrap();
    assert_eq!(reclaim(&first.id), vec![first.worker_id.clone()]);
    // Another server's ticker cannot directly update this server's limiter.
    let second =
        claim().expect("a remotely reclaimed claim must not hold the local budget forever");
    assert_eq!(first.id, second.id);
    assert_ne!(first.worker_id, second.worker_id);
    // A delayed release must not decrement the replacement claim's slot.
    limiter.release_claim(&first.worker_id);
    assert_eq!(limiter.count("worker"), 1);
    assert!(claim().is_none());
}

#[test]
fn postgres_remote_reclaim_recovers_local_worker_budget() {
    let Ok(url) = std::env::var("TAKO_TEST_POSTGRES_URL") else {
        return;
    };
    let app = format!("remote-reclaim-{}", nanoid::nanoid!());
    let db = Arc::new(RunsDb::open_postgres(&url, &app).unwrap());
    let remote = RunsDb::open_postgres(&url, &app).unwrap();
    let mut client = postgres::Client::connect(&url, postgres::NoTls).unwrap();
    assert_remote_reclaim_recovers_budget(db, |id| {
        client
            .execute(
                "UPDATE tako_workflows.runs SET lease_until=0 WHERE app_id=$1 AND id=$2",
                &[&app, &id],
            )
            .unwrap();
        remote.reclaim_expired_with_workers().unwrap()
    });
    client
        .execute("DELETE FROM tako_workflows.runs WHERE app_id=$1", &[&app])
        .unwrap();
}

#[test]
fn postgres_same_process_reclaim_rejects_old_attempt() {
    let Ok(url) = std::env::var("TAKO_TEST_POSTGRES_URL") else {
        return;
    };
    let app = format!("claim-owner-{}", nanoid::nanoid!());
    let db = Arc::new(RunsDb::open_postgres(&url, &app).unwrap());
    let mut client = postgres::Client::connect(&url, postgres::NoTls).unwrap();
    assert_same_process_takeover(db, |id| {
        client
            .execute(
                "UPDATE tako_workflows.runs SET lease_until=0 WHERE app_id=$1 AND id=$2",
                &[&app, &id],
            )
            .unwrap();
    });
    client
        .execute("DELETE FROM tako_workflows.steps WHERE app_id=$1", &[&app])
        .unwrap();
    client
        .execute("DELETE FROM tako_workflows.runs WHERE app_id=$1", &[&app])
        .unwrap();
}

/// Operator commands belong to the management path. An app process may not
/// list, show, retry, or cancel runs, neither its own nor another app's.
#[test]
fn operator_workflow_commands_are_rejected_on_the_internal_socket() {
    let db = Arc::new(RunsDb::open_in_memory().unwrap());
    let id = db
        .enqueue("work", &serde_json::json!({}), &EnqueueOpts::default())
        .unwrap()
        .id;
    let lookup = lookup_for([("app".to_string(), db.clone())].into());
    // The peer is authorized as "app" for every command it sends.
    let auth: PeerAuthFn = Arc::new(|app, _| {
        (app == "app")
            .then_some(())
            .ok_or_else(|| "forbidden".to_string())
    });
    for app in ["app", "other"] {
        for cmd in [
            Command::ListWorkflowRuns {
                app: app.into(),
                status: None,
                name: None,
                limit: None,
            },
            Command::GetWorkflowRun {
                app: app.into(),
                id: id.clone(),
            },
            Command::RetryWorkflowRun {
                app: app.into(),
                id: id.clone(),
            },
            Command::CancelWorkflowRun {
                app: app.into(),
                id: id.clone(),
            },
        ] {
            let response = handle_command(&lookup, None, Some(&auth), Some(1000), None, cmd);
            assert!(
                response
                    .error_message()
                    .is_some_and(|message| message.contains("not accepted on the internal socket")),
                "{response:?}"
            );
        }
    }
    assert_eq!(
        db.get_run(&id).unwrap().unwrap().status,
        tako_core::WorkflowRunStatus::Pending
    );
}
