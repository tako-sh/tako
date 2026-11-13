use super::*;
use tako_network::{
    Approval, DeviceKind, LocalDeviceKeys, NetworkDescriptor, Permissions, ServerInfo, WgChallenge,
};

#[test]
fn invocations_require_server_attestation_and_bind_target_body_and_nonce() {
    use crate::native_invocation::{ReplayGuard, SignedInvocation};
    use tako_core::network::{NetworkInvocation, NetworkRequest};
    let (mut registry, keys, mut directory) = fixture();
    let laptop = LocalDeviceKeys::generate().unwrap();
    let request = laptop
        .enrollment_request(None, DeviceKind::Laptop, "laptop".parse().unwrap())
        .unwrap();
    let (challenge, pending) =
        WgChallenge::begin(&request, registry.network.id.clone(), 1, 60).unwrap();
    let verified = pending
        .verify(
            &laptop.answer_challenge(&challenge, &request, 1).unwrap(),
            1,
        )
        .unwrap();
    let member = registry
        .approve(
            verified,
            Approval {
                kind: MemberKind::Laptop,
                permissions: Permissions::default(),
                approved_by: Some(registry.network.initial_admin.clone()),
                independent_admin: false,
                created_at: 1,
                expires_at: None,
            },
        )
        .unwrap();
    registry.members.push(member);
    let services = lease(&registry, &keys, &["api/prod"], 10);
    directory.replace(&registry, &[services], 10).unwrap();
    let network = registry.network.id.clone();
    let server = keys.device_id(&network, DeviceKind::Server).unwrap();
    let target: AppEnv = "api/prod".parse().unwrap();
    let invocation = NetworkInvocation {
        name: "lookup".into(),
        request: NetworkRequest::Call {
            input: serde_json::json!({"id":"one"}),
        },
    };
    let call = SignedInvocation::sign(
        &keys,
        network.clone(),
        server.clone(),
        "caller/prod".parse().unwrap(),
        target.clone(),
        invocation.clone(),
        10,
    )
    .unwrap();
    let peer = IpAddr::V6(network.address(&keys.wg_public_key().unwrap()));
    let key = directory.invocation_key(&call, &target, peer, 10).unwrap();
    assert!(call.verify(&key, 10).is_ok());
    let mut replay = ReplayGuard::default();
    assert!(replay.accept(&call));
    assert!(!replay.accept(&call));
    assert!(
        directory
            .invocation_key(&call, &"other/prod".parse().unwrap(), peer, 10)
            .is_none()
    );
    let laptop_peer = IpAddr::V6(network.address(&laptop.wg_public_key().unwrap()));
    // Laptop membership permits ordinary private HTTP, but never app invocation.
    assert!(directory.admits(peer, laptop_peer, 10));
    assert!(
        directory
            .invocation_key(&call, &target, laptop_peer, 10)
            .is_none()
    );
    let forged = SignedInvocation::sign(
        &laptop,
        network.clone(),
        server,
        "caller/prod".parse().unwrap(),
        target.clone(),
        invocation,
        10,
    )
    .unwrap();
    assert!(
        directory
            .invocation_key(&forged, &target, laptop_peer, 10)
            .is_none()
    );
    let mut changed = call.clone();
    changed.invocation.name = "delete".into();
    assert!(changed.verify(&key, 10).is_err());
    let mut changed = call.clone();
    changed.target = "api/staging".parse().unwrap();
    assert!(changed.verify(&key, 10).is_err());
    let mut changed = call.clone();
    changed.caller = "unknown/prod".parse().unwrap();
    assert!(changed.verify(&key, 10).is_err());
    assert!(call.verify(&key, 41).is_err());
}

pub(super) fn fixture() -> (RegistrySnapshot, LocalDeviceKeys, ServiceDirectory) {
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
            id: network.clone(),
            initial_admin: id.clone(),
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
                permissions: Permissions {
                    network_admin: true,
                    server_admin: true,
                    ..Default::default()
                },
                approved_by: None,
                independent_admin: false,
                created_at: 1,
                expires_at: None,
            },
        )
        .unwrap();
    registry.members.push(member);
    let gateway = ComposeGateway::new(
        "10.70.80.1".parse().unwrap(),
        "10.70.80.0/24".parse().unwrap(),
    )
    .unwrap();
    let directory = ServiceDirectory::new(network, id, Some(gateway));
    (registry, keys, directory)
}

