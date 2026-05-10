use super::*;
use tempfile::TempDir;

const TEST_KEY: [u8; 32] = [0xAA; 32];

fn temp_store() -> (TempDir, SqliteStateStore) {
    let temp = TempDir::new().unwrap();
    let store = SqliteStateStore::new(temp.path().join("tako.db"), TEST_KEY);
    (temp, store)
}

fn sample_config() -> AppConfig {
    AppConfig {
        name: "my-app".to_string(),
        environment: "production".to_string(),
        version: "v1".to_string(),
        min_instances: 2,
        max_instances: 4,
        ..Default::default()
    }
}

#[test]
fn init_creates_schema() {
    let (_temp, store) = temp_store();
    store.init().unwrap();

    let conn = store.open_connection().unwrap();
    let user_version: i32 = conn
        .query_row("PRAGMA user_version;", [], |row| row.get(0))
        .unwrap();
    assert_eq!(user_version, STATE_SCHEMA_VERSION);

    let columns: Vec<String> = conn
        .prepare("PRAGMA table_info(apps);")
        .unwrap()
        .query_map([], |row| row.get(1))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(
        columns,
        vec![
            "name".to_string(),
            "environment".to_string(),
            "version".to_string(),
            "min_instances".to_string(),
            "max_instances".to_string(),
        ]
    );
}

#[test]
fn init_rejects_newer_unknown_schema() {
    let (_temp, store) = temp_store();
    let conn = store.open_connection().unwrap();
    conn.execute_batch("PRAGMA user_version = 999;").unwrap();
    drop(conn);

    let err = store.init().unwrap_err();
    match err {
        StateStoreError::UnsupportedSchemaVersion { found } => assert_eq!(found, 999),
        _ => panic!("unexpected error: {err}"),
    }
}

#[test]
fn upsert_and_load_round_trip() {
    let (_temp, store) = temp_store();
    store.init().unwrap();

    let cfg = sample_config();
    let routes = vec![
        "api.example.com".to_string(),
        "example.com/api/*".to_string(),
    ];
    store.upsert_app(&cfg, &routes).unwrap();

    let apps = store.load_apps().unwrap();
    assert_eq!(apps.len(), 1);

    let app = &apps[0];
    assert_eq!(app.config.name, "my-app");
    assert_eq!(app.config.environment, "production");
    assert_eq!(app.config.version, "v1");
    // env_vars and secrets are loaded from files by the caller after restore
    assert!(app.config.env_vars.is_empty());
    assert!(app.config.secrets.is_empty());
    assert_eq!(app.config.min_instances, 2);
    assert_eq!(app.config.max_instances, 4);
    assert_eq!(
        app.routes,
        vec![
            "api.example.com".to_string(),
            "example.com/api/*".to_string()
        ]
    );
}

#[test]
fn delete_app_removes_persisted_app() {
    let (_temp, store) = temp_store();
    store.init().unwrap();

    let cfg = sample_config();
    let routes = vec!["api.example.com".to_string()];
    store.upsert_app(&cfg, &routes).unwrap();

    store.delete_app("my-app", "production").unwrap();

    let apps = store.load_apps().unwrap();
    assert!(apps.is_empty());
}

#[test]
fn server_mode_defaults_to_normal() {
    let (_temp, store) = temp_store();
    store.init().unwrap();
    assert_eq!(store.server_mode().unwrap(), UpgradeMode::Normal);
}

#[test]
fn server_mode_round_trip_persists() {
    let (_temp, store) = temp_store();
    store.init().unwrap();

    store.set_server_mode(UpgradeMode::Upgrading).unwrap();
    assert_eq!(store.server_mode().unwrap(), UpgradeMode::Upgrading);

    // Verify persistence across new connection/process.
    let reopened = SqliteStateStore::new(store.path().to_path_buf(), TEST_KEY);
    reopened.init().unwrap();
    assert_eq!(reopened.server_mode().unwrap(), UpgradeMode::Upgrading);

    reopened.set_server_mode(UpgradeMode::Normal).unwrap();
    assert_eq!(reopened.server_mode().unwrap(), UpgradeMode::Normal);
}

#[test]
fn upgrade_lock_is_single_owner() {
    let (_temp, store) = temp_store();
    store.init().unwrap();

    assert!(store.try_acquire_upgrade_lock("controller-a").unwrap());
    assert!(!store.try_acquire_upgrade_lock("controller-b").unwrap());
    assert!(store.try_acquire_upgrade_lock("controller-a").unwrap());
}

