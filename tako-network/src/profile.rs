use crate::{
    DeviceId, EnrollmentRequest, Error, LocalDeviceKeys, MemberKind, RegistrySnapshot, Result,
    ServerInfo,
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

impl NetworkPublicProfile {
    pub fn validate_for(&self, keys: &LocalDeviceKeys, now: u64) -> Result<()> {
        self.request.verify()?;
        if self.request.ssh_public_key != keys.ssh_public_key()?
            || self.request.wg_public_key != keys.wg_public_key()?
        {
            return Err(Error::ProfileMismatch);
        }
        let id = self.request.device_id_for(&self.snapshot.network.id)?;
        let effective = self.snapshot.effective_members(now)?;
        let own = effective.get(&id).ok_or(Error::ProfileMismatch)?;
        if own.request != self.request {
            return Err(Error::ProfileMismatch);
        }
        let servers: BTreeMap<_, _> = effective
            .iter()
            .filter_map(|(id, member)| match &member.kind {
                MemberKind::Server(server) => Some((id.clone(), server.clone())),
                MemberKind::Laptop => None,
            })
            .collect();
        if servers.is_empty() || servers != self.trusted_servers {
            return Err(Error::ProfileMismatch);
        }
        Ok(())
    }
}
