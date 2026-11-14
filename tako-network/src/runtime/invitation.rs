use super::*;
use crate::invitation::{ClaimInvitation, CreateInvitation};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredInvitation {
    grant: CreateInvitation,
    approver: DeviceId,
    #[serde(default)]
    independent_admin: bool,
    claim: Option<crate::EnrollmentRequest>,
}

impl<S: RegistryStore> ServerRuntime<S> {
    pub(super) fn create_invitation(
        &mut self,
        grant: CreateInvitation,
        approver: DeviceId,
        snapshot: RegistrySnapshot,
        now: u64,
    ) -> RuntimeResult<RpcResponse> {
        self.store_invitation(grant, approver, false, snapshot, now)
    }

    /// Host-console recovery only. The caller MUST authenticate the local root
    /// operator independently of network grants. No public RPC calls this.
    pub fn create_recovery_invitation(
        &mut self,
        grant: CreateInvitation,
        now: u64,
    ) -> RuntimeResult<RpcResponse> {
        self.advance_clock(now)?;
        let snapshot = self.snapshot()?;
        self.require_server(&snapshot, now)?;
        if !grant.permissions.network_admin
            || !grant.permissions.server_admin
            || grant.member_expires_at.is_some()
        {
            return Err(RuntimeError::Denied);
        }
        self.store_invitation(grant, self.config.server_id.clone(), true, snapshot, now)
    }

    fn store_invitation(
        &mut self,
        grant: CreateInvitation,
        approver: DeviceId,
        independent_admin: bool,
        snapshot: RegistrySnapshot,
        now: u64,
    ) -> RuntimeResult<RpcResponse> {
        grant.permissions.validate()?;
        if !independent_admin {
            let parent = snapshot
                .effective_members(now)?
                .get(&approver)
                .copied()
                .ok_or(RuntimeError::Denied)?;
            if !parent.permissions.can_delegate(&grant.permissions)
                || parent.expires_at.is_some_and(|expiry| {
                    grant.member_expires_at.is_none_or(|child| child > expiry)
                })
            {
                return Err(RuntimeError::Denied);
            }
        }
        if grant.expires_at <= now
            || grant.expires_at > now + 86400
            || grant
                .member_expires_at
                .is_some_and(|expiry| expiry <= grant.expires_at)
            || grant.token_hash.len() != 64
            || !grant
                .token_hash
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(RuntimeError::Invalid("invitation lifetime or token hash"));
        }
        if !server_pins(&snapshot, now)?
            .get(&self.config.server_id)
            .is_some_and(|s| s.https_certificate_pem.is_some())
        {
            return Err(RuntimeError::Invalid(
                "invitation issuer requires HTTPS pin",
            ));
        }
        self.db.execute(
            "DELETE FROM invitations WHERE expires<=?1",
            [sql_time(now)?],
        )?;
        let previous: Option<String> = self
            .db
            .query_row(
                "SELECT payload FROM invitations WHERE id=?1",
                [grant.id.to_string()],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(previous) = previous {
            let previous: StoredInvitation = serde_json::from_str(&previous)?;
            if previous.grant != grant
                || previous.approver != approver
                || previous.independent_admin != independent_admin
            {
                return Err(RuntimeError::Conflict("invitation ID"));
            }
        } else {
            let count: i64 = self
                .db
                .query_row("SELECT count(*) FROM invitations", [], |r| r.get(0))?;
            if count >= self.config.limits.max_pending as i64 {
                return Err(RuntimeError::Capacity("invitations"));
            }
            self.db.execute(
                "INSERT INTO invitations VALUES (?1, ?2, ?3)",
                params![
                    grant.id.to_string(),
                    sql_time(grant.expires_at)?,
                    serde_json::to_string(&StoredInvitation {
                        grant: grant.clone(),
                        approver,
                        independent_admin,
                        claim: None
                    })?
                ],
            )?;
        }
        Ok(RpcResponse::InvitationCreated { id: grant.id })
    }

    pub(super) fn claim_invitation(
        &mut self,
        claim: ClaimInvitation,
        snapshot: RegistrySnapshot,
        now: u64,
    ) -> RuntimeResult<RpcResponse> {
        claim.request.verify()?;
        if claim.request.kind != DeviceKind::Laptop
            || claim.request.network_id.as_ref() != Some(&self.config.network.id)
        {
            return Err(RuntimeError::Denied);
        }
        let payload: Option<String> = self
            .db
            .query_row(
                "SELECT payload FROM invitations WHERE id=?1 AND expires>?2",
                params![claim.id.to_string(), sql_time(now)?],
                |r| r.get(0),
            )
            .optional()?;
        let mut invitation: StoredInvitation =
            serde_json::from_str(&payload.ok_or(RuntimeError::PendingUnavailable)?)?;
        if !openssl::memcmp::eq(
            claim.token.hash()?.as_bytes(),
            invitation.grant.token_hash.as_bytes(),
        ) {
            return Err(RuntimeError::Authentication);
        }
        if invitation
            .claim
            .as_ref()
            .is_some_and(|previous| previous != &claim.request)
        {
            return Err(RuntimeError::Conflict("invitation already claimed"));
        }
        let grant = ApproveRequest {
            pending_id: invitation.grant.id.clone(),
            request: claim.request.clone(),
            kind: MemberKind::Laptop,
            permissions: invitation.grant.permissions.clone(),
            expires_at: invitation.grant.expires_at,
            member_expires_at: invitation.grant.member_expires_at,
        };
        self.check_grant(
            &Entitlement {
                grant: grant.clone(),
                approver: invitation.approver.clone(),
                independent_admin: invitation.independent_admin,
                candidate: None,
            },
            &snapshot,
            now,
        )?;
        // Bind before any next step. A failed response can only resume this same
        // signed enrollment; the token cannot admit a second device.
        invitation.claim = Some(claim.request);
        self.db.execute(
            "UPDATE invitations SET payload=?1 WHERE id=?2",
            params![serde_json::to_string(&invitation)?, claim.id.to_string()],
        )?;
        self.approve_pending_with_authority(
            grant,
            invitation.approver,
            invitation.independent_admin,
            snapshot,
            now,
        )
    }
}
