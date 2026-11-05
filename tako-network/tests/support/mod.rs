use std::collections::BTreeSet;
use tako_network::*;

pub fn admin() -> Permissions {
    Permissions {
        network_admin: true,
        server_admin: true,
        ..Permissions::default()
    }
}

pub fn verified(
    keys: &LocalDeviceKeys,
    network: &NetworkId,
    kind: DeviceKind,
    now: u64,
) -> VerifiedEnrollment {
    let request = keys
        .enrollment_request(None, kind, "test-device".parse().unwrap())
        .unwrap();
    let (challenge, pending) =
        WgChallenge::begin(&request, network.clone(), now, now + 60).unwrap();
    let response = keys.answer_challenge(&challenge, &request, now).unwrap();
    pending.verify(&response, now).unwrap()
}

pub fn approval(
    kind: MemberKind,
    permissions: Permissions,
    by: Option<DeviceId>,
    now: u64,
) -> Approval {
    Approval {
        kind,
        permissions,
        approved_by: by,
        independent_admin: false,
        created_at: now,
        expires_at: None,
    }
}

pub fn bootstrap() -> (RegistrySnapshot, LocalDeviceKeys, DeviceId) {
    let keys = LocalDeviceKeys::generate().unwrap();
    let network = NetworkId::random().unwrap();
    let id = keys.device_id(&network, DeviceKind::Laptop).unwrap();
    let mut snapshot = RegistrySnapshot {
        network: NetworkDescriptor {
            id: network,
            initial_admin: id.clone(),
        },
        members: vec![],
        revocations: vec![],
        acks: vec![],
    };
    let root = snapshot
        .approve(
            verified(&keys, &snapshot.network.id, DeviceKind::Laptop, 1),
            approval(MemberKind::Laptop, admin(), None, 1),
        )
        .unwrap();
    snapshot.members.push(root);
    (snapshot, keys, id)
}

pub fn add_laptop(
    snapshot: &mut RegistrySnapshot,
    by: &DeviceId,
    permissions: Permissions,
    independent: bool,
    now: u64,
) -> (LocalDeviceKeys, DeviceId) {
    let keys = LocalDeviceKeys::generate().unwrap();
    let mut command = approval(MemberKind::Laptop, permissions, Some(by.clone()), now);
    command.independent_admin = independent;
    let evidence = verified(&keys, &snapshot.network.id, DeviceKind::Laptop, now);
    let member = if independent {
        command.approved_by = None;
        snapshot.approve_independent_admin(evidence, command)
    } else {
        snapshot.approve(evidence, command)
    }
    .unwrap();
    let id = member.device_id.clone();
    snapshot.members.push(member);
    (keys, id)
}

pub fn add_server(snapshot: &mut RegistrySnapshot, by: &DeviceId, now: u64) -> DeviceId {
    let keys = LocalDeviceKeys::generate().unwrap();
    let server = ServerInfo {
        wireguard_endpoint: "203.0.113.7:51820".parse().unwrap(),
        ssh_host_key: keys.ssh_public_key().unwrap(),
        ssh_port: 22,
        https_port: 443,
        https_spki_sha256: Some(KeyFingerprint::of_bytes(b"test tls pin")),
    };
    let member = snapshot
        .approve(
            verified(&keys, &snapshot.network.id, DeviceKind::Server, now),
            approval(
                MemberKind::Server(server),
                Permissions::default(),
                Some(by.clone()),
                now,
            ),
        )
        .unwrap();
    let id = member.device_id.clone();
    snapshot.members.push(member);
    id
}

pub fn revoke(
    snapshot: &mut RegistrySnapshot,
    by: &DeviceId,
    target: &DeviceId,
    now: u64,
) -> RevocationId {
    let revocation = snapshot
        .plan_revocation(RevocationId::random().unwrap(), by, target, now)
        .unwrap();
    let id = revocation.id.clone();
    snapshot.revocations.push(revocation);
    id
}

pub fn scopes(values: &[&str]) -> BTreeSet<AppEnv> {
    values.iter().map(|value| value.parse().unwrap()).collect()
}
