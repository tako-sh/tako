use clap::{Parser, Subcommand};

#[cfg(test)]
use crate::commands::server;
use crate::commands::{
    self, backups, credentials, delete, releases, scale, secret, sqlite, storage, upgrade,
    workflows,
};
use clap::CommandFactory;

const VERSION_BASE: &str = env!("CARGO_PKG_VERSION");
const VERSION_BUILD_SHA: Option<&str> = option_env!("TAKO_BUILD_SHA");

pub fn display_version() -> String {
    format_display_version(VERSION_BASE, VERSION_BUILD_SHA)
}

fn format_display_version(base: &str, build_sha: Option<&str>) -> String {
    let Some(raw_sha) = build_sha else {
        return base.to_owned();
    };
    let sha = raw_sha.trim();
    if sha.is_empty() {
        return base.to_owned();
    }
    let short_sha = &sha[..sha.len().min(7)];
    format!("{base}-{short_sha}")
}

/// Tako - Modern application development, deployment, and runtime platform
#[derive(Parser)]
#[command(name = "tako")]
#[command(version, disable_version_flag = true)]
#[command(about = "Tako - Modern application development, deployment, and runtime platform")]
pub struct Cli {
    /// Show version
    #[arg(long)]
    pub version: bool,

    /// Show verbose output
    #[arg(short = 'v', long, global = true)]
    pub verbose: bool,

    /// Deterministic non-interactive output (no colors, no spinners, no prompts)
    #[arg(long, global = true)]
    pub ci: bool,

    /// Emit structured JSON on stdout
    #[arg(long, global = true)]
    pub json: bool,

    /// Show what would happen without performing any side effects
    #[arg(long, global = true)]
    pub dry_run: bool,

    /// Use an explicit config name/path instead of ./tako.toml (`.toml` suffix optional)
    #[arg(short = 'c', long, global = true, value_name = "CONFIG")]
    pub config: Option<std::path::PathBuf>,

    /// Passphrase for encrypted local SSH private keys
    #[arg(long, global = true, value_name = "PASSPHRASE")]
    pub ssh_passphrase: Option<String>,

    /// Network to act on, by this machine's alias or its ID
    #[arg(long = "network", global = true, value_name = "NETWORK")]
    pub network_flag: Option<String>,

    #[command(subcommand)]
    pub command: Option<Commands>,
}

#[cfg(test)]
mod tests;

#[derive(clap::Args, Debug)]
pub struct DevArgs {
    /// Environment to run locally (defaults to development)
    #[arg(long, global = true)]
    pub env: Option<String>,

    /// Run a variant of the app (e.g. --variant foo → myapp-foo.test)
    #[arg(long, visible_alias = "var")]
    pub variant: Option<String>,

    /// Start with a temporary public tunnel URL enabled
    #[arg(long)]
    pub tunnel: bool,

    /// Leave the app running under the dev daemon and exit after readiness
    #[arg(long)]
    pub background: bool,

    /// Hard-restart the app process with the current config
    #[arg(long)]
    pub restart: bool,
}

#[derive(Subcommand, Debug)]
pub enum DevSubcommands {
    /// Stop a running dev app
    Stop {
        /// App name (defaults to current directory's app)
        name: Option<String>,
        /// Stop all registered apps
        #[arg(long)]
        all: bool,
    },
    /// List registered dev apps
    #[command(visible_alias = "ls")]
    List,
    /// Override the configured dev command
    #[command(external_subcommand)]
    Run(Vec<String>),
}

#[derive(Subcommand)]
pub enum Commands {
    /// Initialize a new tako project
    Init,

    /// Configure coding agents with Tako skills and MCP
    Agents {
        /// Coding agents to configure (detected by default)
        #[arg(long, value_enum, value_delimiter = ',')]
        agent: Vec<commands::agents::Agent>,

        /// Skip confirmation prompts
        #[arg(short = 'y', long)]
        yes: bool,
    },

    /// Serve Tako tools and documentation over MCP stdio
    Mcp,

    /// View remote logs
    Logs {
        /// Environment to view logs from (defaults to production)
        #[arg(long)]
        env: Option<String>,

        /// Stream logs continuously
        #[arg(long, conflicts_with = "days")]
        tail: bool,

        /// Number of days of history to show (default: 3)
        #[arg(long, default_value = "3")]
        days: u32,
    },

