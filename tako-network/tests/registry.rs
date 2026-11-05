mod support;
use std::collections::BTreeMap;
use support::*;
use tako_network::*;

#[test]
fn one_server_bootstrap_then_second_server_preserves_network_and_existing_address() {
    let (mut snapshot, _, root) = bootstrap();
    let network = snapshot.network.id.clone();
    let first = add_server(&mut snapshot, &root, 2);
    let address = snapshot.effective_members(2).unwrap()[&first].allowed_ip();
    assert!(address.ends_with("/128"));
    assert_eq!(snapshot.effective_members(2).unwrap().len(), 2);
    let second = add_server(&mut snapshot, &root, 3);
    assert_eq!(snapshot.network.id, network);
    let effective = snapshot.effective_members(3).unwrap();
    assert_eq!(effective[&first].allowed_ip(), address);
    assert_ne!(effective[&second].allowed_ip(), address);
}

#[test]
fn membership_alone_has_no_management_ssh_or_deployment_authority() {
    let (mut snapshot, _, root) = bootstrap();
    let (_, laptop) = add_laptop(&mut snapshot, &root, Permissions::default(), false, 2);
    let permissions = snapshot.permissions(&laptop, 2).unwrap();
    assert!(!snapshot.can_admin(&laptop, 2).unwrap());
    assert!(!permissions.server_admin);
    assert!(permissions.ssh_user.is_none());
    assert!(!permissions.can_deploy(&"app/dev".parse().unwrap()));
    let keys = LocalDeviceKeys::generate().unwrap();
    assert!(
        snapshot
            .approve(
                verified(&keys, &snapshot.network.id, DeviceKind::Laptop, 3),
                approval(
                    MemberKind::Laptop,
                    Permissions::default(),
                    Some(laptop.clone()),
                    3
                )
            )
            .is_err()
    );
    assert!(
        snapshot
            .plan_revocation(RevocationId::random().unwrap(), &laptop, &root, 3)
            .is_err()
    );
}

#[test]
fn delegation_cannot_expand_environment_ssh_server_rights_or_lifetime() {
    let (mut snapshot, _, root) = bootstrap();
    let restricted = Permissions {
        network_admin: true,
        deployments: scopes(&["app/dev"]),
        ..Permissions::default()
    };
    let (_, admin_id) = add_laptop(&mut snapshot, &root, restricted.clone(), false, 2);
    snapshot
        .members
        .iter_mut()
        .find(|m| m.device_id == admin_id)
        .unwrap()
        .expires_at = Some(20);
    let keys = LocalDeviceKeys::generate().unwrap();
    let evidence = verified(&keys, &snapshot.network.id, DeviceKind::Laptop, 3);
    let mut command = approval(
        MemberKind::Laptop,
        Permissions {
            deployments: scopes(&["app/dev"]),
            ..Permissions::default()
        },
        Some(admin_id.clone()),
        3,
    );
    command.expires_at = Some(19);
    snapshot.approve(evidence.clone(), command.clone()).unwrap();
    for permissions in [
        admin(),
        Permissions {
            deployments: scopes(&["app/prod"]),
            ..Permissions::default()
        },
        Permissions {
            ssh_user: Some("root".parse().unwrap()),
            ..Permissions::default()
        },
    ] {
        let mut expanded = command.clone();
        expanded.permissions = permissions;
        assert!(snapshot.approve(evidence.clone(), expanded).is_err());
    }
    command.expires_at = None;
    assert!(snapshot.approve(evidence.clone(), command.clone()).is_err());
    command.expires_at = Some(19);
    command.independent_admin = true;
    command.permissions = admin();
    assert!(snapshot.approve(evidence, command).is_err());
    assert!(!snapshot.can_admin(&admin_id, 20).unwrap());
    assert!(snapshot.can_admin(&root, 20).unwrap());
}

