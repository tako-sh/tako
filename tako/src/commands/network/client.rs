use super::*;
use std::io::{IsTerminal, Read};
use tako_network::api::{RpcRequest, RpcResponse, SignedRequest};
use tako_network::invitation::{ClaimInvitation, CreateInvitation, Invitation, InvitationToken};
use zeroize::Zeroizing;

#[derive(Subcommand)]
pub enum ClientCommands {
    /// Create a single-use invitation for a new client
    Add {
        #[arg(long)]
        network: Option<String>,
        /// Grant network and server administration
        #[arg(long, required_unless_present = "deploy")]
        admin: bool,
        /// Grant an administrator shell (requires --admin)
        #[arg(long, requires = "admin")]
        ssh: bool,
        /// Grant application deployment and secret access; repeat for more scopes
        #[arg(long, value_name = "APP/ENV", required_unless_present = "admin")]
        deploy: Vec<tako_network::AppEnv>,
        /// Invitation validity in seconds (at most one day)
        #[arg(long, default_value_t = 3600, value_parser = clap::value_parser!(u64).range(60..=86400))]
        expires_in: u64,
    },
    /// Refresh and list clients in the network
    #[command(visible_alias = "ls")]
    List {
        #[arg(long)]
        network: Option<String>,
    },
    /// Revoke a client and approvals derived from it
    #[command(visible_alias = "rm")]
    Remove {
        client: String,
        #[arg(long)]
        network: Option<String>,
    },
}

pub(super) fn run(command: ClientCommands, config: Option<&Path>) -> Result<()> {
    match command {
        ClientCommands::Add {
            network,
            admin,
            ssh,
            deploy,
            expires_in,
        } => {
            let alias = network::resolve_network(network.as_deref(), config)?;
            if output::is_dry_run() {
                return finish(
                    "Would create a client invitation",
                    serde_json::json!({"dry_run":true,"network":alias.name}),
                );
            }
            let (profile, _) = network::secrets::pull(&alias, None)?;
            let server = network::client::available_server(&profile)?;
            let info = network::server_members(&profile)?
                .get(&server)
                .and_then(|m| match &m.kind {
                    MemberKind::Server(info) => Some(info.clone()),
                    _ => None,
                })
                .ok_or("Invitation issuer is unavailable")?;
            let token = InvitationToken::random()?;
            let id = tako_network::RequestId::random()?;
            let expires_at = network::now()? + expires_in;
            let grant = CreateInvitation {
                id: id.clone(),
                token_hash: token.hash()?,
                permissions: Permissions {
                    network_admin: admin,
                    server_admin: admin,
                    ssh_user: ssh.then(|| "tako".parse()).transpose()?,
                    deployments: deploy.into_iter().collect(),
                },
                expires_at,
                member_expires_at: None,
            };
            match network::client::call(
                &profile,
                &server,
                RpcRequest::CreateInvitation(Box::new(grant)),
            )? {
                RpcResponse::InvitationCreated { id: returned } if returned == id => (),
                _ => return Err("Unexpected invitation response".into()),
            }
            let invitation = Invitation {
                version: 1,
                network: profile.snapshot().network.clone(),
                issuer: server,
                server: info,
                id,
                token,
                expires_at,
            }
            .encode()?;
            if output::is_json() {
                output::json_result(
                    serde_json::json!({"network":alias.name,"invitation":invitation.as_str(),"expires_at":expires_at}),
                )?;
            } else {
                // Deliberate credential output; never put it in tracing or progress logs.
                println!("{}", invitation.as_str());
                output::success(&format!("Created invitation for {}", alias.name));
            }
            output::hint(
                "On the new machine, run `tako network join` and paste the invitation. It can enroll one client.",
            );
            Ok(())
        }
        ClientCommands::List { network: value } => {
            let alias = network::resolve_network(value.as_deref(), config)?;
            let (profile, _) = network::secrets::pull(&alias, None)?;
            let members = profile.snapshot().effective_members(network::now()?)?;
            let clients: Vec<_> = members
                .values()
                .filter(|m| matches!(m.kind, MemberKind::Laptop))
                .collect();
            if output::is_json() {
                return output::json_result(
                    serde_json::json!({"network":alias.name,"clients":clients}),
                );
            }
            for client in clients {
                output::info(client.request.name.as_str());
                tracing::info!("{}", client.request.name);
            }
            Ok(())
        }
        ClientCommands::Remove {
            client,
            network: value,
        } => {
            let alias = network::resolve_network(value.as_deref(), config)?;
            let profile = if output::is_dry_run() {
                network::load_profile(&alias)?
            } else {
                network::client::refresh(&alias)?
            };
            let candidates: Vec<_> = profile
                .snapshot()
                .members
                .iter()
                .filter(|m| {
                    matches!(m.kind, MemberKind::Laptop)
                        && (m.device_id.to_string() == client || m.request.name.as_str() == client)
                })
                .collect();
            let [member] = candidates.as_slice() else {
                return Err("Client must uniquely match a client name or device ID".into());
            };
            super::revoke(&member.device_id.to_string(), Some(&alias.id), config)
        }
    }
}

