use super::*;
use crate::network::{
    self,
    tests::{Fixture, TestHome, fixture},
};

fn saved(fixture: &Fixture) -> NetworkAlias {
    network::store_keys(&fixture.profile.request().request_id, &fixture.keys).unwrap();
    network::save_profile("test", &fixture.profile).unwrap()
}

fn journal(alias: &NetworkAlias) -> Journal {
    network::read_bounded_json(
        network::private_file(
            &alias
                .profile_path()
                .unwrap()
                .with_file_name("revocation.json"),
            false,
        )
        .unwrap(),
    )
    .unwrap()
}

fn fail_publication(alias: &NetworkAlias, fixture: &Fixture, device: &str) -> SignedRequest {
    let mut attempted = None;
    let result = run_with(
        alias,
        device,
        |_| Ok(fixture.profile.clone()),
        |_, signed| match &signed.request {
            RpcRequest::Snapshot => Ok(RpcResponse::Snapshot(fixture.profile.snapshot().clone())),
            RpcRequest::Revoke { id, target } => {
                let saved = journal(alias);
                assert_eq!(saved.expected.id, *id);
                assert_eq!(saved.expected.target, *target);
                assert_eq!(saved.issuer, signed.server_id);
                assert!(saved.recorded.is_none());
                attempted = Some(signed.clone());
                Err("response lost".into())
            }
            _ => panic!("unexpected RPC"),
        },
        network::now,
    );
    assert!(result.unwrap_err().to_string().contains("remains pending"));
    attempted.unwrap()
}

fn response(fixture: &Fixture, signed: &SignedRequest) -> Revocation {
    let RpcRequest::Revoke { id, target } = &signed.request else {
        panic!()
    };
    fixture
        .profile
        .snapshot()
        .plan_revocation(id.clone(), &signed.signer, target, signed.timestamp)
        .unwrap()
}

#[test]
fn lost_response_retries_after_self_revocation_with_the_same_id_issuer_and_new_envelope() {
    let _home = TestHome::new();
    let fixture = fixture();
    let alias = saved(&fixture);
    let first = fail_publication(&alias, &fixture, &alias.device);
    let denied = response(&fixture, &first);
    network::record_revocation(&alias, denied.clone()).unwrap();
    assert!(network::load_profile(&alias).is_err());
    let before = std::fs::read(
        alias
            .profile_path()
            .unwrap()
            .with_file_name("revocation.json"),
    )
    .unwrap();
    assert_eq!(project(&alias, &alias.device).unwrap().id, denied.id);
    assert_eq!(
        std::fs::read(
            alias
                .profile_path()
                .unwrap()
                .with_file_name("revocation.json")
        )
        .unwrap(),
        before
    );
    let result = run_with(
        &alias,
        &alias.device,
        |_| panic!("retry must not refresh revoked membership"),
        |pin, signed| {
            let saved = journal(&alias);
            assert_eq!(pin, &saved.pin);
            assert_eq!(signed.request, first.request);
            assert_eq!(signed.server_id, first.server_id);
            assert_eq!(signed.signer, first.signer);
            assert_eq!(signed.network_id, first.network_id);
            assert_ne!(signed.nonce, first.nonce);
            assert_eq!(signed.timestamp, first.timestamp + 1);
            assert_ne!(signed.signature, first.signature);
            Ok(RpcResponse::Revoked(denied.clone()))
        },
        || Ok(first.timestamp + 1),
    )
    .unwrap();
    assert_eq!(result, denied);
    assert_eq!(journal(&alias).recorded, Some(denied.clone()));
    let profile: Profile = network::read_json(&alias.profile_path().unwrap()).unwrap();
    assert_eq!(profile.snapshot().revocations, vec![denied]);
    assert!(profile.validate(&fixture.keys).is_err());
    let bytes = std::fs::read_to_string(
        alias
            .profile_path()
            .unwrap()
            .with_file_name("revocation.json"),
    )
    .unwrap();
    assert!(!bytes.contains("PRIVATE KEY"));
    assert!(!bytes.contains(fixture.keys.wireguard_private_key().unwrap().as_str()));
}