#[test]
fn stale_approvals_and_all_descendants_die_when_approving_admin_is_revoked() {
    let (mut snapshot, _, root) = bootstrap();
    let (_, successor) = add_laptop(&mut snapshot, &root, admin(), true, 2);
    let (_, delegated) = add_laptop(&mut snapshot, &root, admin(), false, 3);
    let mut stale = snapshot.clone();
    revoke(&mut snapshot, &successor, &root, 4);
    let (_, late) = add_laptop(&mut stale, &delegated, Permissions::default(), false, 5);
    snapshot.members.push(stale.members.last().unwrap().clone());
    let effective = snapshot.effective_members(5).unwrap();
    assert!(effective.contains_key(&successor));
    for denied in [&root, &delegated, &late] {
        assert!(!effective.contains_key(denied));
    }
    let (_, replacement) = add_laptop(&mut snapshot, &successor, Permissions::default(), false, 6);
    assert!(
        snapshot
            .effective_members(6)
            .unwrap()
            .contains_key(&replacement)
    );
}

#[test]
fn accepted_denials_survive_revocation_of_their_actor_and_stale_actor_acceptance() {
    let (mut snapshot, _, root) = bootstrap();
    let (_, second) = add_laptop(&mut snapshot, &root, admin(), true, 2);
    let (_, target) = add_laptop(&mut snapshot, &second, Permissions::default(), false, 3);
    let mut stale = snapshot.clone();
    revoke(&mut snapshot, &second, &root, 4);
    let denial = stale
        .plan_revocation(RevocationId::random().unwrap(), &root, &target, 5)
        .unwrap();
    stale.revocations.push(denial.clone());
    snapshot.revocations.push(denial);
    assert!(!snapshot.effective_members(6).unwrap().contains_key(&target));
    assert!(snapshot.can_admin(&second, 6).unwrap());
}

#[test]
fn revocation_bans_reused_keys_even_with_new_device_ids_and_fresh_approvals() {
    let (mut snapshot, _, root) = bootstrap();
    let (victim_keys, victim) = add_laptop(&mut snapshot, &root, Permissions::default(), false, 2);
    revoke(&mut snapshot, &root, &victim, 3);
    let attacker = LocalDeviceKeys::generate().unwrap();
    let mut reused_wg = snapshot
        .members
        .iter()
        .find(|m| m.device_id == victim)
        .unwrap()
        .clone();
    reused_wg.request.ssh_public_key = attacker.ssh_public_key().unwrap();
    reused_wg.request.device_id = None;
    reused_wg.request.proof = attacker
        .sign(
            ENROLLMENT_NAMESPACE,
            &reused_wg.request.canonical_bytes().unwrap(),
        )
        .unwrap()
        .to_pem(ssh_key::LineEnding::LF)
        .unwrap();
    reused_wg.device_id = reused_wg
        .request
        .device_id_for(&snapshot.network.id)
        .unwrap();
    let wg_id = reused_wg.device_id.clone();
    snapshot.members.push(reused_wg);
    let mut reused_ssh = snapshot
        .members
        .iter()
        .find(|m| m.device_id == victim)
        .unwrap()
        .clone();
    reused_ssh.request.wg_public_key = attacker.wg_public_key().unwrap();
    reused_ssh.request.proof = victim_keys
        .sign(
            ENROLLMENT_NAMESPACE,
            &reused_ssh.request.canonical_bytes().unwrap(),
        )
        .unwrap()
        .to_pem(ssh_key::LineEnding::LF)
        .unwrap();
    reused_ssh.device_id = reused_ssh
        .request
        .device_id_for(&snapshot.network.id)
        .unwrap();
    let ssh_id = reused_ssh.device_id.clone();
    snapshot.members.push(reused_ssh);
    let effective = snapshot.effective_members(4).unwrap();
    assert!(!effective.contains_key(&victim));
    assert!(!effective.contains_key(&wg_id));
    assert!(!effective.contains_key(&ssh_id));
    assert!(effective.contains_key(&root));
}

#[test]
fn unverified_copied_key_claim_does_not_quarantine_existing_peer() {
    let (mut snapshot, _, root) = bootstrap();
    let (_, victim) = add_laptop(&mut snapshot, &root, Permissions::default(), false, 2);
    let attacker = LocalDeviceKeys::generate().unwrap();
    let mut claim = snapshot
        .members
        .iter()
        .find(|m| m.device_id == victim)
        .unwrap()
        .clone();
    claim.wg_verified = false;
    claim.request.ssh_public_key = attacker.ssh_public_key().unwrap();
    claim.request.proof = attacker
        .sign(
            ENROLLMENT_NAMESPACE,
            &claim.request.canonical_bytes().unwrap(),
        )
        .unwrap()
        .to_pem(ssh_key::LineEnding::LF)
        .unwrap();
    claim.device_id = claim.request.device_id_for(&snapshot.network.id).unwrap();
    let attacker_id = claim.device_id.clone();
    snapshot.members.push(claim);
    let effective = snapshot.effective_members(3).unwrap();
    assert!(effective.contains_key(&victim));
    assert!(!effective.contains_key(&attacker_id));
}