#[test]
fn upgrade_lock_release_requires_owner() {
    let (_temp, store) = temp_store();
    store.init().unwrap();
    assert!(store.try_acquire_upgrade_lock("controller-a").unwrap());

    assert!(!store.release_upgrade_lock("controller-b").unwrap());
    assert!(store.release_upgrade_lock("controller-a").unwrap());
    assert!(store.try_acquire_upgrade_lock("controller-b").unwrap());
}

#[test]
fn upgrade_lock_force_acquires_stale_lock() {
    let (_temp, store) = temp_store();
    store.init().unwrap();
    assert!(store.try_acquire_upgrade_lock("controller-a").unwrap());

    // Backdate the lock to make it stale.
    let conn = store.open_connection().unwrap();
    let stale_time = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
        - SqliteStateStore::UPGRADE_LOCK_STALE_SECS
        - 1;
    conn.execute(
        "UPDATE upgrade_lock SET acquired_at_unix_secs = ?1 WHERE id = 1;",
        rusqlite::params![stale_time],
    )
    .unwrap();

    // A different owner can now force-acquire the stale lock.
    assert!(store.try_acquire_upgrade_lock("controller-b").unwrap());
    assert_eq!(
        store.upgrade_lock_owner().unwrap().as_deref(),
        Some("controller-b")
    );
}

#[test]
fn upgrade_lock_owner_cleared_allows_new_owner() {
    let (_temp, store) = temp_store();
    store.init().unwrap();

    // Simulate: owner-a acquires lock then crashes (no release).
    assert!(store.try_acquire_upgrade_lock("owner-a").unwrap());
    assert_eq!(
        store.upgrade_lock_owner().unwrap().as_deref(),
        Some("owner-a")
    );

    // Manual cleanup (as server startup would do): read owner, release.
    if let Some(owner) = store.upgrade_lock_owner().unwrap() {
        assert!(store.release_upgrade_lock(&owner).unwrap());
    }

    // New owner can acquire immediately without waiting for stale timeout.
    assert!(store.try_acquire_upgrade_lock("owner-b").unwrap());
    assert_eq!(
        store.upgrade_lock_owner().unwrap().as_deref(),
        Some("owner-b")
    );
}

#[test]
fn set_and_get_secrets_round_trip() {
    let (_temp, store) = temp_store();
    store.init().unwrap();

    let secrets = HashMap::from([
        ("API_KEY".to_string(), "secret123".to_string()),
        ("DB_URL".to_string(), "postgres://db".to_string()),
    ]);
    store.set_secrets("my-app", &secrets).unwrap();

    let loaded = store.get_secrets("my-app").unwrap();
    assert_eq!(loaded.get("API_KEY"), Some(&"secret123".to_string()));
    assert_eq!(loaded.get("DB_URL"), Some(&"postgres://db".to_string()));
}

#[test]
fn get_or_create_image_secret_generates_and_reuses_app_secret() {
    let (_temp, store) = temp_store();
    store.init().unwrap();

    let first = store.get_or_create_image_secret("my-app").unwrap();
    let second = store.get_or_create_image_secret("my-app").unwrap();

    assert_eq!(first, second);
    assert!(first.len() >= 32);
}

#[test]
fn get_or_create_image_secret_is_scoped_per_app() {
    let (_temp, store) = temp_store();
    store.init().unwrap();

    let first = store.get_or_create_image_secret("my-app").unwrap();
    let second = store.get_or_create_image_secret("other-app").unwrap();

    assert_ne!(first, second);
}

#[test]
fn get_secrets_returns_empty_when_not_set() {
    let (_temp, store) = temp_store();
    store.init().unwrap();

    let loaded = store.get_secrets("nonexistent").unwrap();
    assert!(loaded.is_empty());
}

#[test]
fn set_secrets_overwrites_previous() {
    let (_temp, store) = temp_store();
    store.init().unwrap();

    let secrets1 = HashMap::from([("KEY".to_string(), "value1".to_string())]);
    store.set_secrets("my-app", &secrets1).unwrap();

    let secrets2 = HashMap::from([("KEY".to_string(), "value2".to_string())]);
    store.set_secrets("my-app", &secrets2).unwrap();

    let loaded = store.get_secrets("my-app").unwrap();
    assert_eq!(loaded.get("KEY"), Some(&"value2".to_string()));
}

#[test]
fn delete_secrets_removes_app_secrets() {
    let (_temp, store) = temp_store();
    store.init().unwrap();

    let secrets = HashMap::from([("KEY".to_string(), "value".to_string())]);
    store.set_secrets("my-app", &secrets).unwrap();

    store.delete_secrets("my-app").unwrap();

    let loaded = store.get_secrets("my-app").unwrap();
    assert!(loaded.is_empty());
}

