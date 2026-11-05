use crate::support::*;
use std::collections::BTreeMap;
use tako_network::{
    Approval, DeviceId, DeviceKind, EnrollmentRequest, LocalDeviceKeys, MemberKind,
    NetworkDescriptor, NetworkId, NetworkPublicProfile, Permissions, RegistrySnapshot, ServerInfo,
    WgChallenge,
};

struct Fixture {
    directory: TempDir,
    home: PathBuf,
    tako_home: PathBuf,
    profile_path: PathBuf,
    profile: NetworkPublicProfile,
    servers: Vec<DeviceId>,
}

fn admit(
    snapshot: &mut RegistrySnapshot,
    keys: &LocalDeviceKeys,
    request: EnrollmentRequest,
    kind: MemberKind,
    approver: Option<DeviceId>,
) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
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
                approved_by: approver,
                independent_admin: false,
                created_at: now,
                expires_at: None,
            },
        )
        .unwrap();
    snapshot.members.push(member);
}

impl Fixture {
    fn new() -> Self {
        let directory = TempDir::new().unwrap();
        let home = directory.path().join("home");
        let tako_home = directory.path().join("tako-home");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(tako_home.join("network-keys")).unwrap();
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
        keys.save_new(
            &tako_home
                .join("network-keys")
                .join(format!("{}.json", request.request_id)),
        )
        .unwrap();
        let mut snapshot = RegistrySnapshot {
            network: NetworkDescriptor {
                id: network.clone(),
                initial_admin: own.clone(),
            },
            members: Vec::new(),
            revocations: Vec::new(),
            acks: Vec::new(),
        };
        admit(
            &mut snapshot,
            &keys,
            request.clone(),
            MemberKind::Laptop,
            None,
        );
        let mut trusted_servers = BTreeMap::new();
        for index in [10, 11] {
            let server_keys = LocalDeviceKeys::generate().unwrap();
            let request = server_keys
                .enrollment_request(
                    Some(network.clone()),
                    DeviceKind::Server,
                    "node".parse().unwrap(),
                )
                .unwrap();
            let id = request.device_id_for(&network).unwrap();
            let info = ServerInfo {
                wireguard_endpoint: format!("192.0.2.{index}:51820").parse().unwrap(),
                ssh_host_key: server_keys.ssh_public_key().unwrap(),
                ssh_port: 22,
                https_port: 8443,
                https_certificate_pem: None,
                https_spki_sha256: None,
                corrosion_ca_certificate_pem: None,
            };
            admit(
                &mut snapshot,
                &server_keys,
                request,
                MemberKind::Server(info.clone()),
                Some(own.clone()),
            );
            trusted_servers.insert(id, info);
        }
        let servers = trusted_servers.keys().cloned().collect();
        let profile = NetworkPublicProfile {
            request,
            snapshot,
            trusted_servers,
        };
        let profile_path = tako_home
            .join("networks")
            .join(network.to_string())
            .join(own.to_string())
            .join("profile.json");
        fs::create_dir_all(profile_path.parent().unwrap()).unwrap();
        fs::write(
            tako_home.join("config.toml"),
            format!("[[networks]]\nname = 'fleet'\nid = '{network}'\ndevice = '{own}'\n"),
        )
        .unwrap();
        let fixture = Self {
            directory,
            home,
            tako_home,
            profile_path,
            profile,
            servers,
        };
        fixture.save();
        fixture
    }

    fn save(&self) {
        fs::write(
            &self.profile_path,
            serde_json::to_vec(&serde_json::json!({"state": "active", "profile": self.profile}))
                .unwrap(),
        )
        .unwrap();
    }

    fn status(&self) -> serde_json::Value {
        let output = run_tako_with_env(
            &["--dry-run", "--json", "status"],
            self.directory.path(),
            &self.home,
            &self.tako_home,
        );
        assert!(output.status.success(), "{}", stderr_str(&output));
        serde_json::from_slice(&output.stdout).unwrap()
    }
}

