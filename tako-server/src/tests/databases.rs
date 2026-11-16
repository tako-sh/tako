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
    let listed = ok(&state, Command::ListDatabaseSnapshots { name: ID.into() }).await;
    let reasons: Vec<_> = listed["snapshots"]
        .as_array()
        .unwrap()
        .iter()
        .map(|snapshot| snapshot["reason"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(reasons.len(), 2, "a manual and a pre-restore snapshot");
    assert!(reasons.contains(&"restore".to_owned()));
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
            .starts_with(&format!("_tako/databases/{ID}/{GENERATION}/snapshots/"))
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

    // A new, empty database has nothing to snapshot yet.
    let empty = ok(
        &state,
        Command::SnapshotDatabase {
            name: ID.into(),
            reason: SnapshotReason::Release,
            app: None,
        },
    )
    .await;
    assert!(empty["skipped"].is_string());
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

    let first = ok(
        &state,
        Command::SnapshotDatabase {
            name: ID.into(),
            reason: SnapshotReason::Release,
            app: Some("web/production".into()),
        },
    )
    .await;
    assert!(
        first["snapshot"].is_object(),
        "a changed database is snapshotted"
    );
    let second = ok(
        &state,
        Command::SnapshotDatabase {
            name: ID.into(),
            reason: SnapshotReason::Release,
            app: Some("web/production".into()),
        },
    )
    .await;
    assert!(
        second["skipped"].is_string(),
        "an unchanged database is skipped"
    );
    // Deployers take only the release snapshot, which skips an unchanged
    // database; any other reason would let them add full snapshots.
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
            refused.contains("only the snapshot before a release"),
            "a deployer's {reason:?} snapshot is refused"
        );
    }
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
        "the scheduler retries the snapshot"
    );
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
async fn latest_restores_skip_the_safety_copies_restores_take() {
    let temp = TempDir::new().unwrap();
    let state = state(&temp);
    create(&state, ID, Some("latest-bucket")).await;
    query(&state, ID, "CREATE TABLE items (name TEXT)").await;
    query(&state, ID, "INSERT INTO items VALUES ('good')").await;
    ok(
        &state,
        Command::SnapshotDatabase {
            name: ID.into(),
            reason: SnapshotReason::Manual,
            app: None,
        },
    )
    .await;
    query(&state, ID, "INSERT INTO items VALUES ('bad')").await;
    let latest = || Command::RestoreDatabase {
        name: ID.into(),
        snapshot: None,
        backup_key_base64: None,
    };
    ok(&state, latest()).await;
    assert_eq!(count(&state, ID).await, 1);
    // The first restore saved the bad contents; repeating it restores the
    // good snapshot again, not that safety copy.
    query(&state, ID, "INSERT INTO items VALUES ('later')").await;
    ok(&state, latest()).await;
    assert_eq!(count(&state, ID).await, 1);
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
    // The fresh database had nothing to copy.
    assert!(snapshot_reasons(&state).await.is_empty());

    let mut more = migrations.clone();
    more.push(migration(
        "0003_more.sql",
        "INSERT INTO items VALUES ('second');",
    ));
    let Response::Ok { data: report } = migrate(&state, more.clone(), false, None).await else {
        panic!("the new migration applies");
    };
    assert_eq!(report["applied"], json!(["0003_more.sql"]));
    assert_eq!(snapshot_reasons(&state).await, ["migration"]);

    let Response::Ok { data: report } = migrate(&state, more, false, None).await else {
        panic!("nothing is pending");
    };
    assert_eq!(report["applied"], json!([]));
    assert_eq!(
        snapshot_reasons(&state).await.len(),
        1,
        "no snapshot without pending migrations"
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
    ok(&state, Command::ResetDatabase { name: ID.into() }).await;
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
    // The safety copy restores the contents.
    let snapshot =
        ok(&state, Command::ListDatabaseSnapshots { name: ID.into() }).await["snapshots"][0]["id"]
            .as_str()
            .unwrap()
            .to_owned();
    ok(
        &state,
        Command::RestoreDatabase {
            name: ID.into(),
            snapshot: Some(snapshot),
            backup_key_base64: None,
        },
    )
    .await;
    assert_eq!(count(&state, ID).await, 1);
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
    let store = crate::databases::snapshots::test_store("delete-bucket").unwrap();
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
            name: ID.into(),
            keep_snapshots: false,
        },
    )
    .await;
    assert_eq!(deleted["warnings"], json!([]));
    assert_eq!(objects().await, 0);
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
            name: ID.into(),
            keep_snapshots: true,
        },
    )
    .await;
    let store = crate::databases::snapshots::test_store("keep-bucket").unwrap();
    use futures_util::TryStreamExt;
    let objects: Vec<_> = store.list(None).try_collect().await.unwrap();
    assert_eq!(objects.len(), 2, "the manifest and its data remain");
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
        crate::databases::snapshots::remove_stale_work_dirs(&root, now),
        0
    );
    assert!(root.join("restore-left-behind").exists());
    let tomorrow = now + std::time::Duration::from_secs(25 * 60 * 60);
    assert_eq!(
        crate::databases::snapshots::remove_stale_work_dirs(&root, tomorrow),
        1
    );
    assert!(!root.join("restore-left-behind").exists());
}
