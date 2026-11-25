//! `tako db`: Tako-hosted SQLite databases.
//!
//! A database belongs to the network, not to an environment: commands find
//! the network from the project's environments, and environments bind
//! databases by name in `tako.toml`.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use clap::{Args, Subcommand};
use serde::de::DeserializeOwned;
use tako_core::{
    Command, DatabaseEngine, DatabaseInfo, DatabaseSnapshot, Response, SnapshotReason,
};

use crate::commands::project_context;
use crate::commands::project_resources::database_user_label;
use crate::config::{ServerInventory, TakoToml};
use crate::management_http::ManagementClient;
use crate::network::databases::DatabaseRecord;
use crate::output;

pub(crate) mod cleanup;
pub(crate) mod replication;
mod shell;
pub(crate) mod target;
pub(crate) mod transfer;

use target::{Home, resolve_home};

mod at;

#[derive(Args)]
pub struct DbArgs {
    #[command(subcommand)]
    command: DbCommands,
}

#[derive(Subcommand)]
pub enum DbCommands {
    /// Create a database on a server of the network
    Create {
        /// Database name, unique within the network
        name: String,
        /// Server that hosts the database (required when the network has several)
        #[arg(long)]
        server: Option<String>,
    },

    /// List the network's databases
    #[command(visible_alias = "ls")]
    List,

    /// Show a database's size, users, and backups
    Status {
        /// Database name (every database the project uses when omitted)
        name: Option<String>,
    },

    /// Run SQL against a database
    Shell {
        /// Database name
        name: String,
        /// SQL to run instead of opening an interactive shell
        sql: Option<String>,
    },

    /// Create, list, and apply SQL migrations
    #[command(subcommand)]
    Migration(MigrationCommands),

    /// Capture a full snapshot now
    Backup {
        /// Database name
        name: String,
    },

    /// List a database's restore points and the time it can be restored to
    Backups {
        /// Database name
        name: String,
    },

    /// Replace a database with its backed-up state
    #[command(group(
        clap::ArgGroup::new("point")
            .required(true)
            .args(["latest", "snapshot", "at"])
    ))]
    Restore {
        /// Database name
        name: String,
        /// Restore the newest backed-up state
        #[arg(long)]
        latest: bool,
        /// Restore point ID from `tako db backups`
        #[arg(long)]
        snapshot: Option<String>,
        /// Restore the state at this time: `2026-11-23 10:15` (local time), RFC 3339, or an age such as `15m`, `2h` or `1d`
        #[arg(long, value_name = "TIME")]
        at: Option<String>,
        /// Skip confirmation prompt
        #[arg(short = 'y', long = "yes")]
        yes: bool,
    },

    /// Replace a database with a local SQLite file
    Import {
        /// Database name
        name: String,
        /// SQLite database file
        file: PathBuf,
        /// Skip confirmation prompt
        #[arg(short = 'y', long = "yes")]
        yes: bool,
    },

    /// Download a consistent copy of a database
    Export {
        /// Database name
        name: String,
        /// Output file
        file: PathBuf,
    },

    /// Empty a database and apply its migrations again
    Reset {
        /// Database name
        name: String,
        /// Reset the local development file instead
        #[arg(long)]
        local: bool,
        /// Reset a database the production environment binds
        #[arg(long)]
        force: bool,
        /// Skip confirmation prompt
        #[arg(short = 'y', long = "yes")]
        yes: bool,
    },

    /// Retire an unused database and its replicas
    #[command(visible_alias = "rm")]
    Delete {
        /// Database name
        name: String,
        /// Skip confirmation prompt
        #[arg(short = 'y', long = "yes")]
        yes: bool,
    },
}

#[derive(Subcommand)]
pub enum MigrationCommands {
    /// Create an empty migration file named with the current UTC time
    New {
        /// What the migration does, such as "add orders"
        name: String,
        /// Binding whose folder receives it, when the project binds several
        #[arg(long)]
        binding: Option<String>,
    },
    /// List the migrations each database has yet to apply
    #[command(visible_alias = "ls")]
    List {
        /// Database name (every database the project binds when omitted)
        database: Option<String>,
        /// Read the local development files instead
        #[arg(long)]
        local: bool,
    },
    /// Apply pending migrations now; deploys and `tako dev` also apply them
    Apply {
        /// Database name (every database the project binds when omitted)
        database: Option<String>,
        /// Apply to the local development files instead
        #[arg(long)]
        local: bool,
    },
}

/// Reports what a dry run skipped; in JSON mode as `{"dry_run": true, ...}`.
pub(crate) fn dry_run(
    message: &str,
    mut value: serde_json::Value,
) -> Result<(), Box<dyn std::error::Error>> {
    output::dry_run_skip(message);
    if output::is_json() {
        value["dry_run"] = serde_json::Value::Bool(true);
        output::json_result(value)?;
    }
    Ok(())
}

pub fn run(args: DbArgs, config_path: Option<&Path>) -> Result<(), Box<dyn std::error::Error>> {
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(run_async(args, config_path))
}

/// The network a project's databases live in: `requested`, else the one its
/// environments record, else this machine's only network or the one picked.
fn project_network(
    config: &TakoToml,
    requested: Option<&str>,
) -> Result<crate::config::LocalNetwork, Box<dyn std::error::Error>> {
    if let Some(requested) = requested {
        return crate::network::find_network(requested);
    }
    let networks: BTreeSet<&str> = config
        .envs
        .iter()
        .filter(|(name, _)| name.as_str() != "development")
        .filter_map(|(_, spec)| spec.network.as_deref())
        .collect();
    match networks.len() {
        0 => Ok(crate::network::resolve_network(None)?),
        1 => Ok(crate::network::find_network(
            networks.into_iter().next().expect("one network"),
        )?),
        _ => Err(format!(
            "This project's environments use several networks ({}); pass --network.",
            networks.into_iter().collect::<Vec<_>>().join(", ")
        )
        .into()),
    }
}

/// An environment's bindings of one engine: `databases` or `kv`.
pub(crate) fn env_bindings(
    env: &crate::config::EnvConfig,
    engine: DatabaseEngine,
) -> &std::collections::HashMap<String, String> {
    match engine {
        DatabaseEngine::Sqlite => &env.databases,
        DatabaseEngine::Kv => &env.kv,
    }
}

/// The CLI command that manages databases of `engine`.
pub(crate) fn command_name(engine: DatabaseEngine) -> &'static str {
    match engine {
        DatabaseEngine::Sqlite => "db",
        DatabaseEngine::Kv => "kv",
    }
}

/// Database name → the binding names that point to it, in environments
/// other than development.
fn project_bindings(
    config: &TakoToml,
    engine: DatabaseEngine,
) -> BTreeMap<String, BTreeSet<String>> {
    let mut bindings: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (env, env_config) in &config.envs {
        if env == "development" {
            continue;
        }
        for (binding, database) in env_bindings(env_config, engine) {
            bindings
                .entry(database.clone())
                .or_default()
                .insert(binding.clone());
        }
    }
    bindings
}

/// The environments that bind `database`, sorted.
fn binding_envs<'a>(config: &'a TakoToml, database: &str, engine: DatabaseEngine) -> Vec<&'a str> {
    let mut envs: Vec<&str> = config
        .envs
        .iter()
        .filter(|(env, env_config)| {
            env.as_str() != "development"
                && env_bindings(env_config, engine)
                    .values()
                    .any(|d| d == database)
        })
        .map(|(env, _)| env.as_str())
        .collect();
    envs.sort_unstable();
    envs
}

