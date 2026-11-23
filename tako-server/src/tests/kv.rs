//! KV stores hosted on one server: created and deleted like databases,
//! bound alongside them, served to apps through the internal socket, and to
//! other servers' apps through the peer port.

use super::*;
use crate::isolation::fixture::TestDataDir as TempDir;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde_json::json;
use tako_core::{DatabaseBinding, DatabaseEngine, KvRequest};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

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

async fn query(state: &ServerState, name: &str, request: KvRequest) -> Response {
    state
        .handle_command(Command::QueryKv {
            name: name.into(),
            request,
        })
        .await
}

async fn get(state: &ServerState, name: &str, key: &str) -> Value {
    match query(state, name, KvRequest::Get { key: key.into() }).await {
        Response::Ok { data } => data,
        Response::Error { message, .. } => panic!("expected a value, got: {message}"),
    }
}

async fn put(state: &ServerState, name: &str, key: &str, json: &str, ttl_ms: Option<u64>) {
    let request = KvRequest::Put {
        key: key.into(),
        json: json.into(),
        ttl_ms,
    };
    match query(state, name, request).await {
        Response::Ok { .. } => {}
        Response::Error { message, .. } => panic!("expected the put to succeed, got: {message}"),
    }
}

fn binding(name: &str, engine: DatabaseEngine) -> DatabaseBinding {
    DatabaseBinding {
        name: name.into(),
        server: String::new(),
        peer_key: None,
        engine,
    }
}

/// Serves the state's KV host to loopback peers on a port of the test's own.
async fn listen_peers(state: &ServerState) -> std::net::SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(tako_kv::serve_peers(
        state.databases.kv.clone(),
        listener,
        Arc::new(|ip: std::net::IpAddr| ip.is_loopback()),
    ));
    address
}

/// Sends one forwarded request as another server would and reads the reply.
async fn forward(address: std::net::SocketAddr, request: Value) -> Value {
    let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
    let mut line = serde_json::to_vec(&request).unwrap();
    line.push(b'\n');
    stream.write_all(&line).await.unwrap();
    let mut reply = String::new();
    tokio::io::BufReader::new(stream)
        .read_line(&mut reply)
        .await
        .unwrap();
    serde_json::from_str(&reply).unwrap()
}

/// An app's call through the internal socket.
async fn call(
    state: &ServerState,
    app: &str,
    token: &str,
    request: KvRequest,
) -> Result<Value, tako_kv::KvError> {
    let kv = state.databases.kv.clone();
    let (app, token) = (app.to_owned(), token.to_owned());
    tokio::task::spawn_blocking(move || kv.call(&app, &token, request))
        .await
        .unwrap()
}