#[test]
fn missing_ancestry_cycles_and_unauthorized_roots_grant_nothing() {
    let (mut snapshot, _, root) = bootstrap();
    let (_, a) = add_laptop(&mut snapshot, &root, admin(), false, 2);
    let (_, b) = add_laptop(&mut snapshot, &a, admin(), false, 3);
    snapshot
        .members
        .iter_mut()
        .find(|m| m.device_id == a)
        .unwrap()
        .approved_by = Some(b.clone());
    let effective = snapshot.effective_members(4).unwrap();
    assert_eq!(effective.len(), 1);
    assert!(effective.contains_key(&root));
    snapshot.members.retain(|m| m.device_id != b);
    assert!(!snapshot.can_admin(&a, 4).unwrap());
    snapshot
        .members
        .iter_mut()
        .find(|m| m.device_id == a)
        .unwrap()
        .approved_by = None;
    assert!(!snapshot.can_admin(&a, 4).unwrap());
}

#[test]
fn immutable_conflicts_fail_closed_but_identical_duplicate_delivery_is_safe() {
    let (mut snapshot, _, root) = bootstrap();
    snapshot.members.push(snapshot.members[0].clone());
    assert!(snapshot.can_admin(&root, 2).unwrap());
    snapshot.members[1].permissions.ssh_user = Some("root".parse().unwrap());
    assert!(matches!(
        snapshot.effective_members(2),
        Err(Error::Conflict("member"))
    ));
}

#[test]
fn offline_restarted_removed_and_new_servers_remain_pending_without_current_ack() {
    let (mut snapshot, _, root) = bootstrap();
    let a = add_server(&mut snapshot, &root, 2);
    let b = add_server(&mut snapshot, &root, 3);
    let (_, target) = add_laptop(&mut snapshot, &root, Permissions::default(), false, 4);
    let denial = revoke(&mut snapshot, &root, &target, 5);
    let boot_a = BootId::random().unwrap();
    let boot_b = BootId::random().unwrap();
    snapshot.acks.push(Ack {
        revocation: denial.clone(),
        server: a.clone(),
        boot: boot_a.clone(),
    });
    snapshot.acks.push(Ack {
        revocation: denial.clone(),
        server: b.clone(),
        boot: boot_b.clone(),
    });
    let mut reachable = BTreeMap::from([(a.clone(), boot_a)]);
    let status = snapshot.revocation_progress(&denial, &reachable).unwrap();
    assert!(status.applied_servers.contains(&a));
    assert!(status.pending_servers.contains(&b));
    reachable.insert(b.clone(), BootId::random().unwrap());
    assert!(
        snapshot
            .revocation_progress(&denial, &reachable)
            .unwrap()
            .pending_servers
            .contains(&b)
    );
    reachable.insert(b.clone(), boot_b);
    assert!(
        snapshot
            .revocation_progress(&denial, &reachable)
            .unwrap()
            .pending_servers
            .is_empty()
    );
    snapshot.members.retain(|m| m.device_id != b);
    reachable.remove(&b);
    assert!(
        snapshot
            .revocation_progress(&denial, &reachable)
            .unwrap()
            .pending_servers
            .contains(&b)
    );
    let c = add_server(&mut snapshot, &root, 6);
    assert!(
        snapshot
            .revocation_progress(&denial, &reachable)
            .unwrap()
            .pending_servers
            .contains(&c)
    );
}

#[test]
fn server_retirement_does_not_invalidate_admissions_accepted_on_that_server() {
    let (mut snapshot, _, root) = bootstrap();
    let server = add_server(&mut snapshot, &root, 2);
    let (_, laptop) = add_laptop(&mut snapshot, &root, Permissions::default(), false, 3);
    revoke(&mut snapshot, &root, &server, 4);
    let effective = snapshot.effective_members(4).unwrap();
    assert!(!effective.contains_key(&server));
    assert!(effective.contains_key(&laptop));
    assert!(effective.contains_key(&root));
}

