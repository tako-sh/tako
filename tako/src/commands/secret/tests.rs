#[cfg(target_os = "macos")]
use super::key::keychain_storage_hint;
use super::key::{
    KeyImportSource, KeyStorageChoice, decode_key_bundle, encode_key_bundle,
    environment_for_key_id, load_or_create_key_for_set, missing_secret_key_message,
    resolve_key_import_source, save_key_with_storage_choice, validate_passphrase_key_for_env,
};
use super::sync::{
    build_update_secrets_command, resolve_secret_sync_server_names, tako_response_has_error,
};
use super::*;
use crate::config::{ServerEntry, ServersToml, TakoToml};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD as BASE64_URL};
use std::collections::HashMap;
use std::ffi::OsString;
use std::path::Path;
use tempfile::TempDir;

fn with_temp_tako_home<T>(f: impl FnOnce() -> T) -> T {
    let _lock = crate::paths::test_tako_home_env_lock();
    let temp = TempDir::new().unwrap();
    let previous = std::env::var_os("TAKO_HOME");
    unsafe {
        std::env::set_var("TAKO_HOME", temp.path());
    }

    struct ResetEnv(Option<OsString>);
    impl Drop for ResetEnv {
        fn drop(&mut self) {
            match self.0.take() {
                Some(value) => unsafe { std::env::set_var("TAKO_HOME", value) },
                None => unsafe { std::env::remove_var("TAKO_HOME") },
            }
        }
    }
    let _reset = ResetEnv(previous);
    f()
}

#[test]
fn ensure_secret_key_available_is_noop_when_env_has_no_secrets() {
    with_temp_tako_home(|| {
        let mut secrets = crate::config::SecretsStore::parse("{}").unwrap();
        secrets.ensure_env_key_id("production").unwrap();
        // Env has a key_id but no secrets: nothing to decrypt later, no prompt needed.
        ensure_secret_key_available("production", &secrets, None)
            .expect("no-op when env has no secrets");
    });
}

#[test]
fn ensure_secret_key_available_is_noop_when_env_has_no_key_id() {
    with_temp_tako_home(|| {
        let secrets = crate::config::SecretsStore::parse("{}").unwrap();
        // Env doesn't exist at all; skip without error.
        ensure_secret_key_available("production", &secrets, None)
            .expect("no-op when env is not initialized");
    });
}

#[test]
fn ensure_secret_key_available_is_noop_when_key_already_cached() {
    with_temp_tako_home(|| {
        let json = r#"{
            "production": {
                "key_id": "0123456789abcdef",
                "app": {"DATABASE_URL": "opaque-encrypted-blob"}
            }
        }"#;
        let secrets = crate::config::SecretsStore::parse(json).unwrap();
        let key_id_b64 = secrets.get_key_id("production").unwrap();
        let key = crate::crypto::EncryptionKey::generate().unwrap();
        let key_store = crate::crypto::KeyStore::for_key_id(key_id_b64).unwrap();
        key_store.save_key(&key).unwrap();

        // With the key already on disk, this must not prompt — if it did, the
        // test would either hang or fail because stdin isn't a tty.
        ensure_secret_key_available("production", &secrets, None)
            .expect("cached key should be used without prompting");
    });
}

#[test]
fn ensure_secret_key_available_errors_when_existing_secrets_have_no_key() {
    with_temp_tako_home(|| {
        let json = r#"{
            "production": {
                "key_id": "0123456789abcdef",
                "app": {"DATABASE_URL": "opaque-encrypted-blob"}
            }
        }"#;
        let secrets = crate::config::SecretsStore::parse(json).unwrap();
        let err = ensure_secret_key_available("production", &secrets, None).unwrap_err();
        assert_eq!(
            err.to_string(),
            "Unable to decrypt production secrets. Run `tako secrets key import` to import an exported key or passphrase."
        );
    });
}

#[test]
fn ensure_secret_key_available_errors_when_dns_credentials_have_no_key() {
    with_temp_tako_home(|| {
        let json = r#"{
            "production": {
                "key_id": "0123456789abcdef",
                "dns": {"cloudflare_api_token": "opaque-encrypted-blob"}
            }
        }"#;
        let secrets = crate::config::SecretsStore::parse(json).unwrap();
        let err = ensure_secret_key_available("production", &secrets, None).unwrap_err();
        assert_eq!(
            err.to_string(),
            "Unable to decrypt production secrets. Run `tako secrets key import` to import an exported key or passphrase."
        );
    });
}

