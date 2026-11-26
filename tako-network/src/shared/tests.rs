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
    admit(&mut registry, &keys, None, Permissions::Full);
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
                independent_root: false,
                created_at: 1,
                expires_at: None,
                client: Some({
                    static NEXT: std::sync::atomic::AtomicU64 =
                        std::sync::atomic::AtomicU64::new(1);
                    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    format!("client-{n}").parse().unwrap()
                }),
            },
        )
        .unwrap();
    let id = member.device_id.clone();
    registry.members.push(member);
    id
}

fn address(name: &str) -> RecordAddress {
    RecordAddress {
        scope: "shop/production".parse().unwrap(),
        kind: RecordKind::App,
        name: name.into(),
    }
}

fn record(
    registry: &RegistrySnapshot,
    admin: &DeviceId,
    name: &str,
    revision: u64,
    value: Option<&[u8]>,
) -> SharedRecord {
    SharedRecord::create(
        &RecordMutation {
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
    left.append_record(&record(&registry, &admin, "API_KEY", 1, Some(b"first")))
        .unwrap();
    let stale = left.clone();
    let mut right = stale.clone();
    right
        .append_record(&record(
            &registry,
            &admin,
            "DATABASE_URL",
            1,
            Some(b"database"),
        ))
        .unwrap();
    left.append_record(&record(&registry, &admin, "API_KEY", 2, None))
        .unwrap();
    let left_before = left.clone();
    left.merge(&right).unwrap();
    right.merge(&left_before).unwrap();
    left.merge(&stale).unwrap();
    assert_eq!(left.latest().unwrap(), right.latest().unwrap());
    let latest = left.latest().unwrap();
    assert!(latest[&address("API_KEY")].is_deletion());
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
            state.append_record(value).unwrap();
        }
        state.append_record(&a).unwrap();
        assert_eq!(state.records.len(), 1);
        assert_eq!(state.latest().unwrap()[&address("API_KEY")], winner);
        let mut conflict = winner.clone();
        conflict.revision += 1;
        assert!(state.append_record(&conflict).is_err());
        assert_eq!(state.records.len(), 1);
    }
}

