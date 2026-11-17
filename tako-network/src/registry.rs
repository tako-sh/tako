use crate::{
    AppEnv, BootId, DeviceId, DeviceKind, EnrollmentRequest, Error, KeyFingerprint, NetworkId,
    Result, RevocationId, SshPublicKey, SshUser, VerifiedEnrollment, error::invalid,
};
use openssl::x509::X509;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    net::SocketAddr,
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkDescriptor {
    pub id: NetworkId,
    pub name: crate::NetworkName,
    pub initial_admin: DeviceId,
}

/// What a member may do in its network.
///
/// `Full` members (every server, and clients added without a restriction) may do
/// everything: read and publish every secret, deploy any app, invite and revoke
/// members, add servers, open the `tako` shell and join the private network.
/// `Deploy` clients may only release code to the listed app environments.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Permissions {
    Full,
    Deploy(BTreeSet<AppEnv>),
}

/// The account a full member's SSH key opens on servers.
pub const SSH_USER: &str = "tako";

impl Permissions {
    pub fn deploy(scopes: impl IntoIterator<Item = AppEnv>) -> Self {
        Self::Deploy(scopes.into_iter().collect())
    }

    pub fn validate(&self) -> Result<()> {
        if let Self::Deploy(scopes) = self
            && scopes.len() > 1024
        {
            return Err(invalid("deployment permissions", "too many scopes"));
        }
        Ok(())
    }

    pub fn is_full(&self) -> bool {
        matches!(self, Self::Full)
    }

    /// Whether this member may release code to `scope`.
    pub fn can_deploy(&self, scope: &AppEnv) -> bool {
        match self {
            Self::Full => true,
            Self::Deploy(scopes) => scopes.contains(scope),
        }
    }

    /// Whether this member may deploy anything at all.
    pub fn deploys(&self) -> bool {
        match self {
            Self::Full => true,
            Self::Deploy(scopes) => !scopes.is_empty(),
        }
    }

    /// The scopes of a deploy client; `None` for full members.
    pub fn deploy_scopes(&self) -> Option<&BTreeSet<AppEnv>> {
        match self {
            Self::Full => None,
            Self::Deploy(scopes) => Some(scopes),
        }
    }

    /// The server account this member's SSH key opens, if any.
    pub fn ssh_user(&self) -> Option<SshUser> {
        self.is_full()
            .then(|| SSH_USER.parse().expect("valid SSH user"))
    }

    /// Only full members invite, and they may invite anyone.
    pub(crate) fn can_delegate(&self, _child: &Self) -> bool {
        self.is_full()
    }
}

