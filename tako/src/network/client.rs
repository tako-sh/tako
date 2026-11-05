use std::collections::BTreeMap;
use std::io::Read;
use std::net::SocketAddr;
use std::time::Duration;

use tako_network::api::{RpcRequest, RpcResponse, SignedRequest};
use tako_network::{
    DeviceId, LocalDeviceKeys, MemberKind, NetworkPublicProfile, RegistrySnapshot, ServerInfo,
};

use super::{Profile, Result};
use crate::config::NetworkAlias;

pub(crate) fn hostname(network: &tako_network::NetworkId, server: &DeviceId) -> String {
    format!(
        "node-{}.{}.tako.internal",
        &server.to_string()[..32],
        network
    )
}

fn trusted_servers(profile: &Profile) -> Result<BTreeMap<DeviceId, ServerInfo>> {
    match profile {
        Profile::Created { .. } => {
            Err("Add the first server before using network management".into())
        }
        Profile::Pending { profile } => Ok(profile.trusted_servers.clone()),
        Profile::Active { profile } => Ok(profile.trusted_servers.clone()),
    }
}

pub(crate) fn call(
    profile: &Profile,
    server: &DeviceId,
    command: RpcRequest,
) -> Result<RpcResponse> {
    let keys = LocalDeviceKeys::load(&super::keys_path(&profile.request().request_id)?)?;
    profile.validate(&keys)?;
    let trusted = trusted_servers(profile)?;
    let info = trusted
        .get(server)
        .ok_or("Server is not trusted by this network profile")?;
    let certificate = reqwest::Certificate::from_pem(
        info.https_certificate_pem
            .as_ref()
            .ok_or("Server has no pinned HTTPS certificate")?
            .as_bytes(),
    )?;
    let network = &profile.snapshot().network.id;
    let host = hostname(network, server);
    let http = reqwest::blocking::Client::builder()
        .tls_certs_only([certificate])
        .resolve(
            &host,
            SocketAddr::new(info.wireguard_endpoint.ip(), info.https_port),
        )
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(20))
        .build()?;
    let path = command.path();
    let request = SignedRequest::sign(
        &keys,
        network.clone(),
        server.clone(),
        profile.request().kind,
        super::now()?,
        command,
    )?;
    let response = http
        .post(format!("https://{host}:{}{path}", info.https_port))
        .json(&request)
        .send()?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!("Network request was rejected by {server} ({status})").into());
    }
    let mut bytes = Vec::new();
    response.take(4 * 1024 * 1024 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > 4 * 1024 * 1024 {
        return Err("Network response exceeds 4 MiB".into());
    }
    Ok(serde_json::from_slice(&bytes)?)
}

pub(crate) fn server(profile: &Profile) -> Result<DeviceId> {
    if let Profile::Pending { profile } = profile {
        return Ok(profile.issuer.clone());
    }
    trusted_servers(profile)?
        .into_keys()
        .next()
        .ok_or_else(|| "Network has no trusted servers".into())
}

pub(crate) fn refresh(alias: &NetworkAlias) -> Result<Profile> {
    let mut profile = super::load_profile(alias)?;
    if let Profile::Pending { profile: pending } = &profile {
        let issuer = pending.issuer.clone();
        let pending_id = pending.pending_id.clone();
        let response = call(
            &profile,
            &issuer,
            RpcRequest::Begin {
                pending_id: pending_id.clone(),
            },
        )?;
        let active = match response {
            RpcResponse::Active(active) => active,
            RpcResponse::Challenge {
                challenge_id,
                challenge,
            } => {
                let keys = LocalDeviceKeys::load(&super::keys_path(&pending.request.request_id)?)?;
                let proof = keys.answer_challenge(&challenge, &pending.request, super::now()?)?;
                match call(
                    &profile,
                    &issuer,
                    RpcRequest::Complete {
                        pending_id,
                        challenge_id,
                        proof,
                    },
                )? {
                    RpcResponse::Active(active) => active,
                    _ => return Err("Unexpected network activation response".into()),
                }
            }
            _ => return Err("Unexpected network enrollment challenge response".into()),
        };
        profile = Profile::Active { profile: active };
        super::save_profile(&alias.name, &profile)?;
    }
    let servers = trusted_servers(&profile)?;
    let mut last_error = None;
    for server in servers.keys() {
        match call(&profile, server, RpcRequest::Snapshot) {
            Ok(RpcResponse::Snapshot(snapshot)) => {
                let refreshed = active_from_snapshot(&profile, snapshot)?;
                super::save_profile(&alias.name, &refreshed)?;
                return Ok(refreshed);
            }
            Ok(_) => return Err("Unexpected network snapshot response".into()),
            Err(error) => last_error = Some(error),
        }
    }
    Err(last_error.unwrap_or_else(|| "No network server is reachable".into()))
}

fn active_from_snapshot(profile: &Profile, mut snapshot: RegistrySnapshot) -> Result<Profile> {
    if snapshot.network != profile.snapshot().network {
        return Err("Server returned a different network identity".into());
    }
    for member in &profile.snapshot().members {
        if !snapshot.members.contains(member) {
            snapshot.members.push(member.clone());
        }
    }
    for revocation in &profile.snapshot().revocations {
        if !snapshot.revocations.contains(revocation) {
            snapshot.revocations.push(revocation.clone());
        }
    }
    for ack in &profile.snapshot().acks {
        if !snapshot.acks.contains(ack) {
            snapshot.acks.push(ack.clone());
        }
    }
    let servers = snapshot
        .effective_members(super::now()?)?
        .into_iter()
        .filter_map(|(id, member)| {
            if let MemberKind::Server(info) = &member.kind {
                Some((id, info.clone()))
            } else {
                None
            }
        })
        .collect();
    let active = Profile::Active {
        profile: NetworkPublicProfile {
            request: profile.request().clone(),
            trusted_servers: servers,
            snapshot,
        },
    };
    active.validate(&LocalDeviceKeys::load(&super::keys_path(
        &active.request().request_id,
    )?)?)?;
    Ok(active)
}
