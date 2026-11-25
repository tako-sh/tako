use super::*;
use base64::Engine as _;
use tako_compose::ComposeRuntime;

const APP: &str = "sdk/production";
/// The group's workflow database, hosted on the test server.
const WORKFLOWS: &str = "sdk-workflows";

/// A server whose database host is open, as after startup.
async fn state(dir: &Path) -> Arc<ServerState> {
    let state = Arc::new(
        ServerState::new(
            dir.into(),
            Arc::new(CertManager::new(CertManagerConfig {
                cert_dir: dir.join("certs"),
                ..Default::default()
            })),
            None,
            empty_challenge_tokens(),
        )
        .unwrap(),
    );
    state.start_databases(None).await.unwrap();
    state
}

/// A shell SDK stand-in: it answers Tako's SDK check with the group token
/// and reports what Tako injected. Images build from a local base so the test
/// pulls nothing.
const HANDLER: &str = r#"#!/bin/sh
token=$(printf '%s' "$TAKO_BOOTSTRAP_DATA" | sed -n 's/.*"token":"\([^"]*\)".*/\1/p')
read -r method path version
auth=
while IFS= read -r line; do
  line=$(printf '%s' "$line" | tr -d '\r')
  [ -z "$line" ] && break
  case "$line" in
    [Xx]-[Tt]ako-[Ii]nternal-[Tt]oken:*) auth=$(printf '%s' "${line#*:}" | tr -d ' ') ;;
  esac
done
if [ "$path" = /status ]; then
  if [ -n "$token" ] && [ "$auth" = "$token" ]; then
    body='{"status":"healthy"}'
    printf 'HTTP/1.1 200 OK\r\nx-tako-internal-token: %s\r\nContent-Length: %s\r\nConnection: close\r\n\r\n%s' "$token" "${#body}" "$body"
  else
    printf 'HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n'
  fi
  exit 0
fi
secret=$(printf '%s' "$TAKO_BOOTSTRAP_DATA" | sed -n 's/.*"API_KEY":"\([^"]*\)".*/\1/p')
socket=no
[ -S /run/tako/internal.sock ] && socket=yes
body=$(printf '{"TAKO_APP_NAME":"%s","TAKO_INTERNAL_SOCKET":"%s","TAKO_DATA_DIR":"%s","TAKO_BUILD":"%s","ENV":"%s","HOST":"%s","PORT":"%s","TAKO_COMPOSE_SERVICE":"%s","token":"%s","secret":"%s","socket":"%s"}' "$TAKO_APP_NAME" "$TAKO_INTERNAL_SOCKET" "$TAKO_DATA_DIR" "$TAKO_BUILD" "$ENV" "$HOST" "$PORT" "$TAKO_COMPOSE_SERVICE" "$token" "$secret" "$socket")
printf 'HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: %s\r\nConnection: close\r\n\r\n%s' "${#body}" "$body"
"#;

const DOCKERFILE: &str = "FROM docker.io/library/busybox:1.37\nCOPY handler.sh /handler.sh\nRUN chmod 0755 /handler.sh\n";

const COMPOSE: &str = r#"
services:
  web:
    build: .
    command: [sh, -c, 'exec nc -lk -p "$$PORT" -e /handler.sh']
    deploy: {update_config: {monitor: 0s}}
    x-tako: {sdk: true, envs: {production: {route: sdk.localhost}}}
  worker:
    build: .
    command: [sleep, '3600']
    deploy: {update_config: {monitor: 0s}}
    x-tako: {sdk: true}
  plain:
    build: .
    command: [sleep, '3600']
    deploy: {update_config: {monitor: 0s}}
"#;

/// Send one command through the group's socket from the host.
async fn socket_command(path: &Path, app: &str) -> Value {
    socket_send(
        path,
        serde_json::json!({
            "command": "channel_publish", "app": app, "channel": "demo",
            "payload": {"type": "m", "data": 1}
        }),
    )
    .await
}

async fn socket_send(path: &Path, command: Value) -> Value {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
    let stream = tokio::net::UnixStream::connect(path).await.unwrap();
    let (read, mut write) = stream.into_split();
    write
        .write_all(format!("{command}\n").as_bytes())
        .await
        .unwrap();
    let mut line = String::new();
    tokio::io::BufReader::new(read)
        .read_line(&mut line)
        .await
        .unwrap();
    serde_json::from_str(&line).unwrap()
}

