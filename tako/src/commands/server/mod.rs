mod crud;
mod native;
mod upgrade;
mod wizard;

pub use wizard::prompt_to_add_server;

use crate::output;
use clap::Subcommand;

#[derive(Subcommand)]
pub enum ServerCommands {
    /// Stop new application traffic and drain a server, keeping management available
    Drain {
        device: String,
        #[arg(long)]
        network: Option<String>,
        #[arg(long, default_value_t = 120, value_name = "SECONDS", value_parser = clap::value_parser!(u64).range(0..=3600))]
        timeout: u64,
    },
    /// Resume persisted applications on a drained server
    Resume {
        device: String,
        #[arg(long)]
        network: Option<String>,
    },

    /// Add a new server
    Add {
        /// Server host. Use admin-user@host to install or repair with that SSH user.
        host: Option<String>,

        /// Tako network alias or stable ID (defaults to the project or sole network).
        #[arg(long)]
        network: Option<String>,

        /// Public WireGuard endpoint (defaults to the server's IP on UDP 51820).
        #[arg(long)]
        wireguard_endpoint: Option<std::net::SocketAddr>,

        /// Server name. Defaults to the host's first DNS label.
        #[arg(long)]
        name: Option<String>,

        /// SSH port
        #[arg(long, default_value_t = 22)]
        port: u16,

        /// SSH private key to connect with (defaults to ~/.ssh keys and ssh-agent)
        #[arg(long, value_name = "PATH")]
        ssh_key: Option<std::path::PathBuf>,

        /// Public HTTP port used by tako-server installs
        #[arg(long)]
        http_port: Option<u16>,

        /// Public HTTPS port used by tako-server installs
        #[arg(long)]
        https_port: Option<u16>,
    },

    /// Remove a server
    #[command(visible_aliases = ["rm", "delete"])]
    Remove {
        /// Server name (omit to choose interactively)
        name: Option<String>,
    },

    /// List all servers
    #[command(visible_alias = "ls")]
    List,

    /// Reload tako-server on a server without downtime
    Reload {
        /// Server name
        name: String,

        /// Force a full service restart with brief downtime
        #[arg(long)]
        force: bool,
    },

    /// Upgrade tako-server via graceful reload with rollback to the previous binary on failure
    Upgrade {
        /// Server name (omit to upgrade all servers)
        name: Option<String>,

        /// Attempt upgrade despite a protocol version mismatch
        #[arg(long)]
        force: bool,
    },

    /// Remove tako-server and all data from a server
    Uninstall {
        /// Server name (omit to choose interactively)
        name: Option<String>,

        /// Skip confirmation prompts
        #[arg(short = 'y', long = "yes")]
        yes: bool,
    },
}

pub fn run(
    cmd: ServerCommands,
    config_path: Option<&std::path::Path>,
) -> Result<(), Box<dyn std::error::Error>> {
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(run_async(cmd, config_path))
}

async fn run_async(
    cmd: ServerCommands,
    config_path: Option<&std::path::Path>,
) -> Result<(), Box<dyn std::error::Error>> {
    match cmd {
        ServerCommands::Drain {
            device,
            network,
            timeout,
        } => tokio::task::block_in_place(|| {
            super::network::manage_server(
                &device,
                network.as_deref(),
                config_path,
                tako_core::Command::DrainServer {
                    timeout_seconds: timeout,
                },
            )
        }),
        ServerCommands::Resume { device, network } => tokio::task::block_in_place(|| {
            super::network::manage_server(
                &device,
                network.as_deref(),
                config_path,
                tako_core::Command::ResumeServer,
            )
        }),
        ServerCommands::Add {
            host,
            network,
            wireguard_endpoint,
            name,
            port,
            ssh_key,
            http_port,
            https_port,
        } => {
            let public_ports = wizard::public_ports_from_cli(http_port, https_port)?;
            wizard::add(
                host.as_deref(),
                network.as_deref(),
                name.as_deref(),
                port,
                ssh_key.as_deref(),
                public_ports,
                wireguard_endpoint,
                config_path,
            )
            .await?;
            Ok(())
        }
        ServerCommands::Remove { name } => crud::remove_server(name.as_deref()).await,
        ServerCommands::List => crud::list_servers().await,
        ServerCommands::Reload { name, force } => crud::restart_server(&name, force).await,
        ServerCommands::Upgrade { name, force } => {
            upgrade::upgrade_servers(name.as_deref(), force).await
        }
        ServerCommands::Uninstall { name, yes } => uninstall_server_cmd(name.as_deref(), yes).await,
    }
}

async fn uninstall_server_cmd(
    name: Option<&str>,
    assume_yes: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let servers = crate::commands::helpers::load_management_servers(None, None).await?;

    if servers.is_empty() {
        output::error("No servers configured.");
        return Ok(());
    }

    let server_name = match name {
        Some(n) => {
            if !servers.contains(n) {
                return Err(format!("Server '{}' not found.", n).into());
            }
            n.to_string()
        }
        None => {
            if !output::is_interactive() {
                return Err(
                    "No server name provided and selection requires an interactive terminal. Run 'tako network server uninstall <name>'."
                        .into(),
                );
            }
            let mut names = servers.names();
            names.sort_unstable();
            let options: Vec<(String, String)> = names
                .into_iter()
                .map(|n| (n.to_string(), n.to_string()))
                .collect();
            output::select("Select server to remove", None, options)?
        }
    };

    let server = servers
        .get(&server_name)
        .ok_or_else(|| format!("Server '{}' not found.", server_name))?
        .clone();

    crate::commands::implode::implode_server(&server_name, &server, assume_yes).await
}

struct ParsedAddHost {
    host: String,
    admin_user: Option<String>,
}

fn parse_add_host(input: &str) -> ParsedAddHost {
    let trimmed = input.trim();
    if let Some((user, host)) = trimmed.rsplit_once('@') {
        let user = user.trim();
        let host = host.trim();
        if !user.is_empty() && !host.is_empty() && !user.contains('@') && !host.contains('@') {
            return ParsedAddHost {
                host: host.to_string(),
                admin_user: Some(user.to_string()),
            };
        }
    }

    ParsedAddHost {
        host: trimmed.to_string(),
        admin_user: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_add_host_extracts_admin_user() {
        let parsed = parse_add_host("ubuntu@my-server");

        assert_eq!(parsed.host, "my-server");
        assert_eq!(parsed.admin_user.as_deref(), Some("ubuntu"));
    }

    #[test]
    fn parse_add_host_keeps_plain_host() {
        let parsed = parse_add_host("my-server");

        assert_eq!(parsed.host, "my-server");
        assert_eq!(parsed.admin_user, None);
    }
}
