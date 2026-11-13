use super::*;
use crate::network::tests::{TestHome, fixture};
use tako_network::shared::SecretRecord;

fn published(profile: &Profile, name: &str, value: Option<&str>, revision: u64) -> SecretRecord {
    let mutation = SecretMutation {
        observed_revision: 0,
        id: RequestId::random().unwrap(),
        address: SecretAddress {
            scope: "demo/production".parse().unwrap(),
            kind: SecretKind::App,
            name: name.into(),
        },
        value: None,
    };
    let content = value.map(|value| {
        serde_json::to_vec(&SecretContent {
            value: Zeroizing::new(value.into()),
            expires_on: None,
        })
        .unwrap()
    });
    let author = profile
        .request()
        .device_id_for(&profile.snapshot().network.id)
        .unwrap();
    SecretRecord::encrypt(
        &mutation,
        content.as_deref(),
        revision,
        author.clone(),
        author,
        profile.snapshot(),
        super::super::now().unwrap(),
    )
    .unwrap()
}
fn decrypted(values: &SecretsStore, name: &str) -> String {
    let key = crate::crypto::KeyStore::for_key_id(values.get_key_id("production").unwrap())
        .unwrap()
        .load_key()
        .unwrap();
    crate::crypto::decrypt(values.get("production", name).unwrap(), &key).unwrap()
}

#[test]
fn two_worktrees_share_published_values_but_keep_drafts_and_deletions_separate() {
    let home = TestHome::new();
    let fixture = fixture();
    super::super::store_keys(&fixture.profile.request().request_id, &fixture.keys).unwrap();
    let alias = super::super::save_profile(&fixture.profile).unwrap();
    let mut cache = SharedSnapshot::empty(fixture.profile.snapshot().network.id.clone());
    cache
        .secrets
        .push(published(&fixture.profile, "TOKEN", Some("published"), 1));
    cache
        .secrets
        .push(published(&fixture.profile, "DELETE_ME", Some("old"), 1));
    cache_merge(&alias, &cache, &fixture.profile).unwrap();
    let a = home.directory.path().join("checkout-a");
    let b = home.directory.path().join("checkout-b");
    for project in [&a, &b] {
        std::fs::create_dir_all(project).unwrap();
        std::fs::write(
            project.join("tako.toml"),
            format!(
                "name = 'demo'\n[envs.production]\nnetwork = '{}'\nroute = 'example.test'\n",
                alias.id
            ),
        )
        .unwrap();
    }
    let mut first = load_local(&a).unwrap();
    assert_eq!(decrypted(&first, "TOKEN"), "published");
    let path = SecretsStore::default_path(&a);
    let modified = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(42);
    std::fs::File::open(&path)
        .unwrap()
        .set_modified(modified)
        .unwrap();
    assert_eq!(decrypted(&load_local(&a).unwrap(), "TOKEN"), "published");
    assert_eq!(
        std::fs::metadata(&path).unwrap().modified().unwrap(),
        modified
    );
    let key = crate::crypto::KeyStore::for_key_id(first.get_key_id("production").unwrap())
        .unwrap()
        .load_key()
        .unwrap();
    first
        .set(
            "production",
            "TOKEN",
            crate::crypto::encrypt("draft-a", &key).unwrap(),
        )
        .unwrap();
    first.remove("production", "DELETE_ME").unwrap();
    save_local(&a, &first).unwrap();
    let second = load_local(&b).unwrap();
    assert_eq!(decrypted(&second, "TOKEN"), "published");
    assert!(second.contains("production", "DELETE_ME"));
    cache.secrets.push(published(
        &fixture.profile,
        "OTHER",
        Some("from-other-client"),
        2,
    ));
    cache
        .secrets
        .push(published(&fixture.profile, "DELETE_ME", None, 2));
    let mut state = read_workspace(&a).unwrap();
    merge_remote(
        &mut state,
        "production",
        &"demo/production".parse().unwrap(),
        &fixture.profile,
        &cache,
    )
    .unwrap();
    assert_eq!(decrypted(&state.values, "TOKEN"), "draft-a");
    assert_eq!(decrypted(&state.values, "OTHER"), "from-other-client");
    assert!(!state.values.contains("production", "DELETE_ME"));
    let baseline: Rows = state.environments["production"]
        .base
        .iter()
        .map(|row| (row.key.clone(), row.clone()))
        .collect();
    let values = local::rows(state.values.environments.get("production")).unwrap();
    let dirty: Vec<_> = values
        .keys()
        .chain(baseline.keys())
        .filter(|key| values.get(*key) != baseline.get(*key))
        .collect();
    assert!(dirty.iter().all(|key| key.name == "TOKEN"));
    // An older server response cannot discard a tombstone already cached.
    cache_merge(&alias, &cache, &fixture.profile).unwrap();
    let older = SharedSnapshot {
        network: cache.network.clone(),
        secrets: cache.secrets[..2].to_vec(),
        grants: vec![],
    };
    let merged = cache_merge(&alias, &older, &fixture.profile).unwrap();
    assert!(
        merged
            .latest()
            .unwrap()
            .values()
            .any(|record| record.address.name == "DELETE_ME" && record.value.is_none())
    );
}

