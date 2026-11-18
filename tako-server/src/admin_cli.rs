//! Host-console recovery. Inviting a full client never needs a pre-existing client key.
use clap::Subcommand;
use std::path::Path;

#[derive(Subcommand)]
pub enum ServerCommand {
    /// Recover network access from the server's root console
    Client {
        #[command(subcommand)]
        command: ClientCommand,
    },
}

#[derive(Subcommand)]
pub enum ClientCommand {
    /// Create a single-use invitation for an independent full client (requires root)
    Invite {
        /// Name of the new client. It must not belong to an active client
        name: tako_network::DeviceName,
        /// Invitation validity in seconds
        #[arg(long, default_value_t = 3600, value_parser = clap::value_parser!(u64).range(60..=86400))]
        expires_in: u64,
        /// Print the invitation and its expiry as JSON
        #[arg(long)]
        json: bool,
    },
}

#[cfg(target_os = "linux")]
pub fn run(command: &ServerCommand, data_dir: &Path) -> Result<(), Box<dyn std::error::Error>> {
    use tako_network::api::RpcResponse;
    use tako_network::invitation::{CreateInvitation, Invitation, InvitationToken};
    use tako_network::{MemberKind, Permissions, RequestId};
    let ServerCommand::Client {
        command:
            ClientCommand::Invite {
                name,
                expires_in,
                json,
            },
    } = command;
    let config = crate::native_agent::recovery_config(data_dir)?;
    let server = config
        .bootstrap
        .members
        .iter()
        .find(|member| member.device_id == config.server_id)
        .and_then(|member| match &member.kind {
            MemberKind::Server(info) => Some(info.clone()),
            _ => None,
        })
        .ok_or("Installed server identity is missing")?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs();
    let expires_at = now
        .checked_add(*expires_in)
        .ok_or("Invitation expiry overflow")?;
    let token = InvitationToken::random()?;
    let id = RequestId::random()?;
    let grant = CreateInvitation {
        id: id.clone(),
        token_hash: token.hash()?,
        expires_at,
        member_expires_at: None,
        permissions: Permissions::Full,
        client: name.clone(),
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let response = runtime.block_on(crate::native_agent::recover_client(data_dir, grant))?;
    match response.response {
        Some(RpcResponse::InvitationCreated { id: returned }) if response.status == 200 && returned == id => (),
        _ => return Err("Recovery invitation was not acknowledged. Check that the native network agent is running and this server is an active network member".into()),
    }
    let invitation = Invitation {
        version: tako_network::invitation::INVITATION_VERSION,
        network: config.network.clone(),
        issuer: config.server_id,
        server,
        id,
        token,
        expires_at,
        client: name.clone(),
    }
    .encode()?;
    // Deliberate credential output, before the normal server logger starts.
    // Only the token digest crossed the local socket or entered the database.
    if *json {
        println!(
            "{}",
            serde_json::json!({"network": config.network.name, "client": name, "invitation": invitation.as_str(), "expires_at": expires_at})
        );
    } else {
        println!("{}", invitation.as_str());
        eprintln!(
            "Created an invitation for full client {name} of {}. Run `tako network join` on the new machine and paste it. The invitation expires in {expires_in} seconds.",
            config.network.name
        );
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub fn run(_: &ServerCommand, _: &Path) -> Result<(), Box<dyn std::error::Error>> {
    Err("Host recovery requires Linux".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn recovery_options_are_bounded_and_do_not_require_a_client_identity() {
        let parsed = crate::Args::try_parse_from([
            "tako-server",
            "client",
            "invite",
            "laptop-b",
            "--json",
            "--expires-in",
            "60",
            "--data-dir",
            "/opt/tako",
        ])
        .unwrap();
        assert!(matches!(
            parsed.command,
            Some(ServerCommand::Client {
                command: ClientCommand::Invite {
                    ref name,
                    expires_in: 60,
                    json: true
                }
            }) if name.as_str() == "laptop-b"
        ));
        for expiry in ["0", "59", "86401"] {
            assert!(
                crate::Args::try_parse_from([
                    "tako-server",
                    "client",
                    "invite",
                    "laptop-b",
                    "--expires-in",
                    expiry
                ])
                .is_err()
            );
        }
        #[cfg(target_os = "linux")]
        if unsafe { libc::geteuid() } != 0 {
            let command = ServerCommand::Client {
                command: ClientCommand::Invite {
                    name: "laptop-b".parse().unwrap(),
                    expires_in: 60,
                    json: false,
                },
            };
            assert!(
                run(&command, Path::new("/does-not-exist"))
                    .unwrap_err()
                    .to_string()
                    .contains("requires root")
            );
        }
    }
}
