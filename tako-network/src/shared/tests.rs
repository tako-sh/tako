use super::*;
use crate::{Approval, DeviceKind, MemberKind, NetworkDescriptor, RevocationId, WgChallenge};

fn bootstrap() -> (RegistrySnapshot, LocalDeviceKeys, DeviceId) {
    let keys = LocalDeviceKeys::generate().unwrap();
    let network = NetworkId::random().unwrap();
    let id = keys.device_id(&network, DeviceKind::Laptop).unwrap();
    let mut registry = RegistrySnapshot {
        network: NetworkDescriptor {
            name: "test".parse().unwrap(),
            id: network,
            initial_admin: id.clone(),
        },
        members: Vec::new(),
        revocations: Vec::new(),
        acks: Vec::new(),
    };
    admit(
        &mut registry,
        &keys,
        None,
        Permissions {
            network_admin: true,
            server_admin: true,
            ..Default::default()
        },
    );
    (registry, keys, id)
}

fn admit(
    registry: &mut RegistrySnapshot,
    keys: &LocalDeviceKeys,
    parent: Option<DeviceId>,
    permissions: Permissions,
) -> DeviceId {
    let request = keys
        .enrollment_request(
            Some(registry.network.id.clone()),
            DeviceKind::Laptop,
            "client".parse().unwrap(),
        )
        .unwrap();
    let (challenge, pending) =
        WgChallenge::begin(&request, registry.network.id.clone(), 1, 61).unwrap();
    let verified = pending
        .verify(&keys.answer_challenge(&challenge, &request, 1).unwrap(), 1)
        .unwrap();
    let member = registry
        .approve(
            verified,
            Approval {
                kind: MemberKind::Laptop,
                permissions,
                approved_by: parent,
                independent_admin: false,
                created_at: 1,
                expires_at: None,
            },
        )
        .unwrap();
    let id = member.device_id.clone();
    registry.members.push(member);
    id
}

fn address(name: &str) -> SecretAddress {
    SecretAddress {
        scope: "shop/production".parse().unwrap(),
        kind: SecretKind::App,
        name: name.into(),
    }
}

fn record(
    registry: &RegistrySnapshot,
    admin: &DeviceId,
    name: &str,
    revision: u64,
    value: Option<&[u8]>,
) -> SecretRecord {
    SecretRecord::encrypt(
        &SecretMutation {
            observed_revision: 0,
            expires_at: 86400,
            id: RequestId::random().unwrap(),
            address: address(name),
            value: None,
        },
        value,
        revision,
        admin.clone(),
        admin.clone(),
        registry,
        2,
    )
    .unwrap()
}

#[test]
fn independent_names_merge_and_stale_caches_cannot_resurrect_deletions() {
    let (registry, _, admin) = bootstrap();
    let mut left = SharedSnapshot::empty(registry.network.id.clone());
    left.append_secret(&record(&registry, &admin, "API_KEY", 1, Some(b"first")))
        .unwrap();
    let stale = left.clone();
    let mut right = stale.clone();
    right
        .append_secret(&record(
            &registry,
            &admin,
            "DATABASE_URL",
            1,
            Some(b"database"),
        ))
        .unwrap();
    left.append_secret(&record(&registry, &admin, "API_KEY", 2, None))
        .unwrap();
    let left_before = left.clone();
    left.merge(&right).unwrap();
    right.merge(&left_before).unwrap();
    left.merge(&stale).unwrap();
    assert_eq!(left.latest().unwrap(), right.latest().unwrap());
    let latest = left.latest().unwrap();
    assert!(latest[&address("API_KEY")].value.is_none());
    assert!(latest[&address("DATABASE_URL")].value.is_some());
}