#[test]
fn missing_secret_key_message_names_environment_and_import_command() {
    assert_eq!(
        missing_secret_key_message("production"),
        "Unable to decrypt production secrets. Run `tako secrets key import` to import an exported key or passphrase."
    );
}

#[test]
fn validate_passphrase_key_for_env_uses_short_invalid_passphrase_error() {
    let correct_key = crate::crypto::EncryptionKey::generate().unwrap();
    let encrypted = crate::crypto::encrypt("postgres://localhost/db", &correct_key).unwrap();
    let secrets = crate::config::SecretsStore::parse(&format!(
        r#"{{
            "production": {{
                "key_id": "0123456789abcdef",
                "app": {{"DATABASE_URL": "{encrypted}"}}
            }}
        }}"#
    ))
    .unwrap();
    let wrong_key = crate::crypto::EncryptionKey::generate().unwrap();

    let err = validate_passphrase_key_for_env(&secrets, "production", &wrong_key).unwrap_err();

    assert_eq!(err.to_string(), "Invalid passphrase.");
}

#[test]
fn load_or_create_key_for_set_creates_random_key_for_empty_env() {
    with_temp_tako_home(|| {
        let mut secrets = crate::config::SecretsStore::parse("{}").unwrap();
        let key_id = secrets.ensure_env_key_id("development").unwrap();

        let key = load_or_create_key_for_set("development", &secrets, Some(Path::new("/tmp/tako")))
            .expect("empty env should get a new local key");
        let key_store = crate::crypto::KeyStore::for_key_id(&key_id).unwrap();
        assert!(key_store.key_exists());
        assert_eq!(key_store.load_key().unwrap().as_bytes(), key.as_bytes());
    });
}

#[cfg(target_os = "macos")]
#[test]
fn i_cloud_keychain_choice_errors_when_signed_app_is_unavailable() {
    with_temp_tako_home(|| {
        let key_store = crate::crypto::KeyStore::for_key_id("0123456789abcdef").unwrap();
        let key = crate::crypto::EncryptionKey::generate().unwrap();

        let err = save_key_with_storage_choice(
            &key_store,
            &key,
            KeyStorageChoice::ICloudKeychain,
            Some(Path::new("/tmp/tako")),
        )
        .unwrap_err();

        assert_eq!(err.to_string(), crate::keychain::unavailable_message());
        assert!(!key_store.key_path().exists());
    });
}

#[test]
fn local_file_choice_saves_key_to_disk() {
    with_temp_tako_home(|| {
        let key_store = crate::crypto::KeyStore::for_key_id("0123456789abcdef").unwrap();
        let key = crate::crypto::EncryptionKey::generate().unwrap();

        save_key_with_storage_choice(&key_store, &key, KeyStorageChoice::LocalFile, None).unwrap();

        assert_eq!(key_store.load_key().unwrap().as_bytes(), key.as_bytes());
    });
}

#[cfg(target_os = "macos")]
#[test]
fn keychain_storage_hint_names_known_environment() {
    assert_eq!(
        keychain_storage_hint(Some("development")),
        "Syncs development key to your other Macs."
    );
}

#[cfg(target_os = "macos")]
#[test]
fn keychain_storage_hint_handles_unknown_environment() {
    assert_eq!(
        keychain_storage_hint(None),
        "Syncs this key to your other Macs."
    );
}

#[test]
fn resolve_key_import_source_uses_passphrase_flag() {
    assert_eq!(
        resolve_key_import_source(true).unwrap(),
        KeyImportSource::Passphrase
    );
}

#[test]
fn resolve_key_import_source_defaults_to_exported_key_non_interactively() {
    assert_eq!(
        resolve_key_import_source(false).unwrap(),
        KeyImportSource::ExportedKey
    );
}

#[test]
fn secret_environment_options_show_defaults_then_existing_then_new() {
    let tako_config = TakoToml::parse(
        r#"
[envs.staging]
route = "staging.example.com"
"#,
    )
    .unwrap();
    let secrets = crate::config::SecretsStore::parse(
        r#"{
            "preview": {
                "key_id": "0123456789abcdef",
                "app": {}
            }
        }"#,
    )
    .unwrap();

    let labels: Vec<String> = secret_environment_options(&tako_config, &secrets)
        .into_iter()
        .map(|(label, _)| label)
        .collect();

    assert_eq!(
        labels,
        vec![
            "development".to_string(),
            "production".to_string(),
            "preview".to_string(),
            "staging".to_string(),
            "New environment".to_string(),
        ]
    );
}