    /// Start development server
    Dev {
        #[command(subcommand)]
        command: Option<DevSubcommands>,

        #[command(flatten)]
        args: DevArgs,
    },

    /// Print a local diagnostic report, with project checks inside a project
    Doctor,

    /// Show deployment status across configured servers, with a project's resources inside one
    Status,

    /// Private network membership and portable configuration
    #[command(subcommand)]
    Network(commands::network::NetworkCommands),

    /// Connect local applications to a private network, switching from any other
    Connect { network: Option<String> },

    /// Disconnect a private network (the connected one by default) and remove its names
    Disconnect { network: Option<String> },

    /// SSH to an approved server using this machine's network identity
    Ssh {
        server: String,
        #[arg(last = true)]
        command: Vec<String>,
    },

    /// Secret management commands
    #[command(subcommand)]
    Secret(secret::SecretCommands),

    /// Object storage commands
    #[command(subcommand)]
    Storage(storage::StorageCommands),

    /// App data backup commands
    #[command(subcommand)]
    Backup(backups::BackupCommands),

    /// Tako-hosted SQLite databases
    #[command(visible_alias = "db")]
    Sqlite(sqlite::SqliteArgs),

    /// Provider credentials used by Tako
    #[command(visible_alias = "creds")]
    Credential {
        #[command(subcommand)]
        command: Option<credentials::CredentialCommands>,
    },

    /// Release history and rollback commands
    #[command(subcommand)]
    Release(releases::ReleaseCommands),

    /// Inspect, retry, and cancel workflow runs of a deployed environment
    #[command(subcommand)]
    Workflow(workflows::WorkflowCommands),

    /// Upgrade the local tako CLI to the latest version
    ///
    /// Installing a new version requests shutdown of the local dev server.
    /// Run `tako dev` in each project when you want to start it again.
    Upgrade,

    /// Install shell completions
    Completions {
        /// Skip confirmation prompts
        #[arg(short = 'y', long = "yes")]
        yes: bool,
    },

    /// Deploy to an environment
    Deploy {
        /// Release description (defaults to the commit subject or source snapshot)
        #[arg(long, value_parser = parse_release_description)]
        description: Option<String>,

        /// Environment to deploy to
        #[arg(long)]
        env: Option<String>,

        /// Skip confirmation prompts
        #[arg(short = 'y', long = "yes")]
        yes: bool,

        /// Attempt deployment despite a protocol version mismatch
        #[arg(long)]
        force: bool,
    },

    /// Delete a deployed app from a specific environment/server deployment
    #[command(visible_aliases = ["rm", "remove", "undeploy", "destroy"])]
    Delete {
        /// Compose children cannot be deleted individually; edit Compose and redeploy
        #[arg(long)]
        service: Option<String>,
        /// Environment to delete from
        #[arg(long)]
        env: Option<String>,

        /// Specific server to delete from
        #[arg(long)]
        server: Option<String>,

        /// Skip confirmation prompts
        #[arg(short = 'y', long = "yes")]
        yes: bool,
    },

    /// Remove Tako CLI and all local data
    Uninstall {
        /// Skip confirmation prompts
        #[arg(short = 'y', long = "yes")]
        yes: bool,
    },

    /// Refresh generated Tako files
    #[command(visible_aliases = ["gen", "g"])]
    Generate {
        /// Fetch remote schemas again and update tako-schema.lock
        #[arg(long)]
        update_schemas: bool,
    },

    /// Run a local command with Tako environment and secrets
    Run {
        /// Environment to load (defaults to development)
        #[arg(long)]
        env: Option<String>,

        /// Evaluate inline code with the project runtime
        #[arg(long)]
        eval: Option<String>,

        /// Script, command, or arguments for --eval
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
    },

    /// Show version information
    Version,

    /// Start a deployed app or application group
    Start {
        /// Start one x-tako app in a Compose group
        #[arg(long)]
        service: Option<String>,
        #[arg(long)]
        env: Option<String>,
        #[arg(long)]
        server: Option<String>,
        /// App or group name (required outside a project directory)
        #[arg(long)]
        app: Option<String>,
    },

    /// Stop a deployed app or application group, preserving its replica counts
    Stop {
        /// Stop one x-tako app in a Compose group
        #[arg(long)]
        service: Option<String>,
        #[arg(long)]
        env: Option<String>,
        #[arg(long)]
        server: Option<String>,
        /// App or group name (required outside a project directory)
        #[arg(long)]
        app: Option<String>,
    },

