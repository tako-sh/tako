use super::*;
use std::os::unix::fs::{PermissionsExt, symlink};
use tako_network::{
    BootstrapRequest, DeviceId, EnrollmentRequest, LocalDeviceKeys, Member, NetworkDescriptor,
    Permissions, ServerInfo, WgChallenge, api::RpcRequest,
};

struct Fixture {
    _temp: tempfile::TempDir,
    data: PathBuf,
    host_key: PathBuf,
    binary: PathBuf,
    receipt: PathBuf,
    network: NetworkId,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let data = temp.path().join("data");
        fs::DirBuilder::new().mode(0o700).create(&data).unwrap();
        let host_key = temp.path().join("ssh_host_ed25519_key.pub");
        let key = LocalDeviceKeys::generate()
            .unwrap()
            .ssh_public_key()
            .unwrap();
        fs::write(&host_key, format!("{} operator@server\n", key.as_str())).unwrap();
        let binary = temp.path().join("corrosion");
        fs::write(&binary, b"test executable bytes").unwrap();
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).unwrap();
        let receipt = temp.path().join("corrosion.sha256");
        fs::write(
            &receipt,
            format!(
                "{}\n",
                hex::encode(openssl::sha::sha256(b"test executable bytes"))
            ),
        )
        .unwrap();
        Self {
            _temp: temp,
            data,
            host_key,
            binary,
            receipt,
            network: NetworkId::random().unwrap(),
        }
    }

    fn host(&self) -> Host<'_> {
        Host {
            host_key: &self.host_key,
            binary: &self.binary,
            receipt: &self.receipt,
            owner: unsafe { libc::geteuid() },
        }
    }

    fn prepare_request(&self) -> BootstrapRequest {
        BootstrapRequest::Prepare {
            network: self.network.clone(),
            name: "server".parse().unwrap(),
            wireguard_endpoint: "192.0.2.10:51820".parse().unwrap(),
            ssh_port: 22,
            https_port: 443,
        }
    }

    fn call(&self, request: BootstrapRequest) -> Result<BootstrapResponse> {
        handle(&self.data, request, &self.host())
    }

    fn prepare(&self) -> (EnrollmentRequest, ServerInfo) {
        match self.call(self.prepare_request()).unwrap() {
            BootstrapResponse::Prepared { request, server } => (request, server),
            _ => panic!("expected prepared identity"),
        }
    }

    fn snapshot(&self) -> RegistrySnapshot {
        let (request, server) = self.prepare();
        let device = request.device_id_for(&self.network).unwrap();
        RegistrySnapshot {
            network: NetworkDescriptor {
                id: self.network.clone(),
                initial_admin: device.clone(),
            },
            members: vec![Member {
                network_id: self.network.clone(),
                device_id: device,
                request,
                kind: MemberKind::Server(server),
                permissions: Permissions {
                    network_admin: true,
                    server_admin: true,
                    ..Default::default()
                },
                approved_by: None,
                independent_admin: true,
                wg_verified: true,
                created_at: now() - 1,
                expires_at: None,
            }],
            revocations: vec![],
            acks: vec![],
        }
    }

    fn network_dir(&self) -> PathBuf {
        self.data.join("networks").join(self.network.to_string())
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

#[test]
fn prepare_answer_sign_install_and_retry_preserve_identity() {
    let f = Fixture::new();
    let (request, server) = f.prepare();
    assert_eq!(f.prepare(), (request.clone(), server));
    let (challenge, pending) =
        WgChallenge::begin(&request, f.network.clone(), now(), now() + 60).unwrap();
    let BootstrapResponse::Proof { proof } = f
        .call(BootstrapRequest::Answer {
            network: f.network.clone(),
            challenge,
        })
        .unwrap()
    else {
        panic!("expected proof")
    };
    pending.verify(&proof, now()).unwrap();
    let destination: DeviceId = "ab".repeat(32).parse().unwrap();
    let BootstrapResponse::Signed { request: signed } = f
        .call(BootstrapRequest::Sign {
            network: f.network.clone(),
            server: destination.clone(),
            request: RpcRequest::Snapshot,
        })
        .unwrap()
    else {
        panic!("expected signed request")
    };
    assert_eq!(signed.server_id, destination);
    assert_eq!(signed.network_id, f.network);
    assert_eq!(signed.signer, request.device_id_for(&f.network).unwrap());
    assert!(!signed.signature.is_empty());
    SignedRequest::from_slice(&serde_json::to_vec(&signed).unwrap()).unwrap();
    let snapshot = f.snapshot();
    f.call(BootstrapRequest::Install {
        snapshot: snapshot.clone(),
    })
    .unwrap();
    let config_bytes = fs::read(f.data.join(NODE_CONFIG_FILE)).unwrap();
    let token = fs::read(f.network_dir().join(TOKEN_FILE)).unwrap();
    let inode = fs::metadata(f.data.join(NODE_CONFIG_FILE)).unwrap().ino();
    f.call(BootstrapRequest::Install { snapshot }).unwrap();
    assert_eq!(
        fs::read(f.data.join(NODE_CONFIG_FILE)).unwrap(),
        config_bytes
    );
    assert_eq!(
        fs::metadata(f.data.join(NODE_CONFIG_FILE)).unwrap().ino(),
        inode
    );
    assert_eq!(fs::read(f.network_dir().join(TOKEN_FILE)).unwrap(), token);
    let config = NodeConfig::load(&f.data).unwrap();
    assert_eq!(config.files.keys, "device-keys.json");
    assert_eq!(config.wireguard_listen_port, 51820);
    for path in [
        f.data.join(NODE_CONFIG_FILE),
        f.network_dir().join(TOKEN_FILE),
    ] {
        let metadata = fs::metadata(path).unwrap();
        assert_eq!(metadata.mode() & 0o7777, 0o600);
        assert_eq!(metadata.nlink(), 1);
        assert_eq!(metadata.uid(), unsafe { libc::geteuid() });
    }
    assert_eq!(
        fs::metadata(f.network_dir()).unwrap().mode() & 0o7777,
        0o700
    );
}

#[test]
fn install_rejects_invalid_members_without_creating_config_or_token() {
    let f = Fixture::new();
    let valid = f.snapshot();
    for case in 0..12 {
        let mut snapshot = valid.clone();
        let member = &mut snapshot.members[0];
        match case {
            0 => snapshot.members.clear(),
            1 => member.permissions.server_admin = false,
            2 => member.wg_verified = false,
            3 => member.created_at = now() + 600,
            4 => {
                member.created_at = now() - 120;
                member.expires_at = Some(now() - 60);
            }
            5 => member.request.proof.clear(),
            6 => {
                if let MemberKind::Server(server) = &mut member.kind {
                    server.ssh_port = 2222;
                }
            }
            7 => {
                if let MemberKind::Server(server) = &mut member.kind {
                    server.https_spki_sha256 = None;
                }
            }
            8 => {
                member.independent_admin = false;
                member.approved_by = Some("ab".repeat(32).parse().unwrap());
            }
            9 => member.permissions.network_admin = false,
            10 => {
                member.request = LocalDeviceKeys::generate()
                    .unwrap()
                    .enrollment_request(
                        Some(f.network.clone()),
                        DeviceKind::Server,
                        "different".parse().unwrap(),
                    )
                    .unwrap();
                member.device_id = member.request.device_id_for(&f.network).unwrap();
                snapshot.network.initial_admin = member.device_id.clone();
            }
            11 => {
                let root_keys = LocalDeviceKeys::generate().unwrap();
                let mut root = member.clone();
                root.request = root_keys
                    .enrollment_request(
                        Some(f.network.clone()),
                        DeviceKind::Laptop,
                        "root".parse().unwrap(),
                    )
                    .unwrap();
                root.device_id = root.request.device_id_for(&f.network).unwrap();
                root.kind = MemberKind::Laptop;
                root.request.proof.clear();
                snapshot.network.initial_admin = root.device_id.clone();
                snapshot.members.push(root);
            }
            _ => unreachable!(),
        }
        assert!(
            f.call(BootstrapRequest::Install { snapshot }).is_err(),
            "case {case}"
        );
        assert!(!f.data.join(NODE_CONFIG_FILE).exists());
        assert!(!f.network_dir().join(TOKEN_FILE).exists());
    }
    let mut revoked = valid.clone();
    revoked.revocations.push(
        valid
            .plan_revocation(
                tako_network::RevocationId::random().unwrap(),
                &valid.members[0].device_id,
                &valid.members[0].device_id,
                now(),
            )
            .unwrap(),
    );
    assert!(
        f.call(BootstrapRequest::Install { snapshot: revoked })
            .is_err()
    );
}

#[test]
fn installed_config_cannot_be_changed_or_repaired_implicitly() {
    let f = Fixture::new();
    let snapshot = f.snapshot();
    f.call(BootstrapRequest::Install {
        snapshot: snapshot.clone(),
    })
    .unwrap();
    let previous = fs::read(f.data.join(NODE_CONFIG_FILE)).unwrap();
    let mut changed = snapshot.clone();
    changed.members[0].created_at -= 1;
    assert!(
        f.call(BootstrapRequest::Install { snapshot: changed })
            .is_err()
    );
    assert_eq!(fs::read(f.data.join(NODE_CONFIG_FILE)).unwrap(), previous);
    fs::remove_file(f.network_dir().join(TOKEN_FILE)).unwrap();
    assert!(f.call(BootstrapRequest::Install { snapshot }).is_err());
    assert!(!f.network_dir().join(TOKEN_FILE).exists());
    let mut request = f.prepare_request();
    if let BootstrapRequest::Prepare { network, .. } = &mut request {
        *network = NetworkId::random().unwrap();
    }
    assert!(f.call(request).is_err());
}

#[test]
fn receipt_checks_digest_owner_mode_and_links() {
    let f = Fixture::new();
    verify_binary(&f.host()).unwrap();
    let mut wrong_owner = f.host();
    wrong_owner.owner = u32::MAX;
    assert!(verify_binary(&wrong_owner).is_err());
    fs::write(&f.binary, b"substituted executable").unwrap();
    assert!(verify_binary(&f.host()).is_err());
    fs::write(&f.binary, b"test executable bytes").unwrap();
    fs::set_permissions(&f.receipt, fs::Permissions::from_mode(0o666)).unwrap();
    assert!(verify_binary(&f.host()).is_err());
    fs::set_permissions(&f.receipt, fs::Permissions::from_mode(0o644)).unwrap();
    let saved = f.receipt.with_extension("saved");
    fs::rename(&f.receipt, &saved).unwrap();
    symlink(&saved, &f.receipt).unwrap();
    assert!(verify_binary(&f.host()).is_err());
    fs::remove_file(&f.receipt).unwrap();
    fs::hard_link(&saved, &f.receipt).unwrap();
    assert!(verify_binary(&f.host()).is_err());
}

#[test]
fn prepare_refuses_unsafe_paths_and_changed_public_configuration() {
    let f = Fixture::new();
    let (request, _) = f.prepare();
    let mut changed = f.prepare_request();
    if let BootstrapRequest::Prepare { ssh_port, .. } = &mut changed {
        *ssh_port = 2222;
    }
    assert!(f.call(changed).is_err());
    assert_eq!(f.prepare().0, request);
    let alias = f.data.with_extension("alias");
    symlink(&f.data, &alias).unwrap();
    assert!(handle(&alias, f.prepare_request(), &f.host()).is_err());
    fs::set_permissions(f.network_dir(), fs::Permissions::from_mode(0o755)).unwrap();
    assert!(f.call(f.prepare_request()).is_err());
    assert!(
        f.call(BootstrapRequest::Sign {
            network: f.network.clone(),
            server: request.device_id.unwrap(),
            request: RpcRequest::Snapshot
        })
        .is_err()
    );
}

#[test]
fn host_key_comments_are_stripped_and_rotation_blocks_install() {
    let f = Fixture::new();
    let snapshot = f.snapshot();
    assert!(
        !read_host_key(&f.host())
            .unwrap()
            .as_str()
            .contains("operator@")
    );
    let key = LocalDeviceKeys::generate()
        .unwrap()
        .ssh_public_key()
        .unwrap();
    fs::write(&f.host_key, key.as_str()).unwrap();
    assert!(f.call(BootstrapRequest::Install { snapshot }).is_err());
    fs::write(&f.host_key, "ssh-ed25519 not-a-key\n").unwrap();
    assert!(read_host_key(&f.host()).is_err());
    fs::write(&f.host_key, "x".repeat(4097)).unwrap();
    assert!(read_host_key(&f.host()).is_err());
}

#[test]
fn protected_token_and_config_reject_links_and_permissions() {
    for token_case in [true, false] {
        let f = Fixture::new();
        let snapshot = f.snapshot();
        let target = if token_case {
            f.network_dir().join(TOKEN_FILE)
        } else {
            f.data.join(NODE_CONFIG_FILE)
        };
        let other = f.data.join("other");
        fs::write(&other, b"must remain unchanged").unwrap();
        symlink(&other, &target).unwrap();
        assert!(
            f.call(BootstrapRequest::Install {
                snapshot: snapshot.clone()
            })
            .is_err()
        );
        assert_eq!(fs::read(&other).unwrap(), b"must remain unchanged");
        fs::remove_file(&target).unwrap();
        fs::write(&target, b"unsafe").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(f.call(BootstrapRequest::Install { snapshot }).is_err());
    }
}

#[test]
fn bootstrap_lock_refuses_concurrent_mutation() {
    let f = Fixture::new();
    let directory = Directory::open(&f.data, false).unwrap();
    let _guard = directory.lock("bootstrap.lock").unwrap();
    assert!(f.call(f.prepare_request()).is_err());
    assert!(!f.data.join("networks").exists());
}

#[test]
fn bounded_exchange_writes_public_json_only_after_success() {
    let f = Fixture::new();
    let input = serde_json::to_vec(&f.prepare_request()).unwrap();
    let mut output = Vec::new();
    exchange(input.as_slice(), &mut output, |request| f.call(request)).unwrap();
    assert_eq!(output.last(), Some(&b'\n'));
    assert!(serde_json::from_slice::<BootstrapResponse>(&output).is_ok());
    assert!(!String::from_utf8(output).unwrap().contains("PRIVATE KEY"));
    for input in [
        b"{".to_vec(),
        vec![b' '; MAX_BOOTSTRAP_BYTES + 1],
        [input.clone(), input].concat(),
    ] {
        let mut output = Vec::new();
        assert!(
            exchange(input.as_slice(), &mut output, |_| panic!(
                "invalid input reached handler"
            ))
            .is_err()
        );
        assert!(output.is_empty());
    }
    let mut output = Vec::new();
    assert!(
        exchange(
            serde_json::to_vec(&f.prepare_request()).unwrap().as_slice(),
            &mut output,
            |_| Err("failure".into())
        )
        .is_err()
    );
    assert!(output.is_empty());
}