#[derive(Default)]
struct DatabaseUsers<'a> {
    environments: BTreeSet<&'a str>,
    channel_replay: BTreeSet<&'a str>,
    workflows: BTreeSet<&'a str>,
}

impl DatabaseUsers<'_> {
    fn replay_env_names(&self) -> String {
        join_envs(&self.channel_replay)
    }

    fn workflow_env_names(&self) -> String {
        join_envs(&self.workflows)
    }

    /// The server-owned uses, for listings: "channel replay for production".
    fn services(&self) -> Vec<String> {
        let mut uses = Vec::new();
        if !self.channel_replay.is_empty() {
            uses.push(format!("channel replay for {}", self.replay_env_names()));
        }
        if !self.workflows.is_empty() {
            uses.push(format!("workflow runs for {}", self.workflow_env_names()));
        }
        uses
    }
}

fn join_envs(envs: &BTreeSet<&str>) -> String {
    envs.iter().copied().collect::<Vec<_>>().join(", ")
}

/// Project uses of each database of `engine` in this network: its bindings,
/// and for SQLite also channel replay and workflow runs, which have no SQL
/// alias.
fn database_users<'a>(
    config: &'a TakoToml,
    network: &str,
    records: &[DatabaseRecord],
    needs_replay: bool,
    needs_workflows: bool,
    engine: DatabaseEngine,
) -> BTreeMap<String, DatabaseUsers<'a>> {
    let mut users = BTreeMap::<String, DatabaseUsers>::new();
    for (env, spec) in &config.envs {
        if env == "development" || spec.network.as_deref().is_some_and(|id| id != network) {
            continue;
        }
        for name in env_bindings(spec, engine).values() {
            users
                .entry(name.clone())
                .or_default()
                .environments
                .insert(env);
        }
        if engine != DatabaseEngine::Sqlite {
            continue;
        }
        if let Some(name) =
            super::project_resources::replay_database(config, env, records, needs_replay)
        {
            let database = users.entry(name.to_owned()).or_default();
            database.environments.insert(env);
            database.channel_replay.insert(env);
        }
        if let Some(name) =
            super::project_resources::workflows_database(config, env, records, needs_workflows)
        {
            let database = users.entry(name.to_owned()).or_default();
            database.environments.insert(env);
            database.workflows.insert(env);
        }
    }
    users
}

/// The project and network a command acts in.
pub(crate) struct Context {
    project_dir: PathBuf,
    config_path: PathBuf,
    config: TakoToml,
    network: String,
    servers: ServerInventory,
    /// Whether the project's code still uses channels, so its generated
    /// replay databases are in use.
    needs_replay: bool,
    /// Whether the project still runs workflows, so its generated workflow
    /// databases are in use.
    needs_workflows: bool,
}

impl Context {
    pub(crate) async fn load(
        config_path: Option<&Path>,
        network: Option<&str>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let project = project_context::resolve_existing(config_path)?;
        let config = TakoToml::load_from_file(&project.config_path)?;
        let network = project_network(&config, network)?;
        let servers =
            crate::commands::helpers::load_management_servers(Some(vec![network.clone()])).await?;
        let needs_replay =
            crate::commands::deploy::project_needs_channel_replay(&project.project_dir, &config);
        let needs_workflows =
            crate::commands::deploy::project_needs_workflows(&project.project_dir, &config);
        Ok(Self {
            project_dir: project.project_dir,
            config_path: project.config_path,
            config,
            network: network.id,
            servers,
            needs_replay,
            needs_workflows,
        })
    }

    fn alias(&self) -> Result<crate::config::LocalNetwork, Box<dyn std::error::Error>> {
        Ok(crate::config::LocalNetworks::load()?
            .select(Some(&self.network))?
            .clone())
    }

    fn is_admin(&self) -> Result<bool, Box<dyn std::error::Error>> {
        crate::commands::helpers::is_full_client(&self.network)
    }

    pub(crate) fn require_admin(&self) -> Result<(), Box<dyn std::error::Error>> {
        if self.is_admin()? {
            Ok(())
        } else {
            Err("Managing databases requires full access.".into())
        }
    }

    /// Every trusted server of the project's network.
    fn network_homes(&self) -> Result<Vec<Home>, Box<dyn std::error::Error>> {
        let keys = crate::commands::helpers::resolve_network_servers(
            &self.servers,
            &self.network,
            &[],
            "this project",
        )?;
        Ok(keys
            .iter()
            .filter_map(|key| self.servers.get(key)?.native.as_ref())
            .filter_map(|native| self.home(native.server_name.as_str()).ok())
            .collect())
    }

    fn home(&self, selector: &str) -> Result<Home, Box<dyn std::error::Error>> {
        Ok(resolve_home(
            &self.servers,
            &self.network,
            selector,
            "this project",
        )?)
    }

    /// A SQLite database of the network and its home server.
    async fn database(
        &self,
        name: &str,
    ) -> Result<(DatabaseRecord, Home), Box<dyn std::error::Error>> {
        self.resource(name, DatabaseEngine::Sqlite).await
    }

    /// A database of `engine` and its home server.
    pub(crate) async fn resource(
        &self,
        name: &str,
        engine: DatabaseEngine,
    ) -> Result<(DatabaseRecord, Home), Box<dyn std::error::Error>> {
        let record = self.record_of(name, engine)?;
        let home = self.database_home(&record).await?;
        Ok((record, home))
    }

    async fn database_home(
        &self,
        record: &DatabaseRecord,
    ) -> Result<Home, Box<dyn std::error::Error>> {
        if !output::is_dry_run() {
            return replication::leader(&self.network_homes()?, &record.generation).await;
        }
        self.home(&record.server).map_err(|error| {
            format!(
                "The server of {} '{}': {error}",
                record.engine.noun(),
                record.name
            )
            .into()
        })
    }

    /// The record of `name`, which must be of `engine`, whether or not its
    /// home is still a member.
    fn record_of(
        &self,
        name: &str,
        engine: DatabaseEngine,
    ) -> Result<DatabaseRecord, Box<dyn std::error::Error>> {
        tako_core::validate_database_name(name)?;
        Ok(find_record_of(&self.records()?, name, engine)?.clone())
    }

    fn backup_key(&self, record: &DatabaseRecord) -> Result<String, Box<dyn std::error::Error>> {
        let alias = self.alias()?;
        let key =
            tokio::task::block_in_place(|| crate::network::databases::backup_key(&alias, record))?;
        Ok(key.to_string())
    }

    fn records(&self) -> Result<Vec<DatabaseRecord>, Box<dyn std::error::Error>> {
        let alias = self.alias()?;
        tokio::task::block_in_place(|| crate::network::databases::list(&alias))
    }
}

/// The record of `name`, which must be of `engine`: databases and KV stores
/// share the network's names, so the other kind is named in the error.
fn find_record_of<'a>(
    records: &'a [DatabaseRecord],
    name: &str,
    engine: DatabaseEngine,
) -> Result<&'a DatabaseRecord, String> {
    let record = records
        .iter()
        .find(|record| record.name == name)
        .ok_or_else(|| {
            format!(
                "The network has no {} named '{name}'. Run `tako {} list` to see them.",
                engine.noun(),
                command_name(engine)
            )
        })?;
    if record.engine != engine {
        return Err(format!(
            "'{name}' is a {}, not a {}; manage it with `tako {}`.",
            record.engine.noun(),
            engine.noun(),
            command_name(record.engine)
        ));
    }
    Ok(record)
}

