use super::*;
use tempfile::TempDir;

// ==================== Parsing Tests ====================

#[test]
fn test_parse_empty() {
    let store = SecretsStore::parse("").unwrap();
    assert!(store.is_empty());
}

#[test]
fn test_parse_empty_object() {
    let store = SecretsStore::parse("{}").unwrap();
    assert!(store.is_empty());
}

#[test]
fn test_parse_new_format() {
    let json = r#"{
            "production": {
                "key_id": "0123456789abcdef",
                "app": {
                    "DATABASE_URL": {"value": "encrypted_value_1"},
                    "API_KEY": {"value": "encrypted_value_2"}
                }
            }
        }"#;

    let store = SecretsStore::parse(json).unwrap();
    assert_eq!(store.environment_names(), vec!["production"]);
    assert_eq!(
        store.get("production", "DATABASE_URL"),
        Some(&"encrypted_value_1".to_string())
    );
    assert_eq!(
        store.get("production", "API_KEY"),
        Some(&"encrypted_value_2".to_string())
    );
    assert_eq!(store.get_key_id("production"), Some("0123456789abcdef"));
}

#[test]
fn parse_reads_app_secrets_and_storage_credentials() {
    let json = r#"{
            "production": {
                "key_id": "0123456789abcdef",
                "app": {
                    "DATABASE_URL": {"value": "encrypted-db"}
                },
                "storages": {
                    "prod_uploads": {
                        "access_key_id": {"value": "encrypted-key-id"},
                        "secret_access_key": {"value": "encrypted-secret"}
                    }
                }
            }
        }"#;

    let store = SecretsStore::parse(json).unwrap();
    assert_eq!(
        store.get("production", "DATABASE_URL"),
        Some(&"encrypted-db".to_string())
    );
    let storage = store
        .get_storage_credentials("production", "prod_uploads")
        .unwrap();
    assert_eq!(storage.access_key_id.value, "encrypted-key-id");
    assert_eq!(storage.secret_access_key.value, "encrypted-secret");
}

#[test]
fn parse_reads_backup_keys_without_app_secret_name() {
    let json = r#"{
            "production": {
                "key_id": "0123456789abcdef",
                "backup_keys": [
                    {
                        "id": "backup-key-0123456789abcdef",
                        "value": "encrypted-backup-key"
                    }
                ],
                "app": {
                    "DATABASE_URL": {"value": "encrypted-db"}
                }
            }
        }"#;

    let store = SecretsStore::parse(json).unwrap();
    let backup_keys = store.get_backup_keys("production").unwrap();
    assert_eq!(backup_keys.len(), 1);
    assert_eq!(backup_keys[0].id, "backup-key-0123456789abcdef");
    assert_eq!(backup_keys[0].value, "encrypted-backup-key");
    assert_eq!(store.all_secret_names(), vec!["DATABASE_URL".to_string()]);
}

#[test]
fn storage_credentials_do_not_appear_as_app_secret_names() {
    let json = r#"{
            "production": {
                "key_id": "0123456789abcdef",
                "app": {
                    "DATABASE_URL": {"value": "encrypted-db"}
                },
                "storages": {
                    "prod_uploads": {
                        "access_key_id": {"value": "encrypted-key-id"},
                        "secret_access_key": {"value": "encrypted-secret"}
                    }
                }
            }
        }"#;

    let store = SecretsStore::parse(json).unwrap();
    assert_eq!(store.all_secret_names(), vec!["DATABASE_URL".to_string()]);
    assert!(!store.contains("production", "prod_uploads"));
}

#[test]
fn parse_reads_credentials_without_app_secret_name() {
    let json = r#"{
            "production": {
                "key_id": "0123456789abcdef",
                "app": {
                    "DATABASE_URL": {"value": "encrypted-db"}
                },
                "credentials": {
                    "ssl.cloudflare": {"value": "encrypted-token"}
                }
            }
        }"#;

    let store = SecretsStore::parse(json).unwrap();
    let credential = store
        .get_credential("production", "ssl.cloudflare")
        .unwrap();
    assert_eq!(credential.value, "encrypted-token");
    assert_eq!(store.all_secret_names(), vec!["DATABASE_URL".to_string()]);
    assert_eq!(
        store.all_credential_names(),
        vec!["ssl.cloudflare".to_string()]
    );
}

