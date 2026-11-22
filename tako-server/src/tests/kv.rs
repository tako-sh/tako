//! KV stores hosted on one server: created and deleted like databases,
//! bound alongside them, and served over the Redis protocol.

use super::*;
use crate::isolation::fixture::TestDataDir as TempDir;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde_json::json;
use tako_core::{DatabaseBinding, DatabaseEngine};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const GENERATION: &str = "0123456789abcdef0123456789abcdef";

fn state(temp: &TempDir) -> Arc<ServerState> {
    let cert_manager = Arc::new(CertManager::new(CertManagerConfig {
        cert_dir: temp.path().join("certs"),
        ..Default::default()
    }));
    Arc::new(
        ServerState::new(
            temp.path().to_path_buf(),
            cert_manager,
            None,
            empty_challenge_tokens(),
        )
        .unwrap(),
    )
}

async fn ok(state: &ServerState, command: Command) -> Value {
    match state.handle_command(command).await {
        Response::Ok { data } => data,
        Response::Error { message, .. } => panic!("expected an ok response, got: {message}"),
    }
}

async fn error(state: &ServerState, command: Command) -> String {
    match state.handle_command(command).await {
        Response::Error { message, .. } => message,
        _ => panic!("expected an error response"),
    }
}

async fn create(state: &ServerState, name: &str, engine: DatabaseEngine) {
    ok(
        state,
        Command::CreateDatabase {
            name: name.into(),
            engine,
            generation: GENERATION.into(),
            backup_key_base64: BASE64.encode([7u8; 32]),
            backup: None,
        },
    )
    .await;
}

async fn kv(state: &ServerState, name: &str, args: &[&str]) -> Value {
    ok(
        state,
        Command::QueryKv {
            name: name.into(),
            args: args.iter().map(|a| a.to_string()).collect(),
        },
    )
    .await
}

fn binding(name: &str, engine: DatabaseEngine) -> DatabaseBinding {
    DatabaseBinding {
        name: name.into(),
        server: String::new(),
        peer_key: None,
        engine,
    }
}

/// Sends raw bytes and reads until `expected` bytes arrive or the server
/// stops sending.
async fn raw(stream: &mut tokio::net::TcpStream, send: &[u8], expected: usize) -> Vec<u8> {
    stream.write_all(send).await.unwrap();
    let mut got = Vec::new();
    while got.len() < expected {
        let mut chunk = [0u8; 4096];
        match tokio::time::timeout(Duration::from_secs(2), stream.read(&mut chunk)).await {
            Ok(Ok(n)) if n > 0 => got.extend_from_slice(&chunk[..n]),
            _ => break,
        }
    }
    got
}

/// Serves the state's KV host on a port of the test's own, rather than the
/// shared default port other tests' listeners also bind.
async fn listen(state: &ServerState, access: tako_kv::Access) -> std::net::SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(tako_kv::serve(state.databases.kv.clone(), listener, access));
    address
}

