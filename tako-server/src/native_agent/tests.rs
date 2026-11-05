//! Offline fixtures only: no installed helper, Corrosion, firewall or WG device.
use super::*;
use std::{cell::RefCell, rc::Rc};
use tako_network::{
    Ack, DeviceKind, LocalDeviceKeys, Member, MemberKind, NetworkDescriptor, NetworkId, NodeConfig,
    NodeFiles, Permissions, RegistrySnapshot, Revocation, RevocationId, ServerInfo,
    api::{RpcRequest, RuntimeResult},
    corrosion::RegistryStore,
    node_config::protected::Directory,
};

pub(super) struct Fixture {
    pub config: NodeConfig,
    pub keys: LocalDeviceKeys,
    pub admin: LocalDeviceKeys,
}
impl Fixture {
    pub fn new() -> Self {
        let network_id = NetworkId::random().unwrap();
        let admin = LocalDeviceKeys::generate().unwrap();
        let keys = LocalDeviceKeys::generate().unwrap();
        let admin_id = admin.device_id(&network_id, DeviceKind::Laptop).unwrap();
        let server_id = keys.device_id(&network_id, DeviceKind::Server).unwrap();
        let mut rights = Permissions {
            network_admin: true,
            server_admin: true,
            ..Default::default()
        };
        rights.ssh_user = Some("tako".parse().unwrap());
        let root = Member {
            network_id: network_id.clone(),
            device_id: admin_id.clone(),
            request: admin
                .enrollment_request(
                    Some(network_id.clone()),
                    DeviceKind::Laptop,
                    "operator".parse().unwrap(),
                )
                .unwrap(),
            kind: MemberKind::Laptop,
            permissions: rights.clone(),
            approved_by: None,
            independent_admin: false,
            wg_verified: true,
            created_at: 1,
            expires_at: None,
        };
        let server = Member {
            network_id: network_id.clone(),
            device_id: server_id.clone(),
            request: keys
                .enrollment_request(
                    Some(network_id.clone()),
                    DeviceKind::Server,
                    "node".parse().unwrap(),
                )
                .unwrap(),
            kind: MemberKind::Server(ServerInfo {
                wireguard_endpoint: "203.0.113.7:51820".parse().unwrap(),
                ssh_host_key: keys.ssh_public_key().unwrap(),
                ssh_port: 22,
                https_port: 443,
                https_certificate_pem: None,
                https_spki_sha256: None,
                corrosion_ca_certificate_pem: Some(certificate()),
            }),
            permissions: rights,
            approved_by: None,
            independent_admin: true,
            wg_verified: true,
            created_at: 1,
            expires_at: None,
        };
        let network = NetworkDescriptor {
            id: network_id,
            initial_admin: admin_id,
        };
        let config = NodeConfig {
            version: 1,
            network: network.clone(),
            server_id,
            bootstrap: RegistrySnapshot {
                network,
                members: vec![root, server],
                revocations: vec![],
                acks: vec![],
            },
            files: NodeFiles {
                keys: "device-keys.json".into(),
                https_certificate: "https-fullchain.pem".into(),
                https_private_key: "https-key.pem".into(),
                gossip_ca_certificate: "gossip-ca.pem".into(),
                gossip_certificate: "gossip-fullchain.pem".into(),
                gossip_private_key: "gossip-key.pem".into(),
                api_token: "api-token".into(),
            },
            corrosion_binary_sha256: "5".repeat(64),
            wireguard_listen_port: 51820,
        };
        config.validate().unwrap();
        Self {
            config,
            keys,
            admin,
        }
    }
    pub fn revoke(&self, snapshot: &mut RegistrySnapshot, target: &tako_network::DeviceId) {
        let revocation = snapshot
            .plan_revocation(
                RevocationId::random().unwrap(),
                &self.config.network.initial_admin,
                target,
                2,
            )
            .unwrap();
        snapshot.revocations.push(revocation);
    }
}
fn certificate() -> String {
    use openssl::{
        asn1::Asn1Time,
        hash::MessageDigest,
        pkey::PKey,
        x509::{X509, X509NameBuilder},
    };
    let key = PKey::generate_ed25519().unwrap();
    let mut name = X509NameBuilder::new().unwrap();
    name.append_entry_by_text("CN", "offline fixture").unwrap();
    let name = name.build();
    let mut cert = X509::builder().unwrap();
    cert.set_version(2).unwrap();
    cert.set_subject_name(&name).unwrap();
    cert.set_issuer_name(&name).unwrap();
    cert.set_pubkey(&key).unwrap();
    cert.set_not_before(&Asn1Time::days_from_now(0).unwrap())
        .unwrap();
    cert.set_not_after(&Asn1Time::days_from_now(1).unwrap())
        .unwrap();
    cert.sign(&key, MessageDigest::null()).unwrap();
    String::from_utf8(cert.build().to_pem().unwrap()).unwrap()
}