#[test]
fn parse_reads_secret_values_with_expires_on_metadata() {
    let json = r#"{
            "production": {
                "key_id": "0123456789abcdef",
                "app": {
                    "DATABASE_URL": {
                        "value": "encrypted-db",
                        "expires_on": "2099-01-01"
                    }
                },
                "storages": {
                    "prod_uploads": {
                        "access_key_id": {
                            "value": "encrypted-key-id",
                            "expires_on": "2099-01-01"
                        },
                        "secret_access_key": {
                            "value": "encrypted-secret",
                            "expires_on": "2099-01-01"
                        }
                    }
                },
                "credentials": {
                    "ssl.cloudflare": {
                        "value": "encrypted-token",
                        "expires_on": "2099-01-01"
                    }
                }
            }
        }"#;

    let store = SecretsStore::parse(json).unwrap();

    let secret = store.get_secret("production", "DATABASE_URL").unwrap();
    assert_eq!(secret.value, "encrypted-db");
    assert_eq!(secret.expires_on.as_deref(), Some("2099-01-01"));
    let storage = store
        .get_storage_credentials("production", "prod_uploads")
        .unwrap();
    assert_eq!(storage.access_key_id.value, "encrypted-key-id");
    assert_eq!(
        storage.access_key_id.expires_on.as_deref(),
        Some("2099-01-01")
    );
    let credential = store
        .get_credential("production", "ssl.cloudflare")
        .unwrap();
    assert_eq!(credential.value, "encrypted-token");
    assert_eq!(credential.expires_on.as_deref(), Some("2099-01-01"));
}

#[test]
fn test_parse_multiple_environments() {
    let json = r#"{
            "production": {
                "key_id": "1111111111111111",
                "app": {
                    "DATABASE_URL": {"value": "prod_db"}
                }
            },
            "staging": {
                "key_id": "2222222222222222",
                "app": {
                    "DATABASE_URL": {"value": "staging_db"},
                    "DEBUG": {"value": "true"}
                }
            }
        }"#;

    let store = SecretsStore::parse(json).unwrap();

    let mut envs = store.environment_names();
    envs.sort();
    assert_eq!(envs, vec!["production", "staging"]);

    assert_eq!(
        store.get("production", "DATABASE_URL"),
        Some(&"prod_db".to_string())
    );
    assert_eq!(
        store.get("staging", "DATABASE_URL"),
        Some(&"staging_db".to_string())
    );
    assert_eq!(store.get("staging", "DEBUG"), Some(&"true".to_string()));
}

// ==================== Validation Tests ====================

#[test]
fn test_validate_secret_name_valid() {
    assert!(validate_secret_name("DATABASE_URL").is_ok());
    assert!(validate_secret_name("API_KEY").is_ok());
    assert!(validate_secret_name("SECRET123").is_ok());
    assert!(validate_secret_name("A").is_ok());
    assert!(validate_secret_name("MY_SECRET_KEY_123").is_ok());
}

#[test]
fn test_validate_secret_name_empty() {
    assert!(validate_secret_name("").is_err());
}

#[test]
fn test_validate_secret_name_must_start_uppercase() {
    assert!(validate_secret_name("database_url").is_err());
    assert!(validate_secret_name("1SECRET").is_err());
    assert!(validate_secret_name("_SECRET").is_err());
}

#[test]
fn test_validate_secret_name_invalid_chars() {
    assert!(validate_secret_name("DATABASE-URL").is_err());
    assert!(validate_secret_name("DATABASE.URL").is_err());
    assert!(validate_secret_name("database_url").is_err());
}

#[test]
fn validate_credential_name_accepts_postgres_url() {
    assert!(validate_credential_name("postgres_url").is_ok());
}