#[tokio::test]
async fn kv_stores_are_created_queried_and_deleted() {
    let temp = TempDir::new().unwrap();
    let state = state(&temp);
    create(&state, "sessions", DatabaseEngine::Kv).await;
    assert!(state.databases.kv_path("sessions").is_file());
    assert!(!state.databases.database_path("sessions").exists());

    assert_eq!(
        kv(
            &state,
            "sessions",
            &["SET", "challenge:1", "abc", "EX", "60"]
        )
        .await,
        "OK"
    );
    assert_eq!(kv(&state, "sessions", &["GET", "challenge:1"]).await, "abc");
    assert_eq!(kv(&state, "sessions", &["TTL", "challenge:1"]).await, 60);
    kv(&state, "sessions", &["HSET", "h", "a", "1"]).await;
    assert_eq!(
        kv(&state, "sessions", &["HGETALL", "h"]).await,
        json!([["a", "1"]])
    );
    assert_eq!(
        kv(&state, "sessions", &["GET", "missing"]).await,
        Value::Null
    );
    assert_eq!(
        kv(&state, "sessions", &["INCR", "h"]).await["error"],
        "WRONGTYPE Operation against a key holding the wrong kind of value"
    );
    assert!(
        kv(&state, "sessions", &["MULTI"]).await["error"]
            .as_str()
            .unwrap()
            .contains("not available here")
    );

    let status = ok(
        &state,
        Command::DatabaseStatus {
            name: Some("sessions".into()),
        },
    )
    .await;
    let info = &status["databases"][0];
    assert_eq!(info["engine"], "kv");
    assert!(info["size_bytes"].as_u64().unwrap() > 0);
    assert!(info["backup"].is_null());

    // Snapshots, SQL, migrations and transfers are SQLite's.
    for command in [
        Command::QueryDatabase {
            name: "sessions".into(),
            sql: "SELECT 1".into(),
        },
        Command::SnapshotDatabase {
            name: "sessions".into(),
            reason: tako_core::SnapshotReason::Manual,
            app: None,
        },
        Command::ConfigureDatabaseBackup {
            name: "sessions".into(),
            backup: None,
        },
        Command::BeginDatabaseExport {
            name: "sessions".into(),
        },
    ] {
        assert!(error(&state, command).await.contains("is a KV store"));
    }
    create(&state, "main", DatabaseEngine::Sqlite).await;
    assert!(
        error(
            &state,
            Command::QueryKv {
                name: "main".into(),
                args: vec!["PING".into()],
            },
        )
        .await
        .contains("SQLite database")
    );
    // Names are unique across engines.
    let duplicate = error(
        &state,
        Command::CreateDatabase {
            name: "main".into(),
            engine: DatabaseEngine::Kv,
            generation: GENERATION.into(),
            backup_key_base64: BASE64.encode([7u8; 32]),
            backup: None,
        },
    )
    .await;
    assert!(duplicate.contains("already exists"));

    ok(
        &state,
        Command::DeleteDatabase {
            name: "sessions".into(),
            generation: None,
            keep_snapshots: false,
        },
    )
    .await;
    assert!(!state.databases.database_dir("sessions").exists());
    assert!(
        !state
            .databases
            .kv
            .is_registered(&tako_kv::StoreId::parse("sessions").unwrap())
    );
    error(
        &state,
        Command::QueryKv {
            name: "sessions".into(),
            args: vec!["PING".into()],
        },
    )
    .await;
}

#[tokio::test]
async fn kv_stores_refuse_backups_at_create() {
    let temp = TempDir::new().unwrap();
    let state = state(&temp);
    let refused = error(
        &state,
        Command::CreateDatabase {
            name: "sessions".into(),
            engine: DatabaseEngine::Kv,
            generation: GENERATION.into(),
            backup_key_base64: BASE64.encode([7u8; 32]),
            backup: Some(tako_core::StorageBinding {
                provider: tako_core::StorageProvider::S3,
                resource: None,
                bucket: Some("b".into()),
                endpoint: Some("https://s3.example.test".into()),
                region: Some("auto".into()),
                access_key_id: Some("k".into()),
                secret_access_key: Some("s".into()),
                force_path_style: true,
                public_base_url: None,
                path: None,
                signing_key: None,
            }),
        },
    )
    .await;
    assert!(refused.contains("no snapshots"));
}

