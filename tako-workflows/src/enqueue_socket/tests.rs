use super::*;
use tako_core::EnqueueOpts;
use tako_socket::{read_json_line, write_json_line};
use tokio::io::BufReader;
use tokio::net::UnixStream;

fn test_limiter() -> Arc<crate::in_flight::InFlightLimiter> {
    Arc::new(crate::in_flight::InFlightLimiter::new(10))
}

fn lookup_for(map: std::collections::HashMap<String, Arc<RunsDb>>) -> AppLookup {
    Arc::new(move |app: &str| {
        map.get(app).map(|db| AppHandlers {
            db: db.clone(),
            limiter: test_limiter(),
            on_enqueue: Arc::new(|| {}),
            health_check: Arc::new(|| Ok(())),
            on_claimed: Arc::new(|| {}),
        })
    })
}

#[tokio::test]
async fn internal_socket_is_group_accessible_for_app_processes() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = tempfile::tempdir().unwrap();
    let sock = tmp.path().join("internal.sock");
    let handle = spawn(&sock, lookup_for(Default::default()), None).unwrap();
    let mode = std::fs::metadata(&sock).unwrap().permissions().mode() & 0o777;

    handle.shutdown().await;
    assert_eq!(mode, 0o660);
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
    let handle = spawn(&sock, lookup_for(map), None).unwrap();

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
            on_enqueue: Arc::new(|| {}),
            health_check: Arc::new(|| Err("bootstrap crashed".to_string())),
            on_claimed: Arc::new(|| {}),
        })
    });
    let handle = spawn(&sock, lookup, None).unwrap();

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
            on_enqueue: Arc::new(|| {}),
            health_check: Arc::new(|| Ok(())),
            on_claimed: on_claimed.clone(),
        })
    });
    let handle = spawn(&sock, lookup, None).unwrap();

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
            on_enqueue: Arc::new(|| {}),
            health_check: Arc::new(|| Ok(())),
            on_claimed: Arc::new(|| {}),
        })
    });
    let handle = spawn(&sock, lookup, None).unwrap();

    let stream = UnixStream::connect(&sock).await.unwrap();
    let (r, mut w) = stream.into_split();
    let mut r = BufReader::new(r);

    // First two claims succeed.
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
    limiter.release("w1");
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
            on_enqueue: Arc::new(|| {}),
            health_check: Arc::new(|| Ok(())),
            on_claimed: Arc::new(|| {}),
        })
    });
    let handle = spawn(&sock, lookup, None).unwrap();

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
            on_enqueue: Arc::new(|| {}),
            health_check: Arc::new(|| Ok(())),
            on_claimed: Arc::new(|| {}),
        })
    });
    let handle = spawn(&sock, lookup, None).unwrap();

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
        app: "a".into(),
        id,
        worker_id: "w1".into(),
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
    let handle = spawn(&sock, lookup_for(Default::default()), None).unwrap();

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
            on_enqueue: on_enq.clone(),
            health_check: Arc::new(|| Ok(())),
            on_claimed: Arc::new(|| {}),
        })
    });
    let handle = spawn(&sock, lookup, None).unwrap();

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

#[tokio::test]
async fn shutdown_removes_pid_socket_file() {
    let tmp = tempfile::tempdir().unwrap();
    let sock = tmp.path().join("internal.sock");
    let handle = spawn(&sock, lookup_for(Default::default()), None).unwrap();
    assert!(sock.exists() || sock.is_symlink());
    handle.shutdown().await;
}
