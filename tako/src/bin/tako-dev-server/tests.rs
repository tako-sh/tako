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

#[tokio::test]
async fn pingora_accepts_and_reads_tcp_connections() {
    use tokio::io::AsyncReadExt;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut client = tokio::net::TcpStream::connect(listener.local_addr().unwrap())
        .await
        .unwrap();
    let (accepted, _) = listener.accept().await.unwrap();
    // Tokio upgrades must preserve Pingora's zero-capacity stream buffers.
    let mut stream = pingora_core::protocols::l4::stream::Stream::from(accepted);
    client.write_all(b"GET").await.unwrap();
    let mut bytes = [0; 3];
    stream.read_exact(&mut bytes).await.unwrap();
    assert_eq!(&bytes, b"GET");
}

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

fn test_state() -> (Arc<Mutex<State>>, tempfile::TempDir) {
    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = tmp.path().join("dev-server.sqlite");
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
            compose: None,
            project_dir: project_dir.to_string(),
            name: name.to_string(),
            variant: None,
            hosts: vec![format!("{name}.test")],
            upstream_port: 3000,
            is_idle: false,
            command: vec!["bun".to_string()],
            worker_command: None,
            workflow_retention_ms: Some(tako_workflows::DEFAULT_RETENTION_MS),
            env: std::collections::HashMap::new(),
            log_buffer: state::LogBuffer::new(),
            pid: None,
            client_pid: None,
            tunnel: None,
            readiness_failure_hint: None,
            bootstrap_token: "dev-token".to_string(),
            secrets: std::collections::HashMap::new(),
            storages: std::collections::HashMap::new(),
        },
    );
    s.routes.set_routes(
        format!("reg:{config_path}"),
        vec![format!("{name}.test")],
        3000,
        true,
    );
}

fn runtime_app_with_env(
    name: &str,
    env: std::collections::HashMap<String, String>,
) -> state::RuntimeApp {
    state::RuntimeApp {
        compose: None,
        project_dir: "/tmp/proj".to_string(),
        name: name.to_string(),
        variant: None,
        hosts: vec![format!("{name}.test")],
        upstream_port: 0,
        is_idle: false,
        command: vec!["bun".to_string()],
        worker_command: None,
        workflow_retention_ms: Some(tako_workflows::DEFAULT_RETENTION_MS),
        env,
        log_buffer: state::LogBuffer::new(),
        pid: None,
        client_pid: None,
        tunnel: None,
        readiness_failure_hint: None,
        bootstrap_token: "dev-token".to_string(),
        secrets: std::collections::HashMap::new(),
        storages: std::collections::HashMap::new(),
    }
}

mod certs;
mod compose;
mod control;
mod logs;
mod network;
mod processes;
mod registration;
mod spawn_env;

#[tokio::test]
async fn network_calls_route_to_registered_apps_with_authenticated_internal_http() {
    use tokio::io::AsyncReadExt;
    let (state, tmp) = test_state();
    insert_test_app(&state, "/tmp/network-mail", "mail");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    state
        .lock()
        .unwrap()
        .routes
        .activate_with_port("reg:/tmp/network-mail/tako.toml", port);
    let backend = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut stream = tokio::io::BufReader::new(stream);
        let mut headers = String::new();
        let mut length = 0;
        loop {
            let mut line = String::new();
            stream.read_line(&mut line).await.unwrap();
            if line == "\r\n" {
                break;
            }
            if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length: ") {
                length = value.trim().parse::<usize>().unwrap();
            }
            headers.push_str(&line.to_ascii_lowercase());
        }
        assert!(headers.starts_with("post /network http/1.1\r\n"));
        assert!(headers.contains("host: mail.tako\r\n"));
        assert!(headers.contains("x-tako-internal-token: dev-token\r\n"));
        let mut body = vec![0; length];
        stream.read_exact(&mut body).await.unwrap();
        let invocation: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(invocation["name"], "send-email");
        assert_eq!(invocation["request"]["payload"]["to"], "a@example.com");
        let body = r#"{"status":"ok","data":{"id":"remote-run","deduplicated":false}}"#;
        stream.get_mut().write_all(format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nX-Tako-Internal-Token: dev-token\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
    });
    let workflows = tako_workflows::WorkflowManager::new(tmp.path());
    crate::network::install(&state, &workflows);
    workflows.start_socket().unwrap();
    let mut socket = tokio::net::UnixStream::connect(workflows.socket_path())
        .await
        .unwrap();
    tako_socket::write_json_line(
        &mut socket,
        &tako_core::Command::NetworkCall {
            app: "web".into(),
            address: "mail.send-email".into(),
            request: tako_core::network::NetworkRequest::Enqueue {
                payload: serde_json::json!({"to":"a@example.com"}),
                opts: Default::default(),
            },
        },
    )
    .await
    .unwrap();
    let response: tako_core::Response = tokio::time::timeout(
        Duration::from_secs(5),
        tako_socket::read_json_line(&mut BufReader::new(socket)),
    )
    .await
    .unwrap()
    .unwrap()
    .unwrap();
    assert_eq!(response.data().unwrap()["id"], "remote-run");
    backend.await.unwrap();
}
