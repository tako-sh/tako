use super::*;
use crate::{
    Approval, DeviceKind, MemberKind, NetworkDescriptor, Permissions, ServerInfo, WgChallenge,
};

pub(super) fn fixture() -> (RegistrySnapshot, LocalDeviceKeys) {
    let keys = LocalDeviceKeys::generate().unwrap();
    let network = NetworkId::random().unwrap();
    let id = keys.device_id(&network, DeviceKind::Server).unwrap();
    let request = keys
        .enrollment_request(None, DeviceKind::Server, "server".parse().unwrap())
        .unwrap();
    let (challenge, pending) = WgChallenge::begin(&request, network.clone(), 1, 60).unwrap();
    let verified = pending
        .verify(&keys.answer_challenge(&challenge, &request, 1).unwrap(), 1)
        .unwrap();
    let mut registry = RegistrySnapshot {
        network: NetworkDescriptor {
            name: "test".parse().unwrap(),
            id: network,
            initial_admin: id,
        },
        members: vec![],
        revocations: vec![],
        acks: vec![],
    };
    let member = registry
        .approve(
            verified,
            Approval {
                kind: MemberKind::Server(ServerInfo {
                    wireguard_endpoint: "192.0.2.1:51820".parse().unwrap(),
                    ssh_host_key: keys.ssh_public_key().unwrap(),
                    ssh_port: 22,
                    https_certificate_pem: None,
                    corrosion_ca_certificate_pem: None,
                    https_spki_sha256: None,
                    https_port: 443,
                }),
                permissions: Permissions::Full,
                approved_by: None,
                independent_root: false,
                created_at: 1,
                expires_at: None,
            },
        )
        .unwrap();
    registry.members.push(member);
    (registry, keys)
}

#[test]
fn lease_round_trip_binds_entire_placement_and_lifetime() {
    let (registry, keys) = fixture();
    let lease = ServiceLease::sign(
        &keys,
        registry.network.id.clone(),
        ["api/prod".parse().unwrap()].into(),
        10,
    )
    .unwrap();
    let decoded = ServiceLease::from_slice(&serde_json::to_vec(&lease).unwrap()).unwrap();
    assert_eq!(
        decoded.verify(&registry, 10).unwrap(),
        registry.network.id.address(&keys.wg_public_key().unwrap())
    );
    assert!(lease.verify(&registry, 4).is_err());
    assert!(lease.verify(&registry, 40).is_err());
    for field in ["apps", "network_id", "server_id", "issued_at", "expires_at"] {
        let mut tampered = serde_json::to_value(&lease).unwrap();
        tampered[field] = match field {
            "apps" => serde_json::json!(["admin/prod"]),
            "network_id" => serde_json::json!(NetworkId::random().unwrap()),
            "server_id" => serde_json::json!("00".repeat(32)),
            "issued_at" => serde_json::json!(9),
            _ => serde_json::json!(39),
        };
        let tampered: ServiceLease = serde_json::from_value(tampered).unwrap();
        assert!(tampered.verify(&registry, 10).is_err(), "{field}");
    }
}

#[test]
fn lease_tolerates_a_publisher_clock_slightly_ahead_of_the_verifier() {
    let (registry, keys) = fixture();
    let lease = ServiceLease::sign(
        &keys,
        registry.network.id.clone(),
        ["api/prod".parse().unwrap()].into(),
        100,
    )
    .unwrap();
    assert_eq!(lease.expires_at, 100 + MAX_LEASE_SECONDS);
    for ahead in 1..=CLOCK_SKEW_SECONDS {
        assert!(lease.verify(&registry, 100 - ahead).is_ok(), "{ahead}");
    }
    assert!(
        lease
            .verify(&registry, 100 - CLOCK_SKEW_SECONDS - 1)
            .is_err()
    );
    // Expiry gets no tolerance: a slow publisher clock never outlives its lease.
    assert!(lease.verify(&registry, 129).is_ok());
    assert!(lease.verify(&registry, 130).is_err());
    assert!(lease.verify(&registry, 130 + CLOCK_SKEW_SECONDS).is_err());
    // A verifier clock near zero cannot underflow the comparison.
    let early = ServiceLease::sign(&keys, registry.network.id.clone(), BTreeSet::new(), 3).unwrap();
    assert!(
        early
            .verify_member(&registry.network.id, &registry.members[0], 0)
            .is_ok()
    );
    let late = ServiceLease::sign(
        &keys,
        registry.network.id.clone(),
        BTreeSet::new(),
        u64::MAX - MAX_LEASE_SECONDS,
    )
    .unwrap();
    assert!(
        late.verify_member(&registry.network.id, &registry.members[0], u64::MAX - 1)
            .is_ok()
    );
}