#[test]
fn uncertain_attempt_rejects_a_new_target_and_never_retargets_a_stopped_issuer() {
    let _home = TestHome::new();
    let fixture = fixture();
    let alias = saved(&fixture);
    let first = fail_publication(&alias, &fixture, &fixture.server.to_string());
    assert!(
        run_with(
            &alias,
            &alias.device,
            |_| panic!("must not refresh"),
            |_, _| panic!("must not send"),
            network::now,
        )
        .unwrap_err()
        .to_string()
        .contains("still pending")
    );
    let error = run_with(
        &alias,
        &fixture.server.to_string(),
        |_| panic!("must not refresh"),
        |_, signed| {
            assert_eq!(signed.server_id, first.server_id);
            assert_eq!(signed.request, first.request);
            Err("issuer storage is stopped".into())
        },
        network::now,
    )
    .unwrap_err();
    assert!(error.to_string().contains("remains pending"));
    assert!(journal(&alias).recorded.is_none());
}

#[test]
fn retry_requires_original_keys_and_immutable_network_request_target_and_pin() {
    let _home = TestHome::new();
    let fixture = fixture();
    let alias = saved(&fixture);
    fail_publication(&alias, &fixture, &fixture.server.to_string());
    let key_path = network::keys_path(&fixture.profile.request().request_id).unwrap();
    std::fs::remove_file(&key_path).unwrap();
    assert!(
        run_with(
            &alias,
            &fixture.server.to_string(),
            |_| panic!("must not refresh"),
            |_, _| panic!("must not send"),
            network::now,
        )
        .is_err()
    );
    LocalDeviceKeys::generate()
        .unwrap()
        .save_new(&key_path)
        .unwrap();
    assert!(
        run_with(
            &alias,
            &fixture.server.to_string(),
            |_| panic!("must not refresh"),
            |_, _| panic!("must not send"),
            network::now,
        )
        .is_err()
    );
    std::fs::remove_file(&key_path).unwrap();
    fixture.keys.save_new(&key_path).unwrap();
    let original = journal(&alias);
    let path = alias
        .profile_path()
        .unwrap()
        .with_file_name("revocation.json");
    for mutation in 0..7 {
        let mut invalid = original.clone();
        match mutation {
            0 => invalid.network.id = tako_network::NetworkId::random().unwrap(),
            1 => invalid.alias.device = fixture.server.to_string(),
            2 => invalid.request.request_id = tako_network::RequestId::random().unwrap(),
            3 => invalid.pin.https_port += 1,
            4 => invalid.issuer = original.expected.by.clone(),
            5 => invalid.expected.wg_fingerprint = tako_network::KeyFingerprint::of_bytes(b"wrong"),
            6 => invalid.expected.target = original.expected.by.clone(),
            _ => unreachable!(),
        }
        network::write_json(&path, &invalid, true).unwrap();
        assert!(
            run_with(
                &alias,
                &fixture.server.to_string(),
                |_| panic!("must not refresh"),
                |_, _| panic!("must not send"),
                network::now,
            )
            .is_err(),
            "mutation {mutation}"
        );
    }
}

#[test]
fn recorded_response_repairs_local_persistence_without_new_authority_or_rpc() {
    let _home = TestHome::new();
    let fixture = fixture();
    let alias = saved(&fixture);
    let first = fail_publication(&alias, &fixture, &alias.device);
    let denied = response(&fixture, &first);
    let mut recorded = journal(&alias);
    recorded.recorded = Some(denied.clone());
    network::write_json(
        &alias
            .profile_path()
            .unwrap()
            .with_file_name("revocation.json"),
        &recorded,
        true,
    )
    .unwrap();
    for _ in 0..2 {
        let result = run_with(
            &alias,
            &alias.device,
            |_| panic!("must not refresh"),
            |_, _| panic!("receipt needs no RPC"),
            network::now,
        )
        .unwrap();
        assert_eq!(result, denied);
        let profile: Profile = network::read_json(&alias.profile_path().unwrap()).unwrap();
        assert_eq!(profile.snapshot().revocations, vec![denied.clone()]);
        assert!(profile.validate(&fixture.keys).is_err());
    }
}

