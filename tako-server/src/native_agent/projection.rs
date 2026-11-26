use super::{AgentError, MAX_HELPER_BYTES, Result};
use serde::Serialize;

use tako_network::{DeviceId, LocalDeviceKeys, MemberKind, NodeConfig, RegistrySnapshot};
use zeroize::Zeroizing;

#[derive(Serialize)]
struct Desired<'a> {
    network_id: String,
    device_id: &'a DeviceId,
    address: String,
    private_key: &'a str,
    listen_port: u16,
    ssh_port: u16,
    peers: Vec<Peer>,
}
#[derive(Serialize)]
struct Peer {
    device_id: DeviceId,
    name: String,
    public_key: String,
    address: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    endpoint: Option<std::net::SocketAddr>,
    kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    ssh_user: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ssh_public_key: Option<String>,
}

/// Exact host-helper contract. `isolate` closes all peer/SSH access on uncertainty.
pub(super) fn desired(
    config: &NodeConfig,
    keys: &LocalDeviceKeys,
    snapshot: &RegistrySnapshot,
    now: u64,
    isolate: bool,
) -> Result<Zeroizing<Vec<u8>>> {
    projected(config, keys, snapshot, now, isolate, true)
}

/// Bring up only authenticated server peers while the catalog catches up. Client
/// SSH and app authority remain withdrawn until a quorum confirms membership.
pub(super) fn recovery(
    config: &NodeConfig,
    keys: &LocalDeviceKeys,
    snapshot: &RegistrySnapshot,
    now: u64,
) -> Result<Zeroizing<Vec<u8>>> {
    projected(config, keys, snapshot, now, false, false)
}
fn projected(
    config: &NodeConfig,
    keys: &LocalDeviceKeys,
    snapshot: &RegistrySnapshot,
    now: u64,
    isolate: bool,
    clients: bool,
) -> Result<Zeroizing<Vec<u8>>> {
    if snapshot.network != config.network {
        return Err(AgentError::Invalid("snapshot network mismatch"));
    }
    // This is the local sshd listener, pinned by the prepared server identity.
    // Keep it available even when the local record is denied/ineffective; peer
    // SSH endpoints never determine the host's INPUT port.
    let local = config
        .bootstrap
        .members
        .iter()
        .find(|member| member.device_id == config.server_id)
        .ok_or(AgentError::Invalid("local server missing from bootstrap"))?;
    let MemberKind::Server(local) = &local.kind else {
        return Err(AgentError::Invalid("local SSH identity is not a server"));
    };
    let ssh_port = local.ssh_port;
    let effective = snapshot.effective_members(now)?;
    let serving = effective
        .get(&config.server_id)
        .is_some_and(|m| matches!(m.kind, MemberKind::Server(_)) && m.permissions.is_full());
    let mut peers = Vec::new();
    if serving && !isolate {
        for (id, member) in effective {
            // Deploy clients manage over pinned HTTPS only: no peer, no shell.
            if id == config.server_id
                || !member.permissions.is_full()
                || (!clients && !matches!(member.kind, MemberKind::Server(_)))
            {
                continue;
            }
            // The host service grants only the service account, and only to
            // full clients. Servers manage each other without a shell.
            let ssh = matches!(member.kind, MemberKind::Laptop)
                .then(|| member.permissions.ssh_user())
                .flatten();
            let ssh = ssh.as_ref();
            peers.push(Peer {
                device_id: id,
                name: member.request.name.as_str().to_owned(),
                public_key: member.request.wg_public_key.to_string(),
                address: member.allowed_ip(),
                endpoint: match &member.kind {
                    MemberKind::Server(s) => Some(s.wireguard_endpoint),
                    _ => None,
                },
                kind: match member.kind {
                    MemberKind::Server(_) => "server",
                    MemberKind::Laptop => "laptop",
                },
                ssh_user: ssh.map(|u| u.as_str().to_owned()),
                ssh_public_key: ssh.map(|_| member.request.ssh_public_key.as_str().to_owned()),
            });
        }
    }
    if peers.len() > 256 {
        return Err(AgentError::Invalid("host peer limit exceeded"));
    }
    let secret = keys.wireguard_private_key()?;
    let bytes = Zeroizing::new(serde_json::to_vec(&Desired {
        network_id: config.network.id.to_string(),
        device_id: &config.server_id,
        address: format!("{}/128", config.network.id.address(&keys.wg_public_key()?)),
        private_key: &secret,
        listen_port: config.wireguard_listen_port,
        ssh_port,
        peers,
    })?);
    if bytes.len() > MAX_HELPER_BYTES {
        return Err(AgentError::Invalid("desired state exceeds limit"));
    }
    Ok(bytes)
}
