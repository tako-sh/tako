//! Trusted Unix-only continuation of an already authenticated management action.
//! The original signature/nonce is an identity+action binding retained by the
//! trusted primary, not a new reusable bearer capability for public callers.
use super::{AgentError, Result, lifecycle::Control};
use std::sync::atomic::Ordering;
use tako_network::{
    MemberKind, NodeConfig, RegistrySnapshot,
    api::{ManagementAction, RpcRequest, RpcResponse, RuntimeError, SignedRequest},
};

pub(super) const METHOD: &str = "INTERNAL";
pub(super) const PATH: &str = "/internal/management-authority";

pub(super) fn reserved_route(method: &str, path: &str) -> bool {
    method == METHOD || path == "/internal" || path.starts_with("/internal/")
}
pub(super) fn action(signed: &SignedRequest) -> Result<&ManagementAction> {
    let RpcRequest::Authorize { action } = &signed.request else {
        return Err(RuntimeError::Authentication.into());
    };
    action.validate()?;
    Ok(action)
}
pub(super) fn validate(config: &NodeConfig, signed: &SignedRequest) -> Result<()> {
    if signed.network_id != config.network.id || signed.server_id != config.server_id {
        return Err(RuntimeError::Authentication.into());
    }
    action(signed)?;
    Ok(())
}