#[test]
fn test_validate_environment_name_valid() {
    assert!(validate_environment_name("production").is_ok());
    assert!(validate_environment_name("staging").is_ok());
    assert!(validate_environment_name("prod-1").is_ok());
}

#[test]
fn test_validate_environment_name_invalid() {
    assert!(validate_environment_name("").is_err());
    assert!(validate_environment_name("Production").is_err());
    assert!(validate_environment_name("prod_1").is_err());
}

#[test]
fn normalize_secret_expires_on_accepts_date_and_never() {
    assert_eq!(
        normalize_secret_expires_on("2099-01-01")
            .unwrap()
            .as_deref(),
        Some("2099-01-01")
    );
    assert_eq!(normalize_secret_expires_on("never").unwrap(), None);
    assert_eq!(normalize_secret_expires_on("").unwrap(), None);
}

#[test]
fn normalize_secret_expires_on_accepts_relative_days() {
    let now = OffsetDateTime::parse(
        "2026-05-17T13:45:12Z",
        &time::format_description::well_known::Rfc3339,
    )
    .unwrap();

    assert_eq!(
        normalize_secret_expires_on_at("in 7 days", now)
            .unwrap()
            .as_deref(),
        Some("2026-05-24")
    );
    assert_eq!(
        normalize_secret_expires_on_at("IN 1 DAY", now)
            .unwrap()
            .as_deref(),
        Some("2026-05-18")
    );
}

#[test]
fn normalize_secret_expires_on_rejects_invalid_values() {
    assert!(normalize_secret_expires_on("tomorrow").is_err());
    assert!(normalize_secret_expires_on("2099-99-99").is_err());
    assert!(normalize_secret_expires_on("2099-1-01").is_err());
    assert!(normalize_secret_expires_on("2099-01-01T00:00:00Z").is_err());
    assert!(normalize_secret_expires_on("in -1 days").is_err());
    assert!(normalize_secret_expires_on("in 0 days").is_err());
    assert!(normalize_secret_expires_on("in days").is_err());
}

// ==================== CRUD Operation Tests ====================

#[test]
fn test_set_secret() {
    let mut store = SecretsStore::default();

    store.ensure_env_key_id("production").unwrap();
    store
        .set("production", "API_KEY", "secret123".to_string())
        .unwrap();

    assert_eq!(
        store.get("production", "API_KEY"),
        Some(&"secret123".to_string())
    );
}

#[test]
fn test_set_secret_requires_initialized_env() {
    let mut store = SecretsStore::default();

    let result = store.set("production", "API_KEY", "secret123".to_string());
    assert!(result.is_err());
}

#[test]
fn test_ensure_env_key_id_creates_environment() {
    let mut store = SecretsStore::default();

    let key_id1 = store.ensure_env_key_id("production").unwrap();
    let key_id2 = store.ensure_env_key_id("staging").unwrap();

    assert_eq!(store.environment_names().len(), 2);
    // Different environments get different key_ids
    assert_ne!(key_id1, key_id2);
}

#[test]
fn test_ensure_env_key_id_is_idempotent() {
    let mut store = SecretsStore::default();

    let key_id1 = store.ensure_env_key_id("production").unwrap();
    let key_id2 = store.ensure_env_key_id("production").unwrap();

    // Same key_id returned on repeated calls
    assert_eq!(key_id1, key_id2);
}

#[test]
fn push_backup_key_appends_and_active_key_is_last() {
    let mut store = SecretsStore::default();
    store.ensure_env_key_id("production").unwrap();

    store
        .push_backup_key(
            "production",
            EncryptedBackupKey::new(
                "backup-key-1111111111111111".to_string(),
                "encrypted-one".to_string(),
            ),
        )
        .unwrap();
    store
        .push_backup_key(
            "production",
            EncryptedBackupKey::new(
                "backup-key-2222222222222222".to_string(),
                "encrypted-two".to_string(),
            ),
        )
        .unwrap();

    let keys = store.get_backup_keys("production").unwrap();
    assert_eq!(keys.len(), 2);
    assert_eq!(
        store
            .active_backup_key("production")
            .map(|key| key.id.as_str()),
        Some("backup-key-2222222222222222")
    );
}