#[test]
fn node_config_fullchain_unknown_fields_and_unsafe_paths() {
    let fixture = Fixture::new();
    let bytes = serde_json::to_vec(&fixture.config).unwrap();
    let config: NodeConfig = serde_json::from_slice(&bytes).unwrap();
    config.validate().unwrap();
    assert_eq!(config.files.https_certificate, "https-fullchain.pem");
    for name in [
        "../key",
        "/key",
        "desired.json",
        "runtime.db-wal",
        "agent-process-lock",
        "api-token",
    ] {
        let mut config = fixture.config.clone();
        config.files.keys = name.into();
        assert!(config.validate().is_err());
    }
    let mut value = serde_json::to_value(&config).unwrap();
    value["api_addr"] = serde_json::json!("0.0.0.0:9834");
    assert!(serde_json::from_value::<NodeConfig>(value).is_err());
}

#[test]
fn helper_projection_revokes_peers_and_ssh_on_self_revocation() {
    let fixture = Fixture::new();
    let mut snapshot = fixture.config.bootstrap.clone();
    let desired = projection::desired(&fixture.config, &fixture.keys, &snapshot, 2, false).unwrap();
    let value: serde_json::Value = serde_json::from_slice(&desired).unwrap();
    assert_eq!(value["peers"].as_array().unwrap().len(), 1);
    assert_eq!(value["peers"][0]["kind"], "laptop");
    assert_eq!(value["peers"][0]["ssh_user"], "tako");
    assert!(value["peers"][0].get("endpoint").is_none());
    assert!(value.get("ssh_authorizations").is_none());
    fixture.revoke(&mut snapshot, &fixture.config.server_id);
    let bytes = projection::desired(&fixture.config, &fixture.keys, &snapshot, 2, false).unwrap();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()["peers"],
        serde_json::json!([])
    );
}

#[test]
fn cached_projection_does_not_map_other_ssh_users_to_tako() {
    let fixture = Fixture::new();
    let mut snapshot = fixture.config.bootstrap.clone();
    snapshot.members[0].permissions.ssh_user = Some("app".parse().unwrap());
    let bytes = projection::desired(&fixture.config, &fixture.keys, &snapshot, 2, false).unwrap();
    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert!(value["peers"][0].get("ssh_user").is_none());
    assert!(value["peers"][0].get("ssh_public_key").is_none());
    let bytes = projection::desired(&fixture.config, &fixture.keys, &snapshot, 2, true).unwrap();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()["peers"],
        serde_json::json!([])
    );
}

