fn invitation_grant(
    _fixture: &Fixture,
) -> (
    crate::invitation::CreateInvitation,
    crate::invitation::InvitationToken,
) {
    let token = crate::invitation::InvitationToken::random().unwrap();
    (
        crate::invitation::CreateInvitation {
            id: RequestId::random().unwrap(),
            token_hash: token.hash().unwrap(),
            permissions: admin(),
            expires_at: 200,
            member_expires_at: None,
        },
        token,
    )
}

#[test]
fn invitation_claim_is_single_use_and_restart_retry_keeps_original_identity() {
    use crate::invitation::ClaimInvitation;
    let fixture = Fixture::new();
    let mut runtime = fixture.open();
    let (grant, token) = invitation_grant(&fixture);
    fixture
        .call(
            &mut runtime,
            &fixture.admin,
            RpcRequest::CreateInvitation(Box::new(grant.clone())),
            10,
        )
        .unwrap();
    let keys = LocalDeviceKeys::generate().unwrap();
    let request = keys
        .enrollment_request(
            Some(fixture.config.network.id.clone()),
            DeviceKind::Laptop,
            "new-machine".parse().unwrap(),
        )
        .unwrap();
    let claim = ClaimInvitation {
        id: grant.id.clone(),
        token: token.clone(),
        request: request.clone(),
    };
    let response = fixture
        .call(
            &mut runtime,
            &keys,
            RpcRequest::ClaimInvitation(Box::new(claim.clone())),
            11,
        )
        .unwrap();
    assert!(matches!(response, RpcResponse::Pending(_)));
    drop(runtime);
    let mut runtime = fixture.open();
    assert_eq!(
        response,
        fixture
            .call(
                &mut runtime,
                &keys,
                RpcRequest::ClaimInvitation(Box::new(claim.clone())),
                12
            )
            .unwrap()
    );
    let other = LocalDeviceKeys::generate().unwrap();
    let mut stolen = claim.clone();
    stolen.request = other
        .enrollment_request(
            Some(fixture.config.network.id.clone()),
            DeviceKind::Laptop,
            "other-machine".parse().unwrap(),
        )
        .unwrap();
    assert!(matches!(
        fixture.call(
            &mut runtime,
            &other,
            RpcRequest::ClaimInvitation(Box::new(stolen)),
            13
        ),
        Err(RuntimeError::Conflict(_))
    ));
    let approval = ApproveRequest {
        pending_id: grant.id,
        request,
        kind: MemberKind::Laptop,
        permissions: grant.permissions,
        expires_at: grant.expires_at,
        member_expires_at: None,
    };
    let complete = fixture.challenge(&mut runtime, &keys, &approval, 14);
    assert!(matches!(
        fixture.call(&mut runtime, &keys, complete, 15).unwrap(),
        RpcResponse::Active(_)
    ));
    // Token is redacted and only its digest is persisted.
    assert!(!format!("{claim:?}").contains(&serde_json::to_string(&token).unwrap()));
    let payload: String = runtime
        .db
        .query_row("SELECT payload FROM invitations", [], |r| r.get(0))
        .unwrap();
    assert!(!payload.contains(serde_json::to_string(&token).unwrap().trim_matches('"')));
}

#[test]
fn invitation_checks_token_expiry_signer_and_current_inviter_authority() {
    use crate::invitation::{ClaimInvitation, InvitationToken};
    let fixture = Fixture::new();
    let mut runtime = fixture.open();
    let (grant, token) = invitation_grant(&fixture);
    fixture
        .call(
            &mut runtime,
            &fixture.admin,
            RpcRequest::CreateInvitation(Box::new(grant.clone())),
            10,
        )
        .unwrap();
    let keys = LocalDeviceKeys::generate().unwrap();
    let request = keys
        .enrollment_request(
            Some(fixture.config.network.id.clone()),
            DeviceKind::Laptop,
            "new-machine".parse().unwrap(),
        )
        .unwrap();
    let mut claim = ClaimInvitation {
        id: grant.id,
        token: InvitationToken::random().unwrap(),
        request,
    };
    assert!(matches!(
        fixture.call(
            &mut runtime,
            &keys,
            RpcRequest::ClaimInvitation(Box::new(claim.clone())),
            11
        ),
        Err(RuntimeError::Authentication)
    ));
    claim.token = token;
    assert!(matches!(
        fixture.call(
            &mut runtime,
            &fixture.admin,
            RpcRequest::ClaimInvitation(Box::new(claim.clone())),
            12
        ),
        Err(RuntimeError::Authentication)
    ));
    let snapshot = fixture.store.0.borrow().snapshot.clone();
    let revocation = snapshot
        .plan_revocation(
            RevocationId::random().unwrap(),
            &fixture.config.server_id,
            &fixture.config.network.initial_admin,
            13,
        )
        .unwrap();
    fixture
        .store
        .0
        .borrow_mut()
        .snapshot
        .revocations
        .push(revocation);
    assert!(matches!(
        fixture.call(
            &mut runtime,
            &keys,
            RpcRequest::ClaimInvitation(Box::new(claim.clone())),
            13
        ),
        Err(RuntimeError::Denied)
    ));
    assert!(matches!(
        fixture.call(
            &mut runtime,
            &keys,
            RpcRequest::ClaimInvitation(Box::new(claim)),
            200
        ),
        Err(RuntimeError::PendingUnavailable)
    ));
}