    /// Change the desired instance count for a deployed app
    Scale {
        /// Desired instance count per targeted server
        #[arg(required_unless_present = "reset", conflicts_with = "reset")]
        instances: Option<u8>,

        /// Compose service to scale (required when the group has multiple apps)
        #[arg(long)]
        service: Option<String>,

        /// Clear a Compose service override and use its deploy.replicas default
        #[arg(long)]
        reset: bool,

        /// Environment to scale
        #[arg(long)]
        env: Option<String>,

        /// Specific server to scale
        #[arg(long)]
        server: Option<String>,

        /// App name (required outside a project directory)
        #[arg(long)]
        app: Option<String>,
    },
}

/// Whether `command` talks to a Tako network, and so may act as the client in
/// `TAKO_CLIENT_IDENTITY`. The others never activate an identity: several run
/// project code (`run`, `dev`, `generate`, `init`), which must not find its keys
/// on disk. `mcp` runs each tool as a child `tako` process that decides for
/// itself.
fn uses_network_identity(command: &Commands) -> bool {
    !matches!(
        command,
        Commands::Version
            | Commands::Init
            | Commands::Agents { .. }
            | Commands::Mcp
            | Commands::Dev { .. }
            | Commands::Run { .. }
            | Commands::Generate { .. }
            | Commands::Upgrade
            | Commands::Completions { .. }
            | Commands::Uninstall { .. }
    )
}

fn telemetry_command(command: &Commands) -> &'static str {
    match command {
        Commands::Init => "init",
        Commands::Agents { .. } => "agents",
        Commands::Mcp => "mcp",
        Commands::Logs { .. } => "logs",
        Commands::Dev {
            command: Some(DevSubcommands::Stop { .. }),
            ..
        } => "dev stop",
        Commands::Dev {
            command: Some(DevSubcommands::List),
            ..
        } => "dev list",
        Commands::Dev { .. } => "dev",
        Commands::Doctor => "doctor",
        Commands::Status => "status",
        Commands::Network(_) => "network",
        Commands::Connect { .. } => "connect",
        Commands::Disconnect { .. } => "disconnect",
        Commands::Ssh { .. } => "ssh",
        Commands::Secret(_) => "secret",
        Commands::Storage(_) => "storage",
        Commands::Backup(_) => "backup",
        Commands::Sqlite(_) => "sqlite",
        Commands::Credential { .. } => "credential",
        Commands::Release(_) => "release",
        Commands::Workflow(_) => "workflow",
        Commands::Upgrade => "upgrade",
        Commands::Completions { .. } => "completions",
        Commands::Uninstall { .. } => "uninstall",
        Commands::Generate { .. } => "generate",
        Commands::Run { .. } => "run",
        Commands::Deploy { .. } => "deploy",
        Commands::Delete { .. } => "delete",
        Commands::Scale { .. } => "scale",
        Commands::Start { .. } => "start",
        Commands::Stop { .. } => "stop",
        Commands::Version => "version",
    }
}

pub(crate) fn supports_dry_run(path: &[&str]) -> bool {
    matches!(
        path,
        ["agents"
            | "deploy"
            | "delete"
            | "scale"
            | "start"
            | "stop"
            | "connect"
            | "disconnect"
            | "ssh"]
            | [
                "network",
                "sync" | "join" | "create" | "status" | "export" | "list" | "rename"
            ]
            | [
                "network",
                "server",
                "drain" | "resume" | "add" | "revoke" | "remove" | "list"
            ]
            | ["network", "client", "add" | "revoke" | "list"]
            | ["secret", "migrate" | "sync" | "clear"]
            | ["backup", "now" | "download" | "restore"]
            | [
                "sqlite",
                "create" | "link" | "backup" | "restore" | "import" | "export" | "delete"
            ]
            | ["workflow", "retry" | "cancel"]
    )
}

pub(crate) fn is_read_only(path: &[&str]) -> bool {
    matches!(
        path,
        ["version" | "status" | "doctor" | "logs" | "completions" | "credential"]
            | [
                "dev" | "secret" | "credential" | "release" | "workflow",
                "list"
            ]
            | ["workflow", "show"]
            | ["backup" | "network", "list" | "status"]
            | ["sqlite", "list" | "status" | "backups"]
            | ["network", "client" | "server", "list"]
    )
}

