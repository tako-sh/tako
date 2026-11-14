//! Host-console administration. Recovery never needs a pre-existing client key.
use clap::Subcommand;
use std::path::Path;

#[derive(Subcommand)]
pub enum ServerCommand {
    /// Recover administrator access from the server's root console
    Admin {
        #[command(subcommand)]
        command: AdminCommand,
    },
}

#[derive(Subcommand)]
pub enum AdminCommand {
    /// Create a single-use invitation for an independent administrator (requires root)
    Invite {
        /// Invitation validity in seconds
        #[arg(long, default_value_t = 3600, value_parser = clap::value_parser!(u64).range(60..=86400))]
        expires_in: u64,
        /// Also grant the new administrator a Tako SSH shell
        #[arg(long)]
        ssh: bool,
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
    let ServerCommand::Admin {
        command:
            AdminCommand::Invite {
                expires_in,
                ssh,
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
        permissions: Permissions {
            network_admin: true,
            server_admin: true,
            ssh_user: ssh.then(|| "tako".parse()).transpose()?,
            deployments: Default::default(),
        },
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let response = runtime.block_on(crate::native_agent::recover_admin(data_dir, grant))?;
    match response.response {
        Some(RpcResponse::InvitationCreated { id: returned }) if response.status == 200 && returned == id => (),
        _ => return Err("Administrator invitation was not acknowledged. Check that the native network agent is running and this server is an active network member".into()),
    }
    let invitation = Invitation {
        version: 1,
        network: config.network.clone(),
        issuer: config.server_id,
        server,
        id,
        token,
        expires_at,
    }
    .encode()?;
    // Deliberate credential output, before the normal server logger starts.
    // Only the token digest crossed the local socket or entered the database.
    if *json {
        println!(
            "{}",
            serde_json::json!({"network": config.network.name, "invitation": invitation.as_str(), "expires_at": expires_at})
        );
    } else {
        println!("{}", invitation.as_str());
        eprintln!(
            "Created administrator invitation for {}. Run `tako network join` on the new machine and paste it. The invitation expires in {expires_in} seconds.",
            config.network.name
        );
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub fn run(_: &ServerCommand, _: &Path) -> Result<(), Box<dyn std::error::Error>> {
    Err("Server administrator recovery requires Linux".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn recovery_options_are_bounded_and_do_not_require_a_client_identity() {
        let parsed = crate::Args::try_parse_from([
            "tako-server",
            "admin",
            "invite",
            "--json",
            "--ssh",
            "--expires-in",
            "60",
            "--data-dir",
            "/opt/tako",
        ])
        .unwrap();
        assert!(matches!(
            parsed.command,
            Some(ServerCommand::Admin {
                command: AdminCommand::Invite {
                    expires_in: 60,
                    ssh: true,
                    json: true
                }
            })
        ));
        for expiry in ["0", "59", "86401"] {
            assert!(
                crate::Args::try_parse_from([
                    "tako-server",
                    "admin",
                    "invite",
                    "--expires-in",
                    expiry
                ])
                .is_err()
            );
        }
        #[cfg(target_os = "linux")]
        if unsafe { libc::geteuid() } != 0 {
            let command = ServerCommand::Admin {
                command: AdminCommand::Invite {
                    expires_in: 60,
                    ssh: false,
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