pub(super) fn lease(
    registry: &RegistrySnapshot,
    keys: &LocalDeviceKeys,
    apps: &[&str],
    now: u64,
) -> ServiceLease {
    ServiceLease::sign(
        keys,
        registry.network.id.clone(),
        apps.iter().map(|s| s.parse().unwrap()).collect(),
        now,
    )
    .unwrap()
}

#[test]
fn private_host_requires_actual_destination_and_peer_even_for_local_app() {
    let (registry, keys, mut directory) = fixture();
    let lease = lease(&registry, &keys, &["api/prod"], 10);
    directory.replace(&registry, &[lease], 10).unwrap();
    let wg = IpAddr::V6(registry.network.id.address(&keys.wg_public_key().unwrap()));
    let host = format!("api.prod.{}.tako.internal", registry.network.id);
    assert_eq!(
        directory.route(&host, wg, wg, 10),
        PrivateRoute::Local("api/prod".parse().unwrap())
    );
    for (destination, peer) in [
        ("192.0.2.1".parse().unwrap(), wg),
        (wg, "192.0.2.1".parse().unwrap()),
        (wg, "127.0.0.1".parse().unwrap()),
        (wg, "fd00::99".parse().unwrap()),
        ("10.70.80.1".parse().unwrap(), "10.71.80.2".parse().unwrap()),
        ("10.70.80.2".parse().unwrap(), "10.70.80.3".parse().unwrap()),
    ] {
        assert_eq!(
            directory.route(&host, destination, peer, 10),
            PrivateRoute::Forbidden
        );
    }
    assert_eq!(
        directory.route(
            &host,
            "10.70.80.1".parse().unwrap(),
            "10.70.80.2".parse().unwrap(),
            10
        ),
        PrivateRoute::Local("api/prod".parse().unwrap())
    );
    assert_eq!(
        directory.route("unknown.tako.internal", wg, wg, 10),
        PrivateRoute::Forbidden
    );
    assert_eq!(
        directory.route("example.com", wg, wg, 10),
        PrivateRoute::Public
    );
}

#[test]
fn withdrawal_stale_replay_expiry_and_revocation_remove_placements() {
    let (mut registry, keys, mut directory) = fixture();
    let app = "api/prod".parse().unwrap();
    let old = lease(&registry, &keys, &["api/prod"], 10);
    directory.replace(&registry, &[old.clone()], 10).unwrap();
    assert_eq!(directory.placements(&app, 10).len(), 1);
    let withdrawn = lease(&registry, &keys, &[], 11);
    directory.replace(&registry, &[withdrawn], 11).unwrap();
    assert!(directory.placements(&app, 11).is_empty());
    directory.replace(&registry, &[old.clone()], 12).unwrap();
    assert!(directory.placements(&app, 12).is_empty());
    let fresh = lease(&registry, &keys, &["api/prod"], 13);
    directory.replace(&registry, &[fresh.clone()], 13).unwrap();
    assert!(directory.placements(&app, 43).is_empty());
    registry.members[0].expires_at = Some(20);
    directory.replace(&registry, &[fresh.clone()], 14).unwrap();
    assert!(directory.placements(&app, 20).is_empty());
    registry.members[0].expires_at = None;
    let id = registry.members[0].device_id.clone();
    registry.revocations.push(
        registry
            .plan_revocation(tako_network::RevocationId::random().unwrap(), &id, &id, 15)
            .unwrap(),
    );
    assert!(directory.replace(&registry, &[fresh], 15).is_err());
    assert!(directory.placements(&app, 15).is_empty());
}

#[test]
fn fast_publisher_clock_places_at_once_without_outliving_the_signed_lifetime() {
    use tako_network::services::CLOCK_SKEW_SECONDS;
    let (registry, keys, mut directory) = fixture();
    let app = "api/prod".parse().unwrap();
    let server = registry.members[0].device_id.clone();
    // The publisher's clock runs ahead of this verifier's by the full tolerance.
    let ahead = lease(&registry, &keys, &["api/prod"], 10 + CLOCK_SKEW_SECONDS);
    directory.replace(&registry, &[ahead.clone()], 10).unwrap();
    let placements = directory.placements(&app, 10);
    assert_eq!(placements.len(), 1);
    assert!(placements[0].expires_at <= 10 + MAX_LEASE_SECONDS);
    // The verifier honors the lease for its signed lifetime, not for that plus
    // the skew, and re-reading the same lease never moves the deadline later.
    let limit = Instant::now() + Duration::from_secs(MAX_LEASE_SECONDS);
    let first = directory.latest[&server].1;
    assert!(first <= limit);
    directory.replace(&registry, &[ahead.clone()], 12).unwrap();
    assert_eq!(directory.latest[&server].1, first);
    assert_eq!(directory.placements(&app, 12).len(), 1);

    // Ordering stays by the publisher's own timestamps: an older lease cannot
    // replace the early one, and the next renewal can.
    let older = lease(&registry, &keys, &["api/prod"], 12);
    directory.replace(&registry, &[older], 13).unwrap();
    assert!(directory.placements(&app, 13).is_empty());
    let renewed = lease(&registry, &keys, &[], 25);
    directory.replace(&registry, &[renewed], 20).unwrap();
    assert!(directory.placements(&app, 20).is_empty());
    assert_eq!(directory.latest[&server].0.issued_at, 25);

    // Beyond the tolerance the lease is ignored until the clocks agree.
    let (registry, keys, mut directory) = fixture();
    let far = lease(&registry, &keys, &["api/prod"], 11 + CLOCK_SKEW_SECONDS);
    directory.replace(&registry, &[far.clone()], 10).unwrap();
    assert!(directory.placements(&app, 10).is_empty());
    assert!(directory.latest.is_empty());
    directory.replace(&registry, &[far], 11).unwrap();
    assert_eq!(directory.placements(&app, 11).len(), 1);
}

