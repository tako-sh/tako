use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::Path;

use serde::{Deserialize, Serialize};
use tako_network::api::{ApproveRequest, RpcRequest, RpcResponse};
use tako_network::{
    Approval, BootstrapRequest, BootstrapResponse, DeviceId, DeviceName, EnrollmentRequest,
    MemberKind, NetworkDescriptor, NetworkPublicProfile, Permissions, RegistrySnapshot, RequestId,
    ServerInfo, WgChallenge,
};

use crate::network::{self, Profile, Result};
use crate::output;
use crate::ssh::{ServerInstallPorts, SshClient, SshConfig};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Enrollment {
    network: NetworkDescriptor,
    request: EnrollmentRequest,
    server: ServerInfo,
    stage: Stage,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "stage", rename_all = "snake_case", deny_unknown_fields)]
enum Stage {
    Approval {
        issuer: DeviceId,
        grant: Box<ApproveRequest>,
    },
    Admitted {
        snapshot: RegistrySnapshot,
    },
}

/// The server-side rule: UDP 443 is an explicit WireGuard option independent
/// of HTTPS, and 8787 is reserved for gossip.
fn validate_wireguard_port(port: u16) -> Result<()> {
    if (port < 1024 && port != 443) || port == 8787 {
        return Err(
            "WireGuard must use UDP 443 or a port from 1024 to 65535 other than 8787".into(),
        );
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn add(
    host: &str,
    operator: &str,
    network: &str,
    name: Option<&str>,
    ssh_port: u16,
    key: Option<&Path>,
    ports: ServerInstallPorts,
    endpoint: Option<SocketAddr>,
) -> Result<()> {
    let alias = network::resolve_network(Some(network), None)?;
    let profile = network::load_profile(&alias)?;
    let default_name = host.split('.').next().unwrap_or("server").replace(':', "-");
    let name: DeviceName = name.unwrap_or(&default_name).parse()?;
    if output::is_dry_run() {
        output::success("Would install and enroll the server in the native network");
        if output::is_json() {
            output::json_result(
                serde_json::json!({"dry_run": true, "network": alias.name, "server": name, "host": host, "operator": operator}),
            )?;
        }
        return Ok(());
    }
    let profile = if matches!(profile, Profile::Created { .. }) {
        profile
    } else {
        tokio::task::block_in_place(|| network::client::refresh(&alias))?
    };
    let own = profile
        .request()
        .device_id_for(&profile.snapshot().network.id)?;
    if !profile.snapshot().can_admin(&own, network::now()?)? {
        return Err("Network administration permission is required to add a server".into());
    }
    let endpoint = match endpoint {
        Some(endpoint) => endpoint,
        None => tokio::net::lookup_host((host.trim_matches(['[', ']']), 51820))
            .await?
            .next()
            .ok_or("The server hostname has no IP address; pass --wireguard-endpoint")?,
    };
    validate_wireguard_port(endpoint.port())?;
    let mut ssh = SshClient::new(SshConfig::for_user(host, ssh_port, operator).with_key_path(key));
    ssh.connect().await?;
    output::info("Installing native network components");
    ssh.install_native_tako_server(ports).await?;
    let network_id = profile.snapshot().network.id.clone();
    let prepared = ssh
        .native_bootstrap(&BootstrapRequest::Prepare {
            network: network_id.clone(),
            name,
            wireguard_endpoint: endpoint,
            ssh_port,
            https_port: ports.https_port,
        })
        .await?;
    let BootstrapResponse::Prepared { request, server } = prepared else {
        return Err("Unexpected server preparation response".into());
    };
    request.verify()?;
    server.validate()?;
    let device = request.device_id_for(&network_id)?;
    if request.kind != tako_network::DeviceKind::Server || server.wireguard_endpoint != endpoint {
        return Err("Prepared server identity does not match the requested endpoint".into());
    }
    let directory = alias
        .profile_path()?
        .parent()
        .unwrap()
        .join("server-enrollments");
    network::private_directory(&directory)?;
    let journal_path = directory.join(format!("{device}.json"));
    let mut enrollment = if journal_path.try_exists()? {
        let saved: Enrollment = network::read_json(&journal_path)?;
        if saved.network != profile.snapshot().network
            || saved.request != request
            || saved.server != server
        {
            return Err("Saved enrollment does not match the prepared server identity".into());
        }
        saved
    } else {
        let now = network::now()?;
        let stage = if let Some(existing) = profile.snapshot().effective_members(now)?.get(&device)
        {
            if existing.request != request || existing.kind != MemberKind::Server(server.clone()) {
                return Err("This server identity has a conflicting network membership".into());
            }
            Stage::Admitted {
                snapshot: profile.snapshot().clone(),
            }
        } else if matches!(profile, Profile::Created { .. }) {
            let (challenge, pending) =
                WgChallenge::begin(&request, network_id.clone(), now, now + 60)?;
            let BootstrapResponse::Proof { proof } = ssh
                .native_bootstrap(&BootstrapRequest::Answer {
                    network: network_id.clone(),
                    challenge,
                })
                .await?
            else {
                return Err("Unexpected server key proof response".into());
            };
            let verified = pending.verify(&proof, network::now()?)?;
            let mut snapshot = profile.snapshot().clone();
            let member = snapshot.approve_independent_admin(
                verified,
                Approval {
                    kind: MemberKind::Server(server.clone()),
                    permissions: server_permissions(),
                    approved_by: None,
                    independent_admin: true,
                    created_at: now,
                    expires_at: None,
                },
            )?;
            snapshot.members.push(member);
            Stage::Admitted { snapshot }
        } else {
            Stage::Approval {
                issuer: tokio::task::block_in_place(|| {
                    network::client::available_server(&profile)
                })?,
                grant: Box::new(ApproveRequest {
                    pending_id: RequestId::random()?,
                    request: request.clone(),
                    kind: MemberKind::Server(server.clone()),
                    permissions: server_permissions(),
                    expires_at: now + 86400,
                    member_expires_at: None,
                }),
            }
        };
        let enrollment = Enrollment {
            network: profile.snapshot().network.clone(),
            request,
            server,
            stage,
        };
        network::write_json(&journal_path, &enrollment, false)?;
        enrollment
    };
    if let Stage::Approval { grant, .. } = &enrollment.stage {
        let effective = profile.snapshot().effective_members(network::now()?)?;
        if let Some(member) = effective.get(&device) {
            if member.request != enrollment.request
                || member.kind != MemberKind::Server(enrollment.server.clone())
            {
                return Err("Activated server does not match its saved enrollment".into());
            }
            enrollment.stage = Stage::Admitted {
                snapshot: profile.snapshot().clone(),
            };
            network::write_json(&journal_path, &enrollment, true)?;
        } else if grant.expires_at <= network::now()? {
            let mut grant = grant.clone();
            grant.pending_id = RequestId::random()?;
            grant.expires_at = network::now()? + 86400;
            enrollment.stage = Stage::Approval {
                issuer: tokio::task::block_in_place(|| {
                    network::client::available_server(&profile)
                })?,
                grant,
            };
            network::write_json(&journal_path, &enrollment, true)?;
        }
    }
    if let Stage::Approval { issuer, grant } = &enrollment.stage {
        let response = tokio::task::block_in_place(|| {
            network::client::call(&profile, issuer, RpcRequest::Approve(grant.clone()))
        })?;
        let RpcResponse::Pending(pending) = response else {
            return Err("Unexpected server approval response".into());
        };
        crate::commands::network::validate_pending_approval(&profile, issuer, grant, &pending)?;
        let response = remote_call(
            &ssh,
            &profile,
            issuer,
            RpcRequest::Begin {
                pending_id: pending.pending_id.clone(),
            },
        )
        .await?;
        let active = match response {
            RpcResponse::Active(active) => active,
            RpcResponse::Challenge {
                challenge_id,
                challenge,
            } => {
                let BootstrapResponse::Proof { proof } = ssh
                    .native_bootstrap(&BootstrapRequest::Answer {
                        network: network_id.clone(),
                        challenge,
                    })
                    .await?
                else {
                    return Err("Unexpected server key proof response".into());
                };
                match remote_call(
                    &ssh,
                    &profile,
                    issuer,
                    RpcRequest::Complete {
                        pending_id: pending.pending_id,
                        challenge_id,
                        proof,
                    },
                )
                .await?
                {
                    RpcResponse::Active(active) => active,
                    _ => return Err("Unexpected server activation response".into()),
                }
            }
            _ => return Err("Unexpected server challenge response".into()),
        };
        if active.request != enrollment.request || active.snapshot.network != enrollment.network {
            return Err("Server activation response changed the enrollment identity".into());
        }
        enrollment.stage = Stage::Admitted {
            snapshot: active.snapshot,
        };
        network::write_json(&journal_path, &enrollment, true)?;
    }
    let Stage::Admitted { snapshot } = &enrollment.stage else {
        unreachable!()
    };
    let mut snapshot = snapshot.clone();
    merge_known_records(&mut snapshot, profile.snapshot());
    let members = snapshot.effective_members(network::now()?)?;
    if !members.contains_key(&device) {
        return Err(
            "This server was revoked or its enrollment authority is no longer active".into(),
        );
    }
    let trusted_servers: BTreeMap<_, _> = members
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
            trusted_servers,
            snapshot: snapshot.clone(),
        },
    };
    let BootstrapResponse::Installed {
        network: installed,
        device: installed_device,
    } = ssh
        .native_bootstrap(&BootstrapRequest::Install { snapshot })
        .await?
    else {
        return Err("Unexpected server installation response".into());
    };
    if installed != network_id || installed_device != device {
        return Err("Installed server identity does not match the approved server".into());
    }
    ssh.start_native_tako_server(ports, network_id.address(&enrollment.request.wg_public_key))
        .await?;
    network::save_profile(&alias.name, &active)?;
    output::success(&format!(
        "Added {} to {}",
        enrollment.request.name, alias.name
    ));
    if output::is_json() {
        output::json_result(
            serde_json::json!({"network": alias.name, "id": network_id, "server": device, "name": enrollment.request.name}),
        )?;
    }
    Ok(())
}

