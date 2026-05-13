use super::process::{
    build_spawn_env, forward_child_log_line, handle_wake_on_request, kill_all_app_processes,
    kill_app_process, push_user_action,
};
use super::redirect::redirect_location;
use super::*;

use openssl::x509::X509;
use tako::dev::LocalCA;
use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncWriteExt;
use tokio::time::Duration;

/// Create a `WorkflowManager` backed by a throwaway tempdir so tests don't
/// touch the user's real `~/Library/Application Support/tako` etc.
fn test_workflows() -> Arc<tako_workflows::WorkflowManager> {
    let tmp = std::env::temp_dir().join(format!(
        "tako-dev-test-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&tmp).ok();
    Arc::new(tako_workflows::WorkflowManager::new(&tmp))
}

async fn query_control_clients(state: Arc<Mutex<State>>) -> u32 {
    let (a, b) = tokio::net::UnixStream::pair().unwrap();
    let h = tokio::spawn(async move { handle_client(a, state).await });

    let (r, mut w) = b.into_split();
    w.write_all(b"{\"type\":\"Info\"}\n").await.unwrap();
    let mut lines = BufReader::new(r).lines();
    let line = lines.next_line().await.unwrap().unwrap();
    let resp: Response = serde_json::from_str(&line).unwrap();

    drop(w);
    h.await.unwrap().unwrap();

    match resp {
        Response::Info { info } => info.control_clients,
        other => panic!("unexpected: {other:?}"),
    }
}

#[tokio::test]
async fn register_app_roundtrip() {
    let (a, b) = tokio::net::UnixStream::pair().unwrap();
    let (shutdown_tx, _shutdown_rx) = watch::channel(false);
    let st = State::new(
        shutdown_tx,
        proxy::Routes::default(),
        EventsHub::default(),
        true,
        53535,
        8443,
        "127.0.0.1:8443".to_string(),
        "127.0.0.1".to_string(),
    );
    let _ = test_workflows();
    let state = Arc::new(Mutex::new(st));
    let h = tokio::spawn(async move { handle_client(a, state).await });

    let (r, mut w) = b.into_split();
    let mut lines = BufReader::new(r).lines();

    let req = serde_json::json!({
        "type": "RegisterApp",
        "config_path": "/tmp/test-proj/tako.toml",
        "project_dir": "/tmp/test-proj",
        "app_name": "my-app",
        "hosts": ["my-app.test"],
        "command": ["node", "index.js"],
        "env": {}
    });
    w.write_all(req.to_string().as_bytes()).await.unwrap();
    w.write_all(b"\n").await.unwrap();

    let reg_line = lines.next_line().await.unwrap().unwrap();
    let reg: Response = serde_json::from_str(&reg_line).unwrap();
    match reg {
        Response::AppRegistered {
            app_name,
            config_path,
            project_dir,
            url,
        } => {
            assert_eq!(app_name, "my-app");
            assert_eq!(config_path, "/tmp/test-proj/tako.toml");
            assert_eq!(project_dir, "/tmp/test-proj");
            assert!(url.contains("my-app.test"));
        }
        other => panic!("unexpected: {other:?}"),
    }

    drop(w);
    h.await.unwrap().unwrap();
}

#[tokio::test]
async fn register_app_starts_workflow_engine_when_worker_command_provided() {
    // Project layout: workflows/ + a fake worker entrypoint + fake bun.
    // We use `true` (exits 0) as the worker command so `ensure()` succeeds
    // without actually running a real worker.
    let proj = tempfile::TempDir::new().unwrap();
    std::fs::create_dir_all(proj.path().join("workflows")).unwrap();
    std::fs::write(
        proj.path().join("workflows").join("broadcast.ts"),
        "export default () => {};",
    )
    .unwrap();

    let (a, b) = tokio::net::UnixStream::pair().unwrap();
    let (shutdown_tx, _shutdown_rx) = watch::channel(false);

    let workflows_dir = tempfile::TempDir::new().unwrap();
    let workflows = Arc::new(tako_workflows::WorkflowManager::new(workflows_dir.path()));
    workflows.start_socket().unwrap();
    let internal_socket = workflows.socket_path();

    let st = State::new(
        shutdown_tx,
        proxy::Routes::default(),
        EventsHub::default(),
        true,
        53535,
        8443,
        "127.0.0.1:8443".to_string(),
        "127.0.0.1".to_string(),
    );
    let state = Arc::new(Mutex::new(st));
    {
        let mut s = state.lock().unwrap();
        s.workflows = Some(workflows.clone());
        s.internal_socket = Some(internal_socket);
    }
    let h = tokio::spawn({
        let state = state.clone();
        async move { handle_client(a, state).await }
    });

    let (r, mut w) = b.into_split();
    let mut lines = BufReader::new(r).lines();

    let config_path = proj.path().join("tako.toml").to_string_lossy().to_string();
    let req = serde_json::json!({
        "type": "RegisterApp",
        "config_path": config_path,
        "project_dir": proj.path().to_string_lossy(),
        "app_name": "wf-app",
        "hosts": ["wf-app.test"],
        "command": ["node", "index.js"],
        "env": {},
        "worker_command": ["true"],
    });
    w.write_all(req.to_string().as_bytes()).await.unwrap();
    w.write_all(b"\n").await.unwrap();

    let reg_line = lines.next_line().await.unwrap().unwrap();
    let reg: Response = serde_json::from_str(&reg_line).unwrap();
    assert!(matches!(reg, Response::AppRegistered { .. }));

    assert!(workflows.has("wf-app"));

    drop(w);
    h.await.unwrap().unwrap();
    workflows.shutdown_all(Duration::from_secs(1)).await;
}

#[tokio::test]
async fn register_app_skips_workflow_engine_when_no_workflows_dir() {
    let proj = tempfile::TempDir::new().unwrap();

    let (a, b) = tokio::net::UnixStream::pair().unwrap();
    let (shutdown_tx, _shutdown_rx) = watch::channel(false);

    let workflows_dir = tempfile::TempDir::new().unwrap();
    let workflows = Arc::new(tako_workflows::WorkflowManager::new(workflows_dir.path()));
    workflows.start_socket().unwrap();

    let st = State::new(
        shutdown_tx,
        proxy::Routes::default(),
        EventsHub::default(),
        true,
        53535,
        8443,
        "127.0.0.1:8443".to_string(),
        "127.0.0.1".to_string(),
    );
    let state = Arc::new(Mutex::new(st));
    let h = tokio::spawn(async move { handle_client(a, state).await });

    let (r, mut w) = b.into_split();
    let mut lines = BufReader::new(r).lines();

    let config_path = proj.path().join("tako.toml").to_string_lossy().to_string();
    let req = serde_json::json!({
        "type": "RegisterApp",
        "config_path": config_path,
        "project_dir": proj.path().to_string_lossy(),
        "app_name": "plain-app",
        "hosts": ["plain.test"],
        "command": ["node", "index.js"],
        "env": {}
    });
    w.write_all(req.to_string().as_bytes()).await.unwrap();
    w.write_all(b"\n").await.unwrap();

    let _reg_line = lines.next_line().await.unwrap().unwrap();
    assert!(!workflows.has("plain-app"));

    drop(w);
    h.await.unwrap().unwrap();
    workflows.shutdown_all(Duration::from_secs(1)).await;
}

#[tokio::test]
async fn info_reports_connected_control_clients() {
    let (a, b) = tokio::net::UnixStream::pair().unwrap();
    let (shutdown_tx, _shutdown_rx) = watch::channel(false);
    let st = State::new(
        shutdown_tx,
        proxy::Routes::default(),
        EventsHub::default(),
        true,
        53535,
        8443,
        "127.0.0.1:8443".to_string(),
        "127.0.0.1".to_string(),
    );
    let _ = test_workflows();
    let state = Arc::new(Mutex::new(st));
    let h = tokio::spawn({
        let state = state.clone();
        async move { handle_client(a, state).await }
    });

    let (r, mut w) = b.into_split();
    let mut lines = BufReader::new(r).lines();
    w.write_all(b"{\"type\":\"SubscribeEvents\"}\n")
        .await
        .unwrap();

    let sub_line = lines.next_line().await.unwrap().unwrap();
    let sub_resp: Response = serde_json::from_str(&sub_line).unwrap();
    assert!(matches!(sub_resp, Response::Subscribed));

    let clients = query_control_clients(state.clone()).await;
    assert_eq!(clients, 1);

    drop(lines);
    drop(w);

    tokio::time::timeout(Duration::from_secs(1), h)
        .await
        .expect("subscribe handler should exit")
        .unwrap()
        .unwrap();

    let clients = query_control_clients(state).await;
    assert_eq!(clients, 0);
}

/// Helper: create a test State with a temp SQLite DB and return (state, _tmpdir).
fn test_state() -> (Arc<Mutex<State>>, tempfile::TempDir) {
    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = tmp.path().join("dev-server.db");
    let db = state::DevStateStore::open(db_path).unwrap();

    let (shutdown_tx, _shutdown_rx) = watch::channel(false);
    let mut st = State::new(
        shutdown_tx,
        proxy::Routes::default(),
        EventsHub::default(),
        true,
        53535,
        8443,
        "127.0.0.1:8443".to_string(),
        "127.0.0.1".to_string(),
    );
    let _ = test_workflows();
    st.db = Some(db);
    (Arc::new(Mutex::new(st)), tmp)
}

fn insert_test_app(state: &Arc<Mutex<State>>, project_dir: &str, name: &str) {
    let config_path = format!("{project_dir}/tako.toml");
    let mut s = state.lock().unwrap();
    s.apps.insert(
        config_path.clone(),
        state::RuntimeApp {
            project_dir: project_dir.to_string(),
            name: name.to_string(),
            variant: None,
            hosts: vec![format!("{name}.test")],
            upstream_port: 3000,
            is_idle: false,
            command: vec!["bun".to_string()],
            env: std::collections::HashMap::new(),
            log_buffer: state::LogBuffer::new(),
            pid: None,
            client_pid: None,
            readiness_failure_hint: None,
            bootstrap_token: "dev-token".to_string(),
            image_secret: "dev-image-secret".to_string(),
        },
    );
    s.routes.set_routes_with_image_secret(
        format!("reg:{config_path}"),
        vec![format!("{name}.test")],
        3000,
        true,
        "dev-image-secret".to_string(),
    );
}

#[tokio::test]
async fn unregister_app_broadcasts_stopped_event_to_subscriber() {
    let (state, _tmp) = test_state();
    insert_test_app(&state, "/proj", "my-app");

    // Subscribe to events.
    let mut ev_rx = {
        let s = state.lock().unwrap();
        s.events.subscribe()
    };

    // Unregister the app on a client connection.
    let (a, b) = tokio::net::UnixStream::pair().unwrap();
    let state_for_handler = state.clone();
    let h = tokio::spawn(async move { handle_client(a, state_for_handler).await });

    let (r, mut w) = b.into_split();
    let req = serde_json::json!({
        "type": "UnregisterApp",
        "config_path": "/proj/tako.toml",
    });
    w.write_all(req.to_string().as_bytes()).await.unwrap();
    w.write_all(b"\n").await.unwrap();

    let mut lines = BufReader::new(r).lines();
    let line = lines.next_line().await.unwrap().unwrap();
    let resp: Response = serde_json::from_str(&line).unwrap();
    assert!(matches!(resp, Response::AppUnregistered { .. }));

    drop(w);
    drop(lines);
    h.await.unwrap().unwrap();

    // The subscriber should have received an AppStatusChanged event.
    let event = tokio::time::timeout(Duration::from_millis(100), ev_rx.recv())
        .await
        .expect("should not time out")
        .unwrap();

    match event {
        Response::Event {
            event:
                protocol::DevEvent::AppStatusChanged {
                    config_path,
                    app_name,
                    status,
                },
        } => {
            assert_eq!(config_path, "/proj/tako.toml");
            assert_eq!(app_name, "my-app");
            assert_eq!(status, "stopped");
        }
        other => panic!("expected AppStatusChanged, got: {other:?}"),
    }
}

#[tokio::test]
async fn restart_app_responds_with_app_restarting() {
    let (state, _tmp) = test_state();
    insert_test_app(&state, "/proj", "my-app");

    // Send RestartApp.
    let (a, b) = tokio::net::UnixStream::pair().unwrap();
    let state_for_handler = state.clone();
    let h = tokio::spawn(async move { handle_client(a, state_for_handler).await });

    let (r, mut w) = b.into_split();
    let req = serde_json::json!({
        "type": "RestartApp",
        "config_path": "/proj/tako.toml",
    });
    w.write_all(req.to_string().as_bytes()).await.unwrap();
    w.write_all(b"\n").await.unwrap();

    let mut lines = BufReader::new(r).lines();
    let line = lines.next_line().await.unwrap().unwrap();
    let resp: Response = serde_json::from_str(&line).unwrap();
    match resp {
        Response::AppRestarting { config_path } => {
            assert_eq!(config_path, "/proj/tako.toml");
        }
        other => panic!("expected AppRestarting, got: {other:?}"),
    }

    drop(w);
    drop(lines);
    h.await.unwrap().unwrap();
}

#[tokio::test]
async fn subscribe_logs_streams_backlog_and_live_entries() {
    let (state, _tmp) = test_state();
    insert_test_app(&state, "/proj", "my-app");

    // Push some entries to the log buffer before subscribing.
    {
        let s = state.lock().unwrap();
        let app = s.apps.get("/proj/tako.toml").unwrap();
        app.log_buffer.push(
            r#"{"timestamp":"00:00:01","level":"Info","scope":"app","message":"line-1"}"#
                .to_string(),
        );
        app.log_buffer.push(
            r#"{"timestamp":"00:00:02","level":"Info","scope":"app","message":"line-2"}"#
                .to_string(),
        );
    }

    let (a, b) = tokio::net::UnixStream::pair().unwrap();
    let state_for_handler = state.clone();
    let h = tokio::spawn(async move { handle_client(a, state_for_handler).await });

    let (r, mut w) = b.into_split();
    let mut lines = BufReader::new(r).lines();

    let req = serde_json::json!({
        "type": "SubscribeLogs",
        "config_path": "/proj/tako.toml",
    });
    w.write_all(req.to_string().as_bytes()).await.unwrap();
    w.write_all(b"\n").await.unwrap();

    // First response: LogsSubscribed
    let line = lines.next_line().await.unwrap().unwrap();
    let resp: Response = serde_json::from_str(&line).unwrap();
    assert!(matches!(resp, Response::LogsSubscribed));

    // Next: two backlog entries
    let line = lines.next_line().await.unwrap().unwrap();
    let resp: Response = serde_json::from_str(&line).unwrap();
    match resp {
        Response::LogEntry { id, line } => {
            assert_eq!(id, 0);
            assert!(line.contains("line-1"));
        }
        other => panic!("expected LogEntry, got: {other:?}"),
    }

    let line = lines.next_line().await.unwrap().unwrap();
    let resp: Response = serde_json::from_str(&line).unwrap();
    match resp {
        Response::LogEntry { id, line } => {
            assert_eq!(id, 1);
            assert!(line.contains("line-2"));
        }
        other => panic!("expected LogEntry, got: {other:?}"),
    }

    // Push a live entry while subscribed.
    {
        let s = state.lock().unwrap();
        let app = s.apps.get("/proj/tako.toml").unwrap();
        app.log_buffer.push(
            r#"{"timestamp":"00:00:03","level":"Info","scope":"app","message":"line-3"}"#
                .to_string(),
        );
    }

    let line = lines.next_line().await.unwrap().unwrap();
    let resp: Response = serde_json::from_str(&line).unwrap();
    match resp {
        Response::LogEntry { id, line } => {
            assert_eq!(id, 2);
            assert!(line.contains("line-3"));
        }
        other => panic!("expected LogEntry, got: {other:?}"),
    }

    drop(w);
    drop(lines);
    h.await.unwrap().unwrap();
}

#[tokio::test]
async fn subscribe_logs_returns_error_for_unknown_app() {
    let (state, _tmp) = test_state();

    let (a, b) = tokio::net::UnixStream::pair().unwrap();
    let state_for_handler = state.clone();
    let h = tokio::spawn(async move { handle_client(a, state_for_handler).await });

    let (r, mut w) = b.into_split();
    let mut lines = BufReader::new(r).lines();

    let req = serde_json::json!({
        "type": "SubscribeLogs",
        "config_path": "/nonexistent/tako.toml",
    });
    w.write_all(req.to_string().as_bytes()).await.unwrap();
    w.write_all(b"\n").await.unwrap();

    let line = lines.next_line().await.unwrap().unwrap();
    let resp: Response = serde_json::from_str(&line).unwrap();
    assert!(matches!(resp, Response::Error { .. }));

    drop(w);
    h.await.unwrap().unwrap();
}

#[tokio::test]
async fn subscribe_logs_counts_as_control_client() {
    let (state, _tmp) = test_state();
    insert_test_app(&state, "/proj", "my-app");

    let (a, b) = tokio::net::UnixStream::pair().unwrap();
    let state_for_handler = state.clone();
    let h = tokio::spawn(async move { handle_client(a, state_for_handler).await });

    let (r, mut w) = b.into_split();
    let mut lines = BufReader::new(r).lines();

    let req = serde_json::json!({
        "type": "SubscribeLogs",
        "config_path": "/proj/tako.toml",
    });
    w.write_all(req.to_string().as_bytes()).await.unwrap();
    w.write_all(b"\n").await.unwrap();

    let line = lines.next_line().await.unwrap().unwrap();
    assert!(line.contains("LogsSubscribed"));

    // While subscribed, control_clients should be 1.
    let clients = query_control_clients(state.clone()).await;
    assert_eq!(clients, 1);

    // Disconnect.
    drop(w);
    drop(lines);
    h.await.unwrap().unwrap();

    // After disconnect, control_clients should be 0.
    let clients = query_control_clients(state).await;
    assert_eq!(clients, 0);
}

#[tokio::test]
async fn set_app_status_broadcasts_status_changed_event() {
    let (state, _tmp) = test_state();
    insert_test_app(&state, "/proj", "my-app");

    let mut ev_rx = {
        let s = state.lock().unwrap();
        s.events.subscribe()
    };

    // Send SetAppStatus.
    let (a, b) = tokio::net::UnixStream::pair().unwrap();
    let state_for_handler = state.clone();
    let h = tokio::spawn(async move { handle_client(a, state_for_handler).await });

    let (r, mut w) = b.into_split();
    let req = serde_json::json!({
        "type": "SetAppStatus",
        "config_path": "/proj/tako.toml",
        "status": "idle",
    });
    w.write_all(req.to_string().as_bytes()).await.unwrap();
    w.write_all(b"\n").await.unwrap();

    let mut lines = BufReader::new(r).lines();
    let line = lines.next_line().await.unwrap().unwrap();
    let resp: Response = serde_json::from_str(&line).unwrap();
    assert!(matches!(resp, Response::AppStatusUpdated { .. }));

    drop(w);
    drop(lines);
    h.await.unwrap().unwrap();

    let event = tokio::time::timeout(Duration::from_millis(100), ev_rx.recv())
        .await
        .expect("should not time out")
        .unwrap();

    match event {
        Response::Event {
            event:
                protocol::DevEvent::AppStatusChanged {
                    config_path,
                    app_name,
                    status,
                },
        } => {
            assert_eq!(config_path, "/proj/tako.toml");
            assert_eq!(app_name, "my-app");
            assert_eq!(status, "idle");
        }
        other => panic!("expected AppStatusChanged, got: {other:?}"),
    }
}

#[test]
fn redirect_location_strips_default_http_port() {
    let location = redirect_location("bun-example.test:80", "/hello");
    assert_eq!(location, "https://bun-example.test/hello");
}

#[test]
fn redirect_location_keeps_non_default_port() {
    let location = redirect_location("bun-example.test:8080", "/");
    assert_eq!(location, "https://bun-example.test:8080/");
}

#[test]
fn ensure_tcp_listener_can_bind_succeeds_when_port_is_available() {
    // On busy CI hosts, another process can race us for a just-freed port.
    // Retry a few times with fresh ephemeral ports to keep this deterministic.
    for _ in 0..8 {
        let Ok(listener) = std::net::TcpListener::bind(("127.0.0.1", 0)) else {
            return;
        };
        let addr = listener.local_addr().unwrap();
        drop(listener);
        if ensure_tcp_listener_can_bind(&addr.to_string()).is_ok() {
            return;
        }
    }
    panic!("failed to find an available loopback port after retries");
}

/// End-to-end test: client B subscribes to events via a real socket
/// handler, client A unregisters an app via a separate socket handler,
/// and client B must receive the AppStatusChanged{stopped} event over
/// the wire. This exercises the exact codepath that the connected dev
/// client uses to detect when the owner stops the app.
#[tokio::test]
async fn subscriber_receives_stopped_event_over_socket_when_app_unregistered() {
    let (state, _tmp) = test_state();
    insert_test_app(&state, "/proj", "my-app");

    // Client B: subscribe to events via a real socket handler.
    let (sub_a, sub_b) = tokio::net::UnixStream::pair().unwrap();
    let sub_handler = tokio::spawn({
        let state = state.clone();
        async move { handle_client(sub_a, state).await }
    });
    let (sub_r, mut sub_w) = sub_b.into_split();
    let mut sub_lines = BufReader::new(sub_r).lines();

    sub_w
        .write_all(b"{\"type\":\"SubscribeEvents\"}\n")
        .await
        .unwrap();
    let resp_line = sub_lines.next_line().await.unwrap().unwrap();
    let resp: Response = serde_json::from_str(&resp_line).unwrap();
    assert!(matches!(resp, Response::Subscribed));

    // Client A: unregister the app via a separate socket handler.
    let (unreg_a, unreg_b) = tokio::net::UnixStream::pair().unwrap();
    let unreg_handler = tokio::spawn({
        let state = state.clone();
        async move { handle_client(unreg_a, state).await }
    });
    let (unreg_r, mut unreg_w) = unreg_b.into_split();

    let req = serde_json::json!({
        "type": "UnregisterApp",
        "config_path": "/proj/tako.toml",
    });
    unreg_w
        .write_all(format!("{}\n", req).as_bytes())
        .await
        .unwrap();

    let mut unreg_lines = BufReader::new(unreg_r).lines();
    let unreg_resp_line = unreg_lines.next_line().await.unwrap().unwrap();
    let unreg_resp: Response = serde_json::from_str(&unreg_resp_line).unwrap();
    assert!(matches!(unreg_resp, Response::AppUnregistered { .. }));

    // Clean up unregister handler.
    drop(unreg_w);
    drop(unreg_lines);
    unreg_handler.await.unwrap().unwrap();

    // Client B should receive the AppStatusChanged event.
    let event_line = tokio::time::timeout(Duration::from_millis(500), sub_lines.next_line())
        .await
        .expect("subscriber should receive event within 500ms")
        .unwrap()
        .unwrap();
    let event_resp: Response = serde_json::from_str(&event_line).unwrap();
    match event_resp {
        Response::Event {
            event:
                protocol::DevEvent::AppStatusChanged {
                    config_path,
                    app_name,
                    status,
                },
        } => {
            assert_eq!(config_path, "/proj/tako.toml");
            assert_eq!(app_name, "my-app");
            assert_eq!(status, "stopped");
        }
        other => panic!("expected AppStatusChanged stopped, got: {other:?}"),
    }

    // Clean up subscriber.
    drop(sub_w);
    drop(sub_lines);
    let _ = tokio::time::timeout(Duration::from_secs(1), sub_handler).await;
}

#[test]
fn ensure_tcp_listener_can_bind_reports_error_when_port_in_use() {
    let Ok(listener) = std::net::TcpListener::bind(("127.0.0.1", 0)) else {
        return;
    };
    let addr = listener.local_addr().unwrap();
    let err = ensure_tcp_listener_can_bind(&addr.to_string())
        .unwrap_err()
        .to_string();
    assert!(err.contains("dev proxy could not bind on"));
    assert!(err.contains(&addr.to_string()));
    drop(listener);
}

/// Verify that the dynamic cert resolver generates a cert whose SAN
/// exactly matches the requested hostname — this is how we sidestep
/// OpenSSL rejecting `*.tako` wildcards (single-label TLD).
#[test]
fn dev_cert_resolver_generates_cert_matching_hostname() {
    let ca = LocalCA::generate().unwrap();
    let resolver = DevCertResolver::new(ca);

    let (x509, _pkey) = resolver
        .get_or_create_cert("foo.test")
        .expect("should generate cert");

    // Verify the SAN contains the exact hostname.
    let pem = x509.to_pem().unwrap();
    let (_, parsed_pem) = x509_parser::pem::parse_x509_pem(&pem).unwrap();
    let parsed = parsed_pem.parse_x509().unwrap();

    let san_ext = parsed
        .extensions()
        .iter()
        .find(|ext| ext.oid == x509_parser::oid_registry::OID_X509_EXT_SUBJECT_ALT_NAME)
        .expect("cert must have SAN extension");

    let san = match san_ext.parsed_extension() {
        x509_parser::extensions::ParsedExtension::SubjectAlternativeName(san) => san,
        other => panic!("expected SubjectAlternativeName, got {:?}", other),
    };

    let dns_names: Vec<&str> = san
        .general_names
        .iter()
        .filter_map(|n| match n {
            x509_parser::extensions::GeneralName::DNSName(d) => Some(*d),
            _ => None,
        })
        .collect();

    assert!(
        dns_names.contains(&"foo.test"),
        "cert must contain foo.test SAN, got: {:?}",
        dns_names
    );
}

/// Verify that the dynamically generated cert chains back to the CA
/// and that the SAN exactly matches — these are the two checks that
/// Chrome/BoringSSL performs during the TLS handshake.
#[test]
fn dev_cert_resolver_cert_is_signed_by_ca() {
    let ca = LocalCA::generate().unwrap();
    let ca_x509 = X509::from_pem(ca.ca_cert_pem().as_bytes()).unwrap();
    let resolver = DevCertResolver::new(ca);

    let (leaf_x509, _) = resolver
        .get_or_create_cert("foo.test")
        .expect("should generate cert");

    // Verify the leaf cert is signed by the CA's public key.
    let ca_pubkey = ca_x509.public_key().unwrap();
    assert!(
        leaf_x509.verify(&ca_pubkey).unwrap(),
        "leaf cert must be signed by the local CA"
    );
}

#[test]
fn dev_cert_resolver_caches_certs() {
    let ca = LocalCA::generate().unwrap();
    let resolver = DevCertResolver::new(ca);

    let (first, _) = resolver.get_or_create_cert("bar.test").unwrap();
    let (second, _) = resolver.get_or_create_cert("bar.test").unwrap();

    // Same DER bytes → same cert object was returned from cache.
    assert_eq!(first.to_der().unwrap(), second.to_der().unwrap());
}

#[test]
fn kill_all_app_processes_sends_sigterm_to_tracked_pids() {
    let (state, _tmp) = test_state();

    // Spawn a long-lived process we can check.
    let mut child = std::process::Command::new("sleep")
        .arg("60")
        .spawn()
        .unwrap();
    let pid = child.id();

    // Register it in memory with a PID.
    {
        let mut s = state.lock().unwrap();
        s.apps.insert(
            "/proj/tako.toml".to_string(),
            state::RuntimeApp {
                project_dir: "/proj".to_string(),
                name: "my-app".to_string(),
                variant: None,
                hosts: vec!["my-app.test".to_string()],
                upstream_port: 3000,
                is_idle: false,
                command: vec!["sleep".to_string(), "60".to_string()],
                env: std::collections::HashMap::new(),
                log_buffer: state::LogBuffer::new(),
                pid: Some(pid),
                client_pid: None,
                readiness_failure_hint: None,
                bootstrap_token: "dev-token".to_string(),
                image_secret: "dev-image-secret".to_string(),
            },
        );
    }

    // Verify the process is alive.
    assert_eq!(unsafe { libc::kill(pid as i32, 0) }, 0);

    kill_all_app_processes(&state);

    // wait() will return once the process has been terminated by SIGTERM.
    let status = child.wait().unwrap();
    assert!(!status.success());
}

// -----------------------------------------------------------------------
// Variant support
// -----------------------------------------------------------------------

#[tokio::test]
async fn register_app_with_variant_roundtrip() {
    let (a, b) = tokio::net::UnixStream::pair().unwrap();
    let (state, _tmp) = test_state();
    let h = tokio::spawn(async move { handle_client(a, state).await });

    let (r, mut w) = b.into_split();
    let mut lines = BufReader::new(r).lines();

    let req = serde_json::json!({
        "type": "RegisterApp",
        "config_path": "/proj/my-app/preview.toml",
        "project_dir": "/proj/my-app",
        "app_name": "my-app-staging",
        "variant": "staging",
        "hosts": ["my-app-staging.test"],
        "upstream_port": 3000,
        "command": ["bun", "run", "index.ts"],
        "env": {},
        "log_path": "/tmp/log.jsonl"
    });
    w.write_all(req.to_string().as_bytes()).await.unwrap();
    w.write_all(b"\n").await.unwrap();

    let line = lines.next_line().await.unwrap().unwrap();
    let resp: Response = serde_json::from_str(&line).unwrap();
    match resp {
        Response::AppRegistered { app_name, .. } => {
            assert_eq!(app_name, "my-app-staging");
        }
        other => panic!("unexpected: {other:?}"),
    }

    // List and verify variant is present.
    let req = serde_json::json!({"type": "ListRegisteredApps"});
    w.write_all(req.to_string().as_bytes()).await.unwrap();
    w.write_all(b"\n").await.unwrap();

    let line = lines.next_line().await.unwrap().unwrap();
    let resp: Response = serde_json::from_str(&line).unwrap();
    match resp {
        Response::RegisteredApps { apps } => {
            assert_eq!(apps.len(), 1);
            assert_eq!(apps[0].app_name, "my-app-staging");
            assert_eq!(apps[0].variant, Some("staging".to_string()));
        }
        other => panic!("unexpected: {other:?}"),
    }

    drop(w);
    h.await.unwrap().unwrap();
}

#[tokio::test]
async fn register_app_without_variant_has_none() {
    let (a, b) = tokio::net::UnixStream::pair().unwrap();
    let (state, _tmp) = test_state();
    let h = tokio::spawn(async move { handle_client(a, state).await });

    let (r, mut w) = b.into_split();
    let mut lines = BufReader::new(r).lines();

    let req = serde_json::json!({
        "type": "RegisterApp",
        "config_path": "/proj/my-app/tako.toml",
        "project_dir": "/proj/my-app",
        "app_name": "my-app",
        "hosts": ["my-app.test"],
        "upstream_port": 3000,
        "command": ["bun", "run", "index.ts"],
        "env": {},
        "log_path": "/tmp/log.jsonl"
    });
    w.write_all(req.to_string().as_bytes()).await.unwrap();
    w.write_all(b"\n").await.unwrap();

    let _line = lines.next_line().await.unwrap().unwrap();

    let req = serde_json::json!({"type": "ListRegisteredApps"});
    w.write_all(req.to_string().as_bytes()).await.unwrap();
    w.write_all(b"\n").await.unwrap();

    let line = lines.next_line().await.unwrap().unwrap();
    let resp: Response = serde_json::from_str(&line).unwrap();
    match resp {
        Response::RegisteredApps { apps } => {
            assert_eq!(apps.len(), 1);
            assert_eq!(apps[0].app_name, "my-app");
            assert!(apps[0].variant.is_none());
        }
        other => panic!("unexpected: {other:?}"),
    }

    drop(w);
    h.await.unwrap().unwrap();
}

#[tokio::test]
async fn variant_and_non_variant_coexist_in_list() {
    let (a, b) = tokio::net::UnixStream::pair().unwrap();
    let (state, _tmp) = test_state();
    let h = tokio::spawn(async move { handle_client(a, state).await });

    let (r, mut w) = b.into_split();
    let mut lines = BufReader::new(r).lines();

    // Register "app-foo" without variant from /proj1
    let req = serde_json::json!({
        "type": "RegisterApp",
        "config_path": "/proj1/tako.toml",
        "project_dir": "/proj1",
        "app_name": "app-foo",
        "hosts": ["app-foo.test"],
        "upstream_port": 3000,
        "command": ["bun", "run", "index.ts"],
        "env": {},
        "log_path": "/tmp/log1.jsonl"
    });
    w.write_all(req.to_string().as_bytes()).await.unwrap();
    w.write_all(b"\n").await.unwrap();
    let _line = lines.next_line().await.unwrap().unwrap();

    // Register "app-foo" with variant "foo" from /proj2
    // (in practice the CLI would have disambiguated the name, but
    //  this tests that both can coexist with different project_dirs)
    let req = serde_json::json!({
        "type": "RegisterApp",
        "config_path": "/proj2/tako.toml",
        "project_dir": "/proj2",
        "app_name": "app-foo-proj2",
        "variant": "foo",
        "hosts": ["app-foo-proj2.test"],
        "upstream_port": 3001,
        "command": ["bun", "run", "index.ts"],
        "env": {},
        "log_path": "/tmp/log2.jsonl"
    });
    w.write_all(req.to_string().as_bytes()).await.unwrap();
    w.write_all(b"\n").await.unwrap();
    let _line = lines.next_line().await.unwrap().unwrap();

    // List all
    let req = serde_json::json!({"type": "ListRegisteredApps"});
    w.write_all(req.to_string().as_bytes()).await.unwrap();
    w.write_all(b"\n").await.unwrap();

    let line = lines.next_line().await.unwrap().unwrap();
    let resp: Response = serde_json::from_str(&line).unwrap();
    match resp {
        Response::RegisteredApps { apps } => {
            assert_eq!(apps.len(), 2);
            let no_variant = apps.iter().find(|a| a.project_dir == "/proj1").unwrap();
            let with_variant = apps.iter().find(|a| a.project_dir == "/proj2").unwrap();
            assert_eq!(no_variant.app_name, "app-foo");
            assert!(no_variant.variant.is_none());
            assert_eq!(with_variant.app_name, "app-foo-proj2");
            assert_eq!(with_variant.variant, Some("foo".to_string()));
        }
        other => panic!("unexpected: {other:?}"),
    }

    drop(w);
    h.await.unwrap().unwrap();
}

#[tokio::test]
async fn register_app_spawns_process_and_sets_pid() {
    let tmp = tempfile::TempDir::new().unwrap();
    let (state, _tmp_db) = test_state();

    let (a, b) = tokio::net::UnixStream::pair().unwrap();
    let state_for_handler = state.clone();
    let h = tokio::spawn(async move { handle_client(a, state_for_handler).await });

    let (r, mut w) = b.into_split();
    let mut lines = BufReader::new(r).lines();

    let req = serde_json::json!({
        "type": "RegisterApp",
        "config_path": "/tmp/test-spawn/tako.toml",
        "project_dir": tmp.path().to_str().unwrap(),
        "app_name": "spawn-test",
        "hosts": ["spawn-test.test"],
        "upstream_port": 19999,
        "command": ["sleep", "60"],
        "env": {},
    });
    w.write_all(format!("{}\n", req).as_bytes()).await.unwrap();

    let line = lines.next_line().await.unwrap().unwrap();
    let resp: Response = serde_json::from_str(&line).unwrap();
    assert!(matches!(resp, Response::AppRegistered { .. }));

    // Wait a moment for the background spawn task.
    tokio::time::sleep(Duration::from_millis(200)).await;

    // The app should now have a PID set by the daemon.
    let pid = {
        let s = state.lock().unwrap();
        s.apps.get("/tmp/test-spawn/tako.toml").and_then(|a| a.pid)
    };
    assert!(pid.is_some(), "daemon should have spawned the app process");

    // Clean up: kill the spawned process.
    if let Some(pid) = pid {
        kill_app_process(pid);
    }

    drop(w);
    drop(lines);
    h.await.unwrap().unwrap();
}

#[tokio::test]
async fn unregister_app_kills_running_process() {
    let tmp = tempfile::TempDir::new().unwrap();
    let (state, _tmp_db) = test_state();

    // Insert an app with a real running process.
    let mut child = std::process::Command::new("sleep")
        .arg("60")
        .spawn()
        .unwrap();
    let pid = child.id();

    {
        let mut s = state.lock().unwrap();
        s.apps.insert(
            "/tmp/test-kill/tako.toml".to_string(),
            state::RuntimeApp {
                project_dir: tmp.path().to_string_lossy().to_string(),
                name: "kill-test".to_string(),
                variant: None,
                hosts: vec!["kill-test.test".to_string()],
                upstream_port: 19998,
                is_idle: false,
                command: vec!["sleep".to_string(), "60".to_string()],
                env: std::collections::HashMap::new(),
                log_buffer: state::LogBuffer::new(),
                pid: Some(pid),
                client_pid: None,
                readiness_failure_hint: None,
                bootstrap_token: "dev-token".to_string(),
                image_secret: "dev-image-secret".to_string(),
            },
        );
    }

    // Unregister via socket.
    let (a, b) = tokio::net::UnixStream::pair().unwrap();
    let state_for_handler = state.clone();
    let h = tokio::spawn(async move { handle_client(a, state_for_handler).await });

    let (r, mut w) = b.into_split();
    let req = serde_json::json!({
        "type": "UnregisterApp",
        "config_path": "/tmp/test-kill/tako.toml",
    });
    w.write_all(format!("{}\n", req).as_bytes()).await.unwrap();

    let mut lines = BufReader::new(r).lines();
    let line = lines.next_line().await.unwrap().unwrap();
    let resp: Response = serde_json::from_str(&line).unwrap();
    assert!(matches!(resp, Response::AppUnregistered { .. }));

    drop(w);
    drop(lines);
    h.await.unwrap().unwrap();

    // The process should have been killed.
    let status = child.wait().unwrap();
    assert!(!status.success(), "process should have been killed");
}

#[test]
fn variant_persisted_in_sqlite() {
    let (state, _tmp) = test_state();
    let s = state.lock().unwrap();
    let db = s.db.as_ref().unwrap();

    db.register("/proj/tako.toml", "/proj", "my-app", Some("staging"))
        .unwrap();
    let app = db.get("/proj/tako.toml").unwrap().unwrap();
    assert_eq!(app.name, "my-app");
    assert_eq!(app.variant.as_deref(), Some("staging"));

    // Re-register without variant clears it.
    db.register("/proj/tako.toml", "/proj", "my-app", None)
        .unwrap();
    let app = db.get("/proj/tako.toml").unwrap().unwrap();
    assert!(app.variant.is_none());
}

/// Concurrent wake-on-request calls must spawn exactly one process.
///
/// Before the fix, all callers raced through the `is_idle` check and each
/// proceeded to spawn, producing N processes for N concurrent requests.
/// After the fix the check-and-clear is atomic under the same lock, so only
/// the first caller spawns; the rest bail out immediately.
#[tokio::test]
async fn wake_on_request_spawns_exactly_one_process() {
    let tmp = tempfile::TempDir::new().unwrap();
    let (state, _tmp_db) = test_state();

    // Directory where each spawn attempt writes a marker file.
    let spawn_dir = tmp.path().join("spawns");
    std::fs::create_dir_all(&spawn_dir).unwrap();

    // The command touches a file named after its own PID, then sleeps.
    // Each distinct spawn produces a distinct file.
    let cmd_str = format!("touch {}/$$; sleep 60", spawn_dir.display());
    let config_path = format!("{}/tako.toml", tmp.path().display());

    {
        let mut s = state.lock().unwrap();
        s.apps.insert(
            config_path.clone(),
            state::RuntimeApp {
                project_dir: tmp.path().to_str().unwrap().to_string(),
                name: "wake-race-test".to_string(),
                variant: None,
                hosts: vec!["wake-race.test".to_string()],
                upstream_port: 0,
                is_idle: true,
                command: vec!["sh".to_string(), "-c".to_string(), cmd_str],
                env: std::collections::HashMap::new(),
                log_buffer: state::LogBuffer::new(),
                pid: None,
                client_pid: None,
                readiness_failure_hint: None,
                bootstrap_token: "dev-token".to_string(),
                image_secret: "dev-image-secret".to_string(),
            },
        );
        s.routes.set_routes(
            format!("reg:{config_path}"),
            vec!["wake-race.test".to_string()],
            0,
            false,
        );
    }

    // Fire 10 concurrent wake tasks — only one should win the spawn race.
    let handles: Vec<_> = (0..10)
        .map(|_| {
            let state = state.clone();
            tokio::spawn(async move {
                handle_wake_on_request(state, "wake-race.test".to_string(), "/".to_string()).await;
            })
        })
        .collect();

    // Give the tasks time to acquire the lock and make their decision.
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Kill the single spawned process so the readiness wait unblocks.
    let pid = state
        .lock()
        .unwrap()
        .apps
        .get(&config_path)
        .and_then(|a| a.pid);
    if let Some(pid) = pid {
        kill_app_process(pid);
    }

    for h in handles {
        let _ = tokio::time::timeout(Duration::from_secs(2), h).await;
    }

    // Exactly one marker file means exactly one spawn.
    let spawn_count = std::fs::read_dir(&spawn_dir).unwrap().count();
    assert_eq!(
        spawn_count, 1,
        "expected exactly 1 spawn, got {spawn_count}"
    );
}

#[test]
fn push_user_action_emits_sdk_wire_format_with_kind() {
    let buf = state::LogBuffer::new();
    let (_backlog, mut rx, _truncated) = buf.subscribe(None);

    push_user_action(&buf, "restarted");

    let entry = rx.try_recv().expect("user-action line pushed");
    let v: serde_json::Value =
        serde_json::from_str(&entry.line).expect("user-action line is valid JSON");

    assert_eq!(v.get("scope").and_then(|x| x.as_str()), Some("tako"));
    assert_eq!(v.get("kind").and_then(|x| x.as_str()), Some("restarted"));
    assert_eq!(v.get("level").and_then(|x| x.as_str()), Some("info"));
    let ts = v.get("ts").and_then(|x| x.as_i64()).expect("numeric ts");
    assert!(ts > 0, "ts should be filled with unix millis, got {ts}");
}

#[test]
fn forward_child_log_line_passes_structured_json_through_verbatim() {
    let buf = state::LogBuffer::new();
    let (_backlog, mut rx, _truncated) = buf.subscribe(None);

    let line = r#"{"ts":42,"level":"info","scope":"worker","msg":"hi"}"#;
    forward_child_log_line(&buf, line.to_string(), "info", "worker");

    let entry = rx.try_recv().expect("line forwarded");
    assert_eq!(entry.line, line);
}

#[test]
fn forward_child_log_line_wraps_plain_text_as_scoped_log() {
    let buf = state::LogBuffer::new();
    let (_backlog, mut rx, _truncated) = buf.subscribe(None);

    forward_child_log_line(&buf, "raw worker output".to_string(), "warn", "worker");

    let entry = rx.try_recv().expect("line forwarded");
    let v: serde_json::Value = serde_json::from_str(&entry.line).expect("valid JSON");
    assert_eq!(v.get("scope").and_then(|x| x.as_str()), Some("worker"));
    assert_eq!(v.get("level").and_then(|x| x.as_str()), Some("warn"));
    assert_eq!(
        v.get("msg").and_then(|x| x.as_str()),
        Some("raw worker output")
    );
    let ts = v.get("ts").and_then(|x| x.as_i64()).expect("numeric ts");
    assert!(ts > 0);
}

fn runtime_app_with_env(
    name: &str,
    env: std::collections::HashMap<String, String>,
) -> state::RuntimeApp {
    state::RuntimeApp {
        project_dir: "/tmp/proj".to_string(),
        name: name.to_string(),
        variant: None,
        hosts: vec![format!("{name}.test")],
        upstream_port: 0,
        is_idle: false,
        command: vec!["bun".to_string()],
        env,
        log_buffer: state::LogBuffer::new(),
        pid: None,
        client_pid: None,
        readiness_failure_hint: None,
        bootstrap_token: "dev-token".to_string(),
        image_secret: "dev-image-secret".to_string(),
    }
}

#[test]
fn build_spawn_env_injects_tako_runtime_contract_when_socket_available() {
    // Regression: dev used to spawn apps without TAKO_INTERNAL_SOCKET /
    // TAKO_APP_NAME, so workflow `.enqueue()` blew up only when a user
    // clicked a button. Both must be present whenever the dev-server has
    // a live internal socket.
    let app = runtime_app_with_env("demo", std::collections::HashMap::new());
    let sock = std::path::PathBuf::from("/tmp/tako.sock");

    let env = build_spawn_env(&app, Some(&sock));

    assert_eq!(env.get("TAKO_APP_NAME").map(String::as_str), Some("demo"));
    assert_eq!(
        env.get("TAKO_INTERNAL_SOCKET").map(String::as_str),
        Some("/tmp/tako.sock"),
    );
    assert_eq!(env.get("PORT").map(String::as_str), Some("0"));
    assert_eq!(env.get("HOST").map(String::as_str), Some("127.0.0.1"));
}

#[test]
fn build_spawn_env_omits_socket_but_still_sets_app_name_when_socket_missing() {
    // start_socket can fail (permissions, etc). In that case TAKO_APP_NAME
    // is still informative; TAKO_INTERNAL_SOCKET stays unset so the SDK's
    // fail-early check pairs cleanly.
    let app = runtime_app_with_env("demo", std::collections::HashMap::new());

    let env = build_spawn_env(&app, None);

    assert_eq!(env.get("TAKO_APP_NAME").map(String::as_str), Some("demo"));
    assert!(!env.contains_key("TAKO_INTERNAL_SOCKET"));
}

#[test]
fn build_spawn_env_contract_wins_over_user_env() {
    // User-supplied env must never shadow Tako's wiring. A stray
    // `HOST=0.0.0.0` would make the app unreachable via the proxy; a
    // stray `TAKO_APP_NAME=impostor` would mis-route every RPC.
    let mut user_env = std::collections::HashMap::new();
    user_env.insert("HOST".to_string(), "0.0.0.0".to_string());
    user_env.insert("TAKO_APP_NAME".to_string(), "impostor".to_string());
    user_env.insert(
        "TAKO_INTERNAL_SOCKET".to_string(),
        "/tmp/wrong.sock".to_string(),
    );
    user_env.insert("FOO".to_string(), "bar".to_string());
    let app = runtime_app_with_env("demo", user_env);
    let sock = std::path::PathBuf::from("/tmp/tako.sock");

    let env = build_spawn_env(&app, Some(&sock));

    assert_eq!(env.get("HOST").map(String::as_str), Some("127.0.0.1"));
    assert_eq!(env.get("TAKO_APP_NAME").map(String::as_str), Some("demo"));
    assert_eq!(
        env.get("TAKO_INTERNAL_SOCKET").map(String::as_str),
        Some("/tmp/tako.sock"),
    );
    // Unrelated user env passes through untouched.
    assert_eq!(env.get("FOO").map(String::as_str), Some("bar"));
}