#[test]
fn monotonic_deadlines_survive_wall_clock_rollback_and_file_rereads() {
    let (registry, keys, mut directory) = fixture();
    let app = "api/prod".parse().unwrap();
    let lease = lease(&registry, &keys, &["api/prod"], 10);
    let mut snapshot = ServiceSnapshot {
        observed_at: 10,
        registry,
        leases: vec![lease],
    };
    directory.replace_snapshot(&snapshot, 12).unwrap();
    let deadline = directory.envelope.unwrap().1;
    assert!(directory.replace_snapshot(&snapshot, 11).is_err());
    assert!(directory.envelope.unwrap().1 <= deadline);
    directory.latest.values_mut().next().unwrap().1 = Instant::now() - Duration::from_secs(1);
    snapshot.observed_at = 13;
    directory.replace_snapshot(&snapshot, 13).unwrap();
    assert!(directory.placements(&app, 13).is_empty());
    snapshot.observed_at = 12;
    assert!(directory.replace_snapshot(&snapshot, 13).is_err());
    assert!(directory.placements(&app, 13).is_empty());
}

#[test]
fn compose_may_relay_but_wireguard_requests_cannot_loop() {
    let (mut registry, keys, mut directory) = fixture();
    let remote_keys = LocalDeviceKeys::generate().unwrap();
    let request = remote_keys
        .enrollment_request(None, DeviceKind::Server, "remote".parse().unwrap())
        .unwrap();
    let (challenge, pending) =
        WgChallenge::begin(&request, registry.network.id.clone(), 2, 60).unwrap();
    let verified = pending
        .verify(
            &remote_keys
                .answer_challenge(&challenge, &request, 2)
                .unwrap(),
            2,
        )
        .unwrap();
    let mut member = registry.members[0].clone();
    let approval = Approval {
        kind: member.kind.clone(),
        permissions: member.permissions.clone(),
        approved_by: Some(member.device_id.clone()),
        independent_admin: false,
        created_at: 2,
        expires_at: None,
    };
    member = registry.approve(verified, approval).unwrap();
    registry.members.push(member);
    let remote = lease(&registry, &remote_keys, &["api/prod"], 10);
    directory.replace(&registry, &[remote], 10).unwrap();
    let host = format!("api.prod.{}.tako.internal", registry.network.id);
    let local_ip = IpAddr::V6(registry.network.id.address(&keys.wg_public_key().unwrap()));
    let remote_ip = registry
        .network
        .id
        .address(&remote_keys.wg_public_key().unwrap());
    assert_eq!(
        directory.route(&host, local_ip, IpAddr::V6(remote_ip), 10),
        PrivateRoute::Unavailable
    );
    assert_eq!(
        directory.route(
            &host,
            "10.70.80.1".parse().unwrap(),
            "10.70.80.2".parse().unwrap(),
            10
        ),
        PrivateRoute::Remote {
            app: "api/prod".parse().unwrap(),
            address: remote_ip
        }
    );
}

#[test]
fn local_export_excludes_stale_or_future_heartbeats() {
    let export = LocalServices {
        observed_at: 10,
        apps: BTreeSet::new(),
    };
    assert!(export.validate(10).is_ok());
    assert!(export.validate(9).is_err());
    assert!(export.validate(25).is_err());
    let serialized = serde_json::to_value(export).unwrap();
    assert_eq!(serialized, serde_json::json!({"observed_at":10,"apps":[]}));
}