#[test]
fn local_writes_reject_stale_editors_and_never_persist_plaintext() {
    let home = TestHome::new();
    let project = home.directory.path().join("project");
    let mut first = load_local(&project).unwrap();
    let mut stale = load_local(&project).unwrap();
    let id = first.ensure_env_key_id("production").unwrap();
    let key = crate::crypto::EncryptionKey::generate().unwrap();
    crate::crypto::KeyStore::for_key_id(&id)
        .unwrap()
        .save_key(&key)
        .unwrap();
    first
        .set(
            "production",
            "TOKEN",
            crate::crypto::encrypt("never-on-disk", &key).unwrap(),
        )
        .unwrap();
    save_local(&project, &first).unwrap();
    stale.ensure_env_key_id("production").unwrap();
    stale
        .set(
            "production",
            "OTHER",
            crate::crypto::encrypt("another-value", &key).unwrap(),
        )
        .unwrap();
    assert!(save_local(&project, &stale).is_err());
    let bytes = std::fs::read_to_string(SecretsStore::default_path(&project)).unwrap();
    assert!(!bytes.contains("never-on-disk"));
    assert_eq!(
        decrypted(&load_local(&project).unwrap(), "TOKEN"),
        "never-on-disk"
    );
    assert_eq!(
        std::fs::read_to_string(project.join(".tako/.gitignore")).unwrap(),
        "*\n"
    );
}

#[test]
fn migration_preserves_existing_ciphertext_and_missing_key_never_generates_replacement() {
    let home = TestHome::new();
    let project = home.directory.path().join("project");
    std::fs::create_dir_all(project.join(".tako")).unwrap();
    let mut legacy = SecretsStore::default();
    let id = legacy.ensure_env_key_id("production").unwrap();
    let key = crate::crypto::EncryptionKey::generate().unwrap();
    legacy
        .set(
            "production",
            "TOKEN",
            crate::crypto::encrypt("historical", &key).unwrap(),
        )
        .unwrap();
    legacy
        .save_to_file(project.join(".tako/secrets.json"))
        .unwrap();
    let values = load_local(&project).unwrap();
    assert_eq!(values, legacy);
    save_local(&project, &values).unwrap();
    assert_eq!(load_local(&project).unwrap(), legacy);
    let fixture = fixture();
    super::super::store_keys(&fixture.profile.request().request_id, &fixture.keys).unwrap();
    let mut state = read_workspace(&project).unwrap();
    let cache = SharedSnapshot::empty(fixture.profile.snapshot().network.id.clone());
    assert!(
        merge_remote(
            &mut state,
            "production",
            &"demo/production".parse().unwrap(),
            &fixture.profile,
            &cache
        )
        .is_err()
    );
    assert!(
        !crate::crypto::KeyStore::for_key_id(&id)
            .unwrap()
            .key_exists()
    );
    assert_eq!(load_local(&project).unwrap(), legacy);
}

