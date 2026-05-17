mod crud;
mod upgrade;
mod wizard;

pub use wizard::{AddServerOptions, add_server, prompt_to_add_server};

use crate::output;
use clap::Subcommand;

#[derive(Subcommand)]
pub enum ServerCommands {
    /// Add a new server
    Add {
        /// Server host. Use admin-user@host to install or repair with that SSH user.
        host: Option<String>,

        /// Server name. Defaults to the host's first DNS label.
        #[arg(long)]
        name: Option<String>,

        /// Optional description shown in server lists (e.g. "Primary EU region")
        #[arg(long)]
        description: Option<String>,

        /// SSH port
        #[arg(long, default_value_t = 22)]
        port: u16,

        /// Public HTTP port used by tako-server installs
        #[arg(long)]
        http_port: Option<u16>,

        /// Public HTTPS port used by tako-server installs
        #[arg(long)]
        https_port: Option<u16>,

        /// Install or repair tako-server over SSH before adding the server
        #[arg(long, conflicts_with = "no_test")]
        install: bool,

        /// SSH user to use for --install
        #[arg(long, requires = "install")]
        admin_user: Option<String>,

        /// Skip SSH connection test
        #[arg(long, hide = true)]
        no_test: bool,
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
    },

    /// Remove tako-server and all data from a server
    Uninstall {
        /// Server name (omit to choose interactively)
        name: Option<String>,

        /// Skip confirmation prompts
        #[arg(short = 'y', long = "yes")]
        yes: bool,
    },

    /// Show global deployment status across configured servers
    #[command(visible_alias = "info")]
    Status,
}

pub fn run(cmd: ServerCommands) -> Result<(), Box<dyn std::error::Error>> {
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(run_async(cmd))
}

async fn run_async(cmd: ServerCommands) -> Result<(), Box<dyn std::error::Error>> {
    match cmd {
        ServerCommands::Add {
            host,
            name,
            description,
            port,
            http_port,
            https_port,
            install,
            admin_user,
            no_test,
        } => {
            let public_ports = wizard::public_ports_from_cli(http_port, https_port)?;
            if let Some(host) = host {
                let parsed_host = parse_add_host(&host);
                let parsed_admin_user = parsed_host.admin_user.as_deref();
                let admin_user = admin_user.as_deref().or(parsed_admin_user);
                let install_if_missing = !no_test && (install || parsed_admin_user.is_some());
                let _ = add_server(
                    &parsed_host.host,
                    AddServerOptions {
                        name: name.as_deref(),
                        description: description.as_deref(),
                        port,
                        public_ports,
                        no_test,
                        pre_detected_target: None,
                        pre_detected_public_ports: None,
                        install_if_missing,
                        allow_install_prompt: !no_test && !install_if_missing,
                        admin_user,
                    },
                )
                .await?;
                Ok(())
            } else {
                let _ = wizard::run_add_server_wizard(
                    name.as_deref(),
                    description.as_deref(),
                    port,
                    public_ports,
                    !no_test,
                    !no_test,
                    admin_user.as_deref(),
                )
                .await?;
                Ok(())
            }
        }
        ServerCommands::Remove { name } => crud::remove_server(name.as_deref()).await,
        ServerCommands::List => crud::list_servers().await,
        ServerCommands::Reload { name, force } => crud::restart_server(&name, force).await,
        ServerCommands::Upgrade { name } => upgrade::upgrade_servers(name.as_deref()).await,
        ServerCommands::Uninstall { name, yes } => uninstall_server_cmd(name.as_deref(), yes).await,
        ServerCommands::Status => crate::commands::status::run().await,
    }
}

async fn uninstall_server_cmd(
    name: Option<&str>,
    assume_yes: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    use crate::config::ServersToml;

    let servers = ServersToml::load()?;

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
                    "No server name provided and selection requires an interactive terminal. Run 'tako servers uninstall <name>'."
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