pub(crate) async fn send<T: DeserializeOwned>(
    home: &Home,
    command: Command,
) -> Result<T, Box<dyn std::error::Error>> {
    let mut client = ManagementClient::for_server(&home.entry).await?;
    match client.send(&command).await? {
        Response::Ok { data } => Ok(serde_json::from_value(data)?),
        Response::Error { message, .. } => Err(message.into()),
    }
}

pub(crate) fn confirm(
    question: &str,
    description: &str,
    yes: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    if yes || !output::is_interactive() {
        if !yes && !output::is_dry_run() {
            return Err(format!("{question} Pass --yes to confirm.").into());
        }
        return Ok(());
    }
    if output::confirm_with_description(question, Some(description), false)? {
        Ok(())
    } else {
        Err(output::operation_cancelled_error().into())
    }
}

async fn run_async(
    args: DbArgs,
    config_path: Option<&Path>,
) -> Result<(), Box<dyn std::error::Error>> {
    let network = crate::network::network_flag();
    match args.command {
        DbCommands::Create { name, server } => {
            let ctx = Context::load(config_path, network).await?;
            create(&ctx, &name, server.as_deref(), DatabaseEngine::Sqlite).await
        }
        DbCommands::List => {
            let ctx = Context::load(config_path, network).await?;
            list(&ctx, DatabaseEngine::Sqlite)
        }
        DbCommands::Status { name } => {
            let ctx = Context::load(config_path, network).await?;
            status(&ctx, name.as_deref(), DatabaseEngine::Sqlite).await
        }
        DbCommands::Shell { name, sql } => {
            let ctx = Context::load(config_path, network).await?;
            ctx.require_admin()?;
            let (_, home) = ctx.database(&name).await?;
            shell::run(&home, &name, sql).await
        }
        DbCommands::Migration(command) => match command {
            MigrationCommands::New { name, binding } => {
                new_migration(config_path, &name, binding.as_deref())
            }
            MigrationCommands::List { database, local } => {
                if local {
                    return migrate_local(config_path, database.as_deref(), true);
                }
                let ctx = Context::load(config_path, network).await?;
                migrate(&ctx, database.as_deref(), true).await
            }
            MigrationCommands::Apply { database, local } => {
                if local {
                    return migrate_local(config_path, database.as_deref(), false);
                }
                let ctx = Context::load(config_path, network).await?;
                migrate(&ctx, database.as_deref(), false).await
            }
        },
        DbCommands::Backup { name } => {
            let ctx = Context::load(config_path, network).await?;
            backup_now(&ctx, &name, DatabaseEngine::Sqlite).await
        }
        DbCommands::Backups { name } => {
            let ctx = Context::load(config_path, network).await?;
            backups(&ctx, &name, DatabaseEngine::Sqlite).await
        }
        DbCommands::Restore {
            name,
            latest: _,
            snapshot,
            at,
            yes,
        } => {
            let at = at
                .map(|at| at::parse_at(&at, chrono::Utc::now(), &chrono::Local))
                .transpose()?;
            let ctx = Context::load(config_path, network).await?;
            restore(&ctx, &name, snapshot, at, yes, DatabaseEngine::Sqlite).await
        }
        DbCommands::Import { name, file, yes } => {
            let ctx = Context::load(config_path, network).await?;
            ctx.require_admin()?;
            let (_, home) = ctx.database(&name).await?;
            confirm(
                &format!("Replace {name} with {}?", file.display()),
                "Apps' requests to the database pause while it is replaced. When backups are configured, the current contents are backed up first and named with a restore point.",
                yes,
            )?;
            if output::is_dry_run() {
                return dry_run(
                    &format!("Import {} into {name}", file.display()),
                    serde_json::json!({"database": name, "file": file}),
                );
            }
            transfer::import(&home, &name, &file, DatabaseEngine::Sqlite).await
        }
        DbCommands::Export { name, file } => {
            let ctx = Context::load(config_path, network).await?;
            ctx.require_admin()?;
            let (_, home) = ctx.database(&name).await?;
            if output::is_dry_run() {
                return dry_run(
                    &format!("Export {name} to {}", file.display()),
                    serde_json::json!({"database": name, "file": file}),
                );
            }
            transfer::export(&home, &name, &file, DatabaseEngine::Sqlite).await
        }
        DbCommands::Reset {
            name,
            local,
            force,
            yes,
        } => {
            if local {
                return reset_local(config_path, &name, yes);
            }
            let ctx = Context::load(config_path, network).await?;
            reset(&ctx, &name, force, yes).await
        }
        DbCommands::Delete { name, yes } => {
            let ctx = Context::load(config_path, network).await?;
            delete(&ctx, &name, yes, DatabaseEngine::Sqlite).await
        }
    }
}

pub(crate) async fn create(
    ctx: &Context,
    name: &str,
    server: Option<&str>,
    engine: DatabaseEngine,
) -> Result<(), Box<dyn std::error::Error>> {
    tako_core::validate_database_name(name)?;
    let noun = engine.noun();
    ctx.require_admin()?;
    let home = match server {
        Some(server) => ctx.home(server)?,
        None => {
            let servers = crate::commands::helpers::resolve_network_servers(
                &ctx.servers,
                &ctx.network,
                &[],
                "this project",
            )?;
            match servers.as_slice() {
                [only] => {
                    let entry = ctx
                        .servers
                        .get(only)
                        .and_then(|entry| entry.native.as_ref())
                        .ok_or("The network's server is not a native network server")?;
                    ctx.home(entry.server_name.as_str())?
                }
                _ => {
                    return Err(format!(
                        "The network has several servers. Pass --server to choose the {noun}'s server."
                    )
                    .into());
                }
            }
        }
    };
    // Names identify databases and KV stores alike in the network.
    if let Some(existing) = ctx
        .records()?
        .into_iter()
        .find(|record| record.name == name)
    {
        let key = match existing.engine {
            DatabaseEngine::Sqlite => "db",
            DatabaseEngine::Kv => "kv",
        };
        return Err(format!(
            "The network already has a {} named '{name}'. To use it, add `{key} = {{ {name} = \"{name}\" }}` under [envs.<env>] in tako.toml{}.",
            existing.engine.noun(),
            match existing.engine {
                DatabaseEngine::Sqlite => format!(
                    "; for channel replay, set `db = \"{name}\"` under [envs.<env>.channels]"
                ),
                DatabaseEngine::Kv => String::new(),
            }
        )
        .into());
    }
    if output::is_dry_run() {
        return dry_run(
            &format!("Create {noun} {name} on {}", home.name),
            serde_json::json!({"database": name, "engine": engine, "server": home.name}),
        );
    }

    let _t = output::timed(&format!("Create {noun} {name}"));
    output::with_spinner_async(
        &format!("Creating {name} on {}", home.name),
        &format!("Created {name} on {}", home.name),
        async {
            create_database(ctx, &home, name, engine)
                .await
                .map_err(|e| e.to_string())
        },
    )
    .await?;
    if output::is_json() {
        return output::json_result(serde_json::json!({
            "ok": true,
            "command": command_name(engine),
            "database": {"name": name, "engine": engine, "server": home.name},
        }));
    }
    output::hint(&match engine {
        DatabaseEngine::Sqlite => format!(
            "To use it for app data, add `db = {{ {name} = \"{name}\" }}` under [envs.<env>] in tako.toml (the key is the binding name the app reads as `tako.db.<binding>`). For channel replay, set `db = \"{name}\"` under [envs.<env>.channels] instead. Then deploy."
        ),
        DatabaseEngine::Kv => format!(
            "To use it, add `kv = {{ {name} = \"{name}\" }}` under [envs.<env>] in tako.toml (the key is the binding name the app reads as `tako.kv.<binding>`), then deploy."
        ),
    });
    Ok(())
}

