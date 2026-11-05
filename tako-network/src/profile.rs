use crate::{
    DeviceId, EnrollmentRequest, Error, LocalDeviceKeys, MemberKind, RegistrySnapshot, RequestId,
    Result, ServerInfo, error::invalid,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Public configuration received through authenticated file transfer or a pinned
/// server connection. It cannot authenticate its own origin or registry contents.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkPublicProfile {
    pub request: EnrollmentRequest,
    pub trusted_servers: BTreeMap<DeviceId, ServerInfo>,
    pub snapshot: RegistrySnapshot,
}

/// Public connection instructions for an entitlement stored on the issuing server.
/// This profile grants no membership. Its pending ID is a lookup key, not a bearer
/// credential. Import must authenticate the file before trusting its HTTPS pin.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PendingApprovalProfile {
    pub request: EnrollmentRequest,
    pub pending_id: RequestId,
    pub issuer: DeviceId,
    pub expires_at: u64,
    pub trusted_servers: BTreeMap<DeviceId, ServerInfo>,
    pub snapshot: RegistrySnapshot,
}

impl PendingApprovalProfile {
    pub fn validate_for(&self, keys: &LocalDeviceKeys, now: u64) -> Result<()> {
        validate_device_request(&self.request, keys)?;
        self.request.device_id_for(&self.snapshot.network.id)?;
        if now >= self.expires_at || self.request.expires_at.is_some_and(|expiry| now >= expiry) {
            return Err(invalid("pending approval", "expired"));
        }
        let servers = server_pins(self.snapshot.effective_members(now)?);
        if servers != self.trusted_servers
            || !servers
                .get(&self.issuer)
                .is_some_and(|server| server.https_certificate_pem.is_some())
        {
            return Err(Error::ProfileMismatch);
        }
        Ok(())
    }
}

impl NetworkPublicProfile {
    pub fn validate_for(&self, keys: &LocalDeviceKeys, now: u64) -> Result<()> {
        validate_device_request(&self.request, keys)?;
        let id = self.request.device_id_for(&self.snapshot.network.id)?;
        let effective = self.snapshot.effective_members(now)?;
        let own = effective.get(&id).ok_or(Error::ProfileMismatch)?;
        if own.request != self.request {
            return Err(Error::ProfileMismatch);
        }
        let servers = server_pins(effective);
        if servers.is_empty() || servers != self.trusted_servers {
            return Err(Error::ProfileMismatch);
        }
        Ok(())
    }
}

fn validate_device_request(request: &EnrollmentRequest, keys: &LocalDeviceKeys) -> Result<()> {
    request.verify()?;
    if request.ssh_public_key != keys.ssh_public_key()?
        || request.wg_public_key != keys.wg_public_key()?
    {
        return Err(Error::ProfileMismatch);
    }
    Ok(())
}

fn server_pins(members: BTreeMap<DeviceId, &crate::Member>) -> BTreeMap<DeviceId, ServerInfo> {
    members
        .into_iter()
        .filter_map(|(id, member)| match &member.kind {
            MemberKind::Server(server) => Some((id, server.clone())),
            MemberKind::Laptop => None,
        })
        .collect()
}
