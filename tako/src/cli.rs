use clap::{Parser, Subcommand};

use crate::commands::{self, delete, releases, scale, secret, server, storage, upgrade};
use clap::CommandFactory;

const DEV_PUBLIC_PORT: u16 = 47831;
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

    /// Show what would happen without performing any side effects
    #[arg(long, global = true)]
    pub dry_run: bool,

    /// Use an explicit config name/path instead of ./tako.toml (`.toml` suffix optional)
    #[arg(short = 'c', long, global = true, value_name = "CONFIG")]
    pub config: Option<std::path::PathBuf>,

    /// Passphrase for encrypted local SSH private keys
    #[arg(long, global = true, value_name = "PASSPHRASE")]
    pub ssh_passphrase: Option<String>,

    #[command(subcommand)]
    pub command: Option<Commands>,
}

#[cfg(test)]
mod tests;

#[derive(clap::Args, Debug)]
pub struct DevArgs {
    /// Run a variant of the app (e.g. --variant foo → myapp-foo.test)
    #[arg(long, visible_alias = "var")]
    pub variant: Option<String>,
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
}

#[derive(Subcommand)]
pub enum Commands {
    /// Initialize a new tako project
    Init,

    /// View remote logs
    Logs {
        /// Environment to view logs from (defaults to production)
        #[arg(long)]
        env: Option<String>,

        /// Emit compact JSONL records for agents and automation
        #[arg(long)]
        json: bool,

        /// Stream logs continuously
        #[arg(long, conflicts_with = "days")]
        tail: bool,

        /// Number of days of history to show (default: 3)
        #[arg(long, default_value = "3")]
        days: u32,
    },

    /// Start development server
    #[command(args_conflicts_with_subcommands = true)]
    Dev {
        #[command(subcommand)]
        command: Option<DevSubcommands>,

        #[command(flatten)]
        args: DevArgs,
    },

    /// Print a local diagnostic report
    Doctor,

    /// Server management commands
    #[command(subcommand)]
    Servers(server::ServerCommands),

    /// Secret management commands
    #[command(subcommand)]
    Secrets(secret::SecretCommands),

    /// Object storage commands
    #[command(subcommand)]
    Storages(storage::StorageCommands),

    /// Release history and rollback commands
    #[command(subcommand)]
    Releases(releases::ReleaseCommands),

    /// Upgrade the local tako CLI to the latest version
    Upgrade,

    /// Deploy to an environment
    Deploy {
        /// Environment to deploy to
        #[arg(long)]
        env: Option<String>,

        /// Skip confirmation prompts
        #[arg(short = 'y', long = "yes")]
        yes: bool,
    },

    /// Delete a deployed app from a specific environment/server deployment
    #[command(visible_aliases = ["rm", "remove", "undeploy", "destroy"])]
    Delete {
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
    Generate,

    /// Show version information
    Version,

    /// Change the desired instance count for a deployed app
    Scale {
        /// Desired instance count per targeted server
        instances: u8,

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

impl Cli {
    pub fn run(self) -> Result<(), Box<dyn std::error::Error>> {
        crate::ssh::set_key_passphrase(self.ssh_passphrase.clone());

        if self.version {
            println!("{}", display_version());
            return Ok(());
        }

        let Some(command) = self.command else {
            Cli::command().print_help()?;
            println!();
            return Ok(());
        };

        match command {
            Commands::Version => {
                println!("{}", display_version());
                Ok(())
            }
            Commands::Init => commands::init::run(self.config.as_deref()),
            Commands::Logs {
                env,
                json,
                tail,
                days,
            } => commands::logs::run(env.as_deref(), tail, days, json, self.config.as_deref()),
            Commands::Dev { command, args } => {
                let rt = tokio::runtime::Runtime::new()?;

                match command {
                    None => rt.block_on(commands::dev::run(
                        DEV_PUBLIC_PORT,
                        args.variant,
                        self.config.as_deref(),
                    )),
                    Some(DevSubcommands::Stop { name, all }) => {
                        rt.block_on(commands::dev::stop(name, all, self.config.as_deref()))
                    }
                    Some(DevSubcommands::List) => rt.block_on(commands::dev::ls()),
                }
            }
            Commands::Doctor => {
                let rt = tokio::runtime::Runtime::new()?;
                rt.block_on(commands::doctor::run())
            }
            Commands::Servers(cmd) => server::run(cmd),
            Commands::Secrets(cmd) => secret::run(cmd, self.config.as_deref()),
            Commands::Storages(cmd) => storage::run(cmd, self.config.as_deref()),
            Commands::Releases(cmd) => releases::run(cmd, self.config.as_deref()),
            Commands::Upgrade => upgrade::run(),
            Commands::Uninstall { yes } => commands::implode::run(yes),
            Commands::Generate => commands::codegen::run(self.config.as_deref()),
            Commands::Deploy { env, yes } => {
                commands::deploy::run(env.as_deref(), yes, self.config.as_deref())
            }
            Commands::Delete { env, server, yes } => delete::run(
                env.as_deref(),
                server.as_deref(),
                yes,
                self.config.as_deref(),
            ),
            Commands::Scale {
                instances,
                env,
                server,
                app,
            } => scale::run(
                instances,
                env.as_deref(),
                server.as_deref(),
                app.as_deref(),
                self.config.as_deref(),
            ),
        }
    }
}