/// Create and publish one database or KV store generation. Both callers
/// have already resolved placement and checked for full access.
async fn create_database(
    ctx: &Context,
    home: &Home,
    name: &str,
    engine: DatabaseEngine,
) -> Result<DatabaseRecord, Box<dyn std::error::Error>> {
    use tako_core::StorageOperation as Op;
    let network = ctx.alias()?;
    let homes = ctx.network_homes()?;
    let catalog = replication::catalog(&homes).await?;
    let record = replication::reserve(
        &network,
        &catalog,
        std::slice::from_ref(home),
        name,
        engine,
        (None, None),
    )
    .await?;
    let key = ctx.backup_key(&record)?;
    let mut targets = record.founders.clone();
    targets.insert(home.device_id.clone());
    for target in &targets {
        let member = ctx.home(target)?;
        replication::command::<serde_json::Value>(
            &member,
            Op::OpenReplica {
                claim: replication::claim(&record),
                backup_key: key.clone(),
            },
        )
        .await?;
    }
    let leader = replication::leader(&homes, &record.generation).await?;
    replication::command::<serde_json::Value>(
        &leader,
        Op::SeedReplica {
            name: record.name.clone(),
            generation: record.generation.clone(),
        },
    )
    .await?;
    replication::command::<serde_json::Value>(
        &leader,
        Op::AddMembers {
            group: record.generation.clone(),
            targets: targets.into_iter().collect(),
        },
    )
    .await?;
    Ok(record)
}

pub(crate) fn list(
    ctx: &Context,
    engine: DatabaseEngine,
) -> Result<(), Box<dyn std::error::Error>> {
    let all = ctx.records()?;
    let users = database_users(
        &ctx.config,
        &ctx.network,
        &all,
        ctx.needs_replay,
        ctx.needs_workflows,
        engine,
    );
    let records: Vec<DatabaseRecord> = all
        .into_iter()
        .filter(|record| record.engine == engine)
        .collect();
    let server_name = |id: &str| {
        ctx.servers
            .servers
            .values()
            .filter_map(|entry| entry.native.as_ref())
            .find(|native| native.server_id.to_string() == id)
            .map(|native| native.server_name.to_string())
            .unwrap_or_else(|| id.to_owned())
    };
    if output::is_json() {
        let empty = DatabaseUsers::default();
        let databases: Vec<_> = records
            .iter()
            .map(|record| {
                serde_json::json!({
                    "name": record.name,
                    "server": server_name(&record.server),
                    "server_id": record.server,
                    "created_at_unix_secs": record.created_at_unix_secs,
                    "in_project": users.contains_key(&record.name),
                    "channel_replay_envs": users.get(&record.name).unwrap_or(&empty).channel_replay,
                    "channel_owner": record.channel_owner,
                    "workflow_envs": users.get(&record.name).unwrap_or(&empty).workflows,
                    "workflows_owner": record.workflows_owner,
                })
            })
            .collect();
        return output::json_result(
            serde_json::json!({"ok": true, "command": command_name(engine), "databases": databases}),
        );
    }
    if records.is_empty() {
        output::info(&match engine {
            DatabaseEngine::Sqlite => {
                "No databases yet. Create one with `tako db create <name>`.".to_string()
            }
            DatabaseEngine::Kv => {
                "No KV stores yet. Create one with `tako kv create <name>`.".to_string()
            }
        });
        return Ok(());
    }
    output::section(match engine {
        DatabaseEngine::Sqlite => "Databases",
        DatabaseEngine::Kv => "KV stores",
    });
    for record in &records {
        let project = match users.get(&record.name) {
            Some(db) if !db.services().is_empty() => format!(" · {}", db.services().join(" · ")),
            Some(_) => String::new(),
            None => " · not in this project".to_owned(),
        };
        output::bullet(&format!(
            "{} on {}{project}",
            output::strong(&record.name),
            server_name(&record.server)
        ));
    }
    Ok(())
}

pub(crate) async fn status(
    ctx: &Context,
    name: Option<&str>,
    engine: DatabaseEngine,
) -> Result<(), Box<dyn std::error::Error>> {
    ctx.require_admin()?;
    if let Some(name) = name {
        tako_core::validate_database_name(name)?;
    }
    let records = ctx.records()?;
    let project = database_users(
        &ctx.config,
        &ctx.network,
        &records,
        ctx.needs_replay,
        ctx.needs_workflows,
        engine,
    );
    let names: Vec<String> = match name {
        Some(name) => vec![name.to_owned()],
        None => project.keys().cloned().collect(),
    };
    let mut infos = Vec::new();
    for name in &names {
        let home = ctx
            .database_home(find_record_of(&records, name, engine)?)
            .await?;
        let value: serde_json::Value = send(
            &home,
            Command::DatabaseStatus {
                name: Some(name.clone()),
            },
        )
        .await?;
        let info: Vec<DatabaseInfo> = serde_json::from_value(value["databases"].clone())?;
        if let Some(info) = info.into_iter().next() {
            infos.push((name.clone(), home.name.clone(), info));
        }
    }
    if output::is_json() {
        let empty = DatabaseUsers::default();
        let databases: Vec<_> = infos
            .iter()
            .map(|(name, server, info)| {
                serde_json::json!({
                    "name": name, "server": server, "status": info,
                    "channel_replay_envs": project.get(name).unwrap_or(&empty).channel_replay,
                    "workflow_envs": project.get(name).unwrap_or(&empty).workflows,
                })
            })
            .collect();
        return output::json_result(
            serde_json::json!({"ok": true, "command": command_name(engine), "databases": databases}),
        );
    }
    if infos.is_empty() {
        output::info(&format!("This project uses no {}s.", engine.noun()));
    }
    for (name, server, info) in &infos {
        output::section(name);
        output::info(&format!(
            "{} on {server}",
            output::format_size(info.size_bytes)
        ));
        if let Some(db) = project.get(name) {
            for service in db.services() {
                let mut chars = service.chars();
                let first = chars.next().map(|c| c.to_ascii_uppercase());
                output::info(&format!("{}{}", first.unwrap_or_default(), chars.as_str()));
            }
        }
        let users: BTreeSet<_> = info
            .bindings
            .iter()
            .chain(info.remote_grants.iter())
            .map(|scope| database_user_label(scope))
            .collect();
        output::info(&if users.is_empty() {
            "Not used by any app yet".to_string()
        } else {
            format!(
                "Used by {}",
                users.into_iter().collect::<Vec<_>>().join(", ")
            )
        });
        match &info.backup {
            None => output::info("Backups not configured"),
            Some(backup) => {
                output::info(&backup_status_line(engine, backup));
                if let Some(error) = &backup.last_error {
                    output::warning(&format!("Last backup failed: {error}"));
                }
            }
        }
    }
    Ok(())
}

/// How `status` describes backups: continuous for SQLite databases,
/// snapshots for KV stores.
fn backup_status_line(engine: DatabaseEngine, backup: &tako_core::DatabaseBackupStatus) -> String {
    let snapshot = backup
        .last_snapshot
        .as_ref()
        .map(|s| format_time(s.created_at_unix_secs));
    match engine {
        DatabaseEngine::Sqlite => {
            let replicated = backup
                .replicated_at_unix_secs
                .map(|at| format!("backed up through {}", format_time(at)))
                .unwrap_or_else(|| "not backed up yet".into());
            let full = snapshot
                .map(|at| format!(" · full snapshot {at}"))
                .unwrap_or_default();
            format!(
                "Continuous backups to {} · {replicated}{full}",
                backup.bucket
            )
        }
        DatabaseEngine::Kv => {
            let last = snapshot
                .map(|at| format!("last snapshot {at}"))
                .unwrap_or_else(|| "no snapshots yet".into());
            format!("Snapshots to {} · {last}", backup.bucket)
        }
    }
}