/// Registry rows written before access levels existed stored four fields.
/// Read them as the access they granted: both administration bits mean full
/// access, anything else keeps only its deployment scopes. New rows are always
/// written in the current form.
impl<'de> Deserialize<'de> for Permissions {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(rename_all = "snake_case")]
        enum Current {
            Full,
            Deploy(BTreeSet<AppEnv>),
        }
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Recorded {
            network_admin: bool,
            server_admin: bool,
            #[allow(dead_code)]
            ssh_user: Option<SshUser>,
            #[serde(default)]
            deployments: BTreeSet<AppEnv>,
        }
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Wire {
            Current(Current),
            Recorded(Recorded),
        }
        Ok(match Wire::deserialize(deserializer)? {
            Wire::Current(Current::Full) => Self::Full,
            Wire::Current(Current::Deploy(scopes)) => Self::Deploy(scopes),
            Wire::Recorded(recorded) if recorded.network_admin && recorded.server_admin => {
                Self::Full
            }
            Wire::Recorded(recorded) => Self::Deploy(recorded.deployments),
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerInfo {
    pub wireguard_endpoint: SocketAddr,
    pub ssh_host_key: SshPublicKey,
    pub ssh_port: u16,
    pub https_certificate_pem: Option<String>,
    pub corrosion_ca_certificate_pem: Option<String>,
    pub https_spki_sha256: Option<KeyFingerprint>,
    pub https_port: u16,
}

impl ServerInfo {
    pub fn https_spki_fingerprint(&self) -> Result<Option<KeyFingerprint>> {
        self.https_certificate_pem
            .as_deref()
            .map(|pem| {
                let certificate = parse_certificate(pem, "HTTPS certificate")?;
                Ok(KeyFingerprint::of_bytes(
                    &certificate.public_key()?.public_key_to_der()?,
                ))
            })
            .transpose()
    }

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
        let derived_spki = self.https_spki_fingerprint()?;
        if self
            .https_spki_sha256
            .as_ref()
            .is_some_and(|pin| Some(pin) != derived_spki.as_ref())
        {
            return Err(invalid("HTTPS SPKI", "must match the supplied certificate"));
        }
        if let Some(pem) = &self.corrosion_ca_certificate_pem {
            parse_certificate(pem, "Corrosion CA certificate")?;
        }
        Ok(())
    }
}

pub const MAX_CERTIFICATE_PEM_BYTES: usize = 16 * 1024;

fn parse_certificate(pem: &str, field: &'static str) -> Result<X509> {
    if pem.is_empty() || pem.len() > MAX_CERTIFICATE_PEM_BYTES {
        return Err(invalid(field, "expected at most 16 KiB of certificate PEM"));
    }
    let certificate = X509::from_pem(pem.as_bytes())
        .map_err(|_| invalid(field, "invalid X.509 certificate PEM"))?;
    let canonical = certificate.to_pem()?;
    let canonical = std::str::from_utf8(&canonical)
        .map_err(|_| invalid(field, "invalid certificate encoding"))?;
    if pem.replace("\r\n", "\n").trim() != canonical.trim() {
        return Err(invalid(
            field,
            "expected one certificate with no extra PEM blocks or data",
        ));
    }
    Ok(certificate)
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
    /// A root of authority with no approver, admitted by a host operator.
    #[serde(alias = "independent_admin")]
    pub independent_root: bool,
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
        self.permissions.validate()?;
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
        if self.independent_root && !self.permissions.is_full() {
            return Err(invalid("independent root", "requires full access"));
        }
        if self.approved_by.as_ref() == Some(&self.device_id)
            || (self.independent_root && self.approved_by.is_some())
        {
            return Err(invalid(
                "approval authority",
                "independent roots require local-operator admission; devices cannot approve themselves",
            ));
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
    #[serde(alias = "independent_admin")]
    pub independent_root: bool,
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

impl Revocation {
    /// The denial of `member`'s device and keys. This checks no authority; use
    /// [`RegistrySnapshot::plan_revocation`] for that.
    pub fn of(
        id: RevocationId,
        member: &Member,
        by: DeviceId,
        captured_servers: BTreeSet<DeviceId>,
        created_at: u64,
    ) -> Result<Self> {
        Ok(Self {
            id,
            target: member.device_id.clone(),
            wg_fingerprint: member.request.wg_public_key.fingerprint(),
            ssh_fingerprint: member.request.ssh_public_key.fingerprint()?,
            captured_servers,
            by,
            created_at,
        })
    }
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
        self.denied_by(member, u64::MAX)
    }

    /// Whether a revocation recorded at or before `at` denies `member`.
    fn denied_by(&self, member: &Member, at: u64) -> Result<bool> {
        let wg = member.request.wg_public_key.fingerprint();
        let ssh = member.request.ssh_public_key.fingerprint()?;
        Ok(self.revocations.iter().any(|denial| {
            denial.created_at <= at
                && (denial.target == member.device_id
                    || denial.wg_fingerprint == wg
                    || denial.ssh_fingerprint == ssh)
        }))
    }

    /// Whether `member` was validly approved. An approval stays valid once made:
    /// the approver must have been live, unrevoked and itself validly approved
    /// when it approved `member`. Revoking an approver later does not affect the
    /// members it approved.
    fn approval_valid<'a>(
        &'a self,
        member: &'a Member,
        verified: &BTreeMap<&'a DeviceId, &'a Member>,
        memo: &mut BTreeMap<&'a DeviceId, Option<bool>>,
    ) -> Result<bool> {
        match memo.get(&member.device_id) {
            Some(Some(valid)) => return Ok(*valid),
            // An approval cycle never reaches a root.
            Some(None) => return Ok(false),
            None => {}
        }
        memo.insert(&member.device_id, None);
        let valid = match &member.approved_by {
            None if member.independent_root => true,
            None => member.device_id == self.network.initial_admin && member.permissions.is_full(),
            Some(parent) => match verified.get(parent) {
                Some(parent) => {
                    member.delegated_by(parent)
                        && parent.live(member.created_at)
                        && !self.denied_by(parent, member.created_at)?
                        && self.approval_valid(parent, verified, memo)?
                }
                None => false,
            },
        };
        memo.insert(&member.device_id, Some(valid));
        Ok(valid)
    }

    /// Evaluate a local view. This is not a global-freshness or enforcement guarantee.
    pub fn effective_members(&self, now: u64) -> Result<BTreeMap<DeviceId, &Member>> {
        self.validate()?;
        let verified: BTreeMap<&DeviceId, &Member> = self
            .members
            .iter()
            .filter(|member| member.wg_verified)
            .map(|member| (&member.device_id, member))
            .collect();
        let mut memo = BTreeMap::new();
        let mut effective = BTreeMap::new();
        for member in verified.values().copied() {
            if member.live(now)
                && !self.denied(member)?
                && self.approval_valid(member, &verified, &mut memo)?
            {
                effective.insert(member.device_id.clone(), member);
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

    pub fn is_full(&self, device: &DeviceId, now: u64) -> Result<bool> {
        Ok(self
            .effective_members(now)?
            .get(device)
            .is_some_and(|member| member.permissions.is_full()))
    }

    pub fn approve(&self, verified: VerifiedEnrollment, approval: Approval) -> Result<Member> {
        self.approve_with_authority(verified, approval, false)
    }

    /// Only call after authenticating a local operator independently of network
    /// device grants, such as a protected root console. A device's full access
    /// is insufficient. This also supports recovery without live full clients.
    pub fn approve_independent_root(
        &self,
        verified: VerifiedEnrollment,
        approval: Approval,
    ) -> Result<Member> {
        if !approval.independent_root || approval.approved_by.is_some() {
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
            independent_root: approval.independent_root,
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
        if member.independent_root && !local_operator {
            return Err(Error::PermissionDenied(
                "device grants cannot create independent roots",
            ));
        }
        match &member.approved_by {
            None if local_operator && member.independent_root => {}
            None => {
                if member.device_id != self.network.initial_admin
                    || !member.permissions.is_full()
                    || !self.members.is_empty()
                {
                    return Err(Error::PermissionDenied(
                        "only the network's first device can bootstrap an empty registry",
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
        // Server names are private DNS names, so active servers must not share one.
        if matches!(member.kind, MemberKind::Server(_))
            && effective.values().any(|existing| {
                existing.device_id != member.device_id
                    && matches!(existing.kind, MemberKind::Server(_))
                    && existing
                        .request
                        .name
                        .as_str()
                        .eq_ignore_ascii_case(member.request.name.as_str())
            })
        {
            return Err(Error::Conflict("server name"));
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
        if !self.permissions(by, now)?.is_full() {
            return Err(Error::PermissionDenied("revocation requires full access"));
        }
        if by == target {
            return Err(Error::PermissionDenied("a device cannot revoke itself"));
        }
        let member = self
            .members
            .iter()
            .find(|member| member.wg_verified && &member.device_id == target)
            .ok_or_else(|| Error::NotEffective(target.clone()))?;
        // Revocation does not cascade, but the network must keep someone who can
        // manage it. Host recovery remains for a network that lost every client.
        let effective = self.effective_members(now)?;
        if effective.contains_key(target)
            && matches!(member.kind, MemberKind::Laptop)
            && member.permissions.is_full()
            && !effective.values().any(|other| {
                &other.device_id != target
                    && matches!(other.kind, MemberKind::Laptop)
                    && other.permissions.is_full()
            })
        {
            return Err(Error::PermissionDenied(
                "revoking the last full client would leave the network without one",
            ));
        }
        let revocation = Revocation::of(id, member, by.clone(), self.server_ids(), now)?;
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
