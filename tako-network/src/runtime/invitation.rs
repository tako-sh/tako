use super::*;
use crate::invitation::{ClaimInvitation, CreateInvitation};
use subtle::ConstantTimeEq;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredInvitation {
    grant: CreateInvitation,
    approver: DeviceId,
    #[serde(default)]
    independent_root: bool,
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
        if !grant.permissions.is_full() || grant.member_expires_at.is_some() {
            return Err(RuntimeError::Denied);
        }
        // Recovery adds a client; replacing an active one goes through a full client.
        let names = snapshot.client_names();
        if snapshot.effective_members(now)?.keys().any(|id| {
            names
                .get(id)
                .is_some_and(|name| name.eq_ignore_ascii_case(grant.client.as_str()))
        }) {
            return Err(RuntimeError::Conflict("client name"));
        }
        self.store_invitation(grant, self.config.server_id.clone(), true, snapshot, now)
    }

    fn store_invitation(
        &mut self,
        grant: CreateInvitation,
        approver: DeviceId,
        independent_root: bool,
        snapshot: RegistrySnapshot,
        now: u64,
    ) -> RuntimeResult<RpcResponse> {
        grant.permissions.validate()?;
        if !independent_root {
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
        // One write transaction: expired invitations are collected, then this
        // one is matched or added; an error leaves the journal unchanged.
        let id = grant.id.to_string();
        let txn = write(&self.db)?;
        INVITATIONS.purge_through(&txn, journal_time(now)?)?;
        let previous = txn
            .open_table(INVITATIONS.0)?
            .get(id.as_str())?
            .map(|row| row.value().1.to_owned());
        if let Some(previous) = previous {
            let previous: StoredInvitation = serde_json::from_str(&previous)?;
            if previous.grant != grant
                || previous.approver != approver
                || previous.independent_root != independent_root
            {
                return Err(RuntimeError::Conflict("invitation ID"));
            }
        } else {
            if INVITATIONS.count(&txn)? >= self.config.limits.max_pending as u64 {
                return Err(RuntimeError::Capacity("invitations"));
            }
            INVITATIONS.put(
                &txn,
                &id,
                journal_time(grant.expires_at)?,
                &serde_json::to_string(&StoredInvitation {
                    grant: grant.clone(),
                    approver,
                    independent_root,
                    claim: None,
                })?,
            )?;
        }
        txn.commit()?;
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
        let live = journal_time(now)?;
        let payload = INVITATIONS
            .get(&self.db, &claim.id.to_string())?
            .and_then(|(expires, payload)| (expires > live).then_some(payload))
            .ok_or(RuntimeError::PendingUnavailable)?;
        let mut invitation: StoredInvitation = serde_json::from_str(&payload)?;
        if !bool::from(
            claim
                .token
                .hash()?
                .as_bytes()
                .ct_eq(invitation.grant.token_hash.as_bytes()),
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
            client: Some(invitation.grant.client.clone()),
        };
        self.check_grant(
            &Entitlement {
                grant: grant.clone(),
                approver: invitation.approver.clone(),
                independent_root: invitation.independent_root,
                candidate: None,
            },
            &snapshot,
            now,
        )?;
        // Bind before any next step. A failed response can only resume this same
        // signed enrollment; the token cannot admit a second device.
        invitation.claim = Some(claim.request);
        let txn = write(&self.db)?;
        INVITATIONS.update(
            &txn,
            &claim.id.to_string(),
            &serde_json::to_string(&invitation)?,
        )?;
        txn.commit()?;
        self.approve_pending_with_authority(
            grant,
            invitation.approver,
            invitation.independent_root,
            snapshot,
            now,
        )
    }
}