#[test]
fn secrets_are_encrypted_at_rest() {
    let (_temp, store) = temp_store();
    store.init().unwrap();

    let secrets = HashMap::from([("API_KEY".to_string(), "supersecret".to_string())]);
    store.set_secrets("my-app", &secrets).unwrap();

    // Read raw blob from SQLite — should not contain plaintext
    let conn = store.open_connection().unwrap();
    let raw: Vec<u8> = conn
        .query_row(
            "SELECT encrypted_data FROM app_secrets WHERE app = ?1;",
            ["my-app"],
            |row| row.get(0),
        )
        .unwrap();
    let raw_str = String::from_utf8_lossy(&raw);
    assert!(!raw_str.contains("supersecret"));
    assert!(!raw_str.contains("API_KEY"));
}

#[test]
fn secrets_encrypted_with_wrong_key_cannot_be_read() {
    let temp = TempDir::new().unwrap();
    let db_path = temp.path().join("state.sqlite3");

    let store1 = SqliteStateStore::new(db_path.clone(), [0x11; 32]);
    store1.init().unwrap();
    let secrets = HashMap::from([("KEY".to_string(), "value".to_string())]);
    store1.set_secrets("my-app", &secrets).unwrap();

    let store2 = SqliteStateStore::new(db_path, [0x22; 32]);
    store2.init().unwrap();
    assert!(store2.get_secrets("my-app").is_err());
}

#[test]
fn migrate_v1_to_current_adds_secret_tables() {
    let temp = TempDir::new().unwrap();
    let db_path = temp.path().join("state.sqlite3");

    // Create a v1 database manually
    {
        let store = SqliteStateStore::new(db_path.clone(), TEST_KEY);
        let conn = store.open_connection().unwrap();
        conn.execute_batch(
                "CREATE TABLE apps (
                    name TEXT NOT NULL,
                    environment TEXT NOT NULL,
                    version TEXT NOT NULL,
                    min_instances INTEGER NOT NULL,
                    max_instances INTEGER NOT NULL,
                    PRIMARY KEY (name, environment)
                );
                CREATE TABLE app_routes (
                    name TEXT NOT NULL,
                    environment TEXT NOT NULL,
                    route TEXT NOT NULL,
                    PRIMARY KEY (name, environment, route),
                    FOREIGN KEY(name, environment) REFERENCES apps(name, environment) ON DELETE CASCADE
                );
                CREATE TABLE server_state (
                    id INTEGER PRIMARY KEY CHECK(id = 1),
                    server_mode TEXT NOT NULL
                );
                CREATE TABLE upgrade_lock (
                    id INTEGER PRIMARY KEY CHECK(id = 1),
                    owner TEXT NOT NULL,
                    acquired_at_unix_secs INTEGER NOT NULL
                );
                INSERT INTO server_state (id, server_mode) VALUES (1, 'normal');
                PRAGMA user_version = 1;",
            )
            .unwrap();
    }

    // Open with current code — should migrate to the latest schema.
    let store = SqliteStateStore::new(db_path, TEST_KEY);
    store.init().unwrap();

    // Verify migration: app_secrets table exists and works.
    let secrets = HashMap::from([("KEY".to_string(), "value".to_string())]);
    store.set_secrets("test-app", &secrets).unwrap();
    let loaded = store.get_secrets("test-app").unwrap();
    assert_eq!(loaded.get("KEY"), Some(&"value".to_string()));

    let image_secret = store.get_or_create_image_secret("test-app").unwrap();
    assert!(!image_secret.is_empty());

    // Verify version bumped
    let conn = store.open_connection().unwrap();
    let version: i32 = conn
        .query_row("PRAGMA user_version;", [], |row| row.get(0))
        .unwrap();
    assert_eq!(version, STATE_SCHEMA_VERSION);
}

#[test]
fn load_or_create_device_key_generates_and_persists() {
    let temp = TempDir::new().unwrap();
    let key_path = temp.path().join("secret.key");

    let key1 = load_or_create_device_key(&key_path).unwrap();
    let key2 = load_or_create_device_key(&key_path).unwrap();
    assert_eq!(key1, key2);

    let raw = std::fs::read(&key_path).unwrap();
    assert_eq!(raw.len(), 32);
}

#[test]
#[cfg(unix)]
fn load_or_create_device_key_sets_mode_0600() {
    use std::os::unix::fs::PermissionsExt;
    let temp = TempDir::new().unwrap();
    let key_path = temp.path().join("secret.key");

    load_or_create_device_key(&key_path).unwrap();

    let mode = std::fs::metadata(&key_path).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
}