#[test]
fn replace_existing_value_prompt_omits_secret_name() {
    assert_eq!(
        replace_existing_value_prompt(),
        "Value is already set. Replace it?"
    );
}

#[test]
fn key_bundle_round_trips_key_id_and_key() {
    with_temp_tako_home(|| {
        let key = crate::crypto::EncryptionKey::generate().unwrap();
        let key_id = "0123456789abcdef";

        let encoded = encode_key_bundle(key_id, &key);

        let (decoded_id, decoded_key) = decode_key_bundle(&encoded).unwrap();
        assert_eq!(decoded_id, key_id);
        assert_eq!(decoded_key.as_bytes(), key.as_bytes());
    });
}

#[test]
fn key_bundle_rejects_invalid_payload() {
    let err = match decode_key_bundle("not-a-tako-key") {
        Ok(_) => panic!("expected invalid payload to fail"),
        Err(err) => err,
    };
    assert!(err.to_string().contains("Invalid key payload"));
}

#[test]
fn key_bundle_rejects_unsupported_version() {
    let payload = serde_json::to_vec(&serde_json::json!({
        "version": 2,
        "id": "0123456789abcdef",
        "key": crate::crypto::EncryptionKey::generate().unwrap().to_base64(),
    }))
    .unwrap();
    let encoded = BASE64_URL.encode(payload);

    let err = match decode_key_bundle(&encoded) {
        Ok(_) => panic!("expected unsupported version to fail"),
        Err(err) => err,
    };
    assert_eq!(err.to_string(), "Unsupported key version: 2");
}

#[test]
fn environment_for_key_id_matches_environment_key_id() {
    let secrets = crate::config::SecretsStore::parse(
        r#"{
            "development": {
                "key_id": "0123456789abcdef",
                "app": {}
            }
        }"#,
    )
    .unwrap();

    assert_eq!(
        environment_for_key_id(&secrets, "0123456789abcdef").as_deref(),
        Some("development")
    );
    assert_eq!(environment_for_key_id(&secrets, "fedcba9876543210"), None);
}

#[test]
fn resolve_secret_sync_server_names_uses_explicit_mapping() {
    let tako_config = TakoToml::parse(
        r#"
[envs.production]
route = "app.example.com"
servers = ["solo"]
"#,
    )
    .unwrap();
    let mut servers = ServersToml::default();
    servers.servers.insert(
        "solo".to_string(),
        ServerEntry {
            host: "127.0.0.1".to_string(),
            port: 22,
            description: None,
            ..Default::default()
        },
    );

    let names = resolve_secret_sync_server_names("production", &tako_config, &servers)
        .expect("should resolve");
    assert_eq!(names, vec!["solo".to_string()]);
}

#[test]
fn resolve_secret_sync_server_names_returns_empty_for_unmapped_non_production() {
    let tako_config = TakoToml::default();
    let mut servers = ServersToml::default();
    servers.servers.insert(
        "solo".to_string(),
        ServerEntry {
            host: "127.0.0.1".to_string(),
            port: 22,
            description: None,
            ..Default::default()
        },
    );

    let names =
        resolve_secret_sync_server_names("staging", &tako_config, &servers).expect("should work");
    assert!(names.is_empty());
}

#[test]
fn build_update_secrets_command_uses_protocol_payload_not_env_file_writes() {
    let secrets = HashMap::from([("API_KEY".to_string(), "secret".to_string())]);
    let command = build_update_secrets_command("my-app", &secrets).expect("serialize command");
    let value: serde_json::Value =
        serde_json::from_str(&command).expect("parse serialized command");

    assert_eq!(
        value.get("command").and_then(|v| v.as_str()),
        Some("update_secrets")
    );
    assert_eq!(value.get("app").and_then(|v| v.as_str()), Some("my-app"));
    assert_eq!(
        value
            .get("secrets")
            .and_then(|v| v.get("API_KEY"))
            .and_then(|v| v.as_str()),
        Some("secret")
    );
    assert!(!command.contains(".env"));
}

#[test]
fn tako_response_has_error_only_accepts_structured_status_errors() {
    let json_err = r#"{"status":"error","message":"nope"}"#;
    let json_ok = r#"{"status":"ok","data":{}}"#;
    let old_error_shape = r#"{"error":"old-shape"}"#;
    let plain_text = "all good";

    assert!(tako_response_has_error(json_err));
    assert!(!tako_response_has_error(json_ok));
    assert!(!tako_response_has_error(old_error_shape));
    assert!(!tako_response_has_error(plain_text));
}