#[test]
fn concurrent_same_name_edits_converge_and_exact_redelivery_is_idempotent() {
    let (registry, _, admin) = bootstrap();
    let a = record(&registry, &admin, "API_KEY", 1, Some(b"a"));
    let b = record(&registry, &admin, "API_KEY", 1, Some(b"b"));
    let winner = if a.order() > b.order() { &a } else { &b };
    for values in [[&a, &b], [&b, &a]] {
        let mut state = SharedSnapshot::empty(registry.network.id.clone());
        for value in values {
            state.append_secret(value).unwrap();
        }
        state.append_secret(&a).unwrap();
        assert_eq!(state.secrets.len(), 1);
        assert_eq!(state.latest().unwrap()[&address("API_KEY")], winner);
        let mut conflict = winner.clone();
        conflict.revision += 1;
        assert!(state.append_secret(&conflict).is_err());
        assert_eq!(state.secrets.len(), 1);
    }
}

#[test]
fn new_members_receive_key_grants_and_revocation_excludes_future_values() {
    let (mut registry, admin_keys, admin) = bootstrap();
    let mut state = SharedSnapshot::empty(registry.network.id.clone());
    let old = record(&registry, &admin, "API_KEY", 1, Some(b"old value"));
    state.append_secret(&old).unwrap();
    let client = LocalDeviceKeys::generate().unwrap();
    let client_id = admit(
        &mut registry,
        &client,
        Some(admin.clone()),
        Permissions {
            deployments: ["shop/production".parse().unwrap()].into(),
            ..Default::default()
        },
    );
    assert!(old.decrypt(&state, &client, &client_id).is_err());
    let effective = registry.effective_members(2).unwrap();
    let grant = old
        .grant(&state, &admin_keys, &admin, effective[&client_id])
        .unwrap();
    state.append_secret(&grant).unwrap();
    assert_eq!(
        &*grant.decrypt(&state, &client, &client_id).unwrap().unwrap(),
        b"old value"
    );
    assert_eq!(state.latest().unwrap()[&address("API_KEY")].revision, 1);
    let denial = registry
        .plan_revocation(RevocationId::random().unwrap(), &admin, &client_id, 3)
        .unwrap();
    registry.revocations.push(denial);
    let new = SecretRecord::encrypt(
        &SecretMutation {
            observed_revision: 0,
            expires_at: 86400,
            id: RequestId::random().unwrap(),
            address: address("API_KEY"),
            value: None,
        },
        Some(b"new value"),
        2,
        admin.clone(),
        admin.clone(),
        &registry,
        4,
    )
    .unwrap();
    assert!(!new.keys.contains_key(&client_id));
    state.append_secret(&new).unwrap();
    assert!(new.decrypt(&state, &client, &client_id).is_err());
    assert_eq!(
        &*new.decrypt(&state, &admin_keys, &admin).unwrap().unwrap(),
        b"new value"
    );
    // Revocation cannot make already-disclosed plaintext disappear.
    assert_eq!(
        &*grant.decrypt(&state, &client, &client_id).unwrap().unwrap(),
        b"old value"
    );
}

#[test]
fn scopes_and_administrative_categories_do_not_share_implicit_access() {
    let scoped = Permissions {
        deployments: ["shop/production".parse().unwrap()].into(),
        ..Default::default()
    };
    assert!(address("API_KEY").allowed(&scoped));
    let mut other = address("API_KEY");
    other.scope = "shop/staging".parse().unwrap();
    assert!(!other.allowed(&scoped));
    for kind in [
        SecretKind::Credential,
        SecretKind::BackupKey,
        SecretKind::Storage,
        SecretKind::BackupActive,
    ] {
        other.kind = kind;
        other.scope = "shop/production".parse().unwrap();
        assert!(!other.allowed(&scoped));
    }
    assert!(!address("API_KEY").allowed(&Permissions {
        network_admin: true,
        ..Default::default()
    }));
}

