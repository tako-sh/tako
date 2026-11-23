use super::*;
use crate::network::state::cache_merge;
use crate::network::tests::{TestHome, fixture};
use tako_network::shared::{SharedRecord, SharedStore};

#[test]
fn concurrent_first_writes_initialize_ignored_state() {
    let home = TestHome::new();
    let project = home.directory.path().join("checkout");
    let ready = std::sync::Barrier::new(8);
    std::thread::scope(|scope| {
        for _ in 0..8 {
            scope.spawn(|| {
                ready.wait();
                let _lock = lock(&project).unwrap();
                assert_eq!(
                    std::fs::read_to_string(project.join(".tako/.gitignore")).unwrap(),
                    "*\n"
                );
            });
        }
    });
}

fn published(profile: &Profile, name: &str, value: Option<&str>, revision: u64) -> SharedRecord {
    let mutation = RecordMutation {
        observed_revision: 0,
        expires_at: 86400,
        id: RequestId::random().unwrap(),
        address: RecordAddress {
            scope: "demo/production".parse().unwrap(),
            kind: RecordKind::App,
            name: name.into(),
        },
        value: None,
    };
    let content = value.map(|value| {
        serde_json::to_vec(&RecordContent {
            value: Zeroizing::new(value.into()),
            expires_on: None,
        })
        .unwrap()
    });
    let author = profile
        .request()
        .device_id_for(&profile.snapshot().network.id)
        .unwrap();
    SharedRecord::create(
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
fn first_uncached_edit_binds_only_its_environment_and_rechecks_the_target_on_save() {
    let home = TestHome::new();
    let project = home.directory.path().join("checkout");
    std::fs::create_dir_all(&project).unwrap();
    let network = NetworkId::random().unwrap();
    let config = |app: &str, network: &NetworkId| {
        TakoToml::parse(&format!(
            "name='{app}'\n\
             [envs.production]\nroute='prod.example.test'\nnetwork='{network}'\n\
             [envs.staging]\nroute='staging.example.test'\nnetwork='{network}'\n"
        ))
        .unwrap()
    };
    let original = config("first", &network);
    let alternate = config("second", &network);
    let mut values = SecretsStore::load_with_config(&project, &original).unwrap();
    assert!(read_workspace(&project).unwrap().environments.is_empty());
    values.ensure_env_key_id("production").unwrap();
    let key = crate::crypto::EncryptionKey::generate().unwrap();
    values
        .set(
            "production",
            "TOKEN",
            crate::crypto::encrypt("draft", &key).unwrap(),
        )
        .unwrap();
    save_local(&project, &values).unwrap();

    let state = read_workspace(&project).unwrap();
    assert_eq!(state.environments.len(), 1);
    assert_eq!(
        state.environments["production"].scope.to_string(),
        "first/production"
    );
    assert_eq!(state.environments["production"].network, network);
    // A deploy checks this before it moves the environment.
    let other = NetworkId::random().unwrap().to_string();
    assert!(unpublished_elsewhere(&project, "production", &other).unwrap());
    assert!(!unpublished_elsewhere(&project, "production", &network.to_string()).unwrap());
    assert!(!unpublished_elsewhere(&project, "staging", &other).unwrap());
    // Unpublished drafts keep the checkout bound to their app and network.
    for changed in [alternate, config("first", &NetworkId::random().unwrap())] {
        let error =
            SecretsStore::load_for_environment(&project, &changed, "production").unwrap_err();
        assert!(
            error.to_string().contains("unpublished production secrets"),
            "{error}"
        );
    }

    // Another command can bind an empty environment to another app. Nothing
    // there is unpublished, so the edit starts the environment over instead.
    let mut editor = SecretsStore::load_for_environment(&project, &original, "staging").unwrap();
    let mut state = read_workspace(&project).unwrap();
    bind(
        &mut state,
        "staging",
        &"second/staging".parse().unwrap(),
        &network,
    )
    .unwrap();
    write_workspace(&project, &state).unwrap();
    editor.ensure_env_key_id("staging").unwrap();
    editor
        .set(
            "staging",
            "TOKEN",
            crate::crypto::encrypt("draft", &key).unwrap(),
        )
        .unwrap();
    save_local(&project, &editor).unwrap();
    let state = read_workspace(&project).unwrap();
    assert_eq!(
        state.environments["staging"].scope.to_string(),
        "first/staging"
    );
    assert!(state.values.contains("staging", "TOKEN"));
}

#[test]
fn selected_environment_cache_and_type_generation_leave_other_environments_untouched() {
    let home = TestHome::new();
    let healthy = fixture();
    let mut stale = fixture();
    let locals = [&healthy, &stale].map(|fixture| {
        super::super::store_keys(&fixture.profile.request().request_id, &fixture.keys).unwrap();
        super::super::save_profile(&fixture.profile).unwrap()
    });
    for (fixture, local) in [(&healthy, &locals[0]), (&stale, &locals[1])] {
        let mut cache = SharedSnapshot::empty(fixture.profile.snapshot().network.id.clone());
        cache
            .records
            .push(published(&fixture.profile, "TOKEN", Some("published"), 1));
        cache_merge(local, &cache, &fixture.profile).unwrap();
    }
    let Profile::Active { profile } = &mut stale.profile else {
        panic!()
    };
    let own = profile.snapshot.network.initial_admin.clone();
    profile
        .snapshot
        .members
        .iter_mut()
        .find(|member| member.device_id == own)
        .unwrap()
        .expires_at = Some(1);
    super::super::write_json(&locals[1].profile_path().unwrap(), &stale.profile, true).unwrap();
    let project = home.directory.path().join("checkout");
    std::fs::create_dir_all(&project).unwrap();
    let config = TakoToml::parse(&format!(
        "name='demo'\n[envs.production]\nroute='demo.example.test'\nnetwork='{}'\n[envs.staging]\nroute='staging.example.test'\nnetwork='{}'\n",
        locals[0].id, locals[1].id,
    ))
    .unwrap();
    let values = SecretsStore::load_for_environment(&project, &config, "production").unwrap();
    assert!(decrypted(&values, "TOKEN") == "published");
    assert!(
        !read_workspace(&project)
            .unwrap()
            .environments
            .contains_key("staging")
    );
    let before = std::fs::read(SecretsStore::default_path(&project)).unwrap();
    crate::build::js::write_tako_declarations_with_config(
        &project,
        crate::build::BuildAdapter::Unknown,
        &config,
        Some("production"),
    )
    .unwrap();
    crate::build::go::write_secret_accessors_with_config(&project, &config, Some("production"))
        .unwrap();
    assert!(
        std::fs::read_to_string(project.join("tako.d.ts"))
            .unwrap()
            .contains("TOKEN")
    );
    assert!(
        std::fs::read_to_string(project.join("tako_secrets.go"))
            .unwrap()
            .contains("Token")
    );
    assert_eq!(
        std::fs::read(SecretsStore::default_path(&project)).unwrap(),
        before
    );
    assert!(SecretsStore::load_for_environment(&project, &config, "staging").is_err());
    assert_eq!(
        std::fs::read(SecretsStore::default_path(&project)).unwrap(),
        before
    );
}

#[test]
fn a_clean_checkout_follows_its_environment_to_another_network() {
    let home = TestHome::new();
    let fixtures = [fixture(), fixture()];
    let locals = fixtures.each_ref().map(|fixture| {
        super::super::store_keys(&fixture.profile.request().request_id, &fixture.keys).unwrap();
        super::super::save_profile(&fixture.profile).unwrap()
    });
    for ((fixture, local), value) in fixtures.iter().zip(&locals).zip(["from-a", "from-b"]) {
        let mut cache = SharedSnapshot::empty(fixture.profile.snapshot().network.id.clone());
        cache
            .records
            .push(published(&fixture.profile, "TOKEN", Some(value), 1));
        cache_merge(local, &cache, &fixture.profile).unwrap();
    }
    let project = home.directory.path().join("checkout");
    std::fs::create_dir_all(&project).unwrap();
    let mut config =
        TakoToml::parse("name='demo'\n[envs.production]\nroute='demo.example.test'\n").unwrap();
    config.envs.get_mut("production").unwrap().network = Some(locals[0].id.clone());
    let values = SecretsStore::load_for_environment(&project, &config, "production").unwrap();
    assert_eq!(decrypted(&values, "TOKEN"), "from-a");

    // Every value is published, so the checkout takes the new network's
    // values instead of carrying the old ones over as drafts.
    config.envs.get_mut("production").unwrap().network = Some(locals[1].id.clone());
    let values = SecretsStore::load_for_environment(&project, &config, "production").unwrap();
    assert_eq!(decrypted(&values, "TOKEN"), "from-b");
    let state = read_workspace(&project).unwrap();
    let entry = &state.environments["production"];
    assert_eq!(entry.network.to_string(), locals[1].id);
    let current = local::rows(state.values.environments.get("production")).unwrap();
    assert!(unpublished(entry, &current).is_empty());
    assert!(entry.pending.is_empty());
}

#[test]
fn two_worktrees_share_published_values_but_keep_drafts_and_deletions_separate() {
    let home = TestHome::new();
    let fixture = fixture();
    super::super::store_keys(&fixture.profile.request().request_id, &fixture.keys).unwrap();
    let alias = super::super::save_profile(&fixture.profile).unwrap();
    let mut cache = SharedSnapshot::empty(fixture.profile.snapshot().network.id.clone());
    cache
        .records
        .push(published(&fixture.profile, "TOKEN", Some("published"), 1));
    cache
        .records
        .push(published(&fixture.profile, "DELETE_ME", Some("old"), 1));
    cache_merge(&alias, &cache, &fixture.profile).unwrap();
    let a = home.directory.path().join("checkout-a");
    let b = home.directory.path().join("checkout-b");
    for project in [&a, &b] {
        std::fs::create_dir_all(project).unwrap();
        std::fs::write(
            project.join("tako.toml"),
            "name = 'demo'\n[envs.production]\nroute = 'example.test'\n",
        )
        .unwrap();
        TakoToml::record_network(&project.join("tako.toml"), "production", &alias.id).unwrap();
    }
    let older = cache.clone();
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
    cache.records.push(published(
        &fixture.profile,
        "OTHER",
        Some("from-other-client"),
        2,
    ));
    cache
        .append_record(&published(&fixture.profile, "DELETE_ME", None, 2))
        .unwrap();
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
    let merged = cache_merge(&alias, &older, &fixture.profile).unwrap();
    assert!(
        merged
            .latest()
            .unwrap()
            .values()
            .any(|record| record.address.name == "DELETE_ME" && record.is_deletion())
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
fn storage_pairs_survive_record_ordering_and_legacy_backup_keys_are_ignored() {
    let key = crate::crypto::EncryptionKey::generate().unwrap();
    let mut values = crate::config::EnvironmentSecrets {
        key_id: "a".repeat(16),
        ..Default::default()
    };
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
    let mut rows = local::rows(Some(&values)).unwrap();
    assert_eq!(
        rows.values()
            .filter(|row| row.key.kind == RecordKind::Storage)
            .count(),
        1
    );
    assert_eq!(
        local::environment(values.key_id.clone(), &rows).unwrap(),
        values
    );
    // Records of app data archive keys, which Tako no longer makes, change nothing.
    for (kind, name) in [
        (RecordKind::BackupKey, "backup-key-0000000000000000"),
        (RecordKind::BackupActive, "active"),
    ] {
        let key = RowKey {
            kind,
            name: name.into(),
        };
        rows.insert(
            key.clone(),
            Row {
                key,
                value: "value".into(),
                expires_on: None,
            },
        );
    }
    assert_eq!(
        local::environment(values.key_id.clone(), &rows).unwrap(),
        values
    );
}

#[test]
fn last_changes_name_the_publishing_device_and_mark_unpublished_drafts() {
    let home = TestHome::new();
    let fixture = fixture();
    super::super::store_keys(&fixture.profile.request().request_id, &fixture.keys).unwrap();
    let alias = super::super::save_profile(&fixture.profile).unwrap();
    let mut cache = SharedSnapshot::empty(fixture.profile.snapshot().network.id.clone());
    for name in ["TOKEN", "EDITED", "REMOVED"] {
        cache
            .records
            .push(published(&fixture.profile, name, Some("published"), 1));
    }
    cache_merge(&alias, &cache, &fixture.profile).unwrap();
    let project = home.directory.path().join("checkout");
    std::fs::create_dir_all(&project).unwrap();
    let toml = "name = 'demo'\n[envs.production]\nroute = 'example.test'\n".to_string();
    std::fs::write(project.join("tako.toml"), &toml).unwrap();
    TakoToml::record_network(&project.join("tako.toml"), "production", &alias.id).unwrap();
    let config = TakoToml::load_from_dir(&project).unwrap();

    let mut values = load_local(&project).unwrap();
    let key = crate::crypto::KeyStore::for_key_id(values.get_key_id("production").unwrap())
        .unwrap()
        .load_key()
        .unwrap();
    for (name, value) in [("EDITED", "draft"), ("NEW", "draft")] {
        values
            .set(
                "production",
                name,
                crate::crypto::encrypt(value, &key).unwrap(),
            )
            .unwrap();
    }
    values.remove("production", "REMOVED").unwrap();
    save_local(&project, &values).unwrap();

    let changes = last_changes(&project, &config, "production").unwrap();
    let device = fixture
        .profile
        .request()
        .device_id_for(&fixture.profile.snapshot().network.id)
        .unwrap();
    assert_eq!(
        changes["TOKEN"],
        LastChange::Published {
            device,
            name: Some(fixture.profile.request().name.as_str().to_owned()),
        }
    );
    assert_eq!(changes["EDITED"], LastChange::Draft);
    assert_eq!(changes["NEW"], LastChange::Draft);
    // A local removal keeps the published entry; the listing shows it unset.
    assert!(matches!(
        changes.get("REMOVED"),
        Some(LastChange::Published { .. })
    ));
    // An environment without a network has no record of who changed it.
    assert!(
        last_changes(&project, &config, "staging")
            .unwrap()
            .is_empty()
    );
}

#[test]
fn apps_without_network_environments_ignore_unreadable_network_profiles() {
    let home = TestHome::new();
    let network = NetworkId::random().unwrap();
    let device = "ab".repeat(32);
    let networks = home.directory.path().join("networks");
    let profile = networks
        .join(network.to_string())
        .join(&device)
        .join("profile.json");
    super::super::private_directory(profile.parent().unwrap()).unwrap();
    super::super::write_bytes(&profile, br#"{"state":"created"}"#, false).unwrap();
    super::super::write_json(
        &networks.join("identities.json"),
        &serde_json::json!([{"id": network.to_string(), "device": device}]),
        false,
    )
    .unwrap();
    let project = home.directory.path().join("checkout");
    let config =
        TakoToml::parse("name='plain'\n[envs.production]\nroute='plain.example.test'\n").unwrap();

    SecretsStore::load_with_config(&project, &config).unwrap();

    let error = LocalNetworks::load().unwrap_err().to_string();
    assert!(
        error.contains(&profile.display().to_string()),
        "error should name the unreadable profile: {error}"
    );
}

#[test]
fn local_state_with_archive_backup_keys_still_loads() {
    let directory = tempfile::TempDir::new().unwrap();
    let project = directory.path();
    std::fs::create_dir_all(project.join(".tako")).unwrap();
    std::fs::write(
        SecretsStore::default_path(project),
        serde_json::json!({
            "values": {
                "production": {
                    "key_id": "k1",
                    "backup_keys": [{"id": "b1", "value": "sealed"}],
                }
            },
            "environments": {},
        })
        .to_string(),
    )
    .unwrap();
    let state = read_workspace(project).unwrap();
    assert_eq!(state.values.environments["production"].key_id, "k1");
}
