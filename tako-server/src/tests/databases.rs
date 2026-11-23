//! Hosted database management on one server: create, query, snapshot,
//! restore, import, export, grants, and delete.

use super::*;
use crate::isolation::fixture::TestDataDir as TempDir;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use object_store::ObjectStoreExt as _;
use serde_json::json;
use sha2::{Digest, Sha256};
use tako_core::{SnapshotReason, StorageBinding, StorageProvider};

const ID: &str = "shop";
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

fn backup(bucket: &str) -> StorageBinding {
    StorageBinding {
        provider: StorageProvider::S3,
        resource: None,
        bucket: Some(bucket.into()),
        endpoint: Some("https://s3.example.test".into()),
        region: Some("auto".into()),
        access_key_id: Some("key".into()),
        secret_access_key: Some("secret".into()),
        force_path_style: true,
        public_base_url: None,
        path: None,
        signing_key: None,
    }
}

fn key() -> String {
    BASE64.encode([7u8; 32])
}

async fn ok(state: &ServerState, command: Command) -> Value {
    match state.handle_command(command).await {
        Response::Ok { data } => data,
        // Responses can carry secrets, so failures name the outcome only.
        Response::Error { .. } => panic!("expected an ok response, got an error"),
    }
}

async fn error(state: &ServerState, command: Command) -> String {
    match state.handle_command(command).await {
        Response::Error { message, .. } => message,
        _ => panic!("expected an error response"),
    }
}

async fn query(state: &ServerState, id: &str, sql: &str) -> Value {
    ok(
        state,
        Command::QueryDatabase {
            name: id.into(),
            sql: sql.into(),
        },
    )
    .await
}

async fn count(state: &ServerState, id: &str) -> i64 {
    query(state, id, "SELECT count(*) FROM items").await["rows"][0][0]
        .as_i64()
        .unwrap()
}

async fn create(state: &ServerState, id: &str, bucket: Option<&str>) {
    ok(
        state,
        Command::CreateDatabase {
            name: id.into(),
            generation: GENERATION.into(),
            backup_key_base64: key(),
            backup: bucket.map(backup),
        },
    )
    .await;
}

#[tokio::test]
async fn snapshots_restore_the_contents_they_captured() {
    let temp = TempDir::new().unwrap();
    let state = state(&temp);
    create(&state, ID, Some("restore-bucket")).await;
    assert!(
        state.databases.database_path(ID).exists(),
        "the file is created under the data directory"
    );

    query(&state, ID, "CREATE TABLE items (name TEXT)").await;
    query(&state, ID, "INSERT INTO items VALUES ('kept')").await;
    let first = ok(
        &state,
        Command::SnapshotDatabase {
            name: ID.into(),
            reason: SnapshotReason::Manual,
            app: None,
        },
    )
    .await;
    let first_id = first["snapshot"]["id"].as_str().unwrap().to_owned();

    query(&state, ID, "INSERT INTO items VALUES ('lost')").await;
    assert_eq!(count(&state, ID).await, 2);

    let restored = ok(
        &state,
        Command::RestoreDatabase {
            name: ID.into(),
            snapshot: Some(first_id.clone()),
            at_unix_ms: None,
            backup_key_base64: None,
        },
    )
    .await;
    assert_eq!(
        restored["restored"]["transaction"]
            .as_u64()
            .unwrap()
            .to_string(),
        first_id
    );
    assert_eq!(count(&state, ID).await, 1);
    assert!(
        state
            .databases
            .database_dir(ID)
            .join("previous.sqlite")
            .exists(),
        "the replaced file is kept"
    );

    // The restore first backed up the contents it replaced.
    let listed = ok(&state, Command::ListDatabaseSnapshots { name: ID.into() }).await;
    let reasons = snapshot_reasons(&state).await;
    assert_eq!(
        reasons.len(),
        2,
        "a manual and a pre-restore point: {reasons:?}"
    );
    assert!(reasons.contains(&"restore".to_owned()));
    assert!(
        listed["windows"][0]["from_transaction"].is_u64(),
        "the history is restorable to any captured transaction"
    );
    let pre_restore = listed["snapshots"]
        .as_array()
        .unwrap()
        .iter()
        .find(|snapshot| snapshot["reason"] == "restore")
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    ok(
        &state,
        Command::RestoreDatabase {
            name: ID.into(),
            snapshot: Some(pre_restore),
            at_unix_ms: None,
            backup_key_base64: None,
        },
    )
    .await;
    assert_eq!(count(&state, ID).await, 2);

    let status = ok(
        &state,
        Command::DatabaseStatus {
            name: Some(ID.into()),
        },
    )
    .await;
    let database = &status["databases"][0];
    assert_eq!(database["name"], "shop");
    assert_eq!(database["backup"]["bucket"], "restore-bucket");
    assert!(database["backup"]["last_snapshot"]["id"].is_string());
}

#[tokio::test]
async fn restoring_one_snapshot_twice_creates_two_new_replay_timelines() {
    let temp = TempDir::new().unwrap();
    let state = state(&temp);
    create(&state, ID, Some("replay-epochs")).await;
    query(
        &state,
        ID,
        "CREATE TABLE tako_replay_apps(app TEXT PRIMARY KEY, epoch TEXT NOT NULL)",
    )
    .await;
    query(&state, ID, "INSERT INTO tako_replay_apps VALUES ('a', lower(hex(randomblob(16)))), ('b', lower(hex(randomblob(16))))").await;
    let epochs = "SELECT epoch FROM tako_replay_apps ORDER BY app";
    let mut seen = std::collections::BTreeSet::new();
    let mut check = |rows: Value| {
        assert_eq!(rows.as_array().unwrap().len(), 2);
        for row in rows.as_array().unwrap() {
            let epoch = row[0].as_str().unwrap();
            assert!(tako_core::validate_database_generation(epoch).is_ok());
            assert!(seen.insert(epoch.to_owned()), "an epoch was reused");
        }
    };
    check(query(&state, ID, epochs).await["rows"].clone());
    let first = ok(
        &state,
        Command::SnapshotDatabase {
            name: ID.into(),
            reason: SnapshotReason::Manual,
            app: None,
        },
    )
    .await;
    for _ in 0..2 {
        ok(
            &state,
            Command::RestoreDatabase {
                name: ID.into(),
                snapshot: Some(first["snapshot"]["id"].as_str().unwrap().into()),
                at_unix_ms: None,
                backup_key_base64: None,
            },
        )
        .await;
        check(query(&state, ID, epochs).await["rows"].clone());
    }
}

