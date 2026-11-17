use std::collections::BTreeSet;
use tako_network::*;

pub fn admin() -> Permissions {
    Permissions::Full
}

pub fn verified(
    keys: &LocalDeviceKeys,
    network: &NetworkId,
    kind: DeviceKind,
    now: u64,
) -> VerifiedEnrollment {
    verified_named(keys, network, kind, "test-device", now)
}

pub fn verified_named(
    keys: &LocalDeviceKeys,
    network: &NetworkId,
    kind: DeviceKind,
    name: &str,
    now: u64,
) -> VerifiedEnrollment {
    let request = keys
        .enrollment_request(None, kind, name.parse().unwrap())
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
        independent_root: false,
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
            name: "test".parse().unwrap(),
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
    command.independent_root = independent;
    let evidence = verified(&keys, &snapshot.network.id, DeviceKind::Laptop, now);
    let member = if independent {
        command.approved_by = None;
        snapshot.approve_independent_root(evidence, command)
    } else {
        snapshot.approve(evidence, command)
    }
    .unwrap();
    let id = member.device_id.clone();
    snapshot.members.push(member);
    (keys, id)
}

pub fn add_server(snapshot: &mut RegistrySnapshot, by: &DeviceId, now: u64) -> DeviceId {
    let name = format!("server-{}", snapshot.members.len());
    add_named_server(snapshot, by, &name, now).unwrap()
}

pub fn server_info(keys: &LocalDeviceKeys) -> ServerInfo {
    ServerInfo {
        wireguard_endpoint: "203.0.113.7:51820".parse().unwrap(),
        ssh_host_key: keys.ssh_public_key().unwrap(),
        ssh_port: 22,
        https_port: 443,
        https_certificate_pem: Some(certificate_pem()),
        corrosion_ca_certificate_pem: Some(certificate_pem()),
        https_spki_sha256: None,
    }
}

pub fn add_named_server(
    snapshot: &mut RegistrySnapshot,
    by: &DeviceId,
    name: &str,
    now: u64,
) -> Result<DeviceId> {
    let keys = LocalDeviceKeys::generate().unwrap();
    let member = snapshot.approve(
        verified_named(&keys, &snapshot.network.id, DeviceKind::Server, name, now),
        approval(
            MemberKind::Server(server_info(&keys)),
            Permissions::Full,
            Some(by.clone()),
            now,
        ),
    )?;
    let id = member.device_id.clone();
    snapshot.members.push(member);
    Ok(id)
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

/// Record a denial directly, as a replica that accepted another device's
/// revocation would. `plan_revocation` refuses self-revocation and revoking the
/// last full client, which some scenarios need to bypass.
pub fn deny(snapshot: &mut RegistrySnapshot, target: &DeviceId, now: u64) {
    let member = snapshot
        .members
        .iter()
        .find(|member| &member.device_id == target)
        .unwrap()
        .clone();
    snapshot.revocations.push(Revocation {
        id: RevocationId::random().unwrap(),
        target: target.clone(),
        wg_fingerprint: member.request.wg_public_key.fingerprint(),
        ssh_fingerprint: member.request.ssh_public_key.fingerprint().unwrap(),
        captured_servers: Default::default(),
        by: target.clone(),
        created_at: now,
    });
}

pub fn scopes(values: &[&str]) -> BTreeSet<AppEnv> {
    values.iter().map(|value| value.parse().unwrap()).collect()
}

pub fn certificate_pem() -> String {
    use openssl::{
        asn1::{Asn1Integer, Asn1Time},
        bn::BigNum,
        hash::MessageDigest,
        pkey::PKey,
        x509::{
            X509, X509NameBuilder,
            extension::{BasicConstraints, SubjectAlternativeName},
        },
    };
    let key = PKey::generate_ed25519().unwrap();
    let mut name = X509NameBuilder::new().unwrap();
    name.append_entry_by_text("CN", "test server").unwrap();
    let name = name.build();
    let mut certificate = X509::builder().unwrap();
    certificate.set_version(2).unwrap();
    certificate
        .set_serial_number(&Asn1Integer::from_bn(&BigNum::from_u32(1).unwrap()).unwrap())
        .unwrap();
    certificate.set_subject_name(&name).unwrap();
    certificate.set_issuer_name(&name).unwrap();
    certificate.set_pubkey(&key).unwrap();
    certificate
        .set_not_before(&Asn1Time::days_from_now(0).unwrap())
        .unwrap();
    certificate
        .set_not_after(&Asn1Time::days_from_now(1).unwrap())
        .unwrap();
    certificate
        .append_extension(BasicConstraints::new().critical().ca().build().unwrap())
        .unwrap();
    let san = SubjectAlternativeName::new()
        .ip("203.0.113.7")
        .ip("fd00::7")
        .build(&certificate.x509v3_context(None, None))
        .unwrap();
    certificate.append_extension(san).unwrap();
    certificate.sign(&key, MessageDigest::null()).unwrap();
    String::from_utf8(certificate.build().to_pem().unwrap()).unwrap()
}