#[test]
fn push_backup_key_rejects_invalid_id() {
    let mut store = SecretsStore::default();
    store.ensure_env_key_id("production").unwrap();

    let err = store
        .push_backup_key(
            "production",
            EncryptedBackupKey::new("not-a-key".to_string(), "encrypted".to_string()),
        )
        .unwrap_err();

    assert!(err.to_string().contains("Backup key id"), "{err}");
}

#[test]
fn test_set_overwrites_existing() {
    let mut store = SecretsStore::default();

    store.ensure_env_key_id("production").unwrap();
    store
        .set("production", "API_KEY", "old_value".to_string())
        .unwrap();
    store
        .set("production", "API_KEY", "new_value".to_string())
        .unwrap();

    assert_eq!(
        store.get("production", "API_KEY"),
        Some(&"new_value".to_string())
    );
}

#[test]
fn test_remove_secret() {
    let mut store = SecretsStore::default();

    store.ensure_env_key_id("production").unwrap();
    store
        .set("production", "API_KEY", "secret".to_string())
        .unwrap();
    store
        .set("production", "DATABASE_URL", "db".to_string())
        .unwrap();

    store.remove("production", "API_KEY").unwrap();

    assert!(!store.contains("production", "API_KEY"));
    assert!(store.contains("production", "DATABASE_URL"));
}

#[test]
fn test_remove_last_secret_removes_environment() {
    let mut store = SecretsStore::default();

    store.ensure_env_key_id("production").unwrap();
    store
        .set("production", "API_KEY", "secret".to_string())
        .unwrap();
    store.remove("production", "API_KEY").unwrap();

    assert!(!store.environments.contains_key("production"));
}

#[test]
fn remove_last_credential_removes_environment() {
    let mut store = SecretsStore::default();

    store.ensure_env_key_id("production").unwrap();
    store
        .set_credential(
            "production",
            "ssl.cloudflare",
            EncryptedSecretValue::new("encrypted-token".to_string(), None),
        )
        .unwrap();

    store
        .remove_credential("production", "ssl.cloudflare")
        .unwrap();

    assert!(!store.environments.contains_key("production"));
}

#[test]
fn test_remove_nonexistent_fails() {
    let mut store = SecretsStore::default();
    store.ensure_env_key_id("production").unwrap();
    store
        .set("production", "API_KEY", "secret".to_string())
        .unwrap();

    let result = store.remove("production", "NONEXISTENT");
    assert!(matches!(result, Err(ConfigError::SecretNotFound(_))));
}

#[test]
fn test_remove_from_nonexistent_env_fails() {
    let mut store = SecretsStore::default();

    let result = store.remove("production", "API_KEY");
    assert!(matches!(result, Err(ConfigError::EnvironmentNotFound(_))));
}

#[test]
fn test_remove_all() {
    let mut store = SecretsStore::default();

    store.ensure_env_key_id("production").unwrap();
    store
        .set("production", "API_KEY", "prod".to_string())
        .unwrap();
    store.ensure_env_key_id("staging").unwrap();
    store
        .set("staging", "API_KEY", "staging".to_string())
        .unwrap();
    store
        .set("staging", "DATABASE_URL", "db".to_string())
        .unwrap();

    let removed_from = store.remove_all("API_KEY").unwrap();

    assert_eq!(removed_from.len(), 2);
    assert!(!store.contains("production", "API_KEY"));
    assert!(!store.contains("staging", "API_KEY"));
    assert!(store.contains("staging", "DATABASE_URL"));

    // production environment should be removed (was only API_KEY)
    assert!(!store.environments.contains_key("production"));
}

// ==================== Discrepancy Tests ====================