pub(super) fn join(name: Option<&str>, from_stdin: bool) -> Result<()> {
    if output::is_dry_run() {
        return finish(
            "Would join a network with a new local identity",
            serde_json::json!({"dry_run":true}),
        );
    }
    let encoded = if from_stdin {
        if std::io::stdin().is_terminal() {
            return Err("Pipe the invitation to --invitation-stdin, or use the protected prompt without this flag".into());
        }
        let mut value = Zeroizing::new(String::new());
        std::io::stdin()
            .take(64 * 1024 + 1)
            .read_to_string(&mut value)?;
        value
    } else {
        Zeroizing::new(output::password_field("Invitation")?)
    };
    let invitation = Invitation::decode(&encoded)?;
    let directory = crate::paths::tako_config_dir()?
        .join("network-joins")
        .join(invitation.network.id.to_string());
    network::private_directory(&directory)?;
    let path = directory.join(format!("{}.json", invitation.id));
    let lock = network::private_file(&path.with_extension("lock"), true)?;
    lock.lock()?;
    let request: tako_network::EnrollmentRequest = if path.try_exists()? {
        network::read_json(&path)?
    } else {
        if invitation.expires_at <= network::now()? {
            return Err(
                "Invitation expired; create a new invitation on an authorized client".into(),
            );
        }
        let keys = LocalDeviceKeys::generate()?;
        let request = keys.enrollment_request(
            Some(invitation.network.id.clone()),
            DeviceKind::Laptop,
            device_name(name)?,
        )?;
        network::store_keys(&request.request_id, &keys)?;
        network::write_json(&path, &request, false)?;
        request
    };
    let keys = LocalDeviceKeys::load(&network::keys_path(&request.request_id)?)?;
    let local = crate::config::LocalNetworks::load()?;
    if let Some(existing) = local.networks.get(&invitation.network.id.to_string()) {
        let previous: Profile = network::read_json(&existing.profile_path()?)?;
        let known = network::server_members(&previous)?;
        if previous.snapshot().network != invitation.network
            || !known
                .get(&invitation.issuer)
                .is_some_and(|member| member.kind == MemberKind::Server(invitation.server.clone()))
        {
            return Err(
                "Invitation conflicts with this machine's existing network identity or server pins"
                    .into(),
            );
        }
    }
    let alias = if let Some(existing) = local.networks.values().find(|n| {
        n.id == invitation.network.id.to_string()
            && n.device
                == request
                    .device_id_for(&invitation.network.id)
                    .map(|id| id.to_string())
                    .unwrap_or_default()
    }) {
        existing.clone()
    } else {
        let signed = SignedRequest::sign(
            &keys,
            invitation.network.id.clone(),
            invitation.issuer.clone(),
            DeviceKind::Laptop,
            network::now()?,
            RpcRequest::ClaimInvitation(Box::new(ClaimInvitation {
                id: invitation.id,
                token: invitation.token,
                request: request.clone(),
            })),
        )?;
        let RpcResponse::Pending(pending) = network::client::send(&invitation.server, &signed)?
        else {
            return Err("Unexpected invitation claim response".into());
        };
        if pending.snapshot.network != invitation.network
            || pending.issuer != invitation.issuer
            || pending.request != request
            || pending.trusted_servers.get(&invitation.issuer) != Some(&invitation.server)
        {
            return Err(
                "Invitation response does not match the pinned network and enrollment".into(),
            );
        }
        pending.validate_for(&keys, network::now()?)?;
        network::save_profile(&Profile::Pending { profile: pending })?
    };
    network::secrets::pull(&alias, None)?;
    finish(
        &format!("Joined network {}", alias.name),
        serde_json::to_value(&alias)?,
    )
}