fn add_server(fixture: &mut Fixture, parent: &DeviceId, name: &str) -> DeviceId {
    let Profile::Active { profile } = &mut fixture.profile else {
        panic!()
    };
    let keys = LocalDeviceKeys::generate().unwrap();
    let request = keys
        .enrollment_request(
            Some(profile.snapshot.network.id.clone()),
            tako_network::DeviceKind::Server,
            name.parse().unwrap(),
        )
        .unwrap();
    let id = request.device_id_for(&profile.snapshot.network.id).unwrap();
    let mut member = profile
        .snapshot
        .members
        .iter()
        .find(|m| m.device_id == fixture.server)
        .unwrap()
        .clone();
    member.device_id = id.clone();
    member.request = request;
    member.approved_by = Some(parent.clone());
    let MemberKind::Server(pin) = &mut member.kind else {
        panic!()
    };
    pin.ssh_host_key = keys.ssh_public_key().unwrap();
    pin.https_certificate_pem = Some(
        rcgen::generate_simple_self_signed(vec![network::client::hostname(
            &profile.snapshot.network.id,
            &id,
        )])
        .unwrap()
        .cert
        .pem(),
    );
    pin.https_spki_sha256 = None;
    profile.trusted_servers.insert(id.clone(), pin.clone());
    profile.snapshot.members.push(member);
    fixture.profile.validate(&fixture.keys).unwrap();
    id
}

#[test]
fn new_attempt_uses_a_surviving_issuer_and_excludes_the_target_and_its_descendants() {
    let _home = TestHome::new();
    let mut fixture = fixture();
    let target = fixture.server.clone();
    let child = add_server(&mut fixture, &target, "child");
    let owner = fixture.profile.snapshot().network.initial_admin.clone();
    let survivor = add_server(&mut fixture, &owner, "survivor");
    let alias = saved(&fixture);
    let projected = project(&alias, &target.to_string()).unwrap();
    assert_eq!(projected.target, target);
    assert!(
        !alias
            .profile_path()
            .unwrap()
            .with_file_name("revocation.json")
            .exists()
    );
    let mut sent = Vec::new();
    let result = run_with(
        &alias,
        &target.to_string(),
        |_| Ok(fixture.profile.clone()),
        |_, signed| {
            sent.push(signed.clone());
            assert_eq!(signed.server_id, survivor);
            match &signed.request {
                RpcRequest::Snapshot => {
                    Ok(RpcResponse::Snapshot(fixture.profile.snapshot().clone()))
                }
                RpcRequest::Revoke { .. } => Ok(RpcResponse::Revoked(response(&fixture, signed))),
                _ => panic!(),
            }
        },
        network::now,
    )
    .unwrap();
    assert_eq!(sent.len(), 2);
    let profile = network::load_profile(&alias).unwrap();
    let effective = profile
        .snapshot()
        .effective_members(network::now().unwrap())
        .unwrap();
    assert!(!effective.contains_key(&target));
    assert!(!effective.contains_key(&child));
    assert!(effective.contains_key(&survivor));
    assert_eq!(profile.snapshot().revocations, vec![result]);
}

#[test]
fn mismatched_response_keeps_the_original_attempt_uncertain() {
    let _home = TestHome::new();
    let fixture = fixture();
    let alias = saved(&fixture);
    let first = fail_publication(&alias, &fixture, &fixture.server.to_string());
    let mut wrong = response(&fixture, &first);
    wrong.id = RevocationId::random().unwrap();
    assert!(
        run_with(
            &alias,
            &fixture.server.to_string(),
            |_| panic!(),
            |_, _| Ok(RpcResponse::Revoked(wrong.clone())),
            network::now,
        )
        .is_err()
    );
    let retained = journal(&alias);
    assert!(retained.recorded.is_none());
    assert_eq!(
        retained
            .signed(&fixture.keys, first.timestamp)
            .unwrap()
            .request,
        first.request
    );
    let profile: Profile = network::read_json(&alias.profile_path().unwrap()).unwrap();
    assert!(profile.snapshot().revocations.is_empty());
}

