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
        assert_eq!(state.secrets.len(), 2);
        assert_eq!(state.latest().unwrap()[&address("API_KEY")], winner);
        let mut conflict = a.clone();
        conflict.revision += 1;
        assert!(state.append_secret(&conflict).is_err());
        assert_eq!(state.secrets.len(), 2);
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
    state.append_secret_grant(&grant).unwrap();
    assert_eq!(
        &*old.decrypt(&state, &client, &client_id).unwrap().unwrap(),
        b"old value"
    );
    assert_eq!(state.latest().unwrap()[&address("API_KEY")].revision, 1);
    let denial = registry
        .plan_revocation(RevocationId::random().unwrap(), &admin, &client_id, 3)
        .unwrap();
    registry.revocations.push(denial);
    let new = SecretRecord::encrypt(
        &SecretMutation {
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
        &*old.decrypt(&state, &client, &client_id).unwrap().unwrap(),
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