fn server_permissions() -> Permissions {
    Permissions {
        network_admin: true,
        server_admin: true,
        ssh_user: None,
        deployments: Default::default(),
    }
}

async fn remote_call(
    ssh: &SshClient,
    profile: &Profile,
    issuer: &DeviceId,
    command: RpcRequest,
) -> Result<RpcResponse> {
    let member = network::server_members(profile)?
        .remove(issuer)
        .ok_or("Enrollment issuer is no longer a server")?;
    let MemberKind::Server(info) = member.kind else {
        unreachable!()
    };
    let BootstrapResponse::Signed { request } = ssh
        .native_bootstrap(&BootstrapRequest::Sign {
            network: profile.snapshot().network.id.clone(),
            server: issuer.clone(),
            request: command,
        })
        .await?
    else {
        return Err("Unexpected signed enrollment response".into());
    };
    tokio::task::block_in_place(|| network::client::send(&info, &request))
}

fn merge_known_records(snapshot: &mut RegistrySnapshot, known: &RegistrySnapshot) {
    for member in &known.members {
        if !snapshot.members.contains(member) {
            snapshot.members.push(member.clone());
        }
    }
    for revocation in &known.revocations {
        if !snapshot.revocations.contains(revocation) {
            snapshot.revocations.push(revocation.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::validate_wireguard_port;

    #[test]
    fn wireguard_endpoint_accepts_udp_443_and_rejects_reserved_ports() {
        for port in [443, 1024, 51820, 65535] {
            assert!(validate_wireguard_port(port).is_ok(), "{port}");
        }
        for port in [0, 22, 80, 1023, 8787] {
            assert!(validate_wireguard_port(port).is_err(), "{port}");
        }
    }
}