#[test]
fn insecure_or_linked_journal_is_rejected_before_any_rpc() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let _home = TestHome::new();
    let fixture = fixture();
    let alias = saved(&fixture);
    fail_publication(&alias, &fixture, &fixture.server.to_string());
    let path = alias
        .profile_path()
        .unwrap()
        .with_file_name("revocation.json");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(
        run_with(
            &alias,
            &fixture.server.to_string(),
            |_| panic!(),
            |_, _| panic!(),
            network::now
        )
        .is_err()
    );
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let source = path.with_file_name("saved-revocation.json");
    std::fs::rename(&path, &source).unwrap();
    for hard in [false, true] {
        if hard {
            std::fs::hard_link(&source, &path).unwrap();
        } else {
            symlink(&source, &path).unwrap();
        }
        assert!(
            run_with(
                &alias,
                &fixture.server.to_string(),
                |_| panic!(),
                |_, _| panic!(),
                network::now
            )
            .is_err()
        );
        std::fs::remove_file(&path).unwrap();
    }
}

#[test]
fn concurrent_attempts_serialize_one_identity_and_reuse_the_durable_id() {
    use std::sync::{
        Arc, Barrier, Mutex,
        atomic::{AtomicUsize, Ordering},
    };
    let _home = TestHome::new();
    let fixture = fixture();
    let alias = saved(&fixture);
    let start = Arc::new(Barrier::new(3));
    let mutations = Arc::new(Mutex::new(Vec::new()));
    let refreshes = Arc::new(AtomicUsize::new(0));
    let mut threads = Vec::new();
    for _ in 0..2 {
        let alias = alias.clone();
        let start = start.clone();
        let mutations = mutations.clone();
        let refreshes = refreshes.clone();
        let snapshot = fixture.profile.snapshot().clone();
        let device = fixture.server.to_string();
        threads.push(std::thread::spawn(move || {
            start.wait();
            let result = run_with(
                &alias,
                &device,
                |alias| {
                    refreshes.fetch_add(1, Ordering::SeqCst);
                    network::load_profile(alias)
                },
                |_, signed| match signed.request {
                    RpcRequest::Snapshot => Ok(RpcResponse::Snapshot(snapshot.clone())),
                    RpcRequest::Revoke { .. } => {
                        assert_eq!(journal(&alias).issuer, signed.server_id);
                        mutations.lock().unwrap().push(signed.clone());
                        Err("response lost".into())
                    }
                    _ => panic!(),
                },
                network::now,
            );
            assert!(result.unwrap_err().to_string().contains("remains pending"));
        }));
    }
    start.wait();
    for thread in threads {
        thread.join().unwrap();
    }
    assert_eq!(refreshes.load(Ordering::SeqCst), 1);
    let mutations = mutations.lock().unwrap();
    assert_eq!(mutations.len(), 2);
    assert_eq!(mutations[0].request, mutations[1].request);
    assert_eq!(mutations[0].server_id, mutations[1].server_id);
    assert_ne!(mutations[0].nonce, mutations[1].nonce);
}

#[test]
fn a_later_revocation_keeps_the_previous_confirmed_denial() {
    let _home = TestHome::new();
    let mut fixture = fixture();
    let owner = fixture.profile.snapshot().network.initial_admin.clone();
    let other = add_server(&mut fixture, &owner, "other");
    let alias = saved(&fixture);
    let mut confirmed = Vec::new();
    for target in [&fixture.server, &other] {
        let denied = run_with(
            &alias,
            &target.to_string(),
            network::load_profile,
            |_, signed| match signed.request {
                RpcRequest::Snapshot => {
                    Ok(RpcResponse::Snapshot(fixture.profile.snapshot().clone()))
                }
                RpcRequest::Revoke { .. } => Ok(RpcResponse::Revoked(response(&fixture, signed))),
                _ => panic!(),
            },
            network::now,
        )
        .unwrap();
        assert_eq!(denied.target, *target);
        confirmed.push(denied);
    }
    assert_ne!(confirmed[0].id, confirmed[1].id);
    let profile: Profile = network::read_json(&alias.profile_path().unwrap()).unwrap();
    assert_eq!(profile.snapshot().revocations, confirmed);
}