#[test]
fn generated_corrosion_toml_uses_exact_local_api_and_mutual_tls() {
    let fixture = Fixture::new();
    let temp = tempfile::tempdir().unwrap();
    let root = fixture.config.network_dir(temp.path());
    std::fs::create_dir_all(&root).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
    let directory = Directory::open(&root, true).unwrap();
    let (bundle, seeds) =
        projection::gossip(&fixture.config, &fixture.config.bootstrap, 2).unwrap();
    assert!(seeds.is_empty()); // The other member is a laptop, never a seed/CA issuer.
    projection::write_corrosion(
        &fixture.config,
        temp.path(),
        &directory,
        &fixture.keys,
        &bundle,
        &seeds,
        "offline-fixture-token",
    )
    .unwrap();
    let bytes = directory.read("corrosion.toml", 65536).unwrap();
    let value: toml::Value = toml::from_str(std::str::from_utf8(&bytes).unwrap()).unwrap();
    assert_eq!(value["api"]["addr"].as_str(), Some("127.0.0.1:9834"));
    assert_eq!(
        value["api"]["authz"]["bearer-token"].as_str(),
        Some("offline-fixture-token")
    );
    assert_eq!(value["gossip"]["plaintext"].as_bool(), Some(false));
    assert_eq!(value["gossip"]["tls"]["insecure"].as_bool(), Some(false));
    assert_eq!(
        value["gossip"]["tls"]["key_file"],
        value["gossip"]["tls"]["client"]["key_file"]
    );
    assert!(
        value["gossip"]["tls"]["cert_file"]
            .as_str()
            .unwrap()
            .ends_with("gossip-fullchain.pem")
    );
}

#[derive(Clone)]
struct MemoryStore(Rc<RefCell<RegistrySnapshot>>);
impl RegistryStore for MemoryStore {
    fn snapshot(&mut self) -> RuntimeResult<RegistrySnapshot> {
        Ok(self.0.borrow().clone())
    }
    fn append_member(&mut self, value: &Member) -> RuntimeResult<()> {
        self.0.borrow_mut().members.push(value.clone());
        Ok(())
    }
    fn append_revocation(&mut self, value: &Revocation) -> RuntimeResult<()> {
        self.0.borrow_mut().revocations.push(value.clone());
        Ok(())
    }
    fn append_ack(&mut self, value: &Ack) -> RuntimeResult<()> {
        self.0.borrow_mut().acks.push(value.clone());
        Ok(())
    }
}

#[test]
fn monotonic_cache_survives_restart_and_rejects_sql_rollback() {
    let fixture = Fixture::new();
    let temp = tempfile::tempdir().unwrap();
    let original = fixture.config.bootstrap.clone();
    let shared = Rc::new(RefCell::new(original.clone()));
    let mut backend = MemoryStore(shared.clone());
    let known = Rc::new(RefCell::new(original.clone()));
    let mut guard = store::MonotonicStore::new(
        backend.clone(),
        Directory::open(temp.path(), true).unwrap(),
        known.clone(),
    );
    let mut updated = original.clone();
    fixture.revoke(&mut updated, &fixture.config.server_id);
    backend.append_revocation(&updated.revocations[0]).unwrap();
    guard.snapshot().unwrap();
    let directory = Directory::open(temp.path(), true).unwrap();
    let persisted: RegistrySnapshot =
        serde_json::from_slice(&directory.read("known-registry.json", 1 << 20).unwrap()).unwrap();
    assert_eq!(persisted.revocations.len(), 1);
    drop(guard);
    *shared.borrow_mut() = original;
    let mut restarted =
        store::MonotonicStore::new(backend, directory, Rc::new(RefCell::new(persisted.clone())));
    assert!(restarted.snapshot().is_err());
    assert_eq!(known.borrow().revocations.len(), 1);
    *shared.borrow_mut() = persisted;
    restarted.snapshot().unwrap();
}