#[test]
fn native_status_resolves_members_without_ssh_keys_and_rejects_missing_pins() {
    let fixture = Fixture::new();
    let before = fs::read(&fixture.profile_path).unwrap();
    let status = fixture.status();
    let servers = status["servers"].as_array().unwrap();
    assert_eq!(
        servers.len(),
        2,
        "same display names must not hide a server"
    );
    for server in servers {
        assert!(server["name"].as_str().unwrap().starts_with("fleet/node/"));
        let error = server["error"].as_str().unwrap();
        assert!(error.contains("pinned HTTPS"), "{error}");
        assert!(!error.contains("SSH"), "{error}");
    }
    assert_eq!(fs::read(&fixture.profile_path).unwrap(), before);
    assert!(!fixture.home.join(".ssh").exists());
    assert_eq!(
        fs::read_dir(fixture.tako_home.join("network-keys"))
            .unwrap()
            .count(),
        1
    );
}

#[test]
fn native_inventory_omits_revoked_server_even_with_stale_saved_pins() {
    let mut fixture = Fixture::new();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let revocation = fixture
        .profile
        .snapshot
        .plan_revocation(
            tako_network::RevocationId::random().unwrap(),
            &fixture.profile.snapshot.network.initial_admin,
            &fixture.servers[0],
            now,
        )
        .unwrap();
    fixture.profile.snapshot.revocations.push(revocation);
    fixture.save();
    let status = fixture.status();
    let servers = status["servers"].as_array().unwrap();
    assert_eq!(servers.len(), 1);
    let expected = fixture.profile.trusted_servers[&fixture.servers[1]]
        .wireguard_endpoint
        .ip()
        .to_string();
    assert_eq!(servers[0]["host"], expected);
}

#[test]
fn project_native_environment_never_falls_back_to_legacy_inventory() {
    let directory = TempDir::new().unwrap();
    let home = directory.path().join("home");
    let tako_home = directory.path().join("tako-home");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&tako_home).unwrap();
    fs::write(
        tako_home.join("config.toml"),
        "[[servers]]\nname = 'legacy'\nhost = '127.0.0.1'\n",
    )
    .unwrap();
    let id = "a".repeat(32);
    fs::write(directory.path().join("tako.toml"), format!("name = 'demo'\nimage = 'example/demo:latest'\n[envs.production]\nnetwork = '{id}'\nroute = 'demo.example.com'\n")).unwrap();
    for command in [
        vec!["logs"],
        vec!["scale", "2"],
        vec!["releases", "list"],
        vec!["backups", "status"],
        vec!["deploy", "--yes"],
    ] {
        let mut args = vec!["--ci", "--dry-run"];
        args.extend(command);
        let output = run_tako_with_env(&args, directory.path(), &home, &tako_home);
        assert!(!output.status.success(), "{args:?}");
        let error = stderr_str(&output);
        assert!(
            error.contains(&format!("Network '{id}' is not configured")),
            "{args:?}: {error}"
        );
        assert!(!error.contains("No SSH keys"), "{args:?}: {error}");
    }
}

#[test]
fn native_scale_resolves_the_enrolled_server_name_and_excludes_legacy_servers() {
    let fixture = Fixture::new();
    fs::write(
        fixture.directory.path().join("tako.toml"),
        format!("name = 'demo'\nimage = 'example/demo:latest'\n[envs.production]\nnetwork = '{}'\nroute = 'demo.example.com'\n", fixture.profile.snapshot.network.id),
    ).unwrap();
    let config = fixture.tako_home.join("config.toml");
    let mut contents = fs::read_to_string(&config).unwrap();
    contents.push_str("\n[[servers]]\nname = 'legacy'\nhost = '127.0.0.1'\n");
    fs::write(config, contents).unwrap();
    let selected = format!("fleet/node/{}", fixture.servers[0]);
    for (server, expected) in [
        (selected.as_str(), "Server has no pinned HTTPS certificate"),
        ("legacy", "Server 'legacy' not found"),
    ] {
        let output = run_tako_with_env(
            &[
                "--ci",
                "--dry-run",
                "scale",
                "2",
                "--env",
                "production",
                "--server",
                server,
            ],
            fixture.directory.path(),
            &fixture.home,
            &fixture.tako_home,
        );
        assert!(!output.status.success());
        let error = stderr_str(&output);
        assert!(error.contains(expected), "{error}");
        assert!(!error.contains("No SSH keys"), "{error}");
    }
}
