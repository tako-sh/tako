use super::*;
use crate::config::{EncryptedBackupKey, EncryptedSecretValue, EncryptedStorageCredentials};

const NETWORK: &str = "0123456789abcdef0123456789abcdef";

fn config() -> TakoToml {
    TakoToml::parse(
        r#"
name = "shop"

[storages.media]
bucket = "media"
endpoint = "https://s3.example.com"
region = "auto"

[envs.production]
route = "*.shop.example.com"
storages = { uploads = "media", cache = "local" }
db = { main = "orders" }
"#,
    )
    .unwrap()
}

fn value(expires_on: Option<&str>) -> EncryptedSecretValue {
    EncryptedSecretValue::new("ciphertext".into(), expires_on.map(str::to_owned))
}

fn secrets() -> SecretsStore {
    let mut secrets = SecretsStore::default();
    secrets.ensure_env_key_id("production").unwrap();
    let env = secrets.environments.get_mut("production").unwrap();
    env.app.insert("API_KEY".into(), value(None));
    env.app.insert("DATABASE_URL".into(), value(None));
    env.credentials.insert("postgres_url".into(), value(None));
    env.storages.insert(
        "media".into(),
        EncryptedStorageCredentials {
            access_key_id: value(None),
            secret_access_key: value(None),
        },
    );
    env.backup_keys
        .push(EncryptedBackupKey::new("k1".into(), "ciphertext".into()));
    secrets
}

fn record(name: &str, server: &str) -> DatabaseRecord {
    DatabaseRecord {
        name: name.into(),
        server: server.into(),
        generation: "g1".into(),
        created_at_unix_secs: 0,
        engine: tako_core::DatabaseEngine::Sqlite,
        channel_owner: None,
    }
}

#[test]
fn a_full_client_sees_counts_credential_names_and_bindings() {
    let env = env_resources(
        "production",
        &config(),
        &secrets(),
        true,
        Access::Full,
        Ok(vec!["edge-a".into()]),
        Some("main".into()),
        Some(NETWORK.into()),
        &[record("orders", "edge-a")],
        &ServerInventory::default(),
    );
    assert_eq!(env.secrets, Some(2));
    assert_eq!(env.credentials, Some(vec!["postgres_url".to_string()]));
    assert_eq!(env.backup_key, Some(true));
    assert_eq!(env.servers, vec!["edge-a".to_string()]);
    assert_eq!(
        env.storages,
        vec![
            StorageBinding {
                binding: "cache".into(),
                resource: "local".into(),
                credentials: None,
            },
            StorageBinding {
                binding: "uploads".into(),
                resource: "media".into(),
                credentials: Some(true),
            },
        ]
    );
    assert_eq!(
        env.databases,
        vec![ProjectDatabase {
            usage: DatabaseUsage::Binding {
                binding: "main".into()
            },
            database: "orders".into(),
            server: Some("edge-a".into()),
        }]
    );
    assert_eq!(env.note, None);
    // No value is ever carried, only names and counts.
    assert!(!serde_json::to_string(&env).unwrap().contains("ciphertext"));
}

#[test]
fn a_deploy_client_sees_databases_and_servers_and_a_note_about_secrets() {
    let env = env_resources(
        "production",
        &config(),
        &SecretsStore::default(),
        true,
        Access::Deploy,
        Ok(vec!["edge-a".into()]),
        Some("main".into()),
        Some(NETWORK.into()),
        &[record("orders", "edge-a")],
        &ServerInventory::default(),
    );
    assert_eq!(env.secrets, None);
    assert_eq!(env.credentials, None);
    assert_eq!(env.backup_key, None);
    assert!(
        env.storages
            .iter()
            .all(|storage| storage.credentials.is_none())
    );
    assert_eq!(env.databases[0].server.as_deref(), Some("edge-a"));
    assert_eq!(env.servers, vec!["edge-a".to_string()]);
    assert_eq!(env.note.as_deref(), Some(DEPLOY_CLIENT_NOTE));
    assert!(!env.can_read_secrets());
}