#[test]
fn ciphertext_is_bound_to_network_scope_name_and_operation() {
    let (registry, keys, admin) = bootstrap();
    let mut state = SharedSnapshot::empty(registry.network.id.clone());
    let original = record(&registry, &admin, "API_KEY", 1, Some(b"private"));
    state.append_secret(&original).unwrap();
    for variant in 0..4 {
        let mut changed = original.clone();
        let mut changed_state = state.clone();
        match variant {
            0 => changed.address.name = "OTHER_KEY".into(),
            1 => changed.address.scope = "shop/staging".parse().unwrap(),
            2 => changed.id = RequestId::random().unwrap(),
            _ => changed_state.network = NetworkId::random().unwrap(),
        }
        assert!(changed.decrypt(&changed_state, &keys, &admin).is_err());
    }
}

#[test]
fn overwrites_do_not_consume_lifetime_capacity_and_backup_key_ids_stay_distinct() {
    let (registry, keys, admin) = bootstrap();
    let mut state = SharedSnapshot::empty(registry.network.id.clone());
    let mut deletion = record(&registry, &admin, "TOKEN", 1, None);
    for revision in 1..=MAX_SHARED_RECORDS as u64 + 1 {
        deletion.id = RequestId::random().unwrap();
        deletion.revision = revision;
        state.append_secret(&deletion).unwrap();
    }
    assert_eq!(state.secrets.len(), 1);
    assert_eq!(state.secrets[0].revision, MAX_SHARED_RECORDS as u64 + 1);
    for name in ["backup-key-one", "backup-key-two"] {
        let mutation = SecretMutation {
            id: RequestId::random().unwrap(),
            observed_revision: 0,
            expires_at: 86400,
            address: SecretAddress {
                kind: SecretKind::BackupKey,
                ..address(name)
            },
            value: None,
        };
        let value = SecretRecord::encrypt(
            &mutation,
            Some(b"backup-key"),
            1,
            admin.clone(),
            admin.clone(),
            &registry,
            2,
        )
        .unwrap();
        state.append_secret(&value).unwrap();
    }
    assert_eq!(state.secrets.len(), 3);
    for value in state
        .secrets
        .iter()
        .filter(|value| value.address.kind == SecretKind::BackupKey)
    {
        assert_eq!(
            &*value.decrypt(&state, &keys, &admin).unwrap().unwrap(),
            b"backup-key"
        );
    }
}

#[test]
fn concurrent_recipient_wraps_merge_and_legacy_grants_migrate_only_for_current_values() {
    let (mut registry, keys, admin) = bootstrap();
    let old = record(&registry, &admin, "TOKEN", 1, Some(b"obsolete"));
    let current = record(&registry, &admin, "TOKEN", 2, Some(b"current"));
    let mut initial = SharedSnapshot::empty(registry.network.id.clone());
    initial.append_secret(&current).unwrap();
    let client = LocalDeviceKeys::generate().unwrap();
    let recipient = admit(
        &mut registry,
        &client,
        Some(admin.clone()),
        Permissions {
            deployments: ["shop/production".parse().unwrap()].into(),
            ..Default::default()
        },
    );
    let members = registry.effective_members(2).unwrap();
    let a = current
        .grant(&initial, &keys, &admin, members[&recipient])
        .unwrap();
    let b = current
        .grant(&initial, &keys, &admin, members[&recipient])
        .unwrap();
    let mut left = initial.clone();
    left.append_secret(&a).unwrap();
    left.append_secret(&b).unwrap();
    let mut right = initial.clone();
    right.append_secret(&b).unwrap();
    right.append_secret(&a).unwrap();
    assert_eq!(left, right);
    assert_eq!(
        &*left.secrets[0]
            .decrypt(&left, &client, &recipient)
            .unwrap()
            .unwrap(),
        b"current"
    );
    let legacy = serde_json::json!({
        "network": registry.network.id, "secrets": [old, current],
        "grants": [{"record": a.id, "recipient": recipient, "issuer": admin, "key": a.keys[&recipient]}]
    });
    let migrated = SharedSnapshot::from_persisted(&serde_json::to_vec(&legacy).unwrap()).unwrap();
    assert_eq!(migrated.secrets.len(), 1);
    assert_eq!(
        &*migrated.secrets[0]
            .decrypt(&migrated, &client, &recipient)
            .unwrap()
            .unwrap(),
        b"current"
    );
    assert!(
        !serde_json::to_value(&migrated)
            .unwrap()
            .as_object()
            .unwrap()
            .contains_key("grants")
    );
}