#[test]
fn new_members_receive_key_grants_and_revocation_excludes_future_values() {
    let (mut registry, admin_keys, admin) = bootstrap();
    let mut state = SharedSnapshot::empty(registry.network.id.clone());
    let old = record(&registry, &admin, "API_KEY", 1, Some(b"old value"));
    state.append_record(&old).unwrap();
    let client = LocalDeviceKeys::generate().unwrap();
    let client_id = admit(
        &mut registry,
        &client,
        Some(admin.clone()),
        Permissions::Full,
    );
    assert!(old.open(&state, &client, &client_id).is_err());
    let effective = registry.effective_members(2).unwrap();
    let grant = old
        .grant(&state, &admin_keys, &admin, effective[&client_id])
        .unwrap();
    state.append_record(&grant).unwrap();
    assert_eq!(
        &*grant.open(&state, &client, &client_id).unwrap().unwrap(),
        b"old value"
    );
    assert_eq!(state.latest().unwrap()[&address("API_KEY")].revision, 1);
    let denial = registry
        .plan_revocation(RevocationId::random().unwrap(), &admin, &client_id, 3)
        .unwrap();
    registry.revocations.push(denial);
    let new = SharedRecord::create(
        &RecordMutation {
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
    state.append_record(&new).unwrap();
    assert!(new.open(&state, &client, &client_id).is_err());
    assert_eq!(
        &*new.open(&state, &admin_keys, &admin).unwrap().unwrap(),
        b"new value"
    );
    // Revocation cannot make already-disclosed plaintext disappear.
    assert_eq!(
        &*grant.open(&state, &client, &client_id).unwrap().unwrap(),
        b"old value"
    );
}

#[test]
fn deploy_clients_read_and_write_no_values() {
    let scoped = Permissions::Deploy(["shop/production".parse().unwrap()].into());
    // A deploy client cannot read or change its own environment's values.
    assert!(!address("API_KEY").readable(&scoped));
    assert!(!address("API_KEY").writable(&scoped));
    assert!(address("API_KEY").readable(&Permissions::Full));
    let mut other = address("API_KEY");
    other.scope = "shop/staging".parse().unwrap();
    assert!(!other.readable(&scoped));
    for kind in [
        RecordKind::Credential,
        RecordKind::Storage,
        RecordKind::DatabaseKey,
    ] {
        other.kind = kind;
        other.scope = "shop/production".parse().unwrap();
        assert!(!other.readable(&scoped));
        assert!(!other.writable(&scoped));
    }
    assert!(!address("API_KEY").readable(&Permissions::Deploy(Default::default())));
}

#[test]
fn deployers_read_database_records_but_not_their_keys() {
    let scoped = Permissions::Deploy(["shop/production".parse().unwrap()].into());
    let admin = Permissions::Full;
    let record = |kind| RecordAddress {
        scope: DATABASE_SCOPE.parse().unwrap(),
        kind,
        name: "shop".into(),
    };
    let directory = record(RecordKind::Database);
    assert!(directory.readable(&scoped));
    assert!(!directory.writable(&scoped), "only administrators publish");
    assert!(
        !directory.readable(&Permissions::Deploy(Default::default())),
        "members that cannot deploy do not need them"
    );
    let key = record(RecordKind::DatabaseKey);
    assert!(!key.readable(&scoped));
    assert!(key.readable(&admin) && key.writable(&admin));
    // A database record outside the database scope grants nothing.
    let mut misplaced = directory.clone();
    misplaced.scope = "shop/production".parse().unwrap();
    assert!(!misplaced.readable(&scoped));
}

#[test]
fn ciphertext_is_bound_to_network_scope_name_and_operation() {
    let (registry, keys, admin) = bootstrap();
    let mut state = SharedSnapshot::empty(registry.network.id.clone());
    let original = record(&registry, &admin, "API_KEY", 1, Some(b"private"));
    state.append_record(&original).unwrap();
    for variant in 0..4 {
        let mut changed = original.clone();
        let mut changed_state = state.clone();
        match variant {
            0 => changed.address.name = "OTHER_KEY".into(),
            1 => changed.address.scope = "shop/staging".parse().unwrap(),
            2 => changed.id = RequestId::random().unwrap(),
            _ => changed_state.network = NetworkId::random().unwrap(),
        }
        assert!(changed.open(&changed_state, &keys, &admin).is_err());
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
        state.append_record(&deletion).unwrap();
    }
    assert_eq!(state.records.len(), 1);
    assert_eq!(state.records[0].revision, MAX_SHARED_RECORDS as u64 + 1);
    for name in ["backup-key-one", "backup-key-two"] {
        let mutation = RecordMutation {
            id: RequestId::random().unwrap(),
            observed_revision: 0,
            expires_at: 86400,
            address: RecordAddress {
                kind: RecordKind::DatabaseKey,
                ..address(name)
            },
            value: None,
        };
        let value = SharedRecord::create(
            &mutation,
            Some(b"backup-key"),
            1,
            admin.clone(),
            admin.clone(),
            &registry,
            2,
        )
        .unwrap();
        state.append_record(&value).unwrap();
    }
    assert_eq!(state.records.len(), 3);
    for value in state
        .records
        .iter()
        .filter(|value| value.address.kind == RecordKind::DatabaseKey)
    {
        assert_eq!(
            &*value.open(&state, &keys, &admin).unwrap().unwrap(),
            b"backup-key"
        );
    }
}

#[test]
fn concurrent_recipient_wraps_merge() {
    let (mut registry, keys, admin) = bootstrap();
    let current = record(&registry, &admin, "TOKEN", 2, Some(b"current"));
    let mut initial = SharedSnapshot::empty(registry.network.id.clone());
    initial.append_record(&current).unwrap();
    let client = LocalDeviceKeys::generate().unwrap();
    let recipient = admit(
        &mut registry,
        &client,
        Some(admin.clone()),
        Permissions::Full,
    );
    let members = registry.effective_members(2).unwrap();
    let a = current
        .grant(&initial, &keys, &admin, members[&recipient])
        .unwrap();
    let b = current
        .grant(&initial, &keys, &admin, members[&recipient])
        .unwrap();
    let mut left = initial.clone();
    left.append_record(&a).unwrap();
    left.append_record(&b).unwrap();
    let mut right = initial.clone();
    right.append_record(&b).unwrap();
    right.append_record(&a).unwrap();
    assert_eq!(left, right);
    assert_eq!(
        &*left.records[0]
            .open(&left, &client, &recipient)
            .unwrap()
            .unwrap(),
        b"current"
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
        records: vec![value.clone()],
    };
    let before = state.clone();
    assert!(matches!(
        state.append_record(&other),
        Err(RuntimeError::Capacity("shared state"))
    ));
    assert_eq!(state, before);
    for i in 100..200 {
        value
            .keys
            .insert(format!("{i:064x}").parse().unwrap(), large_wrap.clone());
    }
    assert!(matches!(
        value.validate(),
        Err(RuntimeError::Capacity("shared record"))
    ));
}

fn directory_entry(
    registry: &RegistrySnapshot,
    admin: &DeviceId,
    revision: u64,
    value: Option<&str>,
) -> SharedRecord {
    let content = value.map(|value| {
        serde_json::to_vec(&RecordContent {
            value: Zeroizing::new(value.into()),
            expires_on: None,
        })
        .unwrap()
    });
    SharedRecord::create(
        &RecordMutation {
            observed_revision: 0,
            expires_at: 86400,
            id: RequestId::random().unwrap(),
            address: RecordAddress {
                scope: "shop/production".parse().unwrap(),
                kind: RecordKind::Database,
                name: "orders".into(),
            },
            value: None,
        },
        content.as_deref(),
        revision,
        admin.clone(),
        admin.clone(),
        registry,
        2,
    )
    .unwrap()
}

#[test]
fn records_that_are_not_secret_are_plaintext_and_readable_without_keys() {
    let (registry, _, admin) = bootstrap();
    let record = directory_entry(
        &registry,
        &admin,
        1,
        Some("live.0123456789abcdef0123456789abcdef"),
    );
    assert!(record.value.is_none());
    assert!(record.keys.is_empty());
    assert_eq!(
        record.plaintext.as_ref().unwrap().value,
        "live.0123456789abcdef0123456789abcdef"
    );
    let mut state = SharedSnapshot::empty(registry.network.id.clone());
    state.append_record(&record).unwrap();
    // Any device can read it; a sealed record needs that device's key.
    let outsider = LocalDeviceKeys::generate().unwrap();
    let outsider_id = outsider
        .device_id(&registry.network.id, DeviceKind::Laptop)
        .unwrap();
    let bytes = record
        .open(&state, &outsider, &outsider_id)
        .unwrap()
        .unwrap();
    let content: RecordContent = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(&*content.value, "live.0123456789abcdef0123456789abcdef");
    let deletion = directory_entry(&registry, &admin, 2, None);
    assert!(deletion.is_deletion());
    assert!(!record.is_deletion());
    state.append_record(&deletion).unwrap();
    assert!(state.records[0].is_deletion());
}

#[test]
fn plaintext_is_refused_for_secrets_and_directory_entries_have_no_keys() {
    let (registry, _, admin) = bootstrap();
    let mut secret = record(&registry, &admin, "API_KEY", 1, None);
    secret.plaintext = Some(PlainContent {
        value: "visible".into(),
        expires_on: None,
    });
    assert!(secret.validate().is_err());
    let mut keyed = directory_entry(
        &registry,
        &admin,
        1,
        Some("live.0123456789abcdef0123456789abcdef"),
    );
    keyed.keys = record(&registry, &admin, "API_KEY", 1, Some(b"x")).keys;
    assert!(keyed.validate().is_err());
}

#[test]
fn sealed_records_serialize_without_a_plaintext_field() {
    let (registry, _, admin) = bootstrap();
    let sealed = record(&registry, &admin, "API_KEY", 1, Some(b"x"));
    let json: serde_json::Value = serde_json::to_value(&sealed).unwrap();
    assert!(json.get("plaintext").is_none());
    let deletion = record(&registry, &admin, "API_KEY", 2, None);
    assert!(
        serde_json::to_value(&deletion)
            .unwrap()
            .get("plaintext")
            .is_none()
    );
}