#[tokio::test]
async fn bindings_split_by_engine_and_serve_apps_over_redis() {
    let temp = TempDir::new().unwrap();
    let state = state(&temp);
    create(&state, "sessions", DatabaseEngine::Kv).await;
    create(&state, "main", DatabaseEngine::Sqlite).await;
    let config = AppConfig::default();
    let bindings = state
        .databases
        .prepare_bindings(
            &HashMap::new(),
            HashMap::from([
                ("auth".to_owned(), binding("sessions", DatabaseEngine::Kv)),
                ("main".to_owned(), binding("main", DatabaseEngine::Sqlite)),
            ]),
            &config,
        )
        .unwrap();
    // A binding of the wrong engine names nothing hosted here.
    let wrong = state
        .databases
        .prepare_bindings(
            &HashMap::new(),
            HashMap::from([("auth".to_owned(), binding("main", DatabaseEngine::Kv))]),
            &config,
        )
        .unwrap_err();
    assert!(
        wrong.contains("names KV store main that is not hosted"),
        "{wrong}"
    );

    let sqlite = state.databases.connections(&bindings);
    let stores = crate::databases::kv_connections(&bindings);
    assert_eq!(sqlite.keys().collect::<Vec<_>>(), vec!["main"]);
    assert_eq!(stores.keys().collect::<Vec<_>>(), vec!["auth"]);
    let token = &bindings["auth"].token;
    assert_eq!(
        stores["auth"].url,
        format!(
            "redis://:{token}@127.0.0.1:{}",
            crate::databases::kv_app_port()
        )
    );

    state
        .databases
        .apply_bindings("web/production", &[&bindings]);
    let address = listen(&state, tako_kv::Access::Apps).await;
    let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
    let auth = format!("AUTH {token}\r\nSET k v\r\nGET k\r\n");
    assert_eq!(
        raw(&mut stream, auth.as_bytes(), 17).await,
        b"+OK\r\n+OK\r\n$1\r\nv\r\n"
    );
    // The SQLite binding's token is not a KV credential.
    let mut other = tokio::net::TcpStream::connect(address).await.unwrap();
    let sqlite_auth = format!("AUTH {}\r\n", bindings["main"].token);
    assert!(
        raw(&mut other, sqlite_auth.as_bytes(), 9)
            .await
            .starts_with(b"-WRONGPASS")
    );

    // Removing the environment's bindings ends its connections.
    state.databases.remove_bindings("web/production");
    let got = raw(&mut stream, b"GET k\r\n", 60).await;
    assert!(String::from_utf8_lossy(&got).contains("binding changed"));
}

#[tokio::test]
async fn grants_reach_the_kv_peer_listener() {
    let temp = TempDir::new().unwrap();
    let state = state(&temp);
    create(&state, "sessions", DatabaseEngine::Kv).await;
    create(&state, "main", DatabaseEngine::Sqlite).await;
    let granted = ok(
        &state,
        Command::GrantDatabases {
            app: "web/production".into(),
            databases: vec!["sessions".into(), "main".into()],
            keep_existing: false,
        },
    )
    .await;
    let key = granted["peer_key"].as_str().unwrap().to_owned();
    let address = listen(
        &state,
        tako_kv::Access::Peers {
            allow: Arc::new(|ip| ip.is_loopback()),
        },
    )
    .await;
    let handshake = |store: &str, key: &str| {
        let mut bytes = Vec::new();
        tako_kv::encode_request(
            &[tako_kv::FORWARD_COMMAND, "web/production", store, key, "2"],
            &mut bytes,
        );
        bytes
    };
    let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
    let mut send = handshake("sessions", &key);
    send.extend_from_slice(b"SET relayed yes\r\n");
    assert_eq!(raw(&mut stream, &send, 10).await, b"+OK\r\n+OK\r\n");
    assert_eq!(kv(&state, "sessions", &["GET", "relayed"]).await, "yes");
    let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
    let got = raw(&mut stream, &handshake("sessions", "wrong"), 30).await;
    assert!(got.starts_with(b"-ERR forwarding refused"));
    // The grant also names the SQLite database, which no KV store serves.
    let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
    let mut send = handshake("main", &key);
    send.extend_from_slice(b"GET k\r\n");
    let got = String::from_utf8(raw(&mut stream, &send, 40).await).unwrap();
    assert!(
        got.ends_with("not serving the store right now; retry\r\n"),
        "{got}"
    );
    // Deleting the store revokes it from the grant.
    ok(
        &state,
        Command::DeleteDatabase {
            name: "sessions".into(),
            generation: None,
            keep_snapshots: false,
        },
    )
    .await;
    let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
    let got = raw(&mut stream, &handshake("sessions", &key), 30).await;
    assert!(got.starts_with(b"-ERR forwarding refused"));
}

#[tokio::test]
async fn kv_stores_reopen_with_their_data_after_a_restart() {
    let temp = TempDir::new().unwrap();
    {
        let state = state(&temp);
        state.open_databases(None).await.unwrap();
        create(&state, "sessions", DatabaseEngine::Kv).await;
        kv(&state, "sessions", &["SET", "kept", "1"]).await;
        state.stop_databases().await;
    }
    let state = state(&temp);
    state.open_databases(None).await.unwrap();
    assert_eq!(kv(&state, "sessions", &["GET", "kept"]).await, "1");
    let status = ok(&state, Command::DatabaseStatus { name: None }).await;
    assert_eq!(status["databases"][0]["engine"], "kv");
}