#[test]
fn actual_method_path_and_nonce_are_checked_by_guarded_runtime() {
    use tako_network::runtime::{RuntimeConfig, RuntimeLimits, ServerRuntime};
    let fixture = Fixture::new();
    let temp = tempfile::tempdir().unwrap();
    let snapshot = fixture.config.bootstrap.clone();
    let guard = store::MonotonicStore::new(
        MemoryStore(Rc::new(RefCell::new(snapshot.clone()))),
        Directory::open(temp.path(), true).unwrap(),
        Rc::new(RefCell::new(snapshot)),
    );
    let config = RuntimeConfig {
        network: fixture.config.network.clone(),
        server_id: fixture.config.server_id.clone(),
        boot_id: BootId::random().unwrap(),
        limits: RuntimeLimits::default(),
    };
    let mut runtime =
        ServerRuntime::open(guard, fixture.keys, config, &temp.path().join("runtime.db")).unwrap();
    let signed = tako_network::api::SignedRequest::sign(
        &fixture.admin,
        fixture.config.network.id.clone(),
        fixture.config.server_id.clone(),
        DeviceKind::Laptop,
        2,
        RpcRequest::Snapshot,
    )
    .unwrap();
    assert!(
        runtime
            .handle(signed.clone(), "GET", "/v1/network/snapshot", 2)
            .is_err()
    );
    assert!(
        runtime
            .handle(signed.clone(), "POST", "/v1/network/revoke", 2)
            .is_err()
    );
    runtime
        .handle(signed.clone(), "POST", "/v1/network/snapshot", 2)
        .unwrap();
    assert!(
        runtime
            .handle(signed, "POST", "/v1/network/snapshot", 2)
            .is_err()
    );
}

#[test]
fn compacted_or_omitted_ack_progress_never_blocks_authority_reads() {
    let fixture = Fixture::new();
    let mut before = fixture.config.bootstrap.clone();
    fixture.revoke(&mut before, &fixture.config.server_id);
    before.acks.push(Ack {
        revocation: before.revocations[0].id.clone(),
        server: fixture.config.server_id.clone(),
        boot: BootId::random().unwrap(),
    });
    let mut after = before.clone();
    after.acks[0].boot = BootId::random().unwrap();
    store::preserves(&before, &after).unwrap();
    after.acks.clear();
    store::preserves(&before, &after).unwrap();
    after.revocations.clear();
    assert!(store::preserves(&before, &after).is_err());
}

#[test]
fn isolated_repair_config_uses_only_loopback_and_no_gossip_seeds() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = Fixture::new();
    let temp = tempfile::tempdir().unwrap();
    let root = fixture.config.network_dir(temp.path());
    std::fs::create_dir_all(&root).unwrap();
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
    let directory = Directory::open(&root, true).unwrap();
    let MemberKind::Server(info) = &fixture.config.bootstrap.members[1].kind else {
        unreachable!()
    };
    let ca = info.corrosion_ca_certificate_pem.as_ref().unwrap();
    directory
        .write(&fixture.config.files.gossip_ca_certificate, ca.as_bytes())
        .unwrap();
    projection::write_repair_corrosion(&fixture.config, temp.path(), &directory, "test-credential")
        .unwrap();
    let text = directory.read("corrosion.toml", 64 * 1024).unwrap();
    let config: toml::Value = toml::from_str(std::str::from_utf8(&text).unwrap()).unwrap();
    assert_eq!(config["api"]["addr"].as_str(), Some("127.0.0.1:9834"));
    assert_eq!(config["gossip"]["addr"].as_str(), Some("[::1]:8787"));
    assert_eq!(config["gossip"]["client_addr"].as_str(), Some("[::1]:0"));
    assert!(config["gossip"]["bootstrap"].as_array().unwrap().is_empty());
    assert_eq!(config["gossip"]["plaintext"].as_bool(), Some(false));
    assert_eq!(config["gossip"]["tls"]["insecure"].as_bool(), Some(false));
    assert_eq!(
        &*directory.read("corrosion-ca.pem", 16 * 1024).unwrap(),
        ca.as_bytes()
    );
    let mut denied = fixture.config.bootstrap.clone();
    fixture.revoke(&mut denied, &fixture.config.server_id);
    // No live gossip issuer remains. Local repair does not depend on recovering
    // positive membership or generating a new issuer to trust.
    assert!(projection::gossip(&fixture.config, &denied, 3).is_err());
    let desired = projection::desired(&fixture.config, &fixture.keys, &denied, 3, true).unwrap();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&desired).unwrap()["peers"],
        serde_json::json!([])
    );
}

