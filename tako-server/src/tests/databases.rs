//! Hosted database management on one server: create, query, snapshot,
//! restore, import, export, grants, and delete.

use super::*;
use crate::isolation::fixture::TestDataDir as TempDir;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use object_store::ObjectStoreExt as _;
use sha2::{Digest, Sha256};
use tako_core::{SnapshotReason, StorageBinding, StorageProvider};

const ID: &str = "0123456789abcdef0123456789abcdef";

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
        other => panic!("expected ok: {other:?}"),
    }
}

async fn error(state: &ServerState, command: Command) -> String {
    match state.handle_command(command).await {
        Response::Error { message, .. } => message,
        other => panic!("expected an error: {other:?}"),
    }
}

async fn query(state: &ServerState, id: &str, sql: &str) -> Value {
    ok(
        state,
        Command::QueryDatabase {
            id: id.into(),
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
            id: id.into(),
            name: "shop".into(),
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
            id: ID.into(),
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
            id: ID.into(),
            snapshot: Some(first_id.clone()),
            backup_key_base64: None,
        },
    )
    .await;
    assert_eq!(restored["snapshot"]["id"], first_id);
    assert_eq!(count(&state, ID).await, 1);
    assert!(
        state
            .databases
            .database_dir(ID)
            .join("previous.sqlite")
            .exists(),
        "the replaced file is kept"
    );

    // The restore first saved the changed contents it replaced.
    let listed = ok(&state, Command::ListDatabaseSnapshots { id: ID.into() }).await;
    let reasons: Vec<_> = listed["snapshots"]
        .as_array()
        .unwrap()
        .iter()
        .map(|snapshot| snapshot["reason"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(reasons.len(), 2, "{listed}");
    assert!(reasons.contains(&"restore".to_owned()), "{reasons:?}");
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
            id: ID.into(),
            snapshot: Some(pre_restore),
            backup_key_base64: None,
        },
    )
    .await;
    assert_eq!(count(&state, ID).await, 2);

    let status = ok(
        &state,
        Command::DatabaseStatus {
            id: Some(ID.into()),
        },
    )
    .await;
    let database = &status["databases"][0];
    assert_eq!(database["name"], "shop");
    assert_eq!(database["backup"]["bucket"], "restore-bucket");
    assert!(database["backup"]["last_snapshot"]["id"].is_string());
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
            id: ID.into(),
            reason: SnapshotReason::Manual,
            app: None,
        },
    )
    .await;

    let store = crate::databases::snapshots::test_store("encrypted-bucket").unwrap();
    let objects: Vec<_> = {
        use futures_util::TryStreamExt;
        store.list(None).try_collect().await.unwrap()
    };
    let data = objects
        .iter()
        .find(|meta| meta.location.as_ref().ends_with(".sqlite.zst.enc"))
        .expect("an encrypted data object");
    assert!(
        data.location
            .as_ref()
            .starts_with(&format!("_tako/databases/{ID}/snapshots/"))
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
            id: ID.into(),
            snapshot: None,
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
async fn release_snapshots_skip_unchanged_databases_and_foreign_apps() {
    let temp = TempDir::new().unwrap();
    let state = state(&temp);
    create(&state, ID, Some("release-bucket")).await;
    ok(
        &state,
        Command::GrantDatabases {
            app: "web/production".into(),
            databases: vec![ID.into()],
        },
    )
    .await;

    let foreign = error(
        &state,
        Command::SnapshotDatabase {
            id: ID.into(),
            reason: SnapshotReason::Release,
            app: Some("other/production".into()),
        },
    )
    .await;
    assert!(foreign.contains("does not use"), "{foreign}");

    let first = ok(
        &state,
        Command::SnapshotDatabase {
            id: ID.into(),
            reason: SnapshotReason::Release,
            app: Some("web/production".into()),
        },
    )
    .await;
    assert!(
        first["snapshot"].is_object(),
        "a new database counts as changed"
    );
    let second = ok(
        &state,
        Command::SnapshotDatabase {
            id: ID.into(),
            reason: SnapshotReason::Release,
            app: Some("web/production".into()),
        },
    )
    .await;
    assert!(second["skipped"].is_string(), "{second}");
}

#[tokio::test]
async fn import_and_export_round_trip_a_file() {
    let temp = TempDir::new().unwrap();
    let state = state(&temp);
    create(&state, ID, None).await;
    query(&state, ID, "CREATE TABLE items (name TEXT)").await;
    query(&state, ID, "INSERT INTO items VALUES ('before')").await;

    // Export.
    let export = ok(&state, Command::BeginDatabaseExport { id: ID.into() }).await;
    let export_id = export["export_id"].as_str().unwrap().to_owned();
    let size = export["size_bytes"].as_u64().unwrap();
    let mut exported = Vec::new();
    while (exported.len() as u64) < size {
        let chunk = ok(
            &state,
            Command::DatabaseExportChunk {
                id: ID.into(),
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
            id: ID.into(),
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
            id: ID.into(),
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
                id: ID.into(),
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
            id: ID.into(),
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
            id: ID.into(),
            size_bytes: garbage.len() as u64,
            sha256: hex::encode(Sha256::digest(&garbage)),
        },
    )
    .await;
    let upload_id = upload["upload_id"].as_str().unwrap().to_owned();
    ok(
        &state,
        Command::DatabaseImportChunk {
            id: ID.into(),
            upload_id: upload_id.clone(),
            offset: 0,
            data_base64: BASE64.encode(&garbage),
        },
    )
    .await;
    error(
        &state,
        Command::FinishDatabaseImport {
            id: ID.into(),
            upload_id,
        },
    )
    .await;
    assert_eq!(count(&state, ID).await, 3);
}

#[tokio::test]
async fn databases_in_use_cannot_be_deleted() {
    let temp = TempDir::new().unwrap();
    let state = state(&temp);
    create(&state, ID, None).await;
    let duplicate = error(
        &state,
        Command::CreateDatabase {
            id: ID.into(),
            name: "shop".into(),
            backup_key_base64: key(),
            backup: None,
        },
    )
    .await;
    assert!(duplicate.contains("already exists"), "{duplicate}");

    ok(
        &state,
        Command::GrantDatabases {
            app: "web/production".into(),
            databases: vec![ID.into()],
        },
    )
    .await;
    let blocked = error(&state, Command::DeleteDatabase { id: ID.into() }).await;
    assert!(blocked.contains("web/production"), "{blocked}");

    ok(
        &state,
        Command::GrantDatabases {
            app: "web/production".into(),
            databases: vec![],
        },
    )
    .await;
    ok(&state, Command::DeleteDatabase { id: ID.into() }).await;
    assert!(!state.databases.database_dir(ID).exists());
    error(
        &state,
        Command::DatabaseStatus {
            id: Some(ID.into()),
        },
    )
    .await;
}

#[tokio::test]
async fn admin_queries_keep_the_engine_restrictions() {
    let temp = TempDir::new().unwrap();
    let state = state(&temp);
    create(&state, ID, None).await;
    let attach = error(
        &state,
        Command::QueryDatabase {
            id: ID.into(),
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