async fn deploy(state: &ServerState, dir: &Path, secret: &str) {
    let digest = crate::secret_snapshot::test_file::pin(
        dir,
        APP,
        crate::secret_snapshot::test_file::TestScope {
            app: [("API_KEY".into(), secret.into())].into(),
            ..Default::default()
        },
    );
    let response = state
        .handle_command(
            serde_json::from_value(serde_json::json!({
                "command":"deploy", "app":APP, "version":"v1",
                "path":crate::release::app_release_root(dir, APP, "v1"),
                "routes":["sdk.localhost"], "secrets_digest":digest,
                "databases": {"@workflows": {"name": WORKFLOWS, "server": "local"}}
            }))
            .unwrap(),
        )
        .await;
    assert!(
        matches!(response, Response::Ok { .. }),
        "deploy failed: {response:?}"
    );
}

async fn report(state: &ServerState) -> Value {
    let app = state.app_manager.get_app(APP).unwrap();
    let instances = app.get_instances();
    assert_eq!(instances.len(), 1);
    let endpoint = instances[0].endpoint().unwrap();
    reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap()
        .get(format!("http://{endpoint}/env"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

async fn container(runtime: &ComposeRuntime, service: &str) -> String {
    runtime
        .containers()
        .await
        .unwrap()
        .into_iter()
        .find(|c| c.service == service && c.running())
        .unwrap()
        .id
}

async fn exec(id: &str, script: &str) -> (bool, String) {
    let output = tokio::process::Command::new("docker")
        .args(["exec", id, "sh", "-c", script])
        .output()
        .await
        .unwrap();
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).trim().to_string(),
    )
}

#[tokio::test]
#[ignore = "requires Docker Compose with shared host/container loopback and project paths"]
async fn sdk_services_receive_the_group_runtime_through_restart_and_secret_rotation() {
    let dir = crate::isolation::fixture::TestDataDir::new().unwrap();
    let data = crate::release::ensure_app_runtime_data_dirs(dir.path(), APP).unwrap();
    let compose = tako_core::compose::ComposeConfig {
        file: "compose.yaml".into(),
        services: serde_json::from_value(serde_json::json!({
            "web": {"sdk": true, "envs": {"production": {"route": "sdk.localhost"}}},
            "worker": {"sdk": true}
        }))
        .unwrap(),
    };
    let release = crate::release::app_release_root(dir.path(), APP, "v1");
    std::fs::create_dir_all(&release).unwrap();
    std::fs::write(
        release.join("app.json"),
        serde_json::json!({
            "protocol_version":tako_core::PROTOCOL_VERSION, "release_kind":"compose",
            "app_name":"sdk", "environment":"production", "version":"v1",
            "runtime":"compose", "main":"", "idle_timeout":300, "compose":compose,
            "env_vars":{"TAKO_BUILD":"v1"},
        })
        .to_string(),
    )
    .unwrap();
    std::fs::write(release.join("compose.yaml"), COMPOSE).unwrap();
    std::fs::write(release.join("Dockerfile"), DOCKERFILE).unwrap();
    std::fs::write(release.join("handler.sh"), HANDLER).unwrap();
    let runtime = ComposeRuntime::new(
        release,
        compose,
        HashMap::from([(
            crate::release::TAKO_APP_DATA_DIR_ENV.into(),
            data.app.display().to_string(),
        )]),
    )
    .unwrap();
    struct Cleanup(ComposeRuntime);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = self
                .0
                .command(false)
                .into_std()
                .args(["down", "--volumes", "--remove-orphans"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    }
    let _cleanup = Cleanup(runtime.clone());
    runtime.prepare("v1").await.unwrap();

    let server = state(dir.path()).await;
    server
        .create_test_database(
            WORKFLOWS.into(),
            tako_core::DatabaseEngine::Turso,
            "0123456789abcdef0123456789abcdef".into(),
            base64::engine::general_purpose::STANDARD.encode([7u8; 32]),
            None,
        )
        .await
        .unwrap();
    deploy(&server, dir.path(), "first").await;
    let token = server.state_store.get_compose_token(APP).unwrap().unwrap();
    assert_eq!(token.len(), 64);
    let app = server.app_manager.get_app(APP).unwrap();
    assert_eq!(app.get_instances()[0].internal_token(), token);
    let env = report(&server).await;
    assert_eq!(env["TAKO_APP_NAME"], APP);
    assert_eq!(env["TAKO_INTERNAL_SOCKET"], "/run/tako/internal.sock");
    assert_eq!(env["TAKO_DATA_DIR"], "/tako/data");
    assert_eq!(env["TAKO_BUILD"], "v1");
    assert_eq!(env["ENV"], "production");
    assert_eq!(env["HOST"], "0.0.0.0");
    assert_eq!(env["PORT"], "3000");
    assert_eq!(env["TAKO_COMPOSE_SERVICE"], "web");
    assert_eq!(env["token"], token.as_str());
    assert_eq!(env["secret"], "first");
    // The relative symlink resolves through the container's mount.
    assert_eq!(env["socket"], "yes");
    let socket = crate::compose_runtime::socket_root(dir.path())
        .join(&runtime.project)
        .join("internal.sock");
    assert_eq!(socket_command(&socket, APP).await["status"], "ok");
    let foreign = socket_command(&socket, "other/production").await;
    assert!(
        foreign["message"]
            .as_str()
            .unwrap()
            .contains("internal socket app mismatch"),
        "{foreign}"
    );

    let web = container(&runtime, "web").await;
    let worker = container(&runtime, "worker").await;
    let plain = container(&runtime, "plain").await;
    assert_eq!(
        exec(
            &worker,
            "test -S /run/tako/internal.sock && printenv TAKO_APP_NAME"
        )
        .await,
        (true, APP.to_string())
    );
    assert!(!exec(&worker, "printenv PORT").await.0);
    assert!(!exec(&plain, "printenv TAKO_BOOTSTRAP_DATA").await.0);
    assert!(!exec(&plain, "printenv TAKO_COMPOSE_SERVICE").await.0);
    assert!(exec(&web, "echo kept > /tako/data/marker").await.0);
    assert_eq!(
        std::fs::read_to_string(data.app.join("marker")).unwrap(),
        "kept\n"
    );
    // The project directory is mounted, not the socket file, behind a
    // directory only Tako can list.
    let socket_root = crate::compose_runtime::socket_root(dir.path());
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&socket_root), 0o700);
        assert_eq!(mode(&socket_root.join(&runtime.project)), 0o711);
    }

    // The group's workflow runtime exists before any worker claims: the
    // queue accepts work, and only workers of the current build register.
    let enqueued = socket_send(
        &socket,
        serde_json::json!({"command": "enqueue_run", "app": APP, "name": "send", "payload": {}}),
    )
    .await;
    assert_eq!(enqueued["status"], "ok", "{enqueued}");
    let register = |build: &str| {
        serde_json::json!({"command": "register_schedules", "app": APP, "build": build,
            "schedules": [{"name": "nightly", "cron": "0 0 0 * * *"}]})
    };
    assert_eq!(
        socket_send(&socket, register("v0")).await["data"]["count"],
        0
    );
    assert_eq!(
        socket_send(&socket, register("v1")).await["data"]["count"],
        1
    );
    let runs = server
        .handle_command(Command::ListWorkflowRuns {
            app: APP.into(),
            status: None,
            name: None,
            limit: None,
        })
        .await;
    let Response::Ok { data: listed } = runs else {
        panic!("workflow runs of the group: {runs:?}")
    };
    assert_eq!(listed["runs"][0]["name"], "send", "{listed}");

    // A restarted server re-binds the socket and re-attaches the containers,
    // which keep matching the stored token.
    server.app_manager.shutdown_all().await;
    server.workflows.shutdown_all(Duration::from_secs(1)).await;
    drop(server);
    let restarted = state(dir.path()).await;
    restarted.restore_from_state_store().await.unwrap();
    assert_eq!(container(&runtime, "web").await, web);
    assert_eq!(container(&runtime, "worker").await, worker);
    let app = restarted.app_manager.get_app(APP).unwrap();
    assert_eq!(app.get_instances()[0].internal_token(), token);
    assert_eq!(report(&restarted).await["socket"], "yes");
    assert_eq!(socket_command(&socket, APP).await["status"], "ok");
    // External workers claim from the recovered runtime; nothing is spawned.
    let claimed = socket_send(
        &socket,
        serde_json::json!({"command": "claim_run", "app": APP, "worker_id": "worker-1",
            "names": ["send"], "lease_ms": 30000}),
    )
    .await;
    assert_eq!(claimed["data"]["name"], "send", "{claimed}");

    // A secret rotation replaces only the SDK services and keeps the token.
    deploy(&restarted, dir.path(), "second").await;
    assert_ne!(container(&runtime, "web").await, web);
    assert_ne!(container(&runtime, "worker").await, worker);
    assert_eq!(container(&runtime, "plain").await, plain);
    let env = report(&restarted).await;
    assert_eq!(env["secret"], "second");
    assert_eq!(env["token"], token.as_str());
    assert_eq!(
        std::fs::read_to_string(data.app.join("marker")).unwrap(),
        "kept\n"
    );

    assert!(matches!(
        restarted
            .handle_command(Command::Delete { app: APP.into() })
            .await,
        Response::Ok { .. }
    ));
    assert!(!socket_root.join(&runtime.project).exists());
    assert_eq!(restarted.state_store.get_compose_token(APP).unwrap(), None);
}