impl Cli {
    pub fn run(self, matches: &clap::ArgMatches) -> Result<(), Box<dyn std::error::Error>> {
        crate::ssh::set_key_passphrase(self.ssh_passphrase.clone());
        crate::network::set_network_flag(self.network_flag.clone());
        let json = self.json;

        if self.version {
            crate::telemetry::maybe_send("version", self.ci, &display_version());
            return print_version(json);
        }

        let Some(command) = self.command else {
            if json {
                return crate::output::json_result(serde_json::json!({
                    "ok": true,
                    "command": "help",
                    "commands": Cli::command()
                        .get_subcommands()
                        .map(|command| command.get_name().to_string())
                        .collect::<Vec<_>>(),
                }));
            } else {
                Cli::command().print_help()?;
                println!();
                return Ok(());
            }
        };

        if self.dry_run && !matches!(command, Commands::Mcp) {
            let path: Vec<_> =
                std::iter::successors(matches.subcommand(), |(_, sub)| sub.subcommand())
                    .map(|(name, _)| name)
                    .collect();
            if !supports_dry_run(&path) && !is_read_only(&path) {
                return Err(format!("tako {} does not support --dry-run", path.join(" ")).into());
            }
        }
        crate::telemetry::maybe_send(telemetry_command(&command), self.ci, &display_version());
        // With TAKO_CLIENT_IDENTITY, network commands act as that client for
        // this process only; its keys stay in a private temporary directory.
        // Commands that need no network, several of which run project code,
        // never write the keys at all.
        let _identity = if uses_network_identity(&command) {
            crate::network::identity::activate_from_env()?
        } else {
            None
        };

        match command {
            Commands::Version => print_version(json),
            Commands::Status => {
                let rt = tokio::runtime::Runtime::new()?;
                rt.block_on(commands::status::run(json, self.config.as_deref()))
            }
            Commands::Init => {
                commands::init::run(self.config.as_deref())?;
                json_success(json, "init")
            }
            Commands::Agents { agent, yes } => {
                commands::agents::run(agent, yes, self.config.as_deref())
            }
            Commands::Mcp => commands::mcp::run(self.config.as_deref()),
            Commands::Logs { env, tail, days } => {
                commands::logs::run(env.as_deref(), tail, days, json, self.config.as_deref())
            }
            Commands::Dev { command, args } => {
                let rt = tokio::runtime::Runtime::new()?;

                match command {
                    None => {
                        rt.block_on(commands::dev::run(
                            args.variant,
                            args.tunnel,
                            args.restart,
                            None,
                            self.config.as_deref(),
                            args.background,
                            args.env.as_deref().unwrap_or("development"),
                        ))?;
                        Ok(())
                    }
                    Some(DevSubcommands::Run(cmd)) => {
                        rt.block_on(commands::dev::run(
                            args.variant,
                            args.tunnel,
                            args.restart,
                            Some(cmd),
                            self.config.as_deref(),
                            args.background,
                            args.env.as_deref().unwrap_or("development"),
                        ))?;
                        Ok(())
                    }
                    Some(DevSubcommands::Stop { name, all }) => {
                        rt.block_on(commands::dev::stop(
                            name,
                            all,
                            self.config.as_deref(),
                            args.env.as_deref(),
                        ))?;
                        json_success(json, "dev")
                    }
                    Some(DevSubcommands::List) => {
                        rt.block_on(commands::dev::ls(args.env.as_deref()))?;
                        Ok(())
                    }
                }
            }
            Commands::Doctor => {
                let rt = tokio::runtime::Runtime::new()?;
                rt.block_on(commands::doctor::run(self.config.as_deref()))
            }
            Commands::Network(cmd) => commands::network::run(cmd),
            Commands::Connect { network } => commands::network::connect(network.as_deref()),
            Commands::Disconnect { network } => commands::network::disconnect(network.as_deref()),
            Commands::Ssh { server, command } => commands::network::ssh(&server, None, &command),
            Commands::Secret(cmd) => {
                let emit_generic = json
                    && !matches!(
                        cmd,
                        secret::SecretCommands::List | secret::SecretCommands::Clear { .. }
                    );
                secret::run(cmd, self.config.as_deref())?;
                if emit_generic {
                    json_success(true, "secret")
                } else {
                    Ok(())
                }
            }
            Commands::Storage(cmd) => {
                storage::run(cmd, self.config.as_deref())?;
                json_success(json, "storage")
            }
            Commands::Backup(cmd) => {
                let emit_generic = json
                    && !matches!(
                        cmd,
                        backups::BackupCommands::List { .. }
                            | backups::BackupCommands::Status { .. }
                    );
                backups::run(cmd, self.config.as_deref())?;
                if emit_generic {
                    json_success(true, "backup")
                } else {
                    Ok(())
                }
            }
            Commands::Sqlite(cmd) => {
                // Each subcommand prints its own JSON result.
                sqlite::run(cmd, self.config.as_deref())
            }
            Commands::Credential { command } => {
                let emit_generic =
                    json && !matches!(command, None | Some(credentials::CredentialCommands::List));
                credentials::run(command, self.config.as_deref())?;
                if emit_generic {
                    json_success(true, "credential")
                } else {
                    Ok(())
                }
            }
            Commands::Release(cmd) => {
                let emit_generic = json && !matches!(cmd, releases::ReleaseCommands::List { .. });
                releases::run(cmd, self.config.as_deref())?;
                if emit_generic {
                    json_success(true, "release")
                } else {
                    Ok(())
                }
            }
            Commands::Workflow(cmd) => workflows::run(cmd, self.config.as_deref()),
            Commands::Upgrade => {
                upgrade::run()?;
                json_success(json, "upgrade")
            }
            Commands::Completions { yes } => {
                commands::completions::run(yes)?;
                json_success(json, "completions")
            }

            Commands::Uninstall { yes } => {
                commands::implode::run(yes)?;
                json_success(json, "uninstall")
            }
            Commands::Generate { update_schemas } => {
                commands::codegen::run(self.config.as_deref(), update_schemas)?;
                json_success(json, "generate")
            }
            Commands::Run { env, eval, command } => commands::run::run(
                env.as_deref(),
                eval.as_deref(),
                command,
                self.config.as_deref(),
            ),
            Commands::Deploy {
                env,
                yes,
                force,
                description,
            } => commands::deploy::run(
                env.as_deref(),
                yes,
                force,
                description.as_deref(),
                self.config.as_deref(),
            ),
            Commands::Delete {
                env,
                server,
                yes,
                service,
            } => {
                if let Some(service) = service {
                    return Err(format!("Cannot delete app '{service}' individually: it belongs to a Compose application group. Remove the service from the Compose file and redeploy, or delete the whole group.").into());
                }
                delete::run(
                    env.as_deref(),
                    server.as_deref(),
                    yes,
                    self.config.as_deref(),
                )?;
                json_success(json, "delete")
            }
            Commands::Start {
                service,
                env,
                server,
                app,
            } => {
                commands::lifecycle::run(
                    true,
                    service.as_deref(),
                    env.as_deref(),
                    server.as_deref(),
                    app.as_deref(),
                    self.config.as_deref(),
                )?;
                json_success(json, "start")
            }
            Commands::Stop {
                service,
                env,
                server,
                app,
            } => {
                commands::lifecycle::run(
                    false,
                    service.as_deref(),
                    env.as_deref(),
                    server.as_deref(),
                    app.as_deref(),
                    self.config.as_deref(),
                )?;
                json_success(json, "stop")
            }
            Commands::Scale {
                instances,
                service,
                reset,
                env,
                server,
                app,
            } => {
                scale::run(
                    instances,
                    service.as_deref(),
                    reset,
                    env.as_deref(),
                    server.as_deref(),
                    app.as_deref(),
                    self.config.as_deref(),
                )?;
                json_success(json, "scale")
            }
        }
    }
}

fn json_success(json: bool, command: &str) -> Result<(), Box<dyn std::error::Error>> {
    if json {
        crate::output::json_success(command)
    } else {
        Ok(())
    }
}

fn print_version(json: bool) -> Result<(), Box<dyn std::error::Error>> {
    if json {
        crate::output::json_result(serde_json::json!({
            "ok": true,
            "command": "version",
            "version": display_version(),
        }))
    } else {
        println!("{}", display_version());
        Ok(())
    }
}

fn parse_release_description(value: &str) -> Result<String, String> {
    let value = value.trim();
    if value.is_empty() {
        return Err("Release description must not be empty".to_string());
    }
    Ok(value.to_string())
}
