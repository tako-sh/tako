//! WireGuard reachability checks for `tako doctor`.
use std::net::SocketAddr;
use std::time::Duration;

use tako_network::{LocalDeviceKeys, MemberKind};
use zeroize::Zeroizing;

use super::ssh_tunnel::{self, Reachability};
use crate::config::LocalNetworks;

/// Covers the first initiation and one BoringTun retry.
pub(crate) const PROBE_TIMEOUT: Duration = Duration::from_secs(6);

pub(crate) struct NetworkReport {
    pub name: String,
    pub id: String,
    pub result: Result<Vec<ServerReport>, String>,
}

pub(crate) struct ServerReport {
    pub name: String,
    pub id: String,
    pub endpoint: SocketAddr,
    pub status: ServerStatus,
}

pub(crate) enum ServerStatus {
    Checked(Reachability),
    /// Probing would move the server's peer endpoint away from an active
    /// transport using the same device identity.
    Skipped(&'static str),
    Error(String),
}

impl ServerStatus {
    pub(crate) fn message(&self, endpoint: SocketAddr) -> Option<String> {
        match self {
            Self::Checked(Reachability::Reachable { .. }) | Self::Skipped(_) => None,
            Self::Checked(Reachability::NoReply) => {
                Some(ssh_tunnel::no_reply_message(endpoint, PROBE_TIMEOUT))
            }
            Self::Checked(Reachability::Refused) => Some(ssh_tunnel::refused_message(endpoint)),
            Self::Error(error) => Some(error.clone()),
        }
    }
}

/// Probes every server in every local network concurrently, from local state only.
/// Fails only when the local network index itself cannot be read.
pub(crate) async fn gather() -> Result<Vec<NetworkReport>, String> {
    let aliases = LocalNetworks::load().map_err(|error| error.to_string())?;
    let reports = aliases.networks.values().map(|alias| async move {
        NetworkReport {
            name: alias.alias.clone(),
            id: alias.id.clone(),
            result: network(alias).await.map_err(|error| error.to_string()),
        }
    });
    Ok(futures_util::future::join_all(reports).await)
}

async fn network(alias: &crate::config::LocalNetwork) -> super::Result<Vec<ServerReport>> {
    let profile = super::load_profile(alias)?;
    let keys = LocalDeviceKeys::load(&super::keys_path(&profile.request().request_id)?)?;
    let network = &profile.snapshot().network.id;
    let mut servers = vec![];
    for (id, member) in super::server_members(&profile)? {
        let MemberKind::Server(info) = &member.kind else {
            unreachable!()
        };
        servers.push((
            id.to_string(),
            member.request.name.to_string(),
            info.wireguard_endpoint,
            member.request.wg_public_key.to_string(),
        ));
    }
    let skip = |reason| {
        servers
            .iter()
            .map(|(id, name, endpoint, _)| ServerReport {
                name: name.clone(),
                id: id.clone(),
                endpoint: *endpoint,
                status: ServerStatus::Skipped(reason),
            })
            .collect()
    };
    // Hold off new transports while probing, and skip while one runs. `connect`
    // holds this lock while it brings its interface up, so the connection state
    // is checked only once the lock is ours.
    let proxies = super::ssh_session::lock(alias, "ssh-proxies.lock")?;
    if proxies.try_lock().is_err() {
        return Ok(skip("`tako ssh` or `tako connect` is active"));
    }
    if ssh_tunnel::local_address_assigned(network.address(&keys.wg_public_key()?))? {
        return Ok(skip("connected with `tako connect`"));
    }
    let private_key = Zeroizing::new(ssh_tunnel::decode_key(&keys.wireguard_private_key()?)?);
    let probes = servers.into_iter().map(|(id, name, endpoint, peer)| {
        let private_key = &private_key;
        async move {
            let status = match ssh_tunnel::decode_key(&peer) {
                Ok(peer) => {
                    match ssh_tunnel::probe(private_key, peer, endpoint, PROBE_TIMEOUT).await {
                        Ok(reachability) => ServerStatus::Checked(reachability),
                        Err(error) => ServerStatus::Error(error.to_string()),
                    }
                }
                Err(error) => ServerStatus::Error(error.to_string()),
            };
            ServerReport {
                name,
                id,
                endpoint,
                status,
            }
        }
    });
    let reports = futures_util::future::join_all(probes).await;
    drop(proxies);
    Ok(reports)
}