#[test]
fn public_profile_binds_device_request_and_server_pins_but_contains_no_private_key() {
    let (mut snapshot, _, root) = bootstrap();
    add_server(&mut snapshot, &root, 2);
    let (keys, id) = add_laptop(&mut snapshot, &root, Permissions::default(), false, 3);
    let request = snapshot
        .members
        .iter()
        .find(|m| m.device_id == id)
        .unwrap()
        .request
        .clone();
    let trusted_servers = snapshot
        .members
        .iter()
        .filter_map(|m| match &m.kind {
            MemberKind::Server(s) => Some((m.device_id.clone(), s.clone())),
            _ => None,
        })
        .collect();
    let mut profile = NetworkPublicProfile {
        request,
        trusted_servers,
        snapshot,
    };
    profile.validate_for(&keys, 3).unwrap();
    assert!(
        profile
            .validate_for(&LocalDeviceKeys::generate().unwrap(), 3)
            .is_err()
    );
    let json = serde_json::to_string(&profile).unwrap();
    assert!(!json.contains(keys.wireguard_private_key().unwrap().as_str()));
    assert!(!json.contains("PRIVATE KEY"));
    profile
        .trusted_servers
        .values_mut()
        .next()
        .unwrap()
        .https_spki_sha256 = Some(KeyFingerprint::of_bytes(b"substituted pin"));
    assert!(profile.validate_for(&keys, 3).is_err());
}

#[test]
fn even_full_device_admin_cannot_mint_an_independent_root_on_a_stale_replica() {
    let (snapshot, _, root) = bootstrap();
    let keys = LocalDeviceKeys::generate().unwrap();
    let mut command = approval(MemberKind::Laptop, admin(), Some(root), 2);
    command.independent_admin = true;
    let evidence = verified(&keys, &snapshot.network.id, DeviceKind::Laptop, 2);
    assert!(snapshot.approve(evidence.clone(), command.clone()).is_err());
    command.approved_by = None;
    assert!(snapshot.approve(evidence.clone(), command.clone()).is_err());
    snapshot
        .approve_independent_admin(evidence, command)
        .unwrap();
}

#[test]
fn endpoint_validation_rejects_loopback_broadcast_mapped_loopback_and_zero_ports() {
    let keys = LocalDeviceKeys::generate().unwrap();
    for endpoint in [
        "127.0.0.1:51820",
        "255.255.255.255:51820",
        "[::ffff:127.0.0.1]:51820",
        "[::]:51820",
        "224.0.0.1:51820",
        "203.0.113.1:0",
    ] {
        let server = ServerInfo {
            wireguard_endpoint: endpoint.parse().unwrap(),
            ssh_host_key: keys.ssh_public_key().unwrap(),
            ssh_port: 22,
            https_port: 443,
            https_certificate_pem: None,
            corrosion_ca_certificate_pem: None,
            https_spki_sha256: None,
        };
        assert!(server.validate().is_err(), "{endpoint}");
    }
}

#[test]
fn independent_operator_recovery_works_after_all_device_admins_are_revoked() {
    let (mut snapshot, _, root) = bootstrap();
    revoke(&mut snapshot, &root, &root, 2);
    assert!(snapshot.effective_members(3).unwrap().is_empty());
    let keys = LocalDeviceKeys::generate().unwrap();
    let mut command = approval(MemberKind::Laptop, admin(), None, 3);
    command.independent_admin = true;
    let member = snapshot
        .approve_independent_admin(
            verified(&keys, &snapshot.network.id, DeviceKind::Laptop, 3),
            command,
        )
        .unwrap();
    let id = member.device_id.clone();
    snapshot.members.push(member);
    assert!(snapshot.can_admin(&id, 3).unwrap());
    assert!(!snapshot.can_admin(&root, 3).unwrap());
}

#[test]
fn conflicting_revocation_and_unknown_nested_permissions_fail_closed() {
    let (mut snapshot, _, root) = bootstrap();
    let (_, target) = add_laptop(&mut snapshot, &root, Permissions::default(), false, 2);
    revoke(&mut snapshot, &root, &target, 3);
    let mut conflicting = snapshot.revocations[0].clone();
    conflicting.target = root;
    snapshot.revocations.push(conflicting);
    assert!(matches!(
        snapshot.effective_members(4),
        Err(Error::Conflict("revocation"))
    ));
    let mut encoded = serde_json::to_value(&snapshot.members[0]).unwrap();
    encoded["permissions"]["sql_writer"] = serde_json::json!(true);
    assert!(serde_json::from_value::<Member>(encoded).is_err());
}

