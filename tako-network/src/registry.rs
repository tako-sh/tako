use crate::{
    AppEnv, BootId, DeviceId, DeviceKind, EnrollmentRequest, Error, KeyFingerprint, NetworkId,
    Result, RevocationId, SshPublicKey, SshUser, VerifiedEnrollment, error::invalid,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    net::SocketAddr,
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkDescriptor {
    pub id: NetworkId,
    pub initial_admin: DeviceId,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Permissions {
    pub network_admin: bool,
    pub server_admin: bool,
    pub ssh_user: Option<SshUser>,
    pub deployments: BTreeSet<AppEnv>,
}

impl Permissions {
    pub fn can_deploy(&self, scope: &AppEnv) -> bool {
        self.deployments.contains(scope)
    }

    fn can_delegate(&self, child: &Self) -> bool {
        self.network_admin
            && (!child.server_admin || self.server_admin)
            && (child.ssh_user.is_none() || self.server_admin || child.ssh_user == self.ssh_user)
            && (self.server_admin || child.deployments.is_subset(&self.deployments))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerInfo {
    pub wireguard_endpoint: SocketAddr,
    pub ssh_host_key: SshPublicKey,
    pub ssh_port: u16,
    pub https_spki_sha256: Option<KeyFingerprint>,
    pub https_port: u16,
}

impl ServerInfo {
    pub fn validate(&self) -> Result<()> {
        let ip = self.wireguard_endpoint.ip().to_canonical();
        if ip.is_unspecified()
            || ip.is_loopback()
            || ip.is_multicast()
            || matches!(ip, std::net::IpAddr::V4(ip) if ip.is_broadcast())
            || matches!(self.wireguard_endpoint, SocketAddr::V6(address) if address.scope_id() != 0 || address.flowinfo() != 0)
            || self.wireguard_endpoint.port() == 0
            || self.ssh_port == 0
            || self.https_port == 0
        {
            return Err(invalid(
                "server endpoint",
                "expected a unicast endpoint and nonzero ports",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum MemberKind {
    Laptop,
    Server(ServerInfo),
}
impl MemberKind {
    pub fn device_kind(&self) -> DeviceKind {
        match self {
            Self::Laptop => DeviceKind::Laptop,
            Self::Server(_) => DeviceKind::Server,
        }
    }
}

/// An immutable admission asserted by a trusted server agent, not client input.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Member {
    pub network_id: NetworkId,
    pub device_id: DeviceId,
    pub request: EnrollmentRequest,
    pub kind: MemberKind,
    pub permissions: Permissions,
    pub approved_by: Option<DeviceId>,
    pub independent_admin: bool,
    pub wg_verified: bool,
    pub created_at: u64,
    pub expires_at: Option<u64>,
}

impl Member {
    pub fn allowed_ip(&self) -> String {
        format!(
            "{}/128",
            self.network_id.address(&self.request.wg_public_key)
        )
    }

    pub fn validate(&self) -> Result<()> {
        self.request.verify()?;
        if self.device_id != self.request.device_id_for(&self.network_id)?
            || self.kind.device_kind() != self.request.kind
        {
            return Err(invalid(
                "member",
                "network, kind, or ID does not match enrollment",
            ));
        }
        if self
            .expires_at
            .is_some_and(|expiry| expiry <= self.created_at)
            || self
                .request
                .expires_at
                .is_some_and(|expiry| expiry <= self.created_at)
        {
            return Err(invalid(
                "member expiry",
                "approval must precede request and membership expiry",
            ));
        }
        if self.independent_admin
            && !(self.permissions.network_admin && self.permissions.server_admin)
        {
            return Err(invalid(
                "independent admin",
                "requires both administrator capabilities",
            ));
        }
        if self.approved_by.as_ref() == Some(&self.device_id)
            || (self.independent_admin && self.approved_by.is_some())
        {
            return Err(invalid(
                "approval authority",
                "independent roots require local-operator admission; devices cannot approve themselves",
            ));
        }
        if self.permissions.deployments.len() > 1024 {
            return Err(invalid("deployment permissions", "too many scopes"));
        }
        if let MemberKind::Server(server) = &self.kind {
            server.validate()?;
        }
        Ok(())
    }

    fn live(&self, now: u64) -> bool {
        self.created_at <= now && self.expires_at.is_none_or(|expiry| now < expiry)
    }

    fn delegated_by(&self, parent: &Member) -> bool {
        parent.created_at <= self.created_at
            && parent.permissions.can_delegate(&self.permissions)
            && (!matches!(self.kind, MemberKind::Server(_)) || parent.permissions.server_admin)
            && match (parent.expires_at, self.expires_at) {
                (Some(parent), Some(child)) => child <= parent,
                (Some(_), None) => false,
                (None, _) => true,
            }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Approval {
    pub kind: MemberKind,
    pub permissions: Permissions,
    pub approved_by: Option<DeviceId>,
    /// Requires the separate independently authenticated local-operator entry point.
    pub independent_admin: bool,
    pub created_at: u64,
    pub expires_at: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Revocation {
    pub id: RevocationId,
    pub target: DeviceId,
    pub wg_fingerprint: KeyFingerprint,
    pub ssh_fingerprint: KeyFingerprint,
    pub captured_servers: BTreeSet<DeviceId>,
    pub by: DeviceId,
    pub created_at: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Ack {
    pub revocation: RevocationId,
    pub server: DeviceId,
    pub boot: BootId,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RegistrySnapshot {
    pub network: NetworkDescriptor,
    pub members: Vec<Member>,
    pub revocations: Vec<Revocation>,
    pub acks: Vec<Ack>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RevocationProgress {
    pub applied_servers: BTreeSet<DeviceId>,
    pub pending_servers: BTreeSet<DeviceId>,
}

impl RegistrySnapshot {
    pub fn validate(&self) -> Result<()> {
        let mut members = BTreeMap::new();
        let mut wg_keys = BTreeMap::new();
        let mut ssh_keys = BTreeMap::new();
        let mut addresses = BTreeMap::new();
        for member in &self.members {
            member.validate()?;
            if member.network_id != self.network.id {
                return Err(invalid("member network", "different network"));
            }
            if !member.wg_verified {
                continue;
            }
            if members
                .insert(&member.device_id, member)
                .is_some_and(|previous| previous != member)
            {
                return Err(Error::Conflict("member"));
            }
            if self.denied(member)? {
                continue;
            }
            if wg_keys
                .insert(&member.request.wg_public_key, &member.device_id)
                .is_some_and(|id| id != &member.device_id)
                || ssh_keys
                    .insert(&member.request.ssh_public_key, &member.device_id)
                    .is_some_and(|id| id != &member.device_id)
            {
                return Err(Error::Conflict("public key binding"));
            }
            let address = self.network.id.address(&member.request.wg_public_key);
            if addresses
                .insert(address, &member.device_id)
                .is_some_and(|id| id != &member.device_id)
            {
                return Err(Error::Conflict("IPv6 address"));
            }
        }
        let mut revocations = BTreeMap::new();
        for revocation in &self.revocations {
            if revocations
                .insert(&revocation.id, revocation)
                .is_some_and(|previous| previous != revocation)
            {
                return Err(Error::Conflict("revocation"));
            }
        }
        Ok(())
    }

    fn denied(&self, member: &Member) -> Result<bool> {
        let wg = member.request.wg_public_key.fingerprint();
        let ssh = member.request.ssh_public_key.fingerprint()?;
        Ok(self.revocations.iter().any(|denial| {
            denial.target == member.device_id
                || denial.wg_fingerprint == wg
                || denial.ssh_fingerprint == ssh
        }))
    }

    /// Evaluate a local view. This is not a global-freshness or enforcement guarantee.
    pub fn effective_members(&self, now: u64) -> Result<BTreeMap<DeviceId, &Member>> {
        self.validate()?;
        let mut pending = BTreeMap::new();
        for member in &self.members {
            if member.wg_verified && member.live(now) && !self.denied(member)? {
                pending.insert(member.device_id.clone(), member);
            }
        }
        let mut effective: BTreeMap<DeviceId, &Member> = BTreeMap::new();
        loop {
            let ready: Vec<_> = pending
                .iter()
                .filter(|(_, member)| match &member.approved_by {
                    None if member.independent_admin => true,
                    None => {
                        member.device_id == self.network.initial_admin
                            && member.permissions.network_admin
                    }
                    Some(parent) => effective
                        .get(parent)
                        .is_some_and(|parent| member.delegated_by(parent)),
                })
                .map(|(id, member)| (id.clone(), *member))
                .collect();
            if ready.is_empty() {
                break;
            }
            for (id, member) in ready {
                pending.remove(&id);
                effective.insert(id, member);
            }
        }
        Ok(effective)
    }

    pub fn permissions(&self, device: &DeviceId, now: u64) -> Result<&Permissions> {
        self.effective_members(now)?
            .get(device)
            .map(|member| &member.permissions)
            .ok_or_else(|| Error::NotEffective(device.clone()))
    }

    pub fn can_admin(&self, device: &DeviceId, now: u64) -> Result<bool> {
        Ok(self
            .effective_members(now)?
            .get(device)
            .is_some_and(|member| member.permissions.network_admin))
    }

    pub fn approve(&self, verified: VerifiedEnrollment, approval: Approval) -> Result<Member> {
        self.approve_with_authority(verified, approval, false)
    }

    /// Only call after authenticating a local operator independently of network
    /// device grants, such as a protected root console. A device's server_admin
    /// permission is insufficient. This also supports recovery without live admins.
    pub fn approve_independent_admin(
        &self,
        verified: VerifiedEnrollment,
        approval: Approval,
    ) -> Result<Member> {
        if !approval.independent_admin || approval.approved_by.is_some() {
            return Err(Error::PermissionDenied(
                "independent approval requires explicit local-operator authority",
            ));
        }
        self.approve_with_authority(verified, approval, true)
    }

    fn approve_with_authority(
        &self,
        verified: VerifiedEnrollment,
        approval: Approval,
        local_operator: bool,
    ) -> Result<Member> {
        let effective = self.effective_members(approval.created_at)?;
        let member = Member {
            network_id: verified.network_id,
            device_id: verified.device_id,
            request: verified.request,
            kind: approval.kind,
            permissions: approval.permissions,
            approved_by: approval.approved_by,
            independent_admin: approval.independent_admin,
            wg_verified: true,
            created_at: approval.created_at,
            expires_at: approval.expires_at,
        };
        member.validate()?;
        if member.network_id != self.network.id {
            return Err(invalid("approval network", "different network"));
        }
        if self.denied(&member)? {
            return Err(Error::PermissionDenied("revocation overrides approval"));
        }
        if member.independent_admin && !local_operator {
            return Err(Error::PermissionDenied(
                "device grants cannot create independent roots",
            ));
        }
        match &member.approved_by {
            None if local_operator && member.independent_admin => {}
            None => {
                if member.device_id != self.network.initial_admin
                    || !member.permissions.network_admin
                    || !self.members.is_empty()
                {
                    return Err(Error::PermissionDenied(
                        "only the descriptor's initial admin can bootstrap an empty registry",
                    ));
                }
            }
            Some(by) => {
                let parent = effective
                    .get(by)
                    .ok_or_else(|| Error::NotEffective(by.clone()))?;
                if !member.delegated_by(parent) {
                    return Err(Error::PermissionDenied(
                        "approval exceeds delegated authority or expiry",
                    ));
                }
            }
        }
        for existing in self.members.iter().filter(|member| member.wg_verified) {
            if existing.device_id == member.device_id {
                return if existing == &member {
                    Ok(existing.clone())
                } else {
                    Err(Error::Conflict("member"))
                };
            }
            if existing.request.wg_public_key == member.request.wg_public_key
                || existing.request.ssh_public_key == member.request.ssh_public_key
            {
                return Err(Error::Conflict("public key binding"));
            }
            if existing.network_id.address(&existing.request.wg_public_key)
                == member.network_id.address(&member.request.wg_public_key)
            {
                return Err(Error::Conflict("IPv6 address"));
            }
        }
        Ok(member)
    }

    pub fn plan_revocation(
        &self,
        id: RevocationId,
        by: &DeviceId,
        target: &DeviceId,
        now: u64,
    ) -> Result<Revocation> {
        let permissions = self.permissions(by, now)?;
        if !permissions.network_admin {
            return Err(Error::PermissionDenied(
                "revocation requires network administration",
            ));
        }
        let member = self
            .members
            .iter()
            .find(|member| member.wg_verified && &member.device_id == target)
            .ok_or_else(|| Error::NotEffective(target.clone()))?;
        if matches!(member.kind, MemberKind::Server(_)) && !permissions.server_admin {
            return Err(Error::PermissionDenied(
                "server revocation requires server administration",
            ));
        }
        let revocation = Revocation {
            id,
            target: target.clone(),
            wg_fingerprint: member.request.wg_public_key.fingerprint(),
            ssh_fingerprint: member.request.ssh_public_key.fingerprint()?,
            captured_servers: self.server_ids(),
            by: by.clone(),
            created_at: now,
        };
        if self
            .revocations
            .iter()
            .any(|previous| previous.id == revocation.id && previous != &revocation)
        {
            return Err(Error::Conflict("revocation"));
        }
        Ok(revocation)
    }

    fn server_ids(&self) -> BTreeSet<DeviceId> {
        self.members
            .iter()
            .filter(|member| member.wg_verified && matches!(member.kind, MemberKind::Server(_)))
            .map(|member| member.device_id.clone())
            .collect()
    }

    /// `current_boots` contains fresh authenticated readiness reports from reachable
    /// agents with successful reconciliation. Remove entries after enforcement failure.
    /// Historical ACKs alone never establish present enforcement or retirement.
    pub fn revocation_progress(
        &self,
        id: &RevocationId,
        current_boots: &BTreeMap<DeviceId, BootId>,
    ) -> Result<RevocationProgress> {
        self.validate()?;
        let revocation = self
            .revocations
            .iter()
            .find(|revocation| &revocation.id == id)
            .ok_or(Error::UnknownRevocation)?;
        let mut required = revocation.captured_servers.clone();
        required.extend(self.server_ids());
        let (applied_servers, pending_servers) = required.into_iter().partition(|server| {
            current_boots.get(server).is_some_and(|boot| {
                self.acks
                    .iter()
                    .any(|ack| &ack.revocation == id && &ack.server == server && &ack.boot == boot)
            })
        });
        Ok(RevocationProgress {
            applied_servers,
            pending_servers,
        })
    }
}