pub(crate) async fn backup_now(
    ctx: &Context,
    name: &str,
    engine: DatabaseEngine,
) -> Result<(), Box<dyn std::error::Error>> {
    ctx.require_admin()?;
    let (_, home) = ctx.resource(name, engine).await?;
    if output::is_dry_run() {
        return dry_run(
            &format!("Full snapshot of {name}"),
            serde_json::json!({"database": name}),
        );
    }
    let _t = output::timed(&format!("Full snapshot of {name}"));
    let value: serde_json::Value = output::with_spinner_async(
        &format!("Capturing a full snapshot of {name}"),
        &format!("Uploaded a full snapshot of {name}"),
        async {
            send(
                &home,
                Command::SnapshotDatabase {
                    name: name.to_owned(),
                    reason: SnapshotReason::Manual,
                    app: None,
                },
            )
            .await
            .map_err(|e| e.to_string())
        },
    )
    .await?;
    if output::is_json() {
        return output::json_result(
            serde_json::json!({"ok": true, "command": command_name(engine), "result": value}),
        );
    }
    if let Ok(snapshot) = serde_json::from_value::<DatabaseSnapshot>(value["snapshot"].clone()) {
        output::hint(&format!(
            "Snapshot {} · {} · restore it with `tako {} restore {name} --snapshot {}`",
            snapshot.id,
            output::format_size(snapshot.stored_bytes),
            command_name(engine),
            snapshot.id
        ));
    }
    Ok(())
}

pub(crate) async fn backups(
    ctx: &Context,
    name: &str,
    engine: DatabaseEngine,
) -> Result<(), Box<dyn std::error::Error>> {
    ctx.require_admin()?;
    let (_, home) = ctx.resource(name, engine).await?;
    let value: serde_json::Value = send(
        &home,
        Command::ListDatabaseSnapshots {
            name: name.to_owned(),
        },
    )
    .await?;
    let snapshots: Vec<DatabaseSnapshot> = serde_json::from_value(value["snapshots"].clone())?;
    let windows = value["windows"].as_array().cloned().unwrap_or_default();
    if output::is_json() {
        return output::json_result(serde_json::json!({
            "ok": true,
            "command": command_name(engine),
            "database": name,
            "windows": windows,
            "snapshots": snapshots,
        }));
    }
    if snapshots.is_empty() && windows.is_empty() {
        output::info(&format!("{name} has no backups yet."));
        return Ok(());
    }
    // The newest window holds the current history; older ones end where the
    // database moved or was replaced from elsewhere.
    if let Some(current) = windows.last() {
        let time = |key: &str| format_time(current[key].as_i64().unwrap_or(0).max(0) as u64 / 1000);
        output::section(&format!("Point-in-time restore of {name}"));
        output::bullet(&format!(
            "Any moment from {} to {} (`tako db restore {name} --at <time>`)",
            time("from_unix_ms"),
            time("to_unix_ms")
        ));
    }
    if snapshots.is_empty() {
        return Ok(());
    }
    output::section(&format!("Restore points of {name}"));
    for snapshot in &snapshots {
        let reason = match snapshot.reason {
            SnapshotReason::Scheduled => "full snapshot",
            SnapshotReason::Manual => "manual full snapshot",
            SnapshotReason::Release => "before release",
            SnapshotReason::Restore => "before restore",
            SnapshotReason::Migration => "before migrations",
        };
        let size = if snapshot.full && snapshot.stored_bytes > 0 {
            format!(" · {}", output::format_size(snapshot.stored_bytes))
        } else {
            String::new()
        };
        output::bullet(&format!(
            "{} · {} · {reason}{size}",
            snapshot.id,
            format_time(snapshot.created_at_unix_secs),
        ));
    }
    Ok(())
}

pub(crate) async fn restore(
    ctx: &Context,
    name: &str,
    snapshot: Option<String>,
    at_unix_ms: Option<i64>,
    yes: bool,
    engine: DatabaseEngine,
) -> Result<(), Box<dyn std::error::Error>> {
    ctx.require_admin()?;
    let (record, target) = ctx.resource(name, engine).await?;
    let which = match (&snapshot, at_unix_ms) {
        (Some(snapshot), _) => format!("restore point {snapshot}"),
        (None, Some(at)) => format!("its state at {}", at::describe(at)),
        (None, None) => "its newest backed-up state".to_owned(),
    };
    confirm(
        &format!("Restore {name} from {which} on {}?", target.name),
        match engine {
            DatabaseEngine::Sqlite => {
                "Apps' requests to the database pause while it is replaced. Writes after that point are replaced; the current contents are backed up first and named with a restore point, so the restore can be undone."
            }
            DatabaseEngine::Kv => {
                "Apps keep using the current keys until the restore replaces them all at once. Writes since the snapshot are lost; a snapshot of the current contents is taken first when possible."
            }
        },
        yes,
    )?;
    if output::is_dry_run() {
        return dry_run(
            &format!("Restore {name}"),
            serde_json::json!({"database": name}),
        );
    }
    let backup_key = ctx.backup_key(&record)?;
    let _t = output::timed(&format!("Restore {name}"));
    let restored: Result<serde_json::Value, String> = output::with_spinner_async(
        &format!("Restoring {name}"),
        &format!("Restored {name}"),
        async {
            send(
                &target,
                Command::RestoreDatabase {
                    name: name.to_owned(),
                    snapshot: snapshot.clone(),
                    at_unix_ms,
                    backup_key_base64: Some(backup_key.clone()),
                },
            )
            .await
            .map_err(|e| e.to_string())
        },
    )
    .await;
    finish_restore(&restored?, engine)
}

fn finish_restore(
    value: &serde_json::Value,
    engine: DatabaseEngine,
) -> Result<(), Box<dyn std::error::Error>> {
    for warning in value["warnings"].as_array().into_iter().flatten() {
        if let Some(warning) = warning.as_str() {
            output::warning(warning);
        }
    }
    if output::is_json() {
        return output::json_result(
            serde_json::json!({"ok": true, "command": command_name(engine), "result": value}),
        );
    }
    if let (Some(name), Some(transaction), Some(at)) = (
        value["name"].as_str(),
        value["restored"]["transaction"].as_u64(),
        value["restored"]["captured_at_unix_ms"].as_i64(),
    ) {
        output::hint(&format!(
            "{name} now has its state of {} (transaction {transaction}).",
            format_time(at.max(0) as u64 / 1000)
        ));
    }
    Ok(())
}

pub(crate) async fn delete(
    ctx: &Context,
    name: &str,
    yes: bool,
    engine: DatabaseEngine,
) -> Result<(), Box<dyn std::error::Error>> {
    ctx.require_admin()?;
    let noun = engine.noun();
    let record = ctx.record_of(name, engine)?;
    if engine.is_sqlite() {
        check_service_delete(
            &ctx.config,
            &ctx.network,
            &record,
            ctx.needs_replay,
            ctx.needs_workflows,
        )?;
    }
    confirm(
        &format!("Delete replicated {noun} {name}?"),
        "All replicas are retired once no active or retained release uses them. Existing backup objects are preserved.",
        yes,
    )?;
    if output::is_dry_run() {
        return dry_run(
            &format!("Delete {noun} {name}"),
            serde_json::json!({"database":name,"engine":engine}),
        );
    }
    cleanup::delete_unused(&ctx.alias()?, &ctx.network_homes()?, &record).await?;
    if engine.is_sqlite() {
        TakoToml::remove_database_from_file(&ctx.config_path, name, &ctx.network)?;
    } else {
        TakoToml::remove_kv_from_file(&ctx.config_path, name, &ctx.network)?;
    }
    if output::is_json() {
        return output::json_success(command_name(engine));
    }
    Ok(())
}