#[test]
fn request_approval_profile_and_online_import_activate_without_a_third_file() {
    let (mut server_state, old_laptop, admin_id) = bootstrap();
    let server_id = add_server(&mut server_state, &admin_id, 2);
    let new_laptop = LocalDeviceKeys::generate().unwrap();
    let request = new_laptop
        .enrollment_request(None, DeviceKind::Laptop, "new-laptop".parse().unwrap())
        .unwrap();
    let request_file = serde_json::to_vec(&request).unwrap();
    let received_request: EnrollmentRequest = serde_json::from_slice(&request_file).unwrap();
    received_request.verify().unwrap();
    assert!(server_state.can_admin(&admin_id, 3).unwrap());
    let stored_entitlement = approval(
        MemberKind::Laptop,
        Permissions::default(),
        Some(admin_id.clone()),
        3,
    );
    let trusted_servers = server_state
        .members
        .iter()
        .filter_map(|member| match &member.kind {
            MemberKind::Server(server) => Some((member.device_id.clone(), server.clone())),
            MemberKind::Laptop => None,
        })
        .collect();
    let pending = PendingApprovalProfile {
        request: received_request,
        pending_id: RequestId::random().unwrap(),
        issuer: server_id,
        expires_at: 3600,
        trusted_servers,
        snapshot: server_state.clone(),
    };
    let approval_file = serde_json::to_vec(&pending).unwrap();
    let imported: PendingApprovalProfile = serde_json::from_slice(&approval_file).unwrap();
    imported.validate_for(&new_laptop, 1000).unwrap();
    let device_id = request.device_id_for(&server_state.network.id).unwrap();
    assert!(
        !server_state
            .effective_members(1000)
            .unwrap()
            .contains_key(&device_id)
    );
    let (challenge, pending_challenge) =
        WgChallenge::begin(&request, server_state.network.id.clone(), 1000, 1060).unwrap();
    assert!(
        old_laptop
            .answer_challenge(&challenge, &request, 1000)
            .is_err()
    );
    let response = new_laptop
        .answer_challenge(&challenge, &request, 1000)
        .unwrap();
    let verified = pending_challenge.verify(&response, 1001).unwrap();
    let mut activation = stored_entitlement;
    activation.created_at = 1001;
    let member = server_state.approve(verified, activation).unwrap();
    server_state.members.push(member);
    let active = NetworkPublicProfile {
        request,
        trusted_servers: imported.trusted_servers,
        snapshot: server_state,
    };
    active.validate_for(&new_laptop, 1001).unwrap();
    assert!(
        active
            .snapshot
            .effective_members(1001)
            .unwrap()
            .contains_key(&device_id)
    );
    let encoded = String::from_utf8(approval_file).unwrap();
    assert!(!encoded.contains(new_laptop.wireguard_private_key().unwrap().as_str()));
}

#[test]
fn pending_profile_requires_own_keys_live_deadlines_and_an_effective_pinned_issuer() {
    let (mut snapshot, _, root) = bootstrap();
    let issuer = add_server(&mut snapshot, &root, 2);
    let keys = LocalDeviceKeys::generate().unwrap();
    let request = keys
        .enrollment_request_expiring(
            None,
            DeviceKind::Laptop,
            "pending".parse().unwrap(),
            Some(50),
        )
        .unwrap();
    let trusted_servers = snapshot
        .members
        .iter()
        .filter_map(|member| match &member.kind {
            MemberKind::Server(server) => Some((member.device_id.clone(), server.clone())),
            MemberKind::Laptop => None,
        })
        .collect();
    let mut profile = PendingApprovalProfile {
        request,
        pending_id: RequestId::random().unwrap(),
        issuer: issuer.clone(),
        expires_at: 40,
        trusted_servers,
        snapshot,
    };
    profile.validate_for(&keys, 3).unwrap();
    assert!(
        profile
            .validate_for(&LocalDeviceKeys::generate().unwrap(), 3)
            .is_err()
    );
    assert!(profile.validate_for(&keys, 40).is_err());
    profile.expires_at = 100;
    assert!(profile.validate_for(&keys, 50).is_err());
    profile.issuer = root;
    assert!(profile.validate_for(&keys, 3).is_err());
    profile.issuer = issuer.clone();
    profile
        .trusted_servers
        .get_mut(&issuer)
        .unwrap()
        .https_certificate_pem = None;
    if let MemberKind::Server(server) = &mut profile
        .snapshot
        .members
        .iter_mut()
        .find(|member| member.device_id == issuer)
        .unwrap()
        .kind
    {
        server.https_certificate_pem = None;
    }
    assert!(profile.validate_for(&keys, 3).is_err());
}

