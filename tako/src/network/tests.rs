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

#[test]
fn snapshot_refresh_keeps_denials_but_allows_ack_compaction() {
    let fixture = fixture();
    let mut previous = fixture.profile.snapshot().clone();
    let revocation = previous
        .plan_revocation(
            tako_network::RevocationId::random().unwrap(),
            &previous.network.initial_admin,
            &fixture.server,
            now().unwrap(),
        )
        .unwrap();
    previous.revocations.push(revocation.clone());
    previous.acks.push(tako_network::Ack {
        revocation: revocation.id,
        server: fixture.server,
        boot: tako_network::BootId::random().unwrap(),
    });
    let mut incoming = fixture.profile.snapshot().clone();
    merge_snapshot(&mut incoming, &previous).unwrap();
    assert_eq!(incoming.revocations, previous.revocations);
    assert_eq!(incoming.members, previous.members);
    assert!(incoming.acks.is_empty());
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

pub(super) struct TestHome {
    previous: Option<std::ffi::OsString>,
    pub directory: tempfile::TempDir,
    _lock: std::sync::MutexGuard<'static, ()>,
}

impl TestHome {
    pub fn new() -> Self {
        let lock = crate::paths::test_tako_home_env_lock();
        let directory = tempfile::tempdir().unwrap();
        let previous = std::env::var_os("TAKO_HOME");
        unsafe {
            std::env::set_var("TAKO_HOME", directory.path());
        }
        Self {
            previous,
            directory,
            _lock: lock,
        }
    }
}

impl Drop for TestHome {
    fn drop(&mut self) {
        unsafe {
            match &self.previous {
                Some(value) => std::env::set_var("TAKO_HOME", value),
                None => std::env::remove_var("TAKO_HOME"),
            }
        }
    }
}

#[test]
fn stale_import_cannot_erase_a_revocation_or_corrupt_saved_state() {
    let _home = TestHome::new();
    let Fixture {
        keys,
        profile,
        server,
    } = fixture();
    store_keys(&profile.request().request_id, &keys).unwrap();
    let alias = save_profile("test", &profile).unwrap();
    let mut revoked = profile.clone();
    let Profile::Active { profile: active } = &mut revoked else {
        panic!()
    };
    let revocation = active
        .snapshot
        .plan_revocation(
            tako_network::RevocationId::random().unwrap(),
            &active.snapshot.network.initial_admin,
            &server,
            now().unwrap(),
        )
        .unwrap();
    active.snapshot.revocations.push(revocation.clone());
    active.trusted_servers.clear();
    assert!(save_profile("test", &revoked).is_err());
    assert!(load_profile(&alias).is_err());
    assert!(save_profile("test", &profile).is_err());
    let saved: Profile = read_json(&alias.profile_path().unwrap()).unwrap();
    assert_eq!(saved.snapshot().revocations, vec![revocation]);
    fs::write(alias.profile_path().unwrap(), b"corrupt").unwrap();
    assert!(save_profile("test", &profile).is_err());
    assert_eq!(fs::read(alias.profile_path().unwrap()).unwrap(), b"corrupt");
}

#[test]
fn revoked_local_identity_is_persisted_and_cannot_be_restored_by_stale_import() {
    let _home = TestHome::new();
    let Fixture { keys, profile, .. } = fixture();
    store_keys(&profile.request().request_id, &keys).unwrap();
    let alias = save_profile("test", &profile).unwrap();
    let mut revoked = profile.clone();
    let Profile::Active { profile: active } = &mut revoked else {
        panic!()
    };
    let own = active.snapshot.network.initial_admin.clone();
    let revocation = active
        .snapshot
        .plan_revocation(
            tako_network::RevocationId::random().unwrap(),
            &own,
            &own,
            now().unwrap(),
        )
        .unwrap();
    active.snapshot.revocations.push(revocation.clone());
    active.trusted_servers.clear();
    revoked.validate_for_storage(&keys).unwrap();
    assert!(revoked.validate(&keys).is_err());
    assert!(save_profile("test", &revoked).is_err());
    assert!(save_profile("test", &profile).is_err());
    assert!(load_profile(&alias).is_err());
    let saved: Profile = read_json(&alias.profile_path().unwrap()).unwrap();
    assert_eq!(saved.snapshot().revocations, vec![revocation]);
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
fn expired_pending_state_does_not_block_a_bound_activation() {
    let _home = TestHome::new();
    let Fixture {
        keys,
        profile,
        server,
    } = fixture();
    let Profile::Active { profile: active } = &profile else {
        panic!()
    };
    store_keys(&profile.request().request_id, &keys).unwrap();
    let mut pending = Profile::Pending {
        profile: PendingApprovalProfile {
            request: active.request.clone(),
            pending_id: RequestId::random().unwrap(),
            issuer: server,
            expires_at: now().unwrap() + 60,
            trusted_servers: active.trusted_servers.clone(),
            snapshot: active.snapshot.clone(),
        },
    };
    let alias = save_profile("test", &pending).unwrap();
    let Profile::Pending { profile: stored } = &mut pending else {
        panic!()
    };
    stored.expires_at = now().unwrap() - 1;
    write_json(&alias.profile_path().unwrap(), &pending, true).unwrap();
    assert!(load_profile(&alias).is_err());
    save_profile("test", &profile).unwrap();
    load_profile(&alias).unwrap().validate(&keys).unwrap();
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

#[test]
fn expired_server_pins_are_pruned_on_load_and_import_without_extending_own_membership() {
    let _home = TestHome::new();
    let Fixture {
        keys,
        mut profile,
        server: expired,
    } = fixture();
    let Profile::Active { profile: active } = &mut profile else {
        panic!()
    };
    let now = now().unwrap();
    for member in &mut active.snapshot.members {
        member.created_at = now - 120;
        if member.device_id == expired {
            member.expires_at = Some(now - 1);
        }
    }
    let server_keys = LocalDeviceKeys::generate().unwrap();
    let request = server_keys
        .enrollment_request(
            Some(active.snapshot.network.id.clone()),
            DeviceKind::Server,
            "surviving-server".parse().unwrap(),
        )
        .unwrap();
    let server = request.device_id_for(&active.snapshot.network.id).unwrap();
    let mut info = active.trusted_servers[&expired].clone();
    info.ssh_host_key = server_keys.ssh_public_key().unwrap();
    info.https_certificate_pem = Some(
        rcgen::generate_simple_self_signed(vec![client::hostname(
            &active.snapshot.network.id,
            &server,
        )])
        .unwrap()
        .cert
        .pem(),
    );
    let own = active.snapshot.network.initial_admin.clone();
    add_member(
        &mut active.snapshot,
        &server_keys,
        request,
        MemberKind::Server(info.clone()),
        Some(own.clone()),
    );
    active.trusted_servers.insert(server.clone(), info);
    assert!(active.validate_for(&keys, now).is_err());
    profile.validate(&keys).unwrap();
    store_keys(&profile.request().request_id, &keys).unwrap();
    let alias = NetworkAlias {
        name: "test".into(),
        id: profile.snapshot().network.id.to_string(),
        device: own.to_string(),
    };
    private_directory(alias.profile_path().unwrap().parent().unwrap()).unwrap();
    write_json(&alias.profile_path().unwrap(), &profile, false).unwrap();
    NetworksToml::save_alias(&alias).unwrap();
    let Profile::Active { profile: loaded } = load_profile(&alias).unwrap() else {
        panic!()
    };
    assert_eq!(
        loaded.trusted_servers.keys().collect::<Vec<_>>(),
        vec![&server]
    );
    save_profile("test", &profile).unwrap();
    let Profile::Active { profile: imported } = load_profile(&alias).unwrap() else {
        panic!()
    };
    assert_eq!(imported.trusted_servers, loaded.trusted_servers);
    let mut missing_pin = profile.clone();
    let Profile::Active { profile: missing } = &mut missing_pin else {
        panic!()
    };
    missing.trusted_servers.remove(&server);
    assert!(missing_pin.validate(&keys).is_err());
    assert!(save_profile("test", &missing_pin).is_err());
    let Profile::Active {
        profile: expired_own,
    } = &mut profile
    else {
        panic!()
    };
    expired_own
        .snapshot
        .members
        .iter_mut()
        .find(|member| member.device_id == own)
        .unwrap()
        .expires_at = Some(now - 1);
    assert!(profile.validate(&keys).is_err());
    assert!(save_profile("test", &profile).is_err());
}

#[test]
fn recording_self_or_last_server_revocation_succeeds_and_is_idempotent() {
    let _home = TestHome::new();
    for revoke_self in [false, true] {
        let Fixture {
            keys,
            profile,
            server,
        } = fixture();
        store_keys(&profile.request().request_id, &keys).unwrap();
        let alias = save_profile(if revoke_self { "self" } else { "server" }, &profile).unwrap();
        let own = &profile.snapshot().network.initial_admin;
        let target = if revoke_self { own } else { &server };
        let revocation = profile
            .snapshot()
            .plan_revocation(
                tako_network::RevocationId::random().unwrap(),
                own,
                target,
                now().unwrap(),
            )
            .unwrap();
        record_revocation(&alias, revocation.clone()).unwrap();
        record_revocation(&alias, revocation.clone()).unwrap();
        let saved: Profile = read_json(&alias.profile_path().unwrap()).unwrap();
        assert_eq!(saved.snapshot().revocations, vec![revocation.clone()]);
        saved.validate_for_storage(&keys).unwrap();
        assert!(load_profile(&alias).is_err());
        assert!(save_profile(&alias.name, &profile).is_err());
        let before = fs::read(alias.profile_path().unwrap()).unwrap();
        let mut conflicting = revocation;
        conflicting.created_at += 1;
        assert!(record_revocation(&alias, conflicting).is_err());
        assert_eq!(fs::read(alias.profile_path().unwrap()).unwrap(), before);
    }
}
