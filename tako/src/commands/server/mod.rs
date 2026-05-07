mod crud;
mod dns;
mod upgrade;
mod wizard;

pub use wizard::{add_server, prompt_to_add_server};

use crate::output;
use clap::Subcommand;

#[derive(Subcommand)]
pub enum ServerCommands {
    /// Add a new server
    Add {
        /// Server host (IP or hostname). Omit to use the interactive setup wizard.
        host: Option<String>,

        /// Server name
        #[arg(long)]
        name: Option<String>,

        /// Optional description shown in server lists (e.g. "Primary EU region")
        #[arg(long)]
        description: Option<String>,

        /// SSH port
        #[arg(long, default_value_t = 22)]
        port: u16,

        /// Skip SSH connection test
        #[arg(long, hide = true)]
        no_test: bool,
    },

    /// Remove a server
    #[command(visible_aliases = ["remove", "delete"])]
    Rm {
        /// Server name (omit to choose interactively)
        name: Option<String>,
    },

    /// List all servers
    #[command(visible_alias = "list")]
    Ls,

    /// Reload tako-server on a server without downtime
    Restart {
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
    #[command(visible_alias = "uninstall")]
    Implode {
        /// Server name (omit to choose interactively)
        name: Option<String>,

        /// Skip confirmation prompts
        #[arg(short = 'y', long = "yes")]
        yes: bool,
    },

    /// Show global deployment status across configured servers
    #[command(visible_alias = "info")]
    Status,

    /// Configure DNS-01 wildcard certificate support
    SetupWildcard {
        /// Target environment
        #[arg(long, short)]
        env: Option<String>,
    },
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
            no_test,
        } => {
            if let Some(host) = host {
                let Some(name) = name.as_deref() else {
                    return Err(
                        "Server name is required when adding with a host. Use --name <name>, or run 'tako servers add' to use the interactive wizard."
                            .into(),
                    );
                };
                let _ = add_server(
                    &host,
                    Some(name),
                    description.as_deref(),
                    port,
                    no_test,
                    None,
                )
                .await?;
                Ok(())
            } else {
                let _ = wizard::run_add_server_wizard(
                    name.as_deref(),
                    description.as_deref(),
                    port,
                    !no_test,
                )
                .await?;
                Ok(())
            }
        }
        ServerCommands::Rm { name } => crud::remove_server(name.as_deref()).await,
        ServerCommands::Ls => crud::list_servers().await,
        ServerCommands::Restart { name, force } => crud::restart_server(&name, force).await,
        ServerCommands::Upgrade { name } => upgrade::upgrade_servers(name.as_deref()).await,
        ServerCommands::Implode { name, yes } => implode_server_cmd(name.as_deref(), yes).await,
        ServerCommands::Status => crate::commands::status::run().await,
        ServerCommands::SetupWildcard { env } => dns::setup_wildcard(env.as_deref()).await,
    }
}

async fn implode_server_cmd(
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
                    "No server name provided and selection requires an interactive terminal. Run 'tako servers implode <name>'."
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