#[test]
fn guarded_runtime_retries_exact_durable_denial_after_local_store_recovers() {
    use std::cell::Cell;
    use tako_network::{
        api::{RpcResponse, RuntimeError, SignedRequest},
        runtime::{RuntimeConfig, RuntimeLimits, ServerRuntime},
    };
    struct RecoveringStore {
        inner: MemoryStore,
        fail: Rc<Cell<bool>>,
    }
    impl RegistryStore for RecoveringStore {
        fn snapshot(&mut self) -> RuntimeResult<RegistrySnapshot> {
            self.inner.snapshot()
        }
        fn append_member(&mut self, member: &Member) -> RuntimeResult<()> {
            self.inner.append_member(member)
        }
        fn append_ack(&mut self, ack: &Ack) -> RuntimeResult<()> {
            self.inner.append_ack(ack)
        }
        fn append_revocation(&mut self, revocation: &Revocation) -> RuntimeResult<()> {
            if self.fail.get() {
                return Err(RuntimeError::Transport);
            }
            self.inner.append_revocation(revocation)
        }
    }
    let f = Fixture::new();
    let temp = tempfile::tempdir().unwrap();
    let key_path = temp.path().join("keys");
    f.keys.save_new(&key_path).unwrap();
    let backend = Rc::new(RefCell::new(f.config.bootstrap.clone()));
    let fail = Rc::new(Cell::new(true));
    let open = || {
        let guard = store::MonotonicStore::new(
            RecoveringStore {
                inner: MemoryStore(backend.clone()),
                fail: fail.clone(),
            },
            Directory::open(temp.path(), true).unwrap(),
            Rc::new(RefCell::new(f.config.bootstrap.clone())),
        );
        ServerRuntime::open(
            guard,
            LocalDeviceKeys::load(&key_path).unwrap(),
            RuntimeConfig {
                network: f.config.network.clone(),
                server_id: f.config.server_id.clone(),
                boot_id: BootId::random().unwrap(),
                limits: RuntimeLimits::default(),
            },
            &temp.path().join("runtime.db"),
        )
        .unwrap()
    };
    let sign = |id, time| {
        SignedRequest::sign(
            &f.admin,
            f.config.network.id.clone(),
            f.config.server_id.clone(),
            DeviceKind::Laptop,
            time,
            RpcRequest::Revoke {
                id,
                target: f.config.server_id.clone(),
            },
        )
        .unwrap()
    };
    let id = RevocationId::random().unwrap();
    let mut runtime = open();
    assert!(
        runtime
            .handle(sign(id.clone(), 2), "POST", "/v1/network/revoke", 2)
            .is_err()
    );
    assert!(backend.borrow().revocations.is_empty());
    assert!(runtime.desired_host_state(2).unwrap().peers.is_empty());
    drop(runtime);
    let mut runtime = open();
    assert!(runtime.desired_host_state(3).unwrap().peers.is_empty());
    fail.set(false); // models isolated loopback SQL becoming available, no WG grant
    let RpcResponse::Revoked(revocation) = runtime
        .handle(sign(id.clone(), 3), "POST", "/v1/network/revoke", 3)
        .unwrap()
    else {
        panic!("wrong response")
    };
    assert_eq!(revocation.id, id);
    assert_eq!(backend.borrow().revocations.as_slice(), &[revocation]);
    assert!(backend.borrow().acks.is_empty()); // local persistence proves no remote enforcement
    assert!(runtime.desired_host_state(4).unwrap().peers.is_empty());
    assert!(matches!(
        runtime.handle(
            sign(RevocationId::random().unwrap(), 4),
            "POST",
            "/v1/network/revoke",
            4
        ),
        Err(RuntimeError::Denied)
    ));
}