fn check_service_delete(
    config: &TakoToml,
    network: &str,
    record: &DatabaseRecord,
    needs_replay: bool,
    needs_workflows: bool,
) -> Result<(), String> {
    let users = database_users(
        config,
        network,
        std::slice::from_ref(record),
        needs_replay,
        needs_workflows,
        DatabaseEngine::Sqlite,
    );
    let Some(db) = users.get(&record.name) else {
        return Ok(());
    };
    for (envs, noun, table) in [
        (db.replay_env_names(), "channel replay", "channels"),
        (db.workflow_env_names(), "workflow", "workflows"),
    ] {
        if !envs.is_empty() {
            return Err(format!(
                "{} is the {noun} database for {envs}. Select a replacement with [envs.<env>.{table}].db and deploy it, or remove the retired environment from this project before deleting the database.",
                record.name
            ));
        }
    }
    Ok(())
}

/// Removes copies of the database (same generation) from the network's
/// servers other than `home`, such as one an unfinished move left behind.
/// Their snapshots are the database's, so they are kept here; a server that
/// cannot be reached is named in a warning.
fn print_warnings(value: &serde_json::Value) {
    for warning in value["warnings"].as_array().into_iter().flatten() {
        if let Some(warning) = warning.as_str() {
            output::warning(warning);
        }
    }
}

/// Sends a binding's migrations to its database's server. `app` names the
/// deploying app environment, for deployers without server administration.
pub(crate) async fn apply_migrations(
    home: &Home,
    database: &str,
    owner: &str,
    migrations: Vec<tako_db::Migration>,
    dry_run: bool,
    app: Option<String>,
) -> Result<tako_core::DatabaseMigrationReport, Box<dyn std::error::Error>> {
    send(
        home,
        Command::MigrateDatabase {
            name: database.to_owned(),
            owner: owner.to_owned(),
            migrations: crate::database_migrations::to_wire(migrations),
            dry_run,
            app,
        },
    )
    .await
}

/// The app the project's migrations belong to.
fn migration_owner(ctx: &Context) -> Result<String, Box<dyn std::error::Error>> {
    Ok(crate::app::require_app_name_from_config_path(
        &ctx.config_path,
    )?)
}

/// Applies the project's pending migrations to each database it binds, or
/// lists them when `dry_run` is set.
async fn migrate(
    ctx: &Context,
    database: Option<&str>,
    dry_run: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut bound = project_bindings(&ctx.config, DatabaseEngine::Sqlite);
    if let Some(database) = database {
        let bindings = bound.remove(database).ok_or_else(|| {
            format!("No environment of this project binds database '{database}'.")
        })?;
        bound = BTreeMap::from([(database.to_owned(), bindings)]);
    }
    let owner = migration_owner(ctx)?;
    let permissions = crate::commands::helpers::network_permissions(&ctx.network)?;
    let full = permissions.is_full();
    let dry_run = dry_run || output::is_dry_run();
    let mut results = Vec::new();
    for (database, bindings) in bound {
        // Deployers act for an environment that binds the database and that
        // they may deploy.
        let app = if full {
            None
        } else {
            let deployable = binding_envs(&ctx.config, &database, DatabaseEngine::Sqlite)
                .into_iter()
                .map(|env| tako_core::deployment_app_id(&owner, env))
                .find(|app| {
                    app.parse::<tako_network::AppEnv>()
                        .is_ok_and(|scope| permissions.can_deploy(&scope))
                });
            Some(deployable.ok_or_else(|| {
                format!(
                    "Applying {database}'s migrations needs permission to deploy an environment that binds it, or full access."
                )
            })?)
        };
        let selected = crate::database_migrations::by_database(
            &ctx.project_dir,
            &ctx.config,
            bindings
                .iter()
                .map(|binding| (binding.as_str(), database.as_str())),
        )?;
        for (_, selected) in selected {
            let (binding, migrations) = (selected.binding, selected.migrations);
            let (_, home) = ctx.database(&database).await?;
            let report =
                apply_migrations(&home, &database, &owner, migrations, dry_run, app.clone())
                    .await
                    .map_err(|e| format!("Migrate {database} ({binding}): {e}"))?;
            for warning in &report.warnings {
                output::warning(&format!("{database}: {warning}"));
            }
            results.push((binding, database.clone(), report));
        }
    }
    if output::is_json() {
        let databases: Vec<_> = results
            .iter()
            .map(|(binding, database, report)| serde_json::json!({"binding": binding, "database": database, "report": report}))
            .collect();
        return output::json_result(serde_json::json!({
            "ok": true, "command": "db", "dry_run": dry_run, "migrations": databases,
        }));
    }
    if results.is_empty() {
        output::info("This project has no migrations for its databases.");
    }
    for (_, database, report) in &results {
        let (verb, names) = if dry_run {
            ("pending", &report.pending)
        } else {
            ("applied", &report.applied)
        };
        if names.is_empty() {
            output::success(&format!("{database} is up to date"));
        } else {
            output::success(&format!("{database}: {verb} {}", names.join(", ")));
        }
    }
    Ok(())
}

fn load_project(
    config_path: Option<&Path>,
) -> Result<(project_context::ProjectContext, TakoToml), Box<dyn std::error::Error>> {
    let project = project_context::resolve_existing(config_path)?;
    let config = TakoToml::load_from_file(&project.config_path)?;
    Ok((project, config))
}

fn migrate_local(
    config_path: Option<&Path>,
    database: Option<&str>,
    dry_run: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let (project, config) = load_project(config_path)?;
    if let Some(database) = database
        && !crate::local_databases::development_bindings(&config)
            .values()
            .any(|name| name == database)
    {
        return Err(format!("This project binds no local database named '{database}'.").into());
    }
    let dry_run = dry_run || output::is_dry_run();
    let migrated = crate::local_databases::development_migrations(
        &project.project_dir,
        &config,
        dry_run,
        database,
    )?;
    for warning in &migrated.warnings {
        output::warning(warning);
    }
    if output::is_json() {
        return output::json_result(serde_json::json!({
            "ok": true, "command": "db", "dry_run": dry_run,
            "applied": migrated.applied, "pending": migrated.pending,
        }));
    }
    let lines = if dry_run {
        &migrated.pending
    } else {
        &migrated.applied
    };
    if lines.is_empty() {
        output::success("Development databases are up to date");
    }
    for line in lines {
        output::success(line);
    }
    Ok(())
}