#[test]
fn channel_replay_status_names_the_database_without_an_app_binding() {
    let config = TakoToml::parse(
        r#"
name = "shop"
[envs.production]
route = "shop.example.com"
channels.db = "events"
[envs.staging]
route = "staging.example.com"
"#,
    )
    .unwrap();
    let owned = DatabaseRecord {
        channel_owner: Some("shop/staging".into()),
        ..record("generated", "edge-b")
    };
    let records = [record("events", "edge-a"), owned];
    for (name, database, server) in [
        ("production", "events", "edge-a"),
        ("staging", "generated", "edge-b"),
    ] {
        let env = env_resources(
            name,
            &config,
            &SecretsStore::default(),
            true,
            Access::Deploy,
            Ok(vec![]),
            Some("main".into()),
            Some(NETWORK.into()),
            &records,
            &ServerInventory::default(),
        );
        assert_eq!(
            serde_json::to_value(env.databases).unwrap(),
            serde_json::json!([{"database":database, "server":server, "usage":"channel_replay"}]),
        );
    }
    let none = SecretsStore::default();
    assert_eq!(
        replay_database(&config, "production", &[], &none, true),
        Some("events")
    );
    assert_eq!(replay_database(&config, "staging", &[], &none, true), None);
    // Postgres replaces a generated database, but never an explicit one.
    let mut postgres = SecretsStore::default();
    for env in ["production", "staging"] {
        postgres.ensure_env_key_id(env).unwrap();
        postgres
            .set_credential(
                env,
                crate::config::POSTGRES_CREDENTIAL_NAME,
                crate::config::EncryptedSecretValue::new("encrypted".into(), None),
            )
            .unwrap();
    }
    assert_eq!(
        replay_database(&config, "staging", &records, &none, true),
        Some("generated")
    );
    // Code without channels leaves only an explicit selection in use.
    assert_eq!(
        replay_database(&config, "staging", &records, &none, false),
        None
    );
    assert_eq!(
        replay_database(&config, "production", &records, &none, false),
        Some("events")
    );
    assert_eq!(
        replay_database(&config, "staging", &records, &postgres, true),
        None
    );
    assert_eq!(
        replay_database(&config, "production", &records, &postgres, true),
        Some("events")
    );
}

#[test]
fn databases_on_a_server_name_the_environments_that_bind_them() {
    let mut config = config();
    config.envs.get_mut("production").unwrap().channels = Some(crate::config::ChannelsConfig {
        db: "orders".into(),
    });
    let env = env_resources(
        "production",
        &config,
        &secrets(),
        true,
        Access::Full,
        Ok(vec![]),
        None,
        Some(NETWORK.into()),
        &[record("orders", "edge-a")],
        &ServerInventory::default(),
    );
    let project = ProjectResources {
        app: "shop".into(),
        config,
        secrets: secrets(),
        envs: vec![env],
        databases: BTreeMap::from([
            (
                NETWORK.to_string(),
                Ok(vec![
                    record("orders", "dev-a"),
                    record("audit", "dev-a"),
                    record("other", "dev-b"),
                ]),
            ),
            // Another network's database of the same name isn't this one.
            (
                "other-network".to_string(),
                Ok(vec![record("orders", "dev-x")]),
            ),
        ]),
    };
    assert_eq!(
        project.databases_on("dev-a"),
        vec![
            ("audit".to_string(), vec![]),
            (
                "orders".to_string(),
                vec![
                    "shop/production".to_string(),
                    "shop/production#channels".to_string()
                ]
            ),
        ]
    );
    assert_eq!(
        project.databases_on("dev-x"),
        vec![("orders".to_string(), vec![])]
    );
    assert!(project.databases_on("dev-c").is_empty());
}

#[test]
fn a_wildcard_route_needs_the_cloudflare_credential() {
    let config = config();
    let mut secrets = secrets();
    assert!(missing_cloudflare_token(&config, &secrets, "production"));
    secrets
        .environments
        .get_mut("production")
        .unwrap()
        .credentials
        .insert(SSL_CLOUDFLARE_CREDENTIAL_NAME.into(), value(None));
    assert!(!missing_cloudflare_token(&config, &secrets, "production"));
}

#[test]
fn expired_and_soon_expiring_values_are_found_by_name() {
    let soon = (::time::OffsetDateTime::now_utc() + ::time::Duration::days(10)).date();
    let soon = format!(
        "{:04}-{:02}-{:02}",
        soon.year(),
        u8::from(soon.month()),
        soon.day()
    );
    let mut secrets = secrets();
    let env = secrets.environments.get_mut("production").unwrap();
    env.app.insert("OLD".into(), value(Some("2001-01-01")));
    env.app.insert("SOON".into(), value(Some(&soon)));
    env.app.insert("LATER".into(), value(Some("2999-01-01")));
    env.credentials
        .insert("ssl.cloudflare".into(), value(Some("2001-01-01")));
    assert_eq!(
        expiring_values(&secrets, "production", 30),
        vec![
            ("OLD".to_string(), true),
            ("SOON".to_string(), false),
            ("ssl.cloudflare".to_string(), true),
        ]
    );
    assert!(expiring_values(&secrets, "staging", 30).is_empty());
}

#[test]
fn servers_missing_key_grants_are_counted() {
    let holders = vec![
        BTreeSet::from(["dev-a".to_string(), "dev-b".to_string()]),
        BTreeSet::from(["dev-a".to_string()]),
        BTreeSet::from(["dev-a".to_string()]),
    ];
    let servers = vec![
        ("edge-a".to_string(), "dev-a".to_string()),
        ("edge-b".to_string(), "dev-b".to_string()),
        ("edge-c".to_string(), "dev-c".to_string()),
    ];
    assert_eq!(
        count_missing_grants(&holders, &servers),
        vec![("edge-b".to_string(), 2), ("edge-c".to_string(), 3)]
    );
}
