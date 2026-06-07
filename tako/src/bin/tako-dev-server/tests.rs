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
        secrets: std::collections::HashMap::new(),
        storages: std::collections::HashMap::new(),
    }
}

mod certs;
mod control;
mod logs;
mod network;
mod processes;
mod registration;
mod spawn_env;