/// Creates `migrations/[<binding>/]<UTC timestamp>_<slug>.sql`.
fn new_migration(
    config_path: Option<&Path>,
    name: &str,
    binding: Option<&str>,
) -> Result<(), Box<dyn std::error::Error>> {
    let (project, config) = load_project(config_path)?;
    let slug = migration_slug(name)
        .ok_or("A migration name needs at least one letter or digit, such as \"add orders\".")?;
    let root = project.project_dir.join("migrations");
    let names = crate::database_migrations::binding_names(&config);
    // A binding's own folder, or the top-level one when the project binds a
    // single name and has no folder for it.
    let folder = |binding: &str| {
        let own = root.join(binding);
        if own.is_dir() || names.len() > 1 {
            own
        } else {
            root.clone()
        }
    };
    let dir = match binding {
        Some(binding) => {
            tako_core::validate_database_name(binding)?;
            if !names.contains(binding) {
                return Err(format!("No environment of this project binds `{binding}`.").into());
            }
            folder(binding)
        }
        None if names.len() > 1 => {
            return Err(format!(
                "This project binds several databases ({}); pass --binding to choose the folder.",
                names.into_iter().collect::<Vec<_>>().join(", ")
            )
            .into());
        }
        None => match names.iter().next() {
            Some(only) => folder(only),
            None => root.clone(),
        },
    };
    let number = unused_migration_number(&dir, utc_timestamp()?.parse()?);
    let file = dir.join(format!("{number}_{slug}.sql"));
    if file.exists() {
        return Err(format!("{} already exists.", file.display()).into());
    }
    if output::is_dry_run() {
        return dry_run(
            &format!("Create {}", file.display()),
            serde_json::json!({"path": file}),
        );
    }
    std::fs::create_dir_all(&dir)?;
    std::fs::write(&file, format!("-- {name}\n"))?;
    let shown = file
        .strip_prefix(&project.project_dir)
        .unwrap_or(&file)
        .display()
        .to_string();
    if output::is_json() {
        return output::json_result(serde_json::json!({
            "ok": true, "command": "db", "path": shown,
        }));
    }
    output::success(&format!("Created {shown}"));
    Ok(())
}

/// `name` as lowercase letters and digits joined by `_`, or `None` when it
/// has neither.
fn migration_slug(name: &str) -> Option<String> {
    let mut slug = String::new();
    for c in name.chars().flat_map(char::to_lowercase) {
        if c.is_ascii_alphanumeric() {
            slug.push(c);
        } else if !slug.is_empty() && !slug.ends_with('_') {
            slug.push('_');
        }
    }
    let slug = slug.trim_end_matches('_');
    let slug: String = slug.chars().take(80).collect();
    let slug = slug.trim_end_matches('_').to_owned();
    (!slug.is_empty()).then_some(slug)
}

/// The current UTC time as `YYYYMMDDHHMMSS`, which orders migrations.
/// `wanted`, or the next number after it that no migration in `dir` uses:
/// two migrations created in the same second must not share a number.
fn unused_migration_number(dir: &Path, wanted: u64) -> u64 {
    let used: std::collections::BTreeSet<u64> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            tako_db::migrate::migration_number(&entry.file_name().to_string_lossy())
        })
        .collect();
    let mut number = wanted;
    while used.contains(&number) {
        number += 1;
    }
    number
}

fn utc_timestamp() -> Result<String, Box<dyn std::error::Error>> {
    let now = time::OffsetDateTime::now_utc();
    Ok(format!(
        "{:04}{:02}{:02}{:02}{:02}{:02}",
        now.year(),
        u8::from(now.month()),
        now.day(),
        now.hour(),
        now.minute(),
        now.second()
    ))
}

fn reset_local(
    config_path: Option<&Path>,
    name: &str,
    yes: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    tako_core::validate_database_name(name)?;
    let (project, config) = load_project(config_path)?;
    let project_dir = project
        .project_dir
        .canonicalize()
        .unwrap_or_else(|_| project.project_dir.clone());
    let path = crate::local_databases::database_path(&project_dir, name);
    confirm(
        &format!("Reset the local database {name}?"),
        "Every table in it is dropped; then the development migrations run again.",
        yes,
    )?;
    if output::is_dry_run() {
        return dry_run(
            &format!("Reset the local database {name}"),
            serde_json::json!({"database": name}),
        );
    }
    if path.exists() {
        tako_db::reset_database_file(&path)?;
    }
    // Only this database: the others keep their state.
    let migrated = crate::local_databases::development_migrations(
        &project.project_dir,
        &config,
        false,
        Some(name),
    )?;
    for warning in &migrated.warnings {
        output::warning(warning);
    }
    if output::is_json() {
        return output::json_result(serde_json::json!({
            "ok": true, "command": "db", "reset": name, "applied": migrated.applied,
        }));
    }
    output::success(&format!("Reset the local database {name}"));
    for line in &migrated.applied {
        output::info(line);
    }
    Ok(())
}

async fn reset(
    ctx: &Context,
    name: &str,
    force: bool,
    yes: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    ctx.require_admin()?;
    let (record, home) = ctx.database(name).await?;
    if !force
        && database_users(
            &ctx.config,
            &ctx.network,
            std::slice::from_ref(&record),
            ctx.needs_replay,
            ctx.needs_workflows,
            DatabaseEngine::Sqlite,
        )
        .get(name)
        .is_some_and(|db| db.environments.contains("production"))
    {
        return Err(format!(
            "{name} is used by the production environment. Pass --force to reset it anyway."
        )
        .into());
    }
    confirm(
        &format!("Reset {name} on {}?", home.name),
        "Every table in it is deleted; then this project's migrations run again. A snapshot of the current contents is taken first when backups are configured.",
        yes,
    )?;
    if output::is_dry_run() {
        return dry_run(
            &format!("Reset {name}"),
            serde_json::json!({"database": name}),
        );
    }
    // The project's migrations for this database are applied to the empty
    // one before it replaces the current contents.
    let owner = migration_owner(ctx)?;
    let bindings = project_bindings(&ctx.config, DatabaseEngine::Sqlite)
        .remove(name)
        .unwrap_or_default();
    let selected = crate::database_migrations::by_database(
        &ctx.project_dir,
        &ctx.config,
        bindings.iter().map(|binding| (binding.as_str(), name)),
    )?;
    let migrations = selected
        .into_iter()
        .next()
        .map(|(_, selected)| selected.migrations)
        .unwrap_or_default();
    let reset: serde_json::Value = output::with_spinner_async(
        &format!("Resetting {name}"),
        &format!("Reset {name}"),
        async {
            send::<serde_json::Value>(
                &home,
                Command::ResetDatabase {
                    name: name.to_owned(),
                    owner: Some(owner),
                    migrations: crate::database_migrations::to_wire(migrations),
                },
            )
            .await
            .map_err(|e| e.to_string())
        },
    )
    .await?;
    print_warnings(&reset);
    let applied: Vec<String> = reset["applied"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|name| name.as_str().map(str::to_owned))
        .collect();
    if output::is_json() {
        return output::json_result(serde_json::json!({
            "ok": true, "command": "db", "reset": name, "applied": applied,
        }));
    }
    if !applied.is_empty() {
        output::info(&format!("Applied {}", applied.join(", ")));
    }
    Ok(())
}