#[test]
fn migration_checks_legacy_keys_even_when_a_different_local_draft_exists() {
    let home = TestHome::new();
    let project = home.directory.path().join("project");
    std::fs::create_dir_all(project.join(".tako")).unwrap();
    let legacy_path = project.join(".tako/secrets.json");
    let mut legacy = SecretsStore::default();
    let legacy_id = legacy.ensure_env_key_id("production").unwrap();
    let legacy_key = crate::crypto::EncryptionKey::generate().unwrap();
    legacy
        .set(
            "production",
            "TOKEN",
            crate::crypto::encrypt("historical", &legacy_key).unwrap(),
        )
        .unwrap();
    legacy.save_to_file(&legacy_path).unwrap();
    let original = std::fs::read(&legacy_path).unwrap();

    let mut draft = SecretsStore::default();
    let draft_id = draft.ensure_env_key_id("production").unwrap();
    let draft_key = crate::crypto::EncryptionKey::generate().unwrap();
    crate::crypto::KeyStore::for_key_id(&draft_id)
        .unwrap()
        .save_key(&draft_key)
        .unwrap();
    draft
        .set(
            "production",
            "TOKEN",
            crate::crypto::encrypt("local-edit", &draft_key).unwrap(),
        )
        .unwrap();
    save_local(&project, &draft).unwrap();
    let local_path = SecretsStore::default_path(&project);
    let local_bytes = std::fs::read(&local_path).unwrap();
    let recovery = project.join(".tako/secrets.legacy.json");

    assert!(
        migrate(&project).is_err(),
        "migration requires the original key"
    );
    assert_eq!(std::fs::read(&legacy_path).unwrap(), original);
    assert_eq!(std::fs::read(&local_path).unwrap(), local_bytes);
    assert!(!recovery.exists());

    crate::crypto::KeyStore::for_key_id(&legacy_id)
        .unwrap()
        .save_key(&legacy_key)
        .unwrap();
    migrate(&project).unwrap();
    assert!(!legacy_path.exists());
    assert_eq!(std::fs::read(&recovery).unwrap(), original);
    assert_eq!(
        decrypted(&load_local(&project).unwrap(), "TOKEN"),
        "local-edit"
    );
}

#[test]
fn backup_selection_and_storage_pairs_survive_record_ordering() {
    let key = crate::crypto::EncryptionKey::generate().unwrap();
    let mut values = crate::config::EnvironmentSecrets {
        key_id: "a".repeat(16),
        ..Default::default()
    };
    for id in ["backup-key-ffffffffffffffff", "backup-key-0000000000000000"] {
        values
            .backup_keys
            .push(crate::config::EncryptedBackupKey::new(
                id.into(),
                crate::crypto::encrypt(id, &key).unwrap(),
            ));
    }
    let pair = crate::config::EncryptedStorageCredentials {
        access_key_id: crate::config::EncryptedSecretValue::new(
            crate::crypto::encrypt("access", &key).unwrap(),
            None,
        ),
        secret_access_key: crate::config::EncryptedSecretValue::new(
            crate::crypto::encrypt("secret", &key).unwrap(),
            None,
        ),
    };
    values.storages.insert("archive".into(), pair);
    let rows = local::rows(Some(&values)).unwrap();
    assert_eq!(
        rows.values()
            .filter(|row| row.key.kind == SecretKind::Storage)
            .count(),
        1
    );
    let rebuilt = local::environment(values.key_id.clone(), &rows).unwrap();
    assert_eq!(rebuilt, values);
    let mut partial = rows.clone();
    partial.remove(&RowKey {
        kind: SecretKind::BackupKey,
        name: "backup-key-0000000000000000".into(),
    });
    assert!(local::environment(values.key_id, &partial).is_err());
}