#[tokio::test]
async fn kv_stores_are_created_queried_and_deleted() {
    let temp = TempDir::new().unwrap();
    let state = state(&temp);
    create(&state, "sessions", DatabaseEngine::Kv).await;
    assert!(state.databases.kv_path("sessions").is_file());
    assert!(!state.databases.database_path("sessions").exists());

    put(&state, "sessions", "challenge:1", r#""abc""#, Some(60_000)).await;
    assert_eq!(
        get(&state, "sessions", "challenge:1").await,
        json!(r#""abc""#)
    );
    assert_eq!(get(&state, "sessions", "missing").await, Value::Null);
    let invalid = query(
        &state,
        "sessions",
        KvRequest::Put {
            key: "k".into(),
            json: "not json".into(),
            ttl_ms: None,
        },
    )
    .await;
    assert_eq!(
        invalid.error_code(),
        Some(tako_core::ErrorCode::InvalidRequest)
    );
    query(
        &state,
        "sessions",
        KvRequest::Delete {
            key: "challenge:1".into(),
        },
    )
    .await;
    assert_eq!(get(&state, "sessions", "challenge:1").await, Value::Null);

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

    // SQL and migrations are SQLite's.
    assert!(
        error(
            &state,
            Command::QueryDatabase {
                name: "sessions".into(),
                sql: "SELECT 1".into(),
            },
        )
        .await
        .contains("is a KV store")
    );
    create(&state, "main", DatabaseEngine::Sqlite).await;
    assert!(
        query(&state, "main", KvRequest::Get { key: "k".into() })
            .await
            .error_message()
            .unwrap()
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
    assert!(
        query(&state, "sessions", KvRequest::Get { key: "k".into() })
            .await
            .error_message()
            .is_some()
    );
}

fn backup(bucket: &str) -> tako_core::StorageBinding {
    tako_core::StorageBinding {
        provider: tako_core::StorageProvider::S3,
        resource: None,
        bucket: Some(bucket.into()),
        endpoint: Some("https://s3.example.test".into()),
        region: Some("auto".into()),
        access_key_id: Some("k".into()),
        secret_access_key: Some("s".into()),
        force_path_style: true,
        public_base_url: None,
        path: None,
        signing_key: None,
    }
}

#[tokio::test]
async fn kv_stores_are_snapshotted_restored_exported_and_imported() {
    let temp = TempDir::new().unwrap();
    let state = state(&temp);
    ok(
        &state,
        Command::CreateDatabase {
            name: "sessions".into(),
            engine: DatabaseEngine::Kv,
            generation: GENERATION.into(),
            backup_key_base64: BASE64.encode([7u8; 32]),
            backup: Some(backup("kv-snapshots")),
        },
    )
    .await;
    let release = |reason| Command::SnapshotDatabase {
        name: "sessions".into(),
        reason,
        app: None,
    };
    // A new, empty store has nothing to snapshot before a release.
    let skipped = ok(&state, release(tako_core::SnapshotReason::Release)).await;
    assert!(skipped["skipped"].is_string(), "{skipped}");

    put(&state, "sessions", "kept", r#""first""#, None).await;
    put(&state, "sessions", "short", "1", Some(1)).await;
    let first = ok(&state, release(tako_core::SnapshotReason::Release)).await;
    let first = first["snapshot"]["id"].as_str().unwrap().to_owned();
    // Unchanged since, so the next release snapshot is skipped.
    let again = ok(&state, release(tako_core::SnapshotReason::Release)).await;
    assert!(again["skipped"].is_string(), "{again}");

    let store = crate::databases::snapshots::test_store("kv-snapshots").unwrap();
    let objects: Vec<_> = {
        use futures_util::TryStreamExt;
        store.list(None).try_collect().await.unwrap()
    };
    assert!(
        objects
            .iter()
            .any(|meta| meta.location.as_ref().ends_with(".redb.zst.enc")),
        "snapshots are encrypted KV exports"
    );

    put(&state, "sessions", "kept", r#""second""#, None).await;
    put(&state, "sessions", "added", "2", None).await;
    let restored = ok(
        &state,
        Command::RestoreDatabase {
            name: "sessions".into(),
            snapshot: Some(first.clone()),
            backup_key_base64: None,
        },
    )
    .await;
    assert_eq!(restored["warnings"], json!([]), "{restored}");
    assert_eq!(get(&state, "sessions", "kept").await, json!(r#""first""#));
    assert_eq!(get(&state, "sessions", "added").await, Value::Null);
    // The replaced contents were snapshotted, and kept on the server.
    let listed = ok(
        &state,
        Command::ListDatabaseSnapshots {
            name: "sessions".into(),
        },
    )
    .await;
    let reasons: Vec<&str> = listed["snapshots"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["reason"].as_str().unwrap())
        .collect();
    assert_eq!(
        reasons.iter().filter(|r| **r == "restore").count(),
        1,
        "{listed}"
    );
    assert!(
        state
            .databases
            .database_dir("sessions")
            .join("previous.redb")
            .exists()
    );
    // `--latest` skips the copy taken before the restore.
    ok(
        &state,
        Command::RestoreDatabase {
            name: "sessions".into(),
            snapshot: None,
            backup_key_base64: None,
        },
    )
    .await;
    assert_eq!(get(&state, "sessions", "added").await, Value::Null);

    // Export and import through the transfer commands.
    let export = ok(
        &state,
        Command::BeginDatabaseExport {
            name: "sessions".into(),
        },
    )
    .await;
    let export_id = export["export_id"].as_str().unwrap().to_owned();
    let size = export["size_bytes"].as_u64().unwrap();
    let mut file = Vec::new();
    while (file.len() as u64) < size {
        let chunk = ok(
            &state,
            Command::DatabaseExportChunk {
                name: "sessions".into(),
                export_id: export_id.clone(),
                offset: file.len() as u64,
            },
        )
        .await;
        file.extend(
            BASE64
                .decode(chunk["data_base64"].as_str().unwrap())
                .unwrap(),
        );
    }
    ok(
        &state,
        Command::FinishDatabaseExport {
            name: "sessions".into(),
            export_id,
        },
    )
    .await;
    let exported = temp.path().join("exported.redb");
    std::fs::write(&exported, &file).unwrap();
    let reader = tako_kv::export::Reader::open(&exported).unwrap();
    reader.check().unwrap();
    // `short` expired; only `kept` is exported.
    assert_eq!(reader.keys(), 1);
    drop(reader);

    put(&state, "sessions", "kept", r#""third""#, None).await;
    let import = |data: Vec<u8>| {
        let state = &state;
        async move {
            use sha2::Digest;
            let begin = ok(
                state,
                Command::BeginDatabaseImport {
                    name: "sessions".into(),
                    size_bytes: data.len() as u64,
                    sha256: hex::encode(sha2::Sha256::digest(&data)),
                },
            )
            .await;
            let upload_id = begin["upload_id"].as_str().unwrap().to_owned();
            ok(
                state,
                Command::DatabaseImportChunk {
                    name: "sessions".into(),
                    upload_id: upload_id.clone(),
                    offset: 0,
                    data_base64: BASE64.encode(&data),
                },
            )
            .await;
            state
                .handle_command(Command::FinishDatabaseImport {
                    name: "sessions".into(),
                    upload_id,
                })
                .await
        }
    };
    // A file that is not a KV export changes nothing.
    let refused = import(b"not a kv export".to_vec()).await;
    assert!(
        refused
            .error_message()
            .unwrap()
            .contains("not a usable KV store export"),
        "{refused:?}"
    );
    assert_eq!(get(&state, "sessions", "kept").await, json!(r#""third""#));
    assert!(matches!(import(file).await, Response::Ok { .. }));
    assert_eq!(get(&state, "sessions", "kept").await, json!(r#""first""#));

    // Deleting the store deletes its snapshots too.
    ok(
        &state,
        Command::DeleteDatabase {
            name: "sessions".into(),
            generation: None,
            keep_snapshots: false,
        },
    )
    .await;
    let left: Vec<_> = {
        use futures_util::TryStreamExt;
        store.list(None).try_collect().await.unwrap()
    };
    assert!(
        !left
            .iter()
            .any(|meta| meta.location.as_ref().ends_with(".redb.zst.enc")),
        "{left:?}"
    );
}

#[tokio::test]
async fn bindings_split_by_engine_and_serve_apps_through_their_tokens() {
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
    let token = bindings["auth"].token.clone();
    assert_eq!(stores["auth"].token, token);

    state
        .databases
        .apply_bindings("web/production", &[&bindings]);
    let request = KvRequest::Put {
        key: "k".into(),
        json: r#""v""#.into(),
        ttl_ms: None,
    };
    call(&state, "web/production", &token, request)
        .await
        .unwrap();
    assert_eq!(
        call(
            &state,
            "web/production",
            &token,
            KvRequest::Get { key: "k".into() }
        )
        .await
        .unwrap(),
        json!(r#""v""#)
    );
    // The token is the environment's: another app cannot use it, and the
    // SQLite binding's token is not a KV credential.
    assert!(
        call(
            &state,
            "other/production",
            &token,
            KvRequest::Get { key: "k".into() }
        )
        .await
        .is_err()
    );
    let sqlite_token = bindings["main"].token.clone();
    assert!(
        call(
            &state,
            "web/production",
            &sqlite_token,
            KvRequest::Get { key: "k".into() }
        )
        .await
        .is_err()
    );

    // Removing the environment's bindings ends its access.
    state.databases.remove_bindings("web/production");
    let error = call(
        &state,
        "web/production",
        &token,
        KvRequest::Get { key: "k".into() },
    )
    .await
    .unwrap_err();
    assert!(
        error.message.contains("binding changed"),
        "{}",
        error.message
    );
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
    let address = listen_peers(&state).await;
    let request = |store: &str, grant: &str| {
        json!({
            "scope": "web/production", "grant": grant, "store": store,
            "operation": "put", "key": "relayed", "json": "\"yes\"",
        })
    };
    assert_eq!(
        forward(address, request("sessions", &key)).await,
        json!({"status": "ok", "data": null})
    );
    assert_eq!(get(&state, "sessions", "relayed").await, json!("\"yes\""));
    let refused = forward(address, request("sessions", "wrong")).await;
    assert!(
        refused["message"]
            .as_str()
            .unwrap()
            .contains("refused this app"),
        "{refused}"
    );
    // The grant also names the SQLite database, which no KV store serves.
    let unserved = forward(address, request("main", &key)).await;
    assert!(
        unserved["message"]
            .as_str()
            .unwrap()
            .contains("not serving the KV store"),
        "{unserved}"
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
    let revoked = forward(address, request("sessions", &key)).await;
    assert!(
        revoked["message"]
            .as_str()
            .unwrap()
            .contains("refused this app"),
        "{revoked}"
    );
}

#[tokio::test]
async fn kv_stores_reopen_with_their_data_after_a_restart() {
    let temp = TempDir::new().unwrap();
    {
        let state = state(&temp);
        state.open_databases(None).await.unwrap();
        create(&state, "sessions", DatabaseEngine::Kv).await;
        put(&state, "sessions", "kept", "1", None).await;
        state.stop_databases().await;
    }
    let state = state(&temp);
    state.open_databases(None).await.unwrap();
    assert_eq!(get(&state, "sessions", "kept").await, json!("1"));
    let status = ok(&state, Command::DatabaseStatus { name: None }).await;
    assert_eq!(status["databases"][0]["engine"], "kv");
}