#[test]
fn revoked_expired_and_unprivileged_publishers_are_not_placements() {
    let (mut registry, keys) = fixture();
    let lease =
        ServiceLease::sign(&keys, registry.network.id.clone(), BTreeSet::new(), 10).unwrap();
    registry.members[0].permissions = crate::Permissions::Deploy(Default::default());
    assert!(lease.verify(&registry, 10).is_err());
    registry.members[0].permissions = crate::Permissions::Full;
    registry.members[0].expires_at = Some(11);
    assert!(lease.verify(&registry, 11).is_err());
    registry.members[0].expires_at = None;
    let id = registry.members[0].device_id.clone();
    let member = registry.members[0].clone();
    registry.revocations.push(crate::Revocation {
        id: crate::RevocationId::random().unwrap(),
        target: id.clone(),
        wg_fingerprint: member.request.wg_public_key.fingerprint(),
        ssh_fingerprint: member.request.ssh_public_key.fingerprint().unwrap(),
        captured_servers: [id.clone()].into(),
        by: id,
        created_at: 10,
    });
    assert!(lease.verify(&registry, 10).is_err());
}

#[test]
fn lease_bounds_and_canonical_app_order_are_enforced() {
    let (registry, keys) = fixture();
    let lease = ServiceLease::sign(
        &keys,
        registry.network.id.clone(),
        ["a/prod".parse().unwrap(), "b/prod".parse().unwrap()].into(),
        10,
    )
    .unwrap();
    let mut unsorted = lease.clone();
    unsorted.apps.reverse();
    assert!(ServiceLease::from_slice(&serde_json::to_vec(&unsorted).unwrap()).is_err());
    let mut duplicate = lease.clone();
    duplicate.apps.push(duplicate.apps[1].clone());
    assert!(duplicate.verify(&registry, 10).is_err());
    let mut long = lease;
    long.expires_at = 41;
    assert!(long.verify(&registry, 10).is_err());
    assert!(ServiceLease::from_slice(&vec![b' '; MAX_LEASE_BYTES + 1]).is_err());
    assert!(ServiceLease::sign(&keys, registry.network.id, BTreeSet::new(), u64::MAX).is_err());
}

#[test]
fn private_hosts_never_fall_through_to_public_wildcards() {
    let app: AppEnv = "api/prod".parse().unwrap();
    let host = service_hostname(&app);
    assert_eq!(host, "api.prod.tako");
    for authority in [&host, &format!("{}.:80", host.to_uppercase())] {
        assert_eq!(classify_host(authority), ServiceHost::App(app.clone()));
    }
    for authority in ["edge-a.tako", "EDGE-A.tako.", "edge-a.tako:22"] {
        assert_eq!(
            classify_host(authority),
            ServiceHost::Server("edge-a".into()),
            "{authority}"
        );
    }
    for authority in [
        "tako".to_string(),
        "TAKO.".to_string(),
        "a.b.c.tako".to_string(),
        "-bad.tako".to_string(),
        "tako.internal".to_string(),
        "node-abc.0123.tako.internal".to_string(),
        format!("{host}:nonsense"),
        format!("{host}:65536"),
        format!("{host}:80:90"),
        format!("{host}:"),
    ] {
        assert_eq!(
            classify_host(&authority),
            ServiceHost::Reserved,
            "{authority}"
        );
    }
    for authority in [
        "example.com",
        "nottako",
        "app.nottako",
        "tako.example.com",
        "app.tako.test",
    ] {
        assert_eq!(classify_host(authority), ServiceHost::Public, "{authority}");
    }
}

#[test]
fn server_names_map_to_single_dns_labels() {
    assert_eq!(server_hostname("Edge-A").as_deref(), Some("edge-a.tako"));
    assert_eq!(server_hostname("edge_1").as_deref(), Some("edge_1.tako"));
    for name in ["edge.a", "", "-edge", &"a".repeat(64)] {
        assert_eq!(server_hostname(name), None, "{name}");
    }
}