pub(crate) fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn format_time(unix: u64) -> String {
    static LOCAL_OFFSET: std::sync::OnceLock<time::UtcOffset> = std::sync::OnceLock::new();
    let offset = *LOCAL_OFFSET
        .get_or_init(|| time::UtcOffset::current_local_offset().unwrap_or(time::UtcOffset::UTC));
    time::OffsetDateTime::from_unix_timestamp(unix as i64)
        .map(|dt| {
            let dt = dt.to_offset(offset);
            format!(
                "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
                dt.year(),
                dt.month() as u8,
                dt.day(),
                dt.hour(),
                dt.minute(),
                dt.second()
            )
        })
        .unwrap_or_else(|_| "-".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_describes_each_engines_backups() {
        let mut backup = tako_core::DatabaseBackupStatus {
            bucket: "backups".into(),
            endpoint: String::new(),
            destination: None,
            last_snapshot: None,
            replicated_at_unix_secs: None,
            last_attempt_unix_secs: None,
            last_error: None,
        };
        assert_eq!(
            backup_status_line(DatabaseEngine::Kv, &backup),
            "Snapshots to backups · no snapshots yet"
        );
        assert!(
            backup_status_line(DatabaseEngine::Sqlite, &backup)
                .starts_with("Continuous backups to backups · not backed up yet")
        );
        backup.last_snapshot = Some(DatabaseSnapshot {
            id: "7".into(),
            created_at_unix_secs: 1_800_000_000,
            reason: SnapshotReason::Scheduled,
            full: true,
            stored_bytes: 1,
        });
        let kv = backup_status_line(DatabaseEngine::Kv, &backup);
        assert!(
            kv.starts_with("Snapshots to backups · last snapshot "),
            "{kv}"
        );
        assert!(!kv.contains("Continuous"), "{kv}");
    }

    #[test]
    fn migrations_created_in_the_same_second_get_distinct_numbers() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            unused_migration_number(dir.path(), 20261117000000),
            20261117000000
        );
        std::fs::write(dir.path().join("20261117000000_users.sql"), "").unwrap();
        std::fs::write(dir.path().join("20261117000001_items.sql"), "").unwrap();
        assert_eq!(
            unused_migration_number(dir.path(), 20261117000000),
            20261117000002
        );
        assert_eq!(
            unused_migration_number(dir.path(), 20261117000005),
            20261117000005
        );
    }

    #[test]
    fn migration_names_become_slugs() {
        assert_eq!(migration_slug("Add orders").as_deref(), Some("add_orders"));
        assert_eq!(
            migration_slug("  rename users.email -> login!  ").as_deref(),
            Some("rename_users_email_login")
        );
        assert_eq!(migration_slug("v2: Ünïcode"), Some("v2_n_code".into()));
        assert_eq!(migration_slug("---"), None);
        let timestamp = utc_timestamp().unwrap();
        assert_eq!(timestamp.len(), 14);
        let name = format!("{timestamp}_add_orders.sql");
        assert!(tako_db::migrate::migration_number(&name).is_some());
    }

    #[test]
    fn the_network_comes_from_the_selected_config() {
        let _home = crate::network::tests::TestHome::new();
        let mut config = TakoToml::parse(
            "name = \"shop\"\n[envs.production]\nroute = \"a.example.com\"\n[envs.staging]\nroute = \"b.example.com\"\n",
        )
        .unwrap();
        // Nothing recorded and no network on this machine.
        let error = project_network(&config, None).unwrap_err();
        assert!(error.to_string().contains("No network"), "{error}");
        config.envs.get_mut("production").unwrap().network =
            Some("0123456789abcdef0123456789abcdef".into());
        config.envs.get_mut("staging").unwrap().network =
            Some("fedcba9876543210fedcba9876543210".into());
        let error = project_network(&config, None).unwrap_err();
        assert!(error.to_string().contains("--network"), "{error}");
    }

    #[test]
    fn bindings_are_grouped_by_database_outside_development() {
        let config = TakoToml::parse(
            "name = \"shop\"\n[envs.development]\ndb = { main = \"local\" }\n[envs.production]\nroute = \"a.example.com\"\ndb = { main = \"shop\", events = \"shop\" }\n[envs.staging]\nroute = \"b.example.com\"\ndb = { main = \"shop_staging\" }\n",
        )
        .unwrap();
        let bound = project_bindings(&config, DatabaseEngine::Sqlite);
        assert_eq!(
            bound["shop"],
            BTreeSet::from(["events".to_owned(), "main".to_owned()])
        );
        assert!(bound.contains_key("shop_staging") && !bound.contains_key("local"));
        assert_eq!(
            binding_envs(&config, "shop", DatabaseEngine::Sqlite),
            ["production"]
        );
    }

    #[test]
    fn kv_bindings_are_kept_apart_from_database_bindings() {
        let config = TakoToml::parse(
            "name = \"shop\"\n[envs.production]\nroute = \"a.example.com\"\ndb = { main = \"shop\" }\nkv = { auth = \"sessions\" }\n[envs.staging]\nroute = \"b.example.com\"\nkv = { auth = \"sessions\" }\n",
        )
        .unwrap();
        let stores = project_bindings(&config, DatabaseEngine::Kv);
        assert_eq!(stores.keys().collect::<Vec<_>>(), ["sessions"]);
        assert!(!project_bindings(&config, DatabaseEngine::Sqlite).contains_key("sessions"));
        assert_eq!(
            binding_envs(&config, "sessions", DatabaseEngine::Kv),
            ["production", "staging"]
        );
        assert!(binding_envs(&config, "sessions", DatabaseEngine::Sqlite).is_empty());
        assert_eq!(command_name(DatabaseEngine::Kv), "kv");
    }

    #[test]
    fn channel_replay_users_select_restore_homes_and_protect_deletion_in_their_network() {
        let config = TakoToml::parse(
            r#"
name = "shop"
[envs.production]
route = "shop.example.com"
network = "0123456789abcdef0123456789abcdef"
channels.db = "events"
[envs.staging]
route = "staging.example.com"
network = "0123456789abcdef0123456789abcdef"
[envs.preview]
route = "preview.example.com"
network = "fedcba9876543210fedcba9876543210"
channels.db = "events"
[envs.development]
channels.db = "local"
"#,
        )
        .unwrap();
        let network = config.envs["production"].network.as_deref().unwrap();
        let record = |name: &str, owner: Option<&str>| DatabaseRecord {
            name: name.into(),
            founders: Default::default(),
            server: "edge".into(),
            generation: "g".into(),
            created_at_unix_secs: 0,
            engine: DatabaseEngine::Sqlite,
            channel_owner: owner.map(str::to_owned),
            workflows_owner: None,
        };
        let records = [
            record("events", None),
            record("generated", Some("shop/staging")),
            record("retired", Some("shop/production")),
        ];
        let users = database_users(
            &config,
            network,
            &records,
            true,
            false,
            DatabaseEngine::Sqlite,
        );
        assert_eq!(users.len(), 2);
        for (db, env) in [("events", "production"), ("generated", "staging")] {
            assert_eq!(users[db].environments, BTreeSet::from([env]));
            assert_eq!(users[db].channel_replay, BTreeSet::from([env]));
        }
        assert!(project_bindings(&config, DatabaseEngine::Sqlite).is_empty());
        assert!(
            check_service_delete(&config, network, &records[0], true, false)
                .unwrap_err()
                .contains("production")
        );
        assert!(
            check_service_delete(&config, network, &records[1], true, false)
                .unwrap_err()
                .contains("staging")
        );
        assert!(check_service_delete(&config, network, &records[2], true, false).is_ok());
        // Once the code drops its channels, only the explicit selection is in use.
        assert!(check_service_delete(&config, network, &records[0], false, false).is_err());
        assert!(check_service_delete(&config, network, &records[1], false, false).is_ok());
        // A generated workflow database is protected the same way.
        let runs = DatabaseRecord {
            channel_owner: None,
            workflows_owner: Some("shop/staging".into()),
            ..record("runs", None)
        };
        let users = database_users(
            &config,
            network,
            std::slice::from_ref(&runs),
            false,
            true,
            DatabaseEngine::Sqlite,
        );
        assert_eq!(users["runs"].workflows, BTreeSet::from(["staging"]));
        assert!(
            check_service_delete(&config, network, &runs, false, true)
                .unwrap_err()
                .contains("workflow database for staging")
        );
        assert!(check_service_delete(&config, network, &runs, false, false).is_ok());
    }
}
