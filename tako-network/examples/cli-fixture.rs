//! Offline, signed membership fixtures for CLI terminal tests; never contacts a host.
use std::{
    collections::BTreeMap,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};
use tako_network::*;
fn admit(
    snapshot: &mut RegistrySnapshot,
    keys: &LocalDeviceKeys,
    request: EnrollmentRequest,
    kind: MemberKind,
    parent: Option<DeviceId>,
) {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let (challenge, pending) =
        WgChallenge::begin(&request, snapshot.network.id.clone(), now, now + 60).unwrap();
    let verified = pending
        .verify(
            &keys.answer_challenge(&challenge, &request, now).unwrap(),
            now,
        )
        .unwrap();
    let member = snapshot
        .approve(
            verified,
            Approval {
                kind,
                permissions: Permissions {
                    network_admin: true,
                    server_admin: true,
                    ..Default::default()
                },
                approved_by: parent,
                independent_admin: false,
                created_at: now,
                expires_at: None,
            },
        )
        .unwrap();
    snapshot.members.push(member);
}
fn main() {
    let args: Vec<_> = std::env::args().collect();
    let home = PathBuf::from(&args[1]);
    let servers: BTreeMap<String, serde_json::Value> = serde_json::from_str(&args[2]).unwrap();
    let network = NetworkId::random().unwrap();
    let owner = LocalDeviceKeys::generate().unwrap();
    let request = owner
        .enrollment_request(
            Some(network.clone()),
            DeviceKind::Laptop,
            "owner".parse().unwrap(),
        )
        .unwrap();
    let own = request.device_id_for(&network).unwrap();
    let mut snapshot = RegistrySnapshot {
        network: NetworkDescriptor {
            id: network.clone(),
            name: "fleet".parse().unwrap(),
            initial_admin: own.clone(),
        },
        members: vec![],
        revocations: vec![],
        acks: vec![],
    };
    admit(
        &mut snapshot,
        &owner,
        request.clone(),
        MemberKind::Laptop,
        None,
    );
    let mut pins = BTreeMap::new();
    for (name, server) in servers {
        let keys = LocalDeviceKeys::generate().unwrap();
        let request = keys
            .enrollment_request(
                Some(network.clone()),
                DeviceKind::Server,
                name.parse().unwrap(),
            )
            .unwrap();
        let info = ServerInfo {
            wireguard_endpoint: format!("{}:51820", server["host"].as_str().unwrap())
                .parse()
                .unwrap(),
            ssh_host_key: keys.ssh_public_key().unwrap(),
            ssh_port: server["port"].as_u64().unwrap_or(22) as u16,
            https_port: 443,
            https_certificate_pem: None,
            https_spki_sha256: None,
            corrosion_ca_certificate_pem: None,
        };
        pins.insert(request.device_id_for(&network).unwrap(), info.clone());
        admit(
            &mut snapshot,
            &keys,
            request,
            MemberKind::Server(info),
            Some(own.clone()),
        );
    }
    let directory = home
        .join("networks")
        .join(network.to_string())
        .join(own.to_string());
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::create_dir_all(home.join("network-keys")).unwrap();
    owner
        .save_new(
            &home
                .join("network-keys")
                .join(format!("{}.json", request.request_id)),
        )
        .unwrap();
    let profile = NetworkPublicProfile {
        request,
        snapshot,
        trusted_servers: pins,
    };
    std::fs::write(
        directory.join("profile.json"),
        serde_json::to_vec(&serde_json::json!({"state":"active","profile":profile})).unwrap(),
    )
    .unwrap();
    std::fs::write(
        home.join("networks/identities.json"),
        serde_json::to_vec(&serde_json::json!([{"id":network,"device":own}])).unwrap(),
    )
    .unwrap();
    println!("{network}");
}