/// `snapshot` must be a fresh desired_host_state read whose exact host projection
/// still matches the successful helper ACK. Never call with cached registry data.
pub(super) fn check(
    config: &NodeConfig,
    signed: &SignedRequest,
    snapshot: &RegistrySnapshot,
    now: u64,
    ready: bool,
    control: &Control,
) -> Result<RpcResponse> {
    validate(config, signed)?;
    if !ready || control.stop.load(Ordering::Acquire) || !control.matches(snapshot) {
        return Err(AgentError::Invalid(
            "management authority is not currently enforced",
        ));
    }
    if snapshot.network != config.network {
        return Err(RuntimeError::Authentication.into());
    }
    let effective = snapshot.effective_members(now)?;
    if !effective.get(&config.server_id).is_some_and(|server| {
        matches!(server.kind, MemberKind::Server(_)) && server.permissions.is_full()
    }) {
        return Err(RuntimeError::Denied.into());
    }
    let caller = effective.get(&signed.signer).ok_or(RuntimeError::Denied)?;
    Ok(RpcResponse::Authorized {
        signer: signed.signer.clone(),
        permissions: caller.permissions.clone(),
        action: action(signed)?.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::super::tests::Fixture;
    use super::*;
    use tako_network::{DeviceKind, LocalDeviceKeys, Permissions};

    fn envelope(f: &Fixture, keys: &LocalDeviceKeys) -> SignedRequest {
        SignedRequest::sign(
            keys,
            f.config.network.id.clone(),
            f.config.server_id.clone(),
            DeviceKind::Laptop,
            2,
            RpcRequest::Authorize {
                action: ManagementAction::ReleaseArtifact {
                    scope: Some("api/prod".parse().unwrap()),
                    sha256: "a".repeat(64),
                },
            },
        )
        .unwrap()
    }
    fn checked(
        f: &Fixture,
        signed: &SignedRequest,
        snapshot: &RegistrySnapshot,
        now: u64,
    ) -> Result<RpcResponse> {
        let (control, _, _) = Control::new();
        control.publish_enforced(snapshot);
        check(&f.config, signed, snapshot, now, true, &control)
    }

    #[test]
    fn recheck_preserves_bound_actor_action_and_returns_current_permissions() {
        let f = Fixture::new();
        let signed = envelope(&f, &f.admin);
        // The one-time authentication window is not a second upload deadline.
        let RpcResponse::Authorized {
            signer,
            permissions,
            action: returned,
        } = checked(&f, &signed, &f.config.bootstrap, 500).unwrap()
        else {
            panic!("wrong response")
        };
        assert_eq!(signer, signed.signer);
        assert_eq!(returned, action(&signed).unwrap().clone());
        assert_eq!(
            permissions,
            f.config
                .bootstrap
                .permissions(&signer, 500)
                .unwrap()
                .clone()
        );
    }

    #[test]
    fn recheck_denies_revoked_or_expired_callers_without_cascading() {
        let f = Fixture::new();
        let signed = envelope(&f, &f.admin);
        let mut snapshot = f.config.bootstrap.clone();
        f.revoke(&mut snapshot, &signed.signer);
        assert!(checked(&f, &signed, &snapshot, 3).is_err());
        let mut snapshot = f.config.bootstrap.clone();
        snapshot
            .members
            .iter_mut()
            .find(|m| m.device_id == signed.signer)
            .unwrap()
            .expires_at = Some(3);
        assert!(checked(&f, &signed, &snapshot, 3).is_err());

        let keys = LocalDeviceKeys::generate().unwrap();
        let child = envelope(&f, &keys);
        let mut snapshot = f.config.bootstrap.clone();
        let mut member = snapshot.members[0].clone();
        member.device_id = child.signer.clone();
        member.request = keys
            .enrollment_request(
                Some(f.config.network.id.clone()),
                DeviceKind::Laptop,
                "uploader".parse().unwrap(),
            )
            .unwrap();
        // A client of its own: a copied name would clash with the approver's.
        member.client = Some("uploader".parse().unwrap());
        member.permissions = Permissions::Deploy(["api/prod".parse().unwrap()].into());
        member.approved_by = Some(signed.signer.clone());
        member.independent_root = false;
        snapshot.members.push(member);
        assert!(checked(&f, &child, &snapshot, 2).is_ok());
        f.revoke(&mut snapshot, &signed.signer);
        // Revoking the approver keeps what it approved earlier...
        assert!(checked(&f, &child, &snapshot, 3).is_ok());
        // ...and voids what it approved after the revocation.
        snapshot.members.last_mut().unwrap().created_at = 3;
        assert!(checked(&f, &child, &snapshot, 3).is_err());
    }

    #[test]
    fn recheck_requires_live_serving_server_and_current_enforcement() {
        let f = Fixture::new();
        let signed = envelope(&f, &f.admin);
        for change in 0..4 {
            let mut snapshot = f.config.bootstrap.clone();
            match change {
                0 => f.revoke(&mut snapshot, &f.config.server_id),
                1 => snapshot
                    .members
                    .retain(|m| m.device_id != f.config.server_id),
                2 => snapshot.members[1].expires_at = Some(3),
                _ => {
                    snapshot.members[1].permissions =
                        tako_network::Permissions::Deploy(Default::default());
                }
            }
            assert!(checked(&f, &signed, &snapshot, 3).is_err());
        }
        let (control, _, _) = Control::new();
        assert!(check(&f.config, &signed, &f.config.bootstrap, 3, true, &control).is_err());
        control.publish_enforced(&f.config.bootstrap);
        assert!(check(&f.config, &signed, &f.config.bootstrap, 3, false, &control).is_err());
        let mut changed = f.config.bootstrap.clone();
        f.revoke(&mut changed, &signed.signer);
        assert!(check(&f.config, &signed, &changed, 3, true, &control).is_err());
        control.shutdown();
        assert!(check(&f.config, &signed, &f.config.bootstrap, 3, true, &control).is_err());
    }

    #[test]
    fn recheck_rejects_wrong_audience_non_authorize_and_invalid_action() {
        let f = Fixture::new();
        let mut signed = envelope(&f, &f.admin);
        signed.server_id = signed.signer.clone();
        assert!(validate(&f.config, &signed).is_err());
        signed = envelope(&f, &f.admin);
        signed.network_id = tako_network::NetworkId::random().unwrap();
        assert!(validate(&f.config, &signed).is_err());
        signed = envelope(&f, &f.admin);
        signed.request = RpcRequest::Snapshot;
        assert!(validate(&f.config, &signed).is_err());
        signed.request = RpcRequest::Authorize {
            action: ManagementAction::ReleaseArtifact {
                scope: None,
                sha256: "a".repeat(64),
            },
        };
        assert!(validate(&f.config, &signed).is_err());
        signed.request = RpcRequest::Authorize {
            action: ManagementAction::Rpc {
                scope: None,
                sha256: "invalid".into(),
            },
        };
        assert!(validate(&f.config, &signed).is_err());
    }
}