#[test]
fn test_find_discrepancies_none() {
    let mut store = SecretsStore::default();

    store.ensure_env_key_id("production").unwrap();
    store
        .set("production", "API_KEY", "prod".to_string())
        .unwrap();
    store
        .set("production", "DATABASE_URL", "prod_db".to_string())
        .unwrap();
    store.ensure_env_key_id("staging").unwrap();
    store
        .set("staging", "API_KEY", "staging".to_string())
        .unwrap();
    store
        .set("staging", "DATABASE_URL", "staging_db".to_string())
        .unwrap();

    assert!(store.is_consistent());
    assert!(store.find_discrepancies().is_empty());
}

#[test]
fn test_find_discrepancies_some() {
    let mut store = SecretsStore::default();

    store.ensure_env_key_id("production").unwrap();
    store
        .set("production", "API_KEY", "prod".to_string())
        .unwrap();
    store
        .set("production", "DATABASE_URL", "prod_db".to_string())
        .unwrap();
    store.ensure_env_key_id("staging").unwrap();
    store
        .set("staging", "API_KEY", "staging".to_string())
        .unwrap();
    // DATABASE_URL missing in staging

    let discrepancies = store.find_discrepancies();
    assert_eq!(discrepancies.len(), 1);
    assert_eq!(discrepancies[0].name, "DATABASE_URL");
    assert_eq!(discrepancies[0].missing_in, vec!["staging"]);
}

#[test]
fn test_all_secret_names() {
    let mut store = SecretsStore::default();

    store.ensure_env_key_id("production").unwrap();
    store.set("production", "API_KEY", "1".to_string()).unwrap();
    store
        .set("production", "DATABASE_URL", "2".to_string())
        .unwrap();
    store.ensure_env_key_id("staging").unwrap();
    store.set("staging", "API_KEY", "3".to_string()).unwrap();
    store.set("staging", "REDIS_URL", "4".to_string()).unwrap();

    let names = store.all_secret_names();
    assert_eq!(names, vec!["API_KEY", "DATABASE_URL", "REDIS_URL"]);
}

// ==================== File I/O Tests ====================

#[test]
fn test_save_and_load() {
    let temp_dir = TempDir::new().unwrap();

    let mut store = SecretsStore::default();
    store.ensure_env_key_id("production").unwrap();
    store
        .set("production", "API_KEY", "secret123".to_string())
        .unwrap();
    store.ensure_env_key_id("staging").unwrap();
    store
        .set("staging", "API_KEY", "secret456".to_string())
        .unwrap();

    store.save_to_dir(&temp_dir).unwrap();

    let loaded = SecretsStore::load_from_dir(&temp_dir).unwrap();

    assert_eq!(
        loaded.get("production", "API_KEY"),
        Some(&"secret123".to_string())
    );
    assert_eq!(
        loaded.get("staging", "API_KEY"),
        Some(&"secret456".to_string())
    );
    // Key ids are preserved
    assert_eq!(
        loaded.get_key_id("production"),
        store.get_key_id("production")
    );
}

#[test]
fn test_default_path_uses_secrets_json() {
    let temp_dir = TempDir::new().unwrap();
    assert_eq!(
        SecretsStore::default_path(temp_dir.path()),
        temp_dir.path().join(".tako").join("secrets.json")
    );
}

#[test]
fn test_load_nonexistent_returns_default() {
    let temp_dir = TempDir::new().unwrap();
    let store = SecretsStore::load_from_dir(&temp_dir).unwrap();
    assert!(store.is_empty());
}

#[test]
fn test_save_to_dir_writes_new_secrets_json_path() {
    let temp_dir = TempDir::new().unwrap();
    let mut store = SecretsStore::default();
    store.ensure_env_key_id("production").unwrap();
    store
        .set("production", "API_KEY", "secret123".to_string())
        .unwrap();

    store.save_to_dir(temp_dir.path()).unwrap();

    assert!(temp_dir.path().join(".tako").join("secrets.json").exists());
    assert!(!temp_dir.path().join(".tako").join("secrets").exists());
}