#[test]
fn online_wg_proof_does_not_activate_an_entitlement_whose_admin_was_revoked() {
    let (mut snapshot, _, root) = bootstrap();
    add_server(&mut snapshot, &root, 2);
    let keys = LocalDeviceKeys::generate().unwrap();
    let request = keys
        .enrollment_request(None, DeviceKind::Laptop, "pending".parse().unwrap())
        .unwrap();
    let mut stored_entitlement = approval(
        MemberKind::Laptop,
        Permissions::default(),
        Some(root.clone()),
        3,
    );
    revoke(&mut snapshot, &root, &root, 4);
    let (challenge, pending) =
        WgChallenge::begin(&request, snapshot.network.id.clone(), 5, 65).unwrap();
    let response = keys.answer_challenge(&challenge, &request, 5).unwrap();
    let verified = pending.verify(&response, 5).unwrap();
    stored_entitlement.created_at = 5;
    assert!(snapshot.approve(verified, stored_entitlement).is_err());
    assert_eq!(snapshot.members.len(), 2);
}

#[test]
fn public_transport_certificates_are_parsed_and_spki_is_only_a_derived_check() {
    let (mut snapshot, _, root) = bootstrap();
    let server_id = add_server(&mut snapshot, &root, 2);
    let MemberKind::Server(mut server) = snapshot
        .members
        .iter()
        .find(|member| member.device_id == server_id)
        .unwrap()
        .kind
        .clone()
    else {
        panic!("server expected")
    };
    server.validate().unwrap();
    let certificate =
        openssl::x509::X509::from_pem(server.https_certificate_pem.as_ref().unwrap().as_bytes())
            .unwrap();
    assert!(
        certificate
            .verify(&certificate.public_key().unwrap())
            .unwrap()
    );
    server.https_spki_sha256 = server.https_spki_fingerprint().unwrap();
    assert!(server.https_spki_sha256.is_some());
    server.validate().unwrap();
    server.https_spki_sha256 = Some(KeyFingerprint::of_bytes(b"different key"));
    assert!(server.validate().is_err());
    server.https_spki_sha256 = server.https_spki_fingerprint().unwrap();
    server.https_certificate_pem = None;
    assert!(server.validate().is_err());
}

#[test]
fn transport_certificate_fields_reject_malformed_oversize_bundled_and_extra_data() {
    let (mut snapshot, _, root) = bootstrap();
    let server_id = add_server(&mut snapshot, &root, 2);
    let MemberKind::Server(server) = snapshot
        .members
        .iter()
        .find(|member| member.device_id == server_id)
        .unwrap()
        .kind
        .clone()
    else {
        panic!("server expected")
    };
    let valid = server.https_certificate_pem.clone().unwrap();
    let invalid = [
        String::new(),
        "not PEM".into(),
        "x".repeat(MAX_CERTIFICATE_PEM_BYTES + 1),
        format!("{valid}{valid}"),
        format!("{valid}unexpected data"),
        format!("{valid}-----BEGIN PRIVATE KEY-----\nnot allowed\n-----END PRIVATE KEY-----\n"),
    ];
    for pem in invalid {
        let mut changed = server.clone();
        changed.https_certificate_pem = Some(pem.clone());
        assert!(changed.validate().is_err());
        let mut changed = server.clone();
        changed.corrosion_ca_certificate_pem = Some(pem);
        assert!(changed.validate().is_err());
    }
    let mut crlf = server;
    crlf.https_certificate_pem = Some(valid.replace('\n', "\r\n"));
    crlf.validate().unwrap();
}