#[test]
fn legacy_grants_can_fill_the_shared_budget_without_losing_recipient_keys() {
    let (registry, admin_keys, admin) = bootstrap();
    let current = record(&registry, &admin, "TOKEN", 1, Some(b"retained"));
    let initial = SharedSnapshot {
        network: registry.network.id.clone(),
        secrets: vec![current.clone()],
    };
    let content_key = current.content_key(&initial, &admin_keys, &admin).unwrap();
    let mut recipients = Vec::new();
    let mut grants = Vec::new();
    for _ in 0..800 {
        let keys = LocalDeviceKeys::generate().unwrap();
        let id = keys
            .device_id(&registry.network.id, DeviceKind::Laptop)
            .unwrap();
        let key = SealedValue::seal(
            &keys.encryption_public_key().unwrap(),
            content_key.as_slice(),
            &current.key_context(&registry.network.id, &id).unwrap(),
        )
        .unwrap();
        grants.push(serde_json::json!({
            "record": current.id, "recipient": id, "issuer": admin, "key": key,
        }));
        recipients.push((id, keys));
    }
    let legacy = serde_json::to_vec(&serde_json::json!({
        "network": registry.network.id, "secrets": [current], "grants": grants,
    }))
    .unwrap();
    assert!(legacy.len() < MAX_SHARED_BYTES);
    let migrated = SharedSnapshot::from_persisted(&legacy).unwrap();
    assert_eq!(migrated.secrets.len(), 1);
    assert_eq!(migrated.secrets[0].keys.len(), 801);
    assert!(serde_json::to_vec(&migrated.secrets[0]).unwrap().len() > 128 * 1024);
    recipients.push((admin, admin_keys));
    for (id, keys) in &recipients {
        assert_eq!(
            &*migrated.secrets[0]
                .decrypt(&migrated, keys, id)
                .unwrap()
                .unwrap(),
            b"retained"
        );
    }
    let persisted = serde_json::to_vec(&migrated).unwrap();
    assert_eq!(
        SharedSnapshot::from_persisted(&persisted).unwrap(),
        migrated
    );
}

#[test]
fn folded_keys_remain_bounded_by_record_and_snapshot_capacity() {
    let (registry, keys, admin) = bootstrap();
    let mut value = record(&registry, &admin, "TOKEN", 1, Some(b"retained"));
    let large_wrap = SealedValue::seal(
        &keys.encryption_public_key().unwrap(),
        &vec![0; crate::sealed::MAX_SECRET_BYTES],
        b"capacity fixture",
    )
    .unwrap();
    for i in 0..100 {
        value
            .keys
            .insert(format!("{i:064x}").parse().unwrap(), large_wrap.clone());
    }
    value.validate().unwrap();
    let mut other = value.clone();
    other.id = RequestId::random().unwrap();
    other.address.name = "OTHER".into();
    let mut state = SharedSnapshot {
        network: registry.network.id.clone(),
        secrets: vec![value.clone()],
    };
    let before = state.clone();
    assert!(matches!(
        state.append_secret(&other),
        Err(RuntimeError::Capacity("shared secret state"))
    ));
    assert_eq!(state, before);
    for i in 100..200 {
        value
            .keys
            .insert(format!("{i:064x}").parse().unwrap(), large_wrap.clone());
    }
    assert!(matches!(
        value.validate(),
        Err(RuntimeError::Capacity("secret record"))
    ));
}