#[test]
fn save_omits_expires_on_when_unknown() {
    let temp_dir = TempDir::new().unwrap();
    let path = temp_dir.path().join(".tako").join("secrets.json");
    let mut store = SecretsStore::default();
    store.ensure_env_key_id("production").unwrap();
    store
        .set("production", "API_KEY", "secret123".to_string())
        .unwrap();

    store.save_to_file(&path).unwrap();

    let parsed: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap();
    assert!(
        parsed["production"]["app"]["API_KEY"]
            .get("expires_on")
            .is_none(),
        "{parsed:#}"
    );
}

#[test]
fn save_preserves_backup_keys() {
    let temp_dir = TempDir::new().unwrap();
    let path = temp_dir.path().join(".tako").join("secrets.json");
    let mut store = SecretsStore::default();
    store.ensure_env_key_id("production").unwrap();
    store
        .push_backup_key(
            "production",
            EncryptedBackupKey::new(
                "backup-key-0123456789abcdef".to_string(),
                "encrypted-backup-key".to_string(),
            ),
        )
        .unwrap();

    store.save_to_file(&path).unwrap();
    let loaded = SecretsStore::load_from_file(&path).unwrap();

    let backup_keys = loaded.get_backup_keys("production").unwrap();
    assert_eq!(backup_keys.len(), 1);
    assert_eq!(backup_keys[0].id, "backup-key-0123456789abcdef");
    assert_eq!(backup_keys[0].value, "encrypted-backup-key");
}

#[test]
fn test_save_to_file_orders_environments_and_secret_names_stably() {
    let temp_dir = TempDir::new().unwrap();
    let path = temp_dir.path().join(".tako").join("secrets.json");
    let mut store = SecretsStore::default();
    store.ensure_env_key_id("staging").unwrap();
    store.set("staging", "Z_KEY", "z".to_string()).unwrap();
    store.ensure_env_key_id("production").unwrap();
    store.set("production", "B_KEY", "b".to_string()).unwrap();
    store.set("production", "A_KEY", "a".to_string()).unwrap();

    store.save_to_file(&path).unwrap();

    let raw = fs::read_to_string(path).unwrap();
    let production_pos = raw.find("\"production\"").unwrap();
    let staging_pos = raw.find("\"staging\"").unwrap();
    let a_key_pos = raw.find("\"A_KEY\"").unwrap();
    let b_key_pos = raw.find("\"B_KEY\"").unwrap();

    assert!(
        production_pos < staging_pos,
        "expected sorted environments: {raw}"
    );
    assert!(a_key_pos < b_key_pos, "expected sorted secret names: {raw}");
}

#[test]
fn test_creates_parent_directory() {
    let temp_dir = TempDir::new().unwrap();
    let path = temp_dir
        .path()
        .join("subdir")
        .join(".tako")
        .join("secrets.json");

    let mut store = SecretsStore::default();
    store.ensure_env_key_id("production").unwrap();
    store
        .set("production", "API_KEY", "secret".to_string())
        .unwrap();
    store.save_to_file(&path).unwrap();

    assert!(path.exists());
}

// ==================== Utility Tests ====================

#[test]
fn test_count_by_env() {
    let mut store = SecretsStore::default();

    store.ensure_env_key_id("production").unwrap();
    store.set("production", "API_KEY", "1".to_string()).unwrap();
    store
        .set("production", "DATABASE_URL", "2".to_string())
        .unwrap();
    store.ensure_env_key_id("staging").unwrap();
    store.set("staging", "API_KEY", "3".to_string()).unwrap();

    let counts = store.count_by_env();
    assert_eq!(counts.get("production"), Some(&2));
    assert_eq!(counts.get("staging"), Some(&1));
}

#[test]
fn test_total_count() {
    let mut store = SecretsStore::default();

    store.ensure_env_key_id("production").unwrap();
    store.set("production", "API_KEY", "1".to_string()).unwrap();
    store
        .set("production", "DATABASE_URL", "2".to_string())
        .unwrap();
    store.ensure_env_key_id("staging").unwrap();
    store.set("staging", "API_KEY", "3".to_string()).unwrap();

    assert_eq!(store.total_count(), 3);
}
