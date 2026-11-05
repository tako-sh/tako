use super::{AgentError, MAX_HELPER_BYTES, Result};
use serde::Serialize;
use std::collections::BTreeSet;
use tako_network::{
    DeviceId, LocalDeviceKeys, MemberKind, NodeConfig, RegistrySnapshot,
    node_config::protected::Directory,
};
use zeroize::Zeroizing;

#[derive(Serialize)]
struct Desired<'a> {
    network_id: String,
    device_id: &'a DeviceId,
    address: String,
    private_key: &'a str,
    listen_port: u16,
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
    if snapshot.network != config.network {
        return Err(AgentError::Invalid("snapshot network mismatch"));
    }
    let effective = snapshot.effective_members(now)?;
    let serving = effective
        .get(&config.server_id)
        .is_some_and(|m| matches!(m.kind, MemberKind::Server(_)) && m.permissions.server_admin);
    let mut peers = Vec::new();
    if serving && !isolate {
        for (id, member) in effective {
            if id == config.server_id {
                continue;
            }
            // The host service intentionally grants only the service account.
            // Unsupported user grants do not become a broader `tako` grant.
            let ssh = member
                .permissions
                .ssh_user
                .as_ref()
                .filter(|u| u.as_str() == "tako");
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
        peers,
    })?);
    if bytes.len() > MAX_HELPER_BYTES {
        return Err(AgentError::Invalid("desired state exceeds limit"));
    }
    Ok(bytes)
}

/// CA trust and bootstrap addresses include effective servers only, in stable order.
pub(super) fn gossip(
    config: &NodeConfig,
    snapshot: &RegistrySnapshot,
    now: u64,
) -> Result<(String, Vec<String>)> {
    let mut cas = BTreeSet::new();
    let mut seeds = Vec::new();
    for (id, member) in snapshot.effective_members(now)? {
        if let MemberKind::Server(server) = &member.kind {
            let ca = server
                .corrosion_ca_certificate_pem
                .as_deref()
                .ok_or(AgentError::Invalid("effective server lacks gossip CA"))?;
            cas.insert(ca.to_owned());
            if id != config.server_id {
                seeds.push(format!(
                    "[{}]:8787",
                    config.network.id.address(&member.request.wg_public_key)
                ));
            }
        }
    }
    if cas.is_empty() {
        return Err(AgentError::Invalid("no effective gossip issuers"));
    }
    let bundle = cas
        .into_iter()
        .map(|pem| format!("{}\n", pem.trim()))
        .collect();
    Ok((bundle, seeds))
}

pub(super) fn write_corrosion(
    config: &NodeConfig,
    data_dir: &std::path::Path,
    directory: &Directory,
    keys: &LocalDeviceKeys,
    bundle: &str,
    seeds: &[String],
    token: &str,
) -> Result<()> {
    write_corrosion_at(
        config,
        data_dir,
        directory,
        config.network.id.address(&keys.wg_public_key()?),
        bundle,
        seeds,
        token,
    )
}

/// No peer transport survives self-revocation. Keep the authenticated SQL API
/// usable for exact durable-denial retries, without granting network authority.
pub(super) fn write_repair_corrosion(
    config: &NodeConfig,
    data_dir: &std::path::Path,
    directory: &Directory,
    token: &str,
) -> Result<()> {
    let ca = directory.read(&config.files.gossip_ca_certificate, 16 * 1024)?;
    let ca =
        std::str::from_utf8(&ca).map_err(|_| AgentError::Invalid("invalid repair CA encoding"))?;
    write_corrosion_at(
        config,
        data_dir,
        directory,
        std::net::Ipv6Addr::LOCALHOST,
        ca,
        &[],
        token,
    )
}

fn write_corrosion_at(
    config: &NodeConfig,
    data_dir: &std::path::Path,
    directory: &Directory,
    ip: std::net::Ipv6Addr,
    bundle: &str,
    seeds: &[String],
    token: &str,
) -> Result<()> {
    let root = config.network_dir(data_dir);
    let path = |name: &str| -> Result<String> {
        Ok(root
            .join(name)
            .to_str()
            .ok_or(AgentError::Invalid("non-UTF8 node path"))?
            .to_owned())
    };
    // TOML quoted strings and arrays use JSON's compatible subset for these
    // validated ASCII paths, IP literals and bearer bytes (not user snippets).
    let q = |s: &str| serde_json::to_string(s);
    let text = Zeroizing::new(format!(
        "[db]\npath = {}\nschema_paths = [{}]\nsubscriptions_path = {}\n\
         [api]\naddr = \"127.0.0.1:9834\"\nauthz.bearer-token = {}\n\
         [gossip]\naddr = \"[{ip}]:8787\"\nclient_addr = \"[{ip}]:0\"\nbootstrap = {}\nplaintext = false\n\
         [gossip.tls]\ncert_file = {}\nkey_file = {}\nca_file = {}\ninsecure = false\n\
         [gossip.tls.client]\ncert_file = {}\nkey_file = {}\n\
         [admin]\npath = {}\n[log]\nformat = \"json\"\n",
        q(&path("corrosion.db")?)?,
        q(&path("corrosion.schema.sql")?)?,
        q(&path("subscriptions")?)?,
        q(token)?,
        serde_json::to_string(seeds)?,
        q(&path(&config.files.gossip_certificate)?)?,
        q(&path(&config.files.gossip_private_key)?)?,
        q(&path("corrosion-ca.pem")?)?,
        q(&path(&config.files.gossip_certificate)?)?,
        q(&path(&config.files.gossip_private_key)?)?,
        q(&path("corrosion-admin.sock")?)?
    ));
    directory.write("corrosion-ca.pem", bundle.as_bytes())?;
    directory.write(
        "corrosion.schema.sql",
        format!(
            "{}\n{}\n",
            tako_network::corrosion::CORROSION_SCHEMA,
            tako_network::services::SERVICE_SCHEMA
        )
        .as_bytes(),
    )?;
    directory.write("corrosion.toml", text.as_bytes())?;
    Ok(())
}
