use super::*;
use tako_network::{
    Approval, DeviceId, MemberKind, NetworkDescriptor, Permissions, ServerInfo, WgChallenge,
};

pub(super) struct Fixture {
    pub keys: LocalDeviceKeys,
    pub profile: Profile,
    pub server: DeviceId,
}

pub(super) fn fixture() -> Fixture {
    let keys = LocalDeviceKeys::generate().unwrap();
    let network = NetworkId::random().unwrap();
    let request = keys
        .enrollment_request(
            Some(network.clone()),
            DeviceKind::Laptop,
            "owner".parse().unwrap(),
        )
        .unwrap();
    let own = request.device_id_for(&network).unwrap();
    let mut snapshot = RegistrySnapshot {
        network: NetworkDescriptor {
            id: network,
            initial_admin: own.clone(),
        },
        members: vec![],
        revocations: vec![],
        acks: vec![],
    };
    add_member(
        &mut snapshot,
        &keys,
        request.clone(),
        MemberKind::Laptop,
        None,
    );
    let server_keys = LocalDeviceKeys::generate().unwrap();
    let server_request = server_keys
        .enrollment_request(
            Some(snapshot.network.id.clone()),
            DeviceKind::Server,
            "server".parse().unwrap(),
        )
        .unwrap();
    let server = server_request.device_id_for(&snapshot.network.id).unwrap();
    let cert =
        rcgen::generate_simple_self_signed(vec![client::hostname(&snapshot.network.id, &server)])
            .unwrap()
            .cert
            .pem();
    let info = ServerInfo {
        wireguard_endpoint: "192.0.2.10:51820".parse().unwrap(),
        ssh_host_key: server_keys.ssh_public_key().unwrap(),
        ssh_port: 22,
        https_port: 8443,
        https_certificate_pem: Some(cert),
        https_spki_sha256: None,
        corrosion_ca_certificate_pem: None,
    };
    add_member(
        &mut snapshot,
        &server_keys,
        server_request,
        MemberKind::Server(info.clone()),
        Some(own),
    );
    let profile = Profile::Active {
        profile: NetworkPublicProfile {
            request,
            snapshot,
            trusted_servers: BTreeMap::from([(server.clone(), info)]),
        },
    };
    profile.validate(&keys).unwrap();
    Fixture {
        keys,
        profile,
        server,
    }
}

fn add_member(
    snapshot: &mut RegistrySnapshot,
    keys: &LocalDeviceKeys,
    request: EnrollmentRequest,
    kind: MemberKind,
    by: Option<DeviceId>,
) {
    let now = now().unwrap();
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
                approved_by: by,
                independent_admin: false,
                permissions: Permissions {
                    network_admin: true,
                    server_admin: true,
                    ..Default::default()
                },
                created_at: now,
                expires_at: None,
            },
        )
        .unwrap();
    snapshot.members.push(member);
}

#[test]
fn public_profile_roundtrip_requires_its_own_keys() {
    let Fixture { keys, profile, .. } = fixture();
    let bytes = serde_json::to_vec(&profile).unwrap();
    let text = std::str::from_utf8(&bytes).unwrap();
    assert!(!text.contains(keys.wireguard_private_key().unwrap().as_str()));
    assert!(!text.contains("PRIVATE KEY"));
    let imported: Profile = serde_json::from_slice(&bytes).unwrap();
    imported.validate(&keys).unwrap();
    assert!(
        imported
            .validate(&LocalDeviceKeys::generate().unwrap())
            .is_err()
    );
}

#[test]
fn private_directory_rejects_symlink_ancestors_without_chmod() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let dir = tempfile::tempdir().unwrap();
    let real = dir.path().join("real");
    fs::create_dir(&real).unwrap();
    fs::set_permissions(&real, fs::Permissions::from_mode(0o755)).unwrap();
    symlink(&real, dir.path().join("link")).unwrap();
    assert!(private_directory(&dir.path().join("link/keys")).is_err());
    assert!(!real.join("keys").exists());
    assert_eq!(
        fs::metadata(&real).unwrap().permissions().mode() & 0o777,
        0o755
    );
}

#[test]
fn json_persistence_rejects_linked_files() {
    use std::os::unix::fs::symlink;
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("source");
    write_json(&source, &serde_json::json!({"original": true}), false).unwrap();
    let before = fs::read(&source).unwrap();
    for hard in [false, true] {
        let path = dir.path().join(if hard { "hard" } else { "symbolic" });
        if hard {
            fs::hard_link(&source, &path).unwrap();
        } else {
            symlink(&source, &path).unwrap();
        }
        assert!(read_json::<serde_json::Value>(&path).is_err());
        assert!(write_json(&path, &serde_json::json!({"replaced": true}), true).is_err());
        assert_eq!(fs::read(&source).unwrap(), before);
        fs::remove_file(path).unwrap();
    }
}

#[test]
fn bounded_json_reads_only_limit_plus_one_and_rejects_trailing_data() {
    use std::cell::Cell;
    struct Infinite<'a>(&'a Cell<usize>);
    impl Read for Infinite<'_> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            buf.fill(b' ');
            self.0.set(self.0.get() + buf.len());
            Ok(buf.len())
        }
    }
    let read = Cell::new(0);
    let error = read_bounded_json::<serde_json::Value>(Infinite(&read)).unwrap_err();
    assert!(error.to_string().contains("4 MiB"));
    assert_eq!(read.get(), 4 * 1024 * 1024 + 1);
    let mut exact = vec![b' '; 4 * 1024 * 1024];
    exact[0..2].copy_from_slice(b"{}");
    assert_eq!(
        read_bounded_json::<serde_json::Value>(&exact[..]).unwrap(),
        serde_json::json!({})
    );
    assert!(read_bounded_json::<serde_json::Value>(&b"{}{}"[..]).is_err());
}

#[test]
fn oversized_json_write_preserves_existing_output() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("profile.json");
    write_json(&path, &serde_json::json!({"valid": true}), false).unwrap();
    let before = fs::read(&path).unwrap();
    assert!(write_json(&path, &"x".repeat(4 * 1024 * 1024), true).is_err());
    assert_eq!(fs::read(path).unwrap(), before);
}