#[test]
fn host_recovery_survives_restart_and_lost_administrator_revocation() {
    use crate::invitation::ClaimInvitation;
    let fixture = Fixture::new();
    let mut runtime = fixture.open();
    let mutation = secret_mutation(&fixture, "shop/production", crate::shared::SecretKind::App,
        "TOKEN", Some("surviving-secret"));
    fixture.call(&mut runtime, &fixture.admin, RpcRequest::PublishSecret(Box::new(mutation)), 5).unwrap();
    let (grant, token) = invitation_grant(&fixture);
    runtime.create_recovery_invitation(grant.clone(), 10).unwrap();
    drop(runtime);
    let mut runtime = fixture.open();
    // No surviving client key is needed to issue or claim this invitation.
    let lost = fixture.config.network.initial_admin.clone();
    let revocation = fixture.store.0.borrow().snapshot.plan_revocation(
        RevocationId::random().unwrap(), &fixture.config.server_id, &lost, 11).unwrap();
    fixture.store.0.borrow_mut().snapshot.revocations.push(revocation);
    let keys = LocalDeviceKeys::generate().unwrap();
    let request = keys.enrollment_request(Some(fixture.config.network.id.clone()),
        DeviceKind::Laptop, "replacement".parse().unwrap()).unwrap();
    let claim = ClaimInvitation { id: grant.id.clone(), token, request: request.clone() };
    fixture.call(&mut runtime, &keys, RpcRequest::ClaimInvitation(Box::new(claim.clone())), 12).unwrap();
    // A lost claim response resumes the same identity after another restart.
    drop(runtime);
    let mut runtime = fixture.open();
    fixture.call(&mut runtime, &keys, RpcRequest::ClaimInvitation(Box::new(claim.clone())), 13).unwrap();
    let other = LocalDeviceKeys::generate().unwrap();
    let mut stolen = claim;
    stolen.request = other.enrollment_request(Some(fixture.config.network.id.clone()),
        DeviceKind::Laptop, "other".parse().unwrap()).unwrap();
    assert!(fixture.call(&mut runtime, &other, RpcRequest::ClaimInvitation(Box::new(stolen)), 14).is_err());
    let approval = ApproveRequest {
        pending_id: grant.id, request, kind: MemberKind::Laptop, permissions: grant.permissions,
        expires_at: grant.expires_at, member_expires_at: None,
    };
    assert!(fixture.call(&mut runtime, &keys, RpcRequest::Snapshot, 15).is_err());
    let complete = fixture.challenge(&mut runtime, &keys, &approval, 16);
    fixture.call(&mut runtime, &keys, complete, 17).unwrap();
    let id = keys.device_id(&fixture.config.network.id, DeviceKind::Laptop).unwrap();
    let snapshot = fixture.store.0.borrow().snapshot.clone();
    let members = snapshot.effective_members(18).unwrap();
    assert!(!members.contains_key(&lost));
    assert!(members[&id].independent_admin);
    assert!(members[&id].approved_by.is_none());
    let RpcResponse::Secrets(shared) = fixture.call(&mut runtime, &keys,
        RpcRequest::PullSecrets { scope: None }, 18).unwrap() else { panic!("expected secrets") };
    let plaintext = shared.secrets[0].decrypt(&shared, &keys, &id).unwrap().unwrap();
    let content: crate::shared::SecretContent = serde_json::from_slice(&plaintext).unwrap();
    assert_eq!(&*content.value, "surviving-secret");
    // Even host recovery respects the issuing server's own revocation.
    let revoke = snapshot.plan_revocation(RevocationId::random().unwrap(), &id,
        &fixture.config.server_id, 19).unwrap();
    fixture.store.0.borrow_mut().snapshot.revocations.push(revoke);
    let (grant, _) = invitation_grant(&fixture);
    assert!(runtime.create_recovery_invitation(grant, 20).is_err());
}

#[test]
fn recovery_invitation_expiry_and_public_request_cannot_elevate() {
    use crate::invitation::ClaimInvitation;
    let fixture = Fixture::new();
    let mut runtime = fixture.open();
    let (grant, token) = invitation_grant(&fixture);
    // Public create has the same fields, but always retains delegated authority.
    fixture.call(&mut runtime, &fixture.admin, RpcRequest::CreateInvitation(Box::new(grant.clone())), 10).unwrap();
    assert!(runtime.create_recovery_invitation(grant.clone(), 11).is_err());
    let (recovery, _) = invitation_grant(&fixture);
    let mut invalid = recovery.clone();
    invalid.permissions.server_admin = false;
    assert!(runtime.create_recovery_invitation(invalid, 12).is_err());
    let keys = LocalDeviceKeys::generate().unwrap();
    let request = keys.enrollment_request(Some(fixture.config.network.id.clone()),
        DeviceKind::Laptop, "late".parse().unwrap()).unwrap();
    let claim = ClaimInvitation { id: grant.id.clone(), token, request };
    assert!(fixture.call(&mut runtime, &keys, RpcRequest::ClaimInvitation(Box::new(claim)), 200).is_err());
}