#[tokio::test]
async fn committed_replay_bindings_survive_restart_without_authorizing_application_sql() {
    use hyper::StatusCode;
    use tako_core::{CHANNEL_REPLAY_BINDING, DatabaseBinding, channel_replay_grant_scope};
    use tako_database::http::{self, Access};

    const APP: &str = "web/production";
    let temp = TempDir::new().unwrap();
    let original = state(&temp);
    create(&original, ID, None).await;
    let scope = channel_replay_grant_scope(APP);
    let grant = |scope: &str| Command::GrantDatabases {
        app: scope.into(),
        databases: vec![ID.into()],
        keep_existing: false,
    };
    for malformed in [
        "web#channels",
        "web/production#release",
        "web/production#other#channels",
    ] {
        assert!(!error(&original, grant(malformed)).await.is_empty());
    }
    let key = ok(&original, grant(&scope)).await["peer_key"]
        .as_str()
        .unwrap()
        .to_owned();
    let mut config = AppConfig {
        name: "web".into(),
        environment: "production".into(),
        version: "v1".into(),
        stopped: true,
        ..Default::default()
    };
    let release = crate::release::release_app_path(temp.path(), &config);
    std::fs::create_dir_all(&release).unwrap();
    write_release_manifest(&release, "go", "", &["true"], None, 300);
    let requested = HashMap::from([(
        CHANNEL_REPLAY_BINDING.into(),
        DatabaseBinding {
            name: ID.into(),
            server: "edge".into(),
            peer_key: Some(key.clone()),
        },
    )]);
    config.databases = original
        .databases
        .prepare_bindings(&HashMap::new(), requested.clone(), &config)
        .unwrap();
    original
        .state_store
        .commit_deployment(&config, &[], &HashMap::new(), None, None, None)
        .unwrap();
    let connection = original.channel_replay_database(APP).unwrap().unwrap();
    assert!(
        original
            .databases
            .begin_release(APP, &config, requested)
            .is_err()
    );
    assert!(original.databases.connections(&config.databases).is_empty());
    let debug = format!("{config:?}");
    assert!(!debug.contains(&key) && !debug.contains(&connection.auth_token));
    drop(original);

    let restored = state(&temp);
    let database = &restored.databases.host;
    restored.start_databases(None).await.unwrap();
    // Status/delete must account for bindings even before the app has been restored.
    let status = ok(
        &restored,
        Command::DatabaseStatus {
            name: Some(ID.into()),
        },
    )
    .await;
    assert_eq!(status["databases"][0]["bindings"], json!([scope]));
    assert_eq!(status["databases"][0]["remote_grants"], json!([scope]));
    let delete = || Command::DeleteDatabase {
        name: ID.into(),
        generation: None,
        keep_snapshots: false,
    };
    assert!(error(&restored, delete()).await.contains(&scope));
    restored.restore_apps_from_state_store(true).await.unwrap();
    assert_eq!(
        restored.channel_replay_database(APP).unwrap().unwrap(),
        connection
    );
    let sql = || hyper::Request::post("/v3/pipeline");
    let body = || {
        http_body_util::Full::new(bytes::Bytes::from_static(
            br#"{"requests":[{"type":"execute","stmt":{"sql":"SELECT 1"}}]}"#,
        ))
    };
    let peer = "127.0.0.1:12345".parse().unwrap();
    let probe = || {
        sql()
            .header("authorization", format!("Bearer {}", connection.auth_token))
            .body(body())
            .unwrap()
    };
    assert_eq!(
        http::handle(database, &Access::Apps, peer, probe())
            .await
            .status(),
        StatusCode::OK
    );
    let access = Access::Peers {
        allow: Arc::new(|ip| ip.is_loopback()),
    };
    for (name, expected) in [(&*scope, StatusCode::OK), (APP, StatusCode::FORBIDDEN)] {
        let request = sql()
            .header(http::SCOPE_HEADER, name)
            .header(http::DATABASE_HEADER, ID)
            .header(http::KEY_HEADER, &key)
            .body(body())
            .unwrap();
        assert_eq!(
            http::handle(database, &access, peer, request)
                .await
                .status(),
            expected
        );
    }
    for app in [APP, scope.as_str()] {
        assert!(matches!(
            migrate(&restored, vec![], false, Some(app)).await,
            Response::Error { .. }
        ));
        assert!(
            error(
                &restored,
                Command::SnapshotDatabase {
                    name: ID.into(),
                    reason: SnapshotReason::Release,
                    app: Some(app.into()),
                }
            )
            .await
            .contains("does not use")
        );
    }
    assert_eq!(query(&restored, ID, "SELECT 1").await["rows"], json!([[1]]));
    ok(&restored, Command::Delete { app: APP.into() }).await;
    assert!(restored.channel_replay_database(APP).unwrap().is_none());
    restored.refresh_database_routes();
    assert_eq!(
        http::handle(database, &Access::Apps, peer, probe())
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
    // A grant may be all that remains on a remote home. Delete revokes it too.
    ok(&restored, delete()).await;
    assert!(
        restored
            .state_store
            .all_database_grants()
            .unwrap()
            .is_empty()
    );
    restored.stop_databases().await;
}

#[tokio::test]
async fn listings_put_the_newest_restore_point_first() {
    let temp = TempDir::new().unwrap();
    let state = state(&temp);
    create(&state, ID, Some("same-second-bucket")).await;
    query(&state, ID, "CREATE TABLE items (name TEXT)").await;
    let snapshot = || Command::SnapshotDatabase {
        name: ID.into(),
        reason: SnapshotReason::Manual,
        app: None,
    };
    // Several pairs, each most likely within one second of each other.
    for round in 0..5 {
        query(
            &state,
            ID,
            &format!("INSERT INTO items VALUES ('first {round}')"),
        )
        .await;
        ok(&state, snapshot()).await;
        query(
            &state,
            ID,
            &format!("INSERT INTO items VALUES ('second {round}')"),
        )
        .await;
        let newest = ok(&state, snapshot()).await;
        let listed = ok(&state, Command::ListDatabaseSnapshots { name: ID.into() }).await;
        assert_eq!(
            listed["snapshots"][0]["id"], newest["snapshot"]["id"],
            "the newest snapshot is listed first"
        );
    }
    ok(
        &state,
        Command::RestoreDatabase {
            name: ID.into(),
            snapshot: None,
            at_unix_ms: None,
            backup_key_base64: None,
        },
    )
    .await;
    assert_eq!(count(&state, ID).await, 10);
}

#[tokio::test]
async fn snapshots_are_encrypted_with_the_database_key() {
    let temp = TempDir::new().unwrap();
    let state = state(&temp);
    create(&state, ID, Some("encrypted-bucket")).await;
    query(&state, ID, "CREATE TABLE items (name TEXT)").await;
    query(&state, ID, "INSERT INTO items VALUES ('plaintext-marker')").await;
    ok(
        &state,
        Command::SnapshotDatabase {
            name: ID.into(),
            reason: SnapshotReason::Manual,
            app: None,
        },
    )
    .await;

    let store = crate::databases::backups::test_store("encrypted-bucket").unwrap();
    let objects: Vec<_> = {
        use futures_util::TryStreamExt;
        store.list(None).try_collect().await.unwrap()
    };
    let data = objects
        .iter()
        .find(|meta| meta.location.as_ref().ends_with(".ltx"))
        .expect("an encrypted segment");
    assert!(
        data.location
            .as_ref()
            .starts_with(&format!("_tako/databases/{ID}/{GENERATION}/ltx/"))
    );
    let bytes = store
        .get(&data.location)
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    assert!(!bytes.windows(16).any(|w| w == b"plaintext-marker"));

    let wrong = error(
        &state,
        Command::RestoreDatabase {
            name: ID.into(),
            snapshot: None,
            at_unix_ms: None,
            backup_key_base64: Some(BASE64.encode([9u8; 32])),
        },
    )
    .await;
    assert!(!wrong.is_empty());
    assert_eq!(
        count(&state, ID).await,
        1,
        "a failed restore changes nothing"
    );
}

#[tokio::test]
async fn release_points_name_the_state_before_each_release() {
    let temp = TempDir::new().unwrap();
    let state = state(&temp);
    create(&state, ID, Some("release-bucket")).await;
    let granted = ok(
        &state,
        Command::GrantDatabases {
            app: "web/production".into(),
            databases: vec![ID.into()],
            keep_existing: false,
        },
    )
    .await;
    assert!(granted["peer_key"].is_string(), "a grant issues a peer key");
    query(&state, ID, "CREATE TABLE items (name TEXT)").await;

    let foreign = error(
        &state,
        Command::SnapshotDatabase {
            name: ID.into(),
            reason: SnapshotReason::Release,
            app: Some("other/production".into()),
        },
    )
    .await;
    assert!(foreign.contains("does not use"));

    let release = || Command::SnapshotDatabase {
        name: ID.into(),
        reason: SnapshotReason::Release,
        app: Some("web/production".into()),
    };
    let first = ok(&state, release()).await;
    assert_eq!(first["snapshot"]["reason"], "release");
    query(
        &state,
        ID,
        "INSERT INTO items VALUES ('after the first release')",
    )
    .await;
    let second = ok(&state, release()).await;
    let (first, second) = (
        first["snapshot"]["id"]
            .as_str()
            .unwrap()
            .parse::<u64>()
            .unwrap(),
        second["snapshot"]["id"]
            .as_str()
            .unwrap()
            .parse::<u64>()
            .unwrap(),
    );
    assert!(
        second > first,
        "each point names the state it was recorded at"
    );
    ok(
        &state,
        Command::RestoreDatabase {
            name: ID.into(),
            snapshot: Some(first.to_string()),
            at_unix_ms: None,
            backup_key_base64: None,
        },
    )
    .await;
    assert_eq!(count(&state, ID).await, 0);
    // Deployers record only the point before a release; any other reason
    // would let them add full images.
    for reason in [
        SnapshotReason::Manual,
        SnapshotReason::Migration,
        SnapshotReason::Restore,
    ] {
        let refused = error(
            &state,
            Command::SnapshotDatabase {
                name: ID.into(),
                reason,
                app: Some("web/production".into()),
            },
        )
        .await;
        // Command responses are not printed: CodeQL treats them as secret-bearing (rust/cleartext-logging).
        assert!(
            refused.contains("only the restore point before a release"),
            "a deployer's {reason:?} restore point is refused"
        );
    }
}

#[tokio::test]
async fn a_release_point_for_a_database_without_backups_is_skipped() {
    let temp = TempDir::new().unwrap();
    let state = state(&temp);
    create(&state, ID, None).await;
    let skipped = ok(
        &state,
        Command::SnapshotDatabase {
            name: ID.into(),
            reason: SnapshotReason::Release,
            app: None,
        },
    )
    .await;
    assert!(skipped["skipped"].is_string());
}

#[tokio::test]
async fn a_failed_upload_keeps_the_database_due_for_a_snapshot() {
    let temp = TempDir::new().unwrap();
    let state = state(&temp);
    create(&state, ID, Some("unreachable")).await;
    query(&state, ID, "CREATE TABLE items (name TEXT)").await;
    let failed = error(
        &state,
        Command::SnapshotDatabase {
            name: ID.into(),
            reason: SnapshotReason::Manual,
            app: None,
        },
    )
    .await;
    assert!(failed.contains("unreachable"));
    let id = tako_database::DatabaseId::parse(ID).unwrap();
    assert!(
        state.databases.host.has_changes(&id),
        "the scheduler captures the changes later"
    );
    let status = ok(
        &state,
        Command::DatabaseStatus {
            name: Some(ID.into()),
        },
    )
    .await;
    assert!(status["databases"][0]["backup"]["last_error"].is_string());
}

#[tokio::test]
async fn a_stalled_upload_does_not_delay_other_databases_backups() {
    use crate::databases::backups::{BackupJobs, schedule};
    const STALLED: &str = "archive";
    let temp = TempDir::new().unwrap();
    let state = state(&temp);
    // Listed first, so a scheduler backing up one database at a time would
    // wait on its upload before reaching the other.
    create(&state, STALLED, Some("stalled")).await;
    create(&state, ID, Some("scheduled-bucket")).await;
    for id in [STALLED, ID] {
        query(&state, id, "CREATE TABLE items (name TEXT)").await;
    }
    let replicated = |id: &str| {
        state
            .databases
            .backups
            .lock()
            .get(id)
            .and_then(|backup| backup.replicated_at_unix_secs)
    };
    let store = crate::databases::backups::test_store("scheduled-bucket").unwrap();
    let segments = || async {
        use futures_util::TryStreamExt;
        store
            .list(None)
            .try_collect::<Vec<_>>()
            .await
            .unwrap()
            .into_iter()
            .filter(|meta| meta.location.as_ref().ends_with(".ltx"))
            .count()
    };
    let mut jobs = BackupJobs::new(4);
    schedule(&state, &mut jobs);
    tokio::time::timeout(Duration::from_secs(10), async {
        while replicated(ID).is_none() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the healthy database is backed up while the other upload stalls");
    assert!(
        jobs.is_pending(STALLED),
        "the stalled upload is in progress"
    );
    assert!(replicated(STALLED).is_none());
    let before = segments().await;

    // The healthy database's next change is not held up behind it.
    query(&state, ID, "INSERT INTO items VALUES ('a')").await;
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            schedule(&state, &mut jobs);
            if segments().await > before {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the healthy database keeps uploading its changes");
    assert!(jobs.is_pending(STALLED));
    // Stopping the scheduler abandons the stalled upload.
    drop(jobs);
}

#[tokio::test]
async fn moving_the_backup_destination_resets_snapshot_status() {
    let temp = TempDir::new().unwrap();
    let state = state(&temp);
    create(&state, ID, Some("first-bucket")).await;
    ok(
        &state,
        Command::SnapshotDatabase {
            name: ID.into(),
            reason: SnapshotReason::Manual,
            app: None,
        },
    )
    .await;
    let status = ok(
        &state,
        Command::DatabaseStatus {
            name: Some(ID.into()),
        },
    )
    .await;
    assert!(status["databases"][0]["backup"]["last_snapshot"].is_object());

    let moved = ok(
        &state,
        Command::ConfigureDatabaseBackup {
            name: ID.into(),
            backup: Some(backup("second-bucket")),
        },
    )
    .await;
    assert_eq!(moved["backup"]["bucket"], "second-bucket");
    assert!(
        moved["backup"]["last_snapshot"].is_null(),
        "the first bucket's snapshot is not in the second"
    );
}

#[tokio::test]
async fn import_and_export_round_trip_a_file() {
    let temp = TempDir::new().unwrap();
    let state = state(&temp);
    create(&state, ID, None).await;
    query(&state, ID, "CREATE TABLE items (name TEXT)").await;
    query(&state, ID, "INSERT INTO items VALUES ('before')").await;

    // Export. It copies the database like a snapshot does, but is not one, so
    // the database stays due for its next scheduled snapshot.
    let db_id = tako_database::DatabaseId::parse(ID).unwrap();
    // Start from a database the scheduler considers backed up.
    state
        .databases
        .host
        .snapshot(&db_id, &temp.path().join("copy.sqlite"))
        .await
        .unwrap();
    assert!(!state.databases.host.has_changes(&db_id));
    let export = ok(&state, Command::BeginDatabaseExport { name: ID.into() }).await;
    assert!(
        state.databases.host.has_changes(&db_id),
        "a write during the export is not forgotten"
    );
    let export_id = export["export_id"].as_str().unwrap().to_owned();
    let size = export["size_bytes"].as_u64().unwrap();
    let mut exported = Vec::new();
    while (exported.len() as u64) < size {
        let chunk = ok(
            &state,
            Command::DatabaseExportChunk {
                name: ID.into(),
                export_id: export_id.clone(),
                offset: exported.len() as u64,
            },
        )
        .await;
        exported.extend(
            BASE64
                .decode(chunk["data_base64"].as_str().unwrap())
                .unwrap(),
        );
    }
    ok(
        &state,
        Command::FinishDatabaseExport {
            name: ID.into(),
            export_id,
        },
    )
    .await;
    assert_eq!(hex::encode(Sha256::digest(&exported)), export["sha256"]);

    // Build a replacement locally and import it.
    let replacement = temp.path().join("replacement.sqlite");
    {
        let db = rusqlite::Connection::open(&replacement).unwrap();
        db.execute_batch(
            "CREATE TABLE items (name TEXT); INSERT INTO items VALUES ('a'), ('b'), ('c');",
        )
        .unwrap();
    }
    let bytes = std::fs::read(&replacement).unwrap();
    let upload = ok(
        &state,
        Command::BeginDatabaseImport {
            name: ID.into(),
            size_bytes: bytes.len() as u64,
            sha256: hex::encode(Sha256::digest(&bytes)),
        },
    )
    .await;
    let upload_id = upload["upload_id"].as_str().unwrap().to_owned();
    let chunk = upload["chunk_bytes"].as_u64().unwrap() as usize;
    for (index, part) in bytes.chunks(chunk).enumerate() {
        ok(
            &state,
            Command::DatabaseImportChunk {
                name: ID.into(),
                upload_id: upload_id.clone(),
                offset: (index * chunk) as u64,
                data_base64: BASE64.encode(part),
            },
        )
        .await;
    }
    ok(
        &state,
        Command::FinishDatabaseImport {
            name: ID.into(),
            upload_id,
        },
    )
    .await;
    assert_eq!(count(&state, ID).await, 3);

    // A file that is not a database is rejected before anything changes.
    let garbage = b"not a database".repeat(100);
    let upload = ok(
        &state,
        Command::BeginDatabaseImport {
            name: ID.into(),
            size_bytes: garbage.len() as u64,
            sha256: hex::encode(Sha256::digest(&garbage)),
        },
    )
    .await;
    let upload_id = upload["upload_id"].as_str().unwrap().to_owned();
    ok(
        &state,
        Command::DatabaseImportChunk {
            name: ID.into(),
            upload_id: upload_id.clone(),
            offset: 0,
            data_base64: BASE64.encode(&garbage),
        },
    )
    .await;
    error(
        &state,
        Command::FinishDatabaseImport {
            name: ID.into(),
            upload_id,
        },
    )
    .await;
    assert_eq!(count(&state, ID).await, 3);
}

#[tokio::test]
async fn deleting_a_database_revokes_its_grants() {
    let temp = TempDir::new().unwrap();
    let state = state(&temp);
    create(&state, ID, None).await;
    let duplicate = error(
        &state,
        Command::CreateDatabase {
            name: ID.into(),
            generation: GENERATION.into(),
            backup_key_base64: key(),
            backup: None,
        },
    )
    .await;
    assert!(duplicate.contains("already exists"));

    const OTHER: &str = "orders";
    create(&state, OTHER, None).await;
    let granted = ok(
        &state,
        Command::GrantDatabases {
            app: "web/production".into(),
            databases: vec![ID.into(), OTHER.into()],
            keep_existing: false,
        },
    )
    .await;
    // Grants can outlive the deployments that used them: deleting revokes
    // this database from them and keeps the environment's other grants.
    ok(
        &state,
        Command::DeleteDatabase {
            generation: None,
            name: ID.into(),
            keep_snapshots: false,
        },
    )
    .await;
    let grants = state.state_store.all_database_grants().unwrap();
    let grant = &grants["web/production"];
    assert_eq!(
        grant.databases,
        std::collections::BTreeSet::from([OTHER.to_owned()])
    );
    assert_eq!(Some(grant.key.as_str()), granted["peer_key"].as_str());
    assert!(!state.databases.database_dir(ID).exists());
    error(
        &state,
        Command::DatabaseStatus {
            name: Some(ID.into()),
        },
    )
    .await;
}

#[tokio::test]
async fn a_time_restores_the_state_the_database_had_then() {
    let temp = TempDir::new().unwrap();
    let state = state(&temp);
    create(&state, ID, Some("time-bucket")).await;
    query(&state, ID, "CREATE TABLE items (name TEXT)").await;
    query(&state, ID, "INSERT INTO items VALUES ('good')").await;
    // A release point captures and uploads what the database has so far.
    let point = || Command::SnapshotDatabase {
        name: ID.into(),
        reason: SnapshotReason::Release,
        app: None,
    };
    ok(&state, point()).await;
    tokio::time::sleep(Duration::from_millis(20)).await;
    let good_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    tokio::time::sleep(Duration::from_millis(20)).await;
    query(&state, ID, "INSERT INTO items VALUES ('bad')").await;
    ok(&state, point()).await;
    assert_eq!(count(&state, ID).await, 2);

    let restored = ok(
        &state,
        Command::RestoreDatabase {
            name: ID.into(),
            snapshot: None,
            at_unix_ms: Some(good_at),
            backup_key_base64: None,
        },
    )
    .await;
    assert!(restored["restored"]["transaction"].as_u64().is_some());
    assert!(
        restored["restored"]["captured_at_unix_ms"]
            .as_i64()
            .unwrap()
            <= good_at
    );
    assert_eq!(count(&state, ID).await, 1);
    // The latest state now is the restored one.
    ok(
        &state,
        Command::RestoreDatabase {
            name: ID.into(),
            snapshot: None,
            at_unix_ms: None,
            backup_key_base64: None,
        },
    )
    .await;
    assert_eq!(count(&state, ID).await, 1);
    let early = error(
        &state,
        Command::RestoreDatabase {
            name: ID.into(),
            snapshot: None,
            at_unix_ms: Some(0),
            backup_key_base64: None,
        },
    )
    .await;
    assert!(early.contains("that early"), "{early}");
}

#[tokio::test]
async fn shell_statements_cannot_leave_a_transaction_open() {
    let temp = TempDir::new().unwrap();
    let state = state(&temp);
    create(&state, ID, None).await;
    query(&state, ID, "CREATE TABLE items (name TEXT)").await;
    let begin = error(
        &state,
        Command::QueryDatabase {
            name: ID.into(),
            sql: "BEGIN".into(),
        },
    )
    .await;
    assert!(begin.contains("own"));
    query(&state, ID, "INSERT INTO items VALUES ('kept')").await;
    assert_eq!(count(&state, ID).await, 1, "later statements commit");
}

#[tokio::test]
async fn admin_queries_keep_the_engine_restrictions() {
    let temp = TempDir::new().unwrap();
    let state = state(&temp);
    create(&state, ID, None).await;
    let attach = error(
        &state,
        Command::QueryDatabase {
            name: ID.into(),
            sql: "ATTACH DATABASE '/etc/passwd' AS escape".into(),
        },
    )
    .await;
    assert!(!attach.is_empty());
    let pragma = query(&state, ID, "PRAGMA foreign_keys").await;
    assert_eq!(pragma["rows"][0][0], 1);
    let rows = query(
        &state,
        ID,
        "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n LIMIT 5000) SELECT i FROM n",
    )
    .await;
    assert_eq!(rows["rows"].as_array().unwrap().len(), 1000);
    assert_eq!(rows["truncated"], true);
}

fn migration(name: &str, sql: &str) -> tako_core::DatabaseMigration {
    tako_core::DatabaseMigration {
        name: name.into(),
        sql: sql.into(),
    }
}

async fn migrate(
    state: &ServerState,
    migrations: Vec<tako_core::DatabaseMigration>,
    dry_run: bool,
    app: Option<&str>,
) -> Response {
    state
        .handle_command(Command::MigrateDatabase {
            name: ID.into(),
            owner: "web".into(),
            migrations,
            dry_run,
            app: app.map(str::to_owned),
        })
        .await
}

async fn snapshot_reasons(state: &ServerState) -> Vec<String> {
    let listed = ok(state, Command::ListDatabaseSnapshots { name: ID.into() }).await;
    listed["snapshots"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["reason"].as_str().unwrap().to_owned())
        // Capture starts with a full image of its own.
        .filter(|reason| reason != "scheduled")
        .collect()
}

#[tokio::test]
async fn migrations_apply_once_after_a_snapshot_of_what_they_change() {
    let temp = TempDir::new().unwrap();
    let state = state(&temp);
    create(&state, ID, Some("migrate-bucket")).await;
    let migrations = vec![
        migration("0001_items.sql", "CREATE TABLE items (name TEXT);"),
        migration("0002_seed.sql", "INSERT INTO items VALUES ('first');"),
    ];
    let Response::Ok { data: preview } = migrate(&state, migrations.clone(), true, None).await
    else {
        panic!("a dry run succeeds");
    };
    assert_eq!(
        preview["pending"],
        json!(["0001_items.sql", "0002_seed.sql"])
    );
    assert!(
        error(
            &state,
            Command::QueryDatabase {
                name: ID.into(),
                sql: "SELECT * FROM items".into()
            }
        )
        .await
        .contains("no such table")
    );

    let Response::Ok { data: report } = migrate(&state, migrations.clone(), false, None).await
    else {
        panic!("migrations apply");
    };
    assert_eq!(
        report["applied"],
        json!(["0001_items.sql", "0002_seed.sql"])
    );
    assert_eq!(count(&state, ID).await, 1);
    assert_eq!(snapshot_reasons(&state).await, ["migration"]);

    let mut more = migrations.clone();
    more.push(migration(
        "0003_more.sql",
        "INSERT INTO items VALUES ('second');",
    ));
    let Response::Ok { data: report } = migrate(&state, more.clone(), false, None).await else {
        panic!("the new migration applies");
    };
    assert_eq!(report["applied"], json!(["0003_more.sql"]));
    assert_eq!(snapshot_reasons(&state).await, ["migration", "migration"]);

    let Response::Ok { data: report } = migrate(&state, more, false, None).await else {
        panic!("nothing is pending");
    };
    assert_eq!(report["applied"], json!([]));
    assert_eq!(
        snapshot_reasons(&state).await.len(),
        2,
        "no restore point without pending migrations"
    );
}

#[tokio::test]
async fn deployers_migrate_only_their_own_apps_databases() {
    let temp = TempDir::new().unwrap();
    let state = state(&temp);
    create(&state, ID, None).await;
    let migrations = vec![migration(
        "0001_items.sql",
        "CREATE TABLE items (name TEXT);",
    )];
    let Response::Error { message, .. } =
        migrate(&state, migrations.clone(), false, Some("web/production")).await
    else {
        panic!("an app that does not use the database is refused");
    };
    // Command responses are not printed: CodeQL treats them as secret-bearing (rust/cleartext-logging).
    assert!(
        message.contains("does not use database"),
        "the refusal names the missing grant"
    );

    ok(
        &state,
        Command::GrantDatabases {
            app: "web/production".into(),
            databases: vec![ID.into()],
            keep_existing: false,
        },
    )
    .await;
    let Response::Ok { .. } =
        migrate(&state, migrations.clone(), false, Some("web/production")).await
    else {
        panic!("the app's own migrations apply");
    };
    let Response::Error { message, .. } = state
        .handle_command(Command::MigrateDatabase {
            name: ID.into(),
            owner: "other".into(),
            migrations,
            dry_run: false,
            app: Some("web/production".into()),
        })
        .await
    else {
        panic!("another app's migrations are refused");
    };
    assert!(
        message.contains("cannot apply migrations of other"),
        "the refusal names the other app"
    );
}

#[tokio::test]
async fn reset_empties_a_database_after_a_safety_copy() {
    let temp = TempDir::new().unwrap();
    let state = state(&temp);
    create(&state, ID, Some("reset-bucket")).await;
    query(&state, ID, "CREATE TABLE items (name TEXT)").await;
    query(&state, ID, "INSERT INTO items VALUES ('gone')").await;
    ok(
        &state,
        Command::ResetDatabase {
            name: ID.into(),
            owner: None,
            migrations: Vec::new(),
        },
    )
    .await;
    let missing = error(
        &state,
        Command::QueryDatabase {
            name: ID.into(),
            sql: "SELECT * FROM items".into(),
        },
    )
    .await;
    assert!(
        missing.contains("no such table"),
        "the reset dropped the table"
    );
    assert_eq!(snapshot_reasons(&state).await, ["restore"]);
    // The restore point before the reset restores the contents.
    let listed = ok(&state, Command::ListDatabaseSnapshots { name: ID.into() }).await;
    let snapshot = listed["snapshots"]
        .as_array()
        .unwrap()
        .iter()
        .find(|snapshot| snapshot["reason"] == "restore")
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    ok(
        &state,
        Command::RestoreDatabase {
            name: ID.into(),
            snapshot: Some(snapshot),
            at_unix_ms: None,
            backup_key_base64: None,
        },
    )
    .await;
    assert_eq!(count(&state, ID).await, 1);
}

#[tokio::test]
async fn resets_apply_migrations_before_the_new_file_is_served() {
    let temp = TempDir::new().unwrap();
    let state = state(&temp);
    create(&state, ID, None).await;
    query(&state, ID, "CREATE TABLE items (name TEXT)").await;
    query(&state, ID, "INSERT INTO items VALUES ('old')").await;
    let migration = |name: &str, sql: &str| tako_core::DatabaseMigration {
        name: name.into(),
        sql: sql.into(),
    };
    // A failing migration leaves the database as it was.
    let failed = error(
        &state,
        Command::ResetDatabase {
            name: ID.into(),
            owner: Some("shop".into()),
            migrations: vec![migration("0001_bad.sql", "CREATE TABLE")],
        },
    )
    .await;
    // CodeQL[rust/cleartext-logging]: test-only assertion on a command error
    // string; these commands carry no secrets.
    assert!(failed.contains("was not reset"), "{failed}");
    assert_eq!(count(&state, ID).await, 1);
    // Otherwise the database is replaced with one that already has its
    // schema.
    let reset = ok(
        &state,
        Command::ResetDatabase {
            name: ID.into(),
            owner: Some("shop".into()),
            migrations: vec![migration(
                "0001_items.sql",
                "CREATE TABLE items (name TEXT);",
            )],
        },
    )
    .await;
    assert_eq!(reset["applied"], json!(["0001_items.sql"]));
    assert_eq!(count(&state, ID).await, 0);
}

#[tokio::test]
async fn deleting_a_database_deletes_its_snapshots() {
    let temp = TempDir::new().unwrap();
    let state = state(&temp);
    create(&state, ID, Some("delete-bucket")).await;
    query(&state, ID, "CREATE TABLE items (name TEXT)").await;
    ok(
        &state,
        Command::SnapshotDatabase {
            name: ID.into(),
            reason: SnapshotReason::Manual,
            app: None,
        },
    )
    .await;
    let store = crate::databases::backups::test_store("delete-bucket").unwrap();
    let objects = || async {
        use futures_util::TryStreamExt;
        store
            .list(None)
            .try_collect::<Vec<_>>()
            .await
            .unwrap()
            .len()
    };
    assert_eq!(objects().await, 2, "a manifest and its data");
    let deleted = ok(
        &state,
        Command::DeleteDatabase {
            generation: None,
            name: ID.into(),
            keep_snapshots: false,
        },
    )
    .await;
    assert_eq!(deleted["warnings"], json!([]));
    // Only the fence that stops copies left elsewhere from publishing.
    assert_eq!(objects().await, 1);
}

#[tokio::test]
async fn deleting_a_copy_can_keep_the_snapshots_it_shares() {
    let temp = TempDir::new().unwrap();
    let state = state(&temp);
    create(&state, ID, Some("keep-bucket")).await;
    query(&state, ID, "CREATE TABLE items (name TEXT)").await;
    ok(
        &state,
        Command::SnapshotDatabase {
            name: ID.into(),
            reason: SnapshotReason::Manual,
            app: None,
        },
    )
    .await;
    // A copy left behind by a move shares its snapshots with the copy that
    // replaced it, so deleting it must leave them.
    ok(
        &state,
        Command::DeleteDatabase {
            generation: None,
            name: ID.into(),
            keep_snapshots: true,
        },
    )
    .await;
    let store = crate::databases::backups::test_store("keep-bucket").unwrap();
    use futures_util::TryStreamExt;
    let objects: Vec<_> = store.list(None).try_collect().await.unwrap();
    assert_eq!(objects.len(), 2, "the manifest and its data remain");
}

#[tokio::test]
async fn a_copy_left_by_a_move_stops_publishing_snapshots() {
    let (old_dir, new_dir) = (TempDir::new().unwrap(), TempDir::new().unwrap());
    let old = state(&old_dir);
    old.open_databases(Some("server-a".into())).await.unwrap();
    create(&old, ID, Some("fence-bucket")).await;
    query(&old, ID, "CREATE TABLE items (name TEXT)").await;
    query(&old, ID, "INSERT INTO items VALUES ('moved')").await;
    let snapshot = |reason| Command::SnapshotDatabase {
        name: ID.into(),
        reason,
        app: None,
    };
    ok(&old, snapshot(SnapshotReason::Manual)).await;

    // The database moves while its old server is unreachable, so nobody
    // tells the old copy to stop.
    let new = state(&new_dir);
    new.open_databases(Some("server-b".into())).await.unwrap();
    create(&new, ID, Some("fence-bucket")).await;
    ok(
        &new,
        Command::RestoreDatabase {
            name: ID.into(),
            snapshot: None,
            at_unix_ms: None,
            backup_key_base64: Some(key()),
        },
    )
    .await;
    assert_eq!(count(&new, ID).await, 1);

    // Back online, the old copy diverges but publishes nothing.
    query(&old, ID, "INSERT INTO items VALUES ('stale')").await;
    let refused = error(&old, snapshot(SnapshotReason::Manual)).await;
    // CodeQL[rust/cleartext-logging]: test-only assertion on a command error
    // string; these commands carry no secrets.
    assert!(refused.contains("was moved to another server"), "{refused}");
    let listed = ok(&new, Command::ListDatabaseSnapshots { name: ID.into() }).await;
    let manual = listed["snapshots"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|snapshot| snapshot["reason"] == "manual")
        .count();
    assert_eq!(manual, 1, "only the snapshot taken before the move");
    // The new copy still snapshots.
    query(&new, ID, "INSERT INTO items VALUES ('new')").await;
    let taken = ok(&new, snapshot(SnapshotReason::Manual)).await;
    ok(
        &new,
        Command::RestoreDatabase {
            name: ID.into(),
            snapshot: Some(taken["snapshot"]["id"].as_str().unwrap().into()),
            at_unix_ms: None,
            backup_key_base64: None,
        },
    )
    .await;
    let names = query(&new, ID, "SELECT name FROM items ORDER BY rowid").await;
    assert_eq!(names["rows"], json!([["moved"], ["new"]]));
}

#[tokio::test]
async fn a_new_copy_backs_up_nothing_until_it_is_restored() {
    let (old_dir, new_dir) = (TempDir::new().unwrap(), TempDir::new().unwrap());
    let old = state(&old_dir);
    old.open_databases(Some("server-a".into())).await.unwrap();
    create(&old, ID, Some("move-target-bucket")).await;
    query(&old, ID, "CREATE TABLE items (name TEXT)").await;
    query(&old, ID, "INSERT INTO items VALUES ('moved')").await;
    ok(
        &old,
        Command::SnapshotDatabase {
            name: ID.into(),
            reason: SnapshotReason::Manual,
            app: None,
        },
    )
    .await;
    // A move creates the copy, and the scheduler can reach it before the
    // restore lists the backups.
    let new = state(&new_dir);
    new.open_databases(Some("server-b".into())).await.unwrap();
    create(&new, ID, Some("move-target-bucket")).await;
    crate::databases::backups::back_up(&new, ID).await;
    let status = ok(
        &new,
        Command::DatabaseStatus {
            name: Some(ID.into()),
        },
    )
    .await;
    // CodeQL[rust/cleartext-logging]: test-only assertion on a status string;
    // it carries no secrets.
    assert!(
        status["databases"][0]["backup"]["last_error"]
            .as_str()
            .is_some_and(|error| error.contains("until it is restored")),
        "{status}"
    );
    // Its empty file was not uploaded, so the latest state is the old copy's.
    ok(
        &new,
        Command::RestoreDatabase {
            name: ID.into(),
            snapshot: None,
            at_unix_ms: None,
            backup_key_base64: Some(key()),
        },
    )
    .await;
    assert_eq!(count(&new, ID).await, 1);
    // Restored, it backs up.
    query(&new, ID, "INSERT INTO items VALUES ('new')").await;
    ok(
        &new,
        Command::SnapshotDatabase {
            name: ID.into(),
            reason: SnapshotReason::Manual,
            app: None,
        },
    )
    .await;
}

fn spool_segments(state: &ServerState) -> usize {
    let ltx = state.databases.database_dir(ID).join("replica").join("ltx");
    std::fs::read_dir(&ltx)
        .into_iter()
        .flatten()
        .flatten()
        .map(|level| {
            std::fs::read_dir(level.path())
                .map(|entries| entries.count())
                .unwrap_or(0)
        })
        .sum()
}

#[tokio::test]
async fn a_copy_that_lost_the_database_stops_capturing() {
    let (old_dir, new_dir) = (TempDir::new().unwrap(), TempDir::new().unwrap());
    let old = state(&old_dir);
    old.open_databases(Some("server-a".into())).await.unwrap();
    create(&old, ID, Some("lost-bucket")).await;
    query(&old, ID, "CREATE TABLE items (name TEXT)").await;
    ok(
        &old,
        Command::SnapshotDatabase {
            name: ID.into(),
            reason: SnapshotReason::Manual,
            app: None,
        },
    )
    .await;
    let new = state(&new_dir);
    new.open_databases(Some("server-b".into())).await.unwrap();
    create(&new, ID, Some("lost-bucket")).await;
    ok(
        &new,
        Command::RestoreDatabase {
            name: ID.into(),
            snapshot: None,
            at_unix_ms: None,
            backup_key_base64: Some(key()),
        },
    )
    .await;
    // The old copy, unreachable during the move, keeps taking writes.
    query(&old, ID, "INSERT INTO items VALUES ('stale')").await;
    crate::databases::backups::back_up(&old, ID).await;
    let status = ok(
        &old,
        Command::DatabaseStatus {
            name: Some(ID.into()),
        },
    )
    .await;
    // CodeQL[rust/cleartext-logging]: test-only assertion on a status string;
    // it carries no secrets.
    assert!(
        status["databases"][0]["backup"]["last_error"]
            .as_str()
            .is_some_and(|error| error.contains("moved to another server")),
        "{status}"
    );
    // It stopped capturing, so its spool no longer grows with each write.
    let held = spool_segments(&old);
    for round in 0..3 {
        query(
            &old,
            ID,
            &format!("INSERT INTO items VALUES ('stale {round}')"),
        )
        .await;
        crate::databases::backups::back_up(&old, ID).await;
    }
    assert_eq!(spool_segments(&old), held);
    // Nor after a restart, which forgets everything but the disk.
    crate::databases::backups::forget(&old, ID, false)
        .await
        .unwrap();
    crate::databases::backups::back_up(&old, ID).await;
    assert_eq!(spool_segments(&old), held);
}

#[tokio::test]
async fn a_failed_move_leaves_the_old_copy_snapshotting() {
    let (old_dir, new_dir) = (TempDir::new().unwrap(), TempDir::new().unwrap());
    let old = state(&old_dir);
    old.open_databases(Some("server-a".into())).await.unwrap();
    create(&old, ID, Some("unfenced-bucket")).await;
    query(&old, ID, "CREATE TABLE items (name TEXT)").await;
    let snapshot = || Command::SnapshotDatabase {
        name: ID.into(),
        reason: SnapshotReason::Manual,
        app: None,
    };
    ok(&old, snapshot()).await;
    // The new server claims the database, then fails to decrypt the
    // snapshot with the wrong key.
    let new = state(&new_dir);
    new.open_databases(Some("server-b".into())).await.unwrap();
    create(&new, ID, Some("unfenced-bucket")).await;
    error(
        &new,
        Command::RestoreDatabase {
            name: ID.into(),
            snapshot: None,
            at_unix_ms: None,
            backup_key_base64: Some(BASE64.encode([9u8; 32])),
        },
    )
    .await;
    // The claim was given back, so the old copy still snapshots.
    let store = crate::databases::backups::test_store("unfenced-bucket").unwrap();
    let holder =
        object_store::path::Path::from(format!("_tako/databases/{ID}/{GENERATION}/holder"));
    assert!(matches!(
        store.get(&holder).await,
        Err(object_store::Error::NotFound { .. })
    ));
    query(&old, ID, "INSERT INTO items VALUES ('later')").await;
    ok(&old, snapshot()).await;
}

#[tokio::test]
async fn deleting_a_database_fences_its_snapshots_from_a_copy_that_holds_them() {
    let (home_dir, target_dir) = (TempDir::new().unwrap(), TempDir::new().unwrap());
    let home = state(&home_dir);
    home.open_databases(Some("server-a".into())).await.unwrap();
    create(&home, ID, Some("held-elsewhere-bucket")).await;
    query(&home, ID, "CREATE TABLE items (name TEXT)").await;
    let snapshot = || Command::SnapshotDatabase {
        name: ID.into(),
        reason: SnapshotReason::Manual,
        app: None,
    };
    ok(&home, snapshot()).await;
    // A move restored a copy elsewhere, which now holds the snapshots, but
    // the network still names the first server (the move did not finish).
    let target = state(&target_dir);
    target
        .open_databases(Some("server-b".into()))
        .await
        .unwrap();
    create(&target, ID, Some("held-elsewhere-bucket")).await;
    ok(
        &target,
        Command::RestoreDatabase {
            name: ID.into(),
            snapshot: None,
            at_unix_ms: None,
            backup_key_base64: None,
        },
    )
    .await;
    // Deleting the database deletes its snapshots all the same, and the
    // copy that held them, unreachable for the rest of the delete,
    // publishes nothing afterwards.
    let deleted = ok(
        &home,
        Command::DeleteDatabase {
            generation: Some(GENERATION.into()),
            name: ID.into(),
            keep_snapshots: false,
        },
    )
    .await;
    // CodeQL[rust/cleartext-logging]: test-only assertions on command results;
    // these commands carry no secrets.
    assert_eq!(deleted["warnings"], json!([]), "{deleted}");
    let listed = ok(&target, Command::ListDatabaseSnapshots { name: ID.into() }).await;
    assert_eq!(listed["snapshots"], json!([]), "{listed}");
    query(&target, ID, "INSERT INTO items VALUES ('later')").await;
    let refused = error(&target, snapshot()).await;
    assert!(refused.contains("was deleted"), "{refused}");
}

#[tokio::test]
async fn a_copy_the_delete_could_not_reach_publishes_nothing_afterwards() {
    let (home_dir, stale_dir) = (TempDir::new().unwrap(), TempDir::new().unwrap());
    let home = state(&home_dir);
    home.open_databases(Some("server-a".into())).await.unwrap();
    create(&home, ID, Some("deleted-fence-bucket")).await;
    query(&home, ID, "CREATE TABLE items (name TEXT)").await;
    let snapshot = || Command::SnapshotDatabase {
        name: ID.into(),
        reason: SnapshotReason::Manual,
        app: None,
    };
    ok(&home, snapshot()).await;
    // A copy of the same database, such as one an unfinished move left on
    // a server that is offline while the database is deleted.
    let stale = state(&stale_dir);
    stale.open_databases(Some("server-b".into())).await.unwrap();
    create(&stale, ID, Some("deleted-fence-bucket")).await;
    ok(
        &home,
        Command::DeleteDatabase {
            generation: None,
            name: ID.into(),
            keep_snapshots: false,
        },
    )
    .await;
    // Back online, it publishes nothing into the deleted history.
    query(&stale, ID, "CREATE TABLE later (name TEXT)").await;
    let refused = error(&stale, snapshot()).await;
    // CodeQL[rust/cleartext-logging]: test-only assertion on a command error
    // string; these commands carry no secrets.
    assert!(refused.contains("was deleted"), "{refused}");
    let store = crate::databases::backups::test_store("deleted-fence-bucket").unwrap();
    let prefix = object_store::path::Path::from(format!("_tako/databases/{ID}/{GENERATION}"));
    let left: Vec<String> =
        futures_util::TryStreamExt::try_collect::<Vec<_>>(store.list(Some(&prefix)))
            .await
            .unwrap()
            .into_iter()
            .map(|meta| meta.location.to_string())
            .collect();
    assert_eq!(left.len(), 1, "only the fence remains: {left:?}");
    assert!(left[0].ends_with("/holder"), "{left:?}");
}

#[tokio::test]
async fn moves_resume_and_abandoned_moves_give_the_snapshots_back() {
    let (old_dir, new_dir) = (TempDir::new().unwrap(), TempDir::new().unwrap());
    let old = state(&old_dir);
    old.open_databases(Some("server-a".into())).await.unwrap();
    create(&old, ID, Some("abandon-bucket")).await;
    query(&old, ID, "CREATE TABLE items (name TEXT)").await;
    let snapshot = || Command::SnapshotDatabase {
        name: ID.into(),
        reason: SnapshotReason::Manual,
        app: None,
    };
    ok(&old, snapshot()).await;
    let new = state(&new_dir);
    new.open_databases(Some("server-b".into())).await.unwrap();
    create(&new, ID, Some("abandon-bucket")).await;
    let restore = || Command::RestoreDatabase {
        name: ID.into(),
        snapshot: None,
        at_unix_ms: None,
        backup_key_base64: Some(key()),
    };
    ok(&new, restore()).await;
    // The status names the generation, so a retried move finds this copy
    // and restores it again instead of creating it.
    let status = ok(&new, Command::DatabaseStatus { name: None }).await;
    assert_eq!(status["databases"][0]["generation"], GENERATION);
    ok(&new, restore()).await;
    // The move is abandoned: removing the new copy gives the snapshots back,
    // so the copy the network still names takes them again.
    assert!(
        error(&old, snapshot())
            .await
            .contains("was moved to another server")
    );
    ok(
        &new,
        Command::DeleteDatabase {
            generation: None,
            name: ID.into(),
            keep_snapshots: true,
        },
    )
    .await;
    query(&old, ID, "INSERT INTO items VALUES ('kept')").await;
    ok(&old, snapshot()).await;
}

#[tokio::test]
async fn a_create_that_fails_to_open_leaves_nothing_behind() {
    let temp = TempDir::new().unwrap();
    let state = state(&temp);
    // Something else already holds the name in the database host, so the
    // new file cannot be registered.
    let other = temp.path().join("other.sqlite");
    tako_database::DatabaseHost::create_file(&other).unwrap();
    let id = tako_database::DatabaseId::parse(ID).unwrap();
    state.databases.host.register(id.clone(), other).unwrap();
    let failed = error(
        &state,
        Command::CreateDatabase {
            name: ID.into(),
            generation: GENERATION.into(),
            backup_key_base64: key(),
            backup: None,
        },
    )
    .await;
    // CodeQL[rust/cleartext-logging]: test-only assertion on a command error
    // string; these commands carry no secrets.
    assert!(failed.contains("open database"), "{failed}");
    assert!(state.state_store.get_database(ID).unwrap().is_none());
    assert!(!state.databases.database_dir(ID).exists());
    // Once the obstacle is gone, the same name can be created.
    state.databases.host.unregister(&id).await.unwrap();
    create(&state, ID, None).await;
}

#[tokio::test]
async fn opening_databases_again_after_a_failure_retries() {
    let temp = TempDir::new().unwrap();
    let state = state(&temp);
    // The databases directory cannot be created while a file is in its place.
    let root = temp.path().join("databases");
    std::fs::write(&root, "").unwrap();
    assert!(state.open_databases(None).await.is_err());
    std::fs::remove_file(&root).unwrap();
    state.open_databases(None).await.unwrap();
    assert!(root.is_dir(), "the second call opened the databases");
}

#[tokio::test]
async fn only_bindings_to_this_server_count_for_its_copy() {
    let temp = TempDir::new().unwrap();
    let state = state(&temp);
    state.open_databases(Some("server-a".into())).await.unwrap();
    create(&state, ID, None).await;
    let binding = |server: &str| tako_core::DatabaseBinding {
        name: ID.into(),
        server: server.into(),
        peer_key: None,
    };
    assert!(state.databases.binds_here(&binding("server-a"), ID));
    // The database moved: a binding to it on server-b leaves the copy here
    // unbound, so it can be removed and an app's deployer cannot migrate it.
    assert!(!state.databases.binds_here(&binding("server-b"), ID));
    assert!(!state.databases.binds_here(&binding("server-a"), "other"));
}

#[tokio::test]
async fn bindings_keep_their_token_only_for_the_same_home() {
    let temp = TempDir::new().unwrap();
    let state = state(&temp);
    create(&state, ID, None).await;
    let binding = |server: &str| tako_core::DatabaseBinding {
        name: ID.into(),
        server: server.into(),
        peer_key: None,
    };
    let previous = HashMap::from([(
        "main".to_owned(),
        crate::state_store::AppDatabase {
            binding: tako_core::DatabaseBinding {
                peer_key: Some("old-key".into()),
                ..binding("edge-a")
            },
            token: "old-token".into(),
        },
    )]);
    let config = crate::instances::AppConfig::default();
    let prepare = |server: &str| {
        state
            .databases
            .prepare_bindings(
                &previous,
                HashMap::from([("main".to_owned(), binding(server))]),
                &config,
            )
            .unwrap()
            .remove("main")
            .unwrap()
    };
    let same = prepare("edge-a");
    assert_eq!(same.token, "old-token");
    assert_eq!(same.binding.peer_key.as_deref(), Some("old-key"));
    // After the database moved, old and new instances need different tokens
    // so each reaches its own home during the rollout.
    let moved = prepare("edge-b");
    assert_ne!(moved.token, "old-token");
    assert_eq!(
        moved.binding.peer_key, None,
        "the old home's key is not reused"
    );
}

#[tokio::test]
async fn deploys_widen_grants_before_a_rollout_and_narrow_them_after() {
    let temp = TempDir::new().unwrap();
    let state = state(&temp);
    create(&state, ID, None).await;
    create(&state, "orders", None).await;
    let grant = |databases: &[&str], keep_existing| Command::GrantDatabases {
        app: "web/production".into(),
        databases: databases.iter().map(|d| (*d).to_owned()).collect(),
        keep_existing,
    };
    let granted = |state: &ServerState| {
        state.state_store.all_database_grants().unwrap()["web/production"]
            .databases
            .clone()
    };
    ok(&state, grant(&[ID], false)).await;
    // The new release binds orders instead; the running one still uses shop.
    let widened = ok(&state, grant(&["orders"], true)).await;
    assert_eq!(
        granted(&state),
        std::collections::BTreeSet::from([ID.to_owned(), "orders".to_owned()])
    );
    // Once the rollout succeeds, only what the new release uses remains,
    // under the same peer key.
    let narrowed = ok(&state, grant(&["orders"], false)).await;
    assert_eq!(
        granted(&state),
        std::collections::BTreeSet::from(["orders".to_owned()])
    );
    assert_eq!(widened["peer_key"], narrowed["peer_key"]);
}

#[tokio::test]
async fn interrupted_work_directories_are_removed_once_stale() {
    let temp = TempDir::new().unwrap();
    let state = state(&temp);
    create(&state, ID, None).await;
    let root = state.databases.work_dir(ID);
    std::fs::create_dir_all(root.join("restore-left-behind")).unwrap();
    std::fs::write(root.join("restore-left-behind/database.sqlite"), "partial").unwrap();
    let now = std::time::SystemTime::now();
    // A directory an operation may still be using stays.
    assert_eq!(
        crate::databases::backups::remove_stale_work_dirs(&root, now),
        0
    );
    assert!(root.join("restore-left-behind").exists());
    let tomorrow = now + std::time::Duration::from_secs(25 * 60 * 60);
    assert_eq!(
        crate::databases::backups::remove_stale_work_dirs(&root, tomorrow),
        1
    );
    assert!(!root.join("restore-left-behind").exists());
}

#[tokio::test]
async fn finishing_a_transfer_under_another_name_leaves_it_in_place() {
    let temp = TempDir::new().unwrap();
    let state = state(&temp);
    create(&state, ID, None).await;
    create(&state, "other", None).await;
    let bytes = b"0123456789".repeat(10);
    let upload = ok(
        &state,
        Command::BeginDatabaseImport {
            name: ID.into(),
            size_bytes: bytes.len() as u64,
            sha256: hex::encode(Sha256::digest(&bytes)),
        },
    )
    .await;
    let upload_id = upload["upload_id"].as_str().unwrap().to_owned();
    error(
        &state,
        Command::FinishDatabaseImport {
            name: "other".into(),
            upload_id: upload_id.clone(),
        },
    )
    .await;
    // The upload goes on under its own name.
    ok(
        &state,
        Command::DatabaseImportChunk {
            name: ID.into(),
            upload_id: upload_id.clone(),
            offset: 0,
            data_base64: BASE64.encode(&bytes),
        },
    )
    .await;
    let export = ok(&state, Command::BeginDatabaseExport { name: ID.into() }).await;
    let export_id = export["export_id"].as_str().unwrap().to_owned();
    error(
        &state,
        Command::FinishDatabaseExport {
            name: "other".into(),
            export_id: export_id.clone(),
        },
    )
    .await;
    ok(
        &state,
        Command::DatabaseExportChunk {
            name: ID.into(),
            export_id,
            offset: 0,
        },
    )
    .await;
}

#[tokio::test]
async fn transfers_still_receiving_chunks_keep_their_work_directory() {
    let temp = TempDir::new().unwrap();
    let state = state(&temp);
    create(&state, ID, None).await;
    let bytes = b"0123456789".repeat(10);
    let upload = ok(
        &state,
        Command::BeginDatabaseImport {
            name: ID.into(),
            size_bytes: bytes.len() as u64,
            sha256: hex::encode(Sha256::digest(&bytes)),
        },
    )
    .await;
    let upload_id = upload["upload_id"].as_str().unwrap().to_owned();
    // The transfer started more than a day ago.
    let root = state.databases.work_dir(ID);
    let dirs: Vec<_> = std::fs::read_dir(&root).unwrap().flatten().collect();
    assert_eq!(dirs.len(), 1);
    let old = std::time::SystemTime::now() - std::time::Duration::from_secs(2 * 24 * 60 * 60);
    std::fs::File::open(dirs[0].path())
        .unwrap()
        .set_modified(old)
        .unwrap();
    // A chunk arriving now marks it as in use.
    ok(
        &state,
        Command::DatabaseImportChunk {
            name: ID.into(),
            upload_id,
            offset: 0,
            data_base64: BASE64.encode(&bytes[..10]),
        },
    )
    .await;
    let now = std::time::SystemTime::now();
    assert_eq!(
        crate::databases::backups::remove_stale_work_dirs(&root, now),
        0
    );
    assert!(dirs[0].path().exists());
}

#[tokio::test]
async fn a_database_a_deploy_is_binding_cannot_be_deleted() {
    let temp = TempDir::new().unwrap();
    let state = state(&temp);
    create(&state, ID, None).await;
    let delete = || Command::DeleteDatabase {
        generation: None,
        name: ID.into(),
        keep_snapshots: false,
    };
    // A deploy has checked the database and is installing its binding.
    let pins = state.databases.pin([ID]).unwrap();
    let refused = error(&state, delete()).await;
    assert!(
        refused.contains("deploy that binds"),
        "the delete waits for the deploy"
    );
    assert!(
        state
            .databases
            .host
            .is_registered(&tako_database::DatabaseId::parse(ID).unwrap())
    );
    drop(pins);
    ok(&state, delete()).await;
    // A deploy that starts once the database is gone fails its own check.
    assert!(state.databases.pin([ID]).is_ok());
}

#[tokio::test]
async fn deleting_a_database_ends_its_transfers() {
    let temp = TempDir::new().unwrap();
    let state = state(&temp);
    create(&state, ID, None).await;
    let bytes = b"0123456789".repeat(10);
    let upload = ok(
        &state,
        Command::BeginDatabaseImport {
            name: ID.into(),
            size_bytes: bytes.len() as u64,
            sha256: hex::encode(Sha256::digest(&bytes)),
        },
    )
    .await;
    let upload_id = upload["upload_id"].as_str().unwrap().to_owned();
    let export = ok(&state, Command::BeginDatabaseExport { name: ID.into() }).await;
    let export_id = export["export_id"].as_str().unwrap().to_owned();
    // Both hold disk space in the ledger snapshots and restores share.
    let size = state
        .databases
        .host
        .size_bytes(&tako_database::DatabaseId::parse(ID).unwrap())
        .unwrap();
    assert_eq!(
        state.databases.snapshot_space.reserved_bytes(),
        2 * bytes.len() as u64 + size
    );
    ok(
        &state,
        Command::DeleteDatabase {
            generation: None,
            name: ID.into(),
            keep_snapshots: false,
        },
    )
    .await;
    assert_eq!(state.databases.snapshot_space.reserved_bytes(), 0);
    // A database of the same name created afterwards receives none of the
    // old transfers' chunks.
    create(&state, ID, None).await;
    let refused = error(
        &state,
        Command::DatabaseImportChunk {
            name: ID.into(),
            upload_id,
            offset: 0,
            data_base64: BASE64.encode(&bytes),
        },
    )
    .await;
    // CodeQL[rust/cleartext-logging]: test-only assertions on command error
    // strings; these commands carry no secrets.
    assert!(refused.contains("not found"), "{refused}");
    let refused = error(
        &state,
        Command::DatabaseExportChunk {
            name: ID.into(),
            export_id,
            offset: 0,
        },
    )
    .await;
    assert!(refused.contains("not found"), "{refused}");
}

#[tokio::test]
async fn a_database_being_deleted_cannot_be_granted() {
    let temp = TempDir::new().unwrap();
    let state = state(&temp);
    create(&state, ID, None).await;
    let grant = || Command::GrantDatabases {
        app: "web/production".into(),
        databases: vec![ID.into()],
        keep_existing: false,
    };
    // A delete has checked the database and is revoking its grants.
    let deleting = state.databases.start_deleting(ID).unwrap();
    let refused = error(&state, grant()).await;
    // CodeQL[rust/cleartext-logging]: test-only assertion on a command error
    // string; these commands carry no secrets.
    assert!(refused.contains("being deleted"), "{refused}");
    assert!(
        state
            .state_store
            .all_database_grants()
            .unwrap()
            .get("web/production")
            .is_none_or(|grant| grant.databases.is_empty())
    );
    drop(deleting);
    ok(&state, grant()).await;
}

#[tokio::test]
async fn a_delete_acts_only_on_the_generation_it_names() {
    let temp = TempDir::new().unwrap();
    let state = state(&temp);
    create(&state, ID, None).await;
    // The caller resolved an earlier database of this name.
    let refused = error(
        &state,
        Command::DeleteDatabase {
            generation: Some("an-earlier-generation".into()),
            name: ID.into(),
            keep_snapshots: false,
        },
    )
    .await;
    // CodeQL[rust/cleartext-logging]: test-only assertion on a command error
    // string; these commands carry no secrets.
    assert!(refused.contains("not the one asked for"), "{refused}");
    query(&state, ID, "SELECT 1").await;
    ok(
        &state,
        Command::DeleteDatabase {
            generation: Some(GENERATION.into()),
            name: ID.into(),
            keep_snapshots: false,
        },
    )
    .await;
}

#[tokio::test]
async fn a_database_a_draining_release_still_binds_cannot_be_deleted() {
    let temp = TempDir::new().unwrap();
    let state = state(&temp);
    create(&state, ID, None).await;
    // A rollout from this database to another keeps the old set active
    // until old instances drain; the app's stored config has the new one.
    let old = std::collections::HashMap::from([(
        "main".to_owned(),
        crate::state_store::AppDatabase {
            binding: tako_core::DatabaseBinding {
                name: ID.into(),
                server: String::new(),
                peer_key: None,
            },
            token: "old-instances".into(),
        },
    )]);
    let new = std::collections::HashMap::new();
    state
        .databases
        .apply_bindings("web/production", &[&old, &new]);
    let refused = error(
        &state,
        Command::DeleteDatabase {
            generation: None,
            name: ID.into(),
            keep_snapshots: false,
        },
    )
    .await;
    // CodeQL[rust/cleartext-logging]: test-only assertion on a command error
    // string; these commands carry no secrets.
    assert!(refused.contains("web/production"), "{refused}");
    // Once the rollout commits, only the new set stays.
    state.databases.apply_bindings("web/production", &[&new]);
    ok(
        &state,
        Command::DeleteDatabase {
            generation: None,
            name: ID.into(),
            keep_snapshots: false,
        },
    )
    .await;
}

#[tokio::test]
async fn databases_that_failed_to_open_are_retried() {
    let temp = TempDir::new().unwrap();
    let state = state(&temp);
    state.open_databases(None).await.unwrap();
    create(&state, ID, None).await;
    let id = tako_database::DatabaseId::parse(ID).unwrap();
    // As if opening it had failed at startup.
    state.databases.host.unregister(&id).await.unwrap();
    assert!(!state.databases.host.is_registered(&id));
    state.retry_database_registration(ID).await;
    assert!(state.databases.host.is_registered(&id));
    query(&state, ID, "SELECT 1").await;
}

#[tokio::test]
async fn administrator_query_results_fit_through_the_management_relay() {
    let temp = TempDir::new().unwrap();
    let state = state(&temp);
    create(&state, ID, None).await;
    // About 12 MiB as JSON: under the app response limit, over what the
    // 16 MiB relay could return with its envelope.
    let refused = error(
        &state,
        Command::QueryDatabase {
            name: ID.into(),
            sql: "SELECT randomblob(9 * 1024 * 1024)".into(),
        },
    )
    .await;
    assert!(
        refused.contains("larger than 8 MiB"),
        "refused as too large"
    );
    query(&state, ID, "SELECT randomblob(1024)").await;
}
