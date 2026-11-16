//! `tako sqlite`: Tako-hosted SQLite databases.

use std::path::{Path, PathBuf};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use clap::{Subcommand, ValueEnum};
use serde::de::DeserializeOwned;
use tako_core::{Command, DatabaseInfo, DatabaseSnapshot, Response, SnapshotReason};

use crate::commands::project_context;
use crate::config::{DatabaseResourceConfig, ServerInventory, TakoToml};
use crate::management_http::ManagementClient;
use crate::network::databases::DatabaseRecord;
use crate::output;

mod shell;
pub(crate) mod target;
mod transfer;

use target::{Home, declared, env_network, resolve_home};

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum AccessArg {
    /// Only apps on the database's server
    Local,
    /// Apps on any server of the network
    Network,
}

impl From<AccessArg> for tako_core::DatabaseAccess {
    fn from(value: AccessArg) -> Self {
        match value {
            AccessArg::Local => Self::Local,
            AccessArg::Network => Self::Network,
        }
    }
}

#[derive(Subcommand)]
pub enum SqliteCommands {
    /// Create a database on a server and bind it to an environment
    Create {
        /// Database name, used in tako.toml as [databases.<name>]
        name: String,
        /// Environment to bind it to (defaults to production)
        #[arg(long)]
        env: Option<String>,
        /// Server that hosts the database (required when the environment has several)
        #[arg(long)]
        server: Option<String>,
        /// Binding name the app uses, as `tako.databases.<binding>` (defaults to the name)
        #[arg(long)]
        binding: Option<String>,
        /// Private S3 storage resource that receives snapshots (defaults to the environment's backup.storage)
        #[arg(long)]
        backup: Option<String>,
        /// Which apps may use the database
        #[arg(long, value_enum, default_value = "local")]
        access: AccessArg,
    },

    /// Add a database of the network to this project
    Link {
        /// Database name
        name: String,
        /// Environment to bind it to (defaults to production)
        #[arg(long)]
        env: Option<String>,
        /// Binding name the app uses (defaults to the name)
        #[arg(long)]
        binding: Option<String>,
        /// Private S3 storage resource that receives snapshots (defaults to the environment's backup.storage)
        #[arg(long)]
        backup: Option<String>,
    },

    /// List the network's databases
    #[command(visible_alias = "ls")]
    List {
        /// Environment whose network to list (defaults to production)
        #[arg(long)]
        env: Option<String>,
    },

    /// Show a database's size, users, and backups
    Status {
        /// Database name (all declared databases when omitted)
        name: Option<String>,
        /// Environment whose network to query (defaults to production)
        #[arg(long)]
        env: Option<String>,
    },

    /// Run SQL against a database
    Shell {
        /// Database name
        name: String,
        /// SQL to run instead of opening an interactive shell
        sql: Option<String>,
        /// Environment whose network to use (defaults to production)
        #[arg(long)]
        env: Option<String>,
    },

    /// Take a snapshot now
    Backup {
        /// Database name
        name: String,
        /// Environment whose network to use (defaults to production)
        #[arg(long)]
        env: Option<String>,
    },

    /// List a database's snapshots
    Backups {
        /// Database name
        name: String,
        /// Environment whose network to use (defaults to production)
        #[arg(long)]
        env: Option<String>,
    },

    /// Replace a database with one of its snapshots
    Restore {
        /// Database name
        name: String,
        /// Environment whose network to use (defaults to production)
        #[arg(long)]
        env: Option<String>,
        /// Restore the newest snapshot
        #[arg(
            long,
            conflicts_with = "snapshot",
            required_unless_present = "snapshot"
        )]
        latest: bool,
        /// Snapshot ID from `tako sqlite backups`
        #[arg(long)]
        snapshot: Option<String>,
        /// Recover onto another server, which becomes the database's server
        #[arg(long)]
        server: Option<String>,
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
        /// Environment whose network to use (defaults to production)
        #[arg(long)]
        env: Option<String>,
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
        /// Environment whose network to use (defaults to production)
        #[arg(long)]
        env: Option<String>,
    },

    /// Delete a database. Its snapshots stay in backup storage
    #[command(visible_alias = "rm")]
    Delete {
        /// Database name
        name: String,
        /// Environment whose network to use (defaults to production)
        #[arg(long)]
        env: Option<String>,
        /// Skip confirmation prompt
        #[arg(short = 'y', long = "yes")]
        yes: bool,
    },
}

/// Reports what a dry run skipped; in JSON mode as `{"dry_run": true, ...}`.
fn dry_run(message: &str, mut value: serde_json::Value) -> Result<(), Box<dyn std::error::Error>> {
    output::dry_run_skip(message);
    if output::is_json() {
        value["dry_run"] = serde_json::Value::Bool(true);
        output::json_result(value)?;
    }
    Ok(())
}

pub fn run(
    cmd: SqliteCommands,
    config_path: Option<&Path>,
) -> Result<(), Box<dyn std::error::Error>> {
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(run_async(cmd, config_path))
}

/// The project and network a command acts in.
struct Context {
    project_dir: PathBuf,
    config_path: PathBuf,
    config: TakoToml,
    env: String,
    network: String,
    servers: ServerInventory,
}

impl Context {
    async fn load(
        config_path: Option<&Path>,
        env: Option<&str>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let project = project_context::resolve_existing(config_path)?;
        let config = TakoToml::load_from_file(&project.config_path)?;
        let env = crate::commands::helpers::resolve_env(env);
        let network = env_network(&config, &env)?.to_owned();
        let servers =
            crate::commands::helpers::load_management_servers(Some(&config), Some(&env)).await?;
        Ok(Self {
            project_dir: project.project_dir,
            config_path: project.config_path,
            config,
            env,
            network,
            servers,
        })
    }

    fn alias(&self) -> Result<crate::config::LocalNetwork, Box<dyn std::error::Error>> {
        Ok(crate::config::LocalNetworks::load()?
            .select(Some(&self.network))?
            .clone())
    }

    fn require_admin(&self) -> Result<(), Box<dyn std::error::Error>> {
        if crate::commands::helpers::can_configure_bindings(&self.config, &self.env)? {
            Ok(())
        } else {
            Err("Managing databases requires network and server administrator permission.".into())
        }
    }

    /// A declared database and its home server.
    fn database(
        &self,
        name: &str,
    ) -> Result<(DatabaseResourceConfig, Home), Box<dyn std::error::Error>> {
        let database = declared(&self.config, name)?.clone();
        let home = resolve_home(&self.servers, &self.network, &database.server, &self.env)?;
        Ok((database, home))
    }

    /// The environment's `backup.storage`, the default snapshot destination.
    fn env_backup_storage(&self) -> Option<String> {
        self.config
            .envs
            .get(&self.env)
            .and_then(|env| env.backup.as_ref())
            .map(|backup| backup.storage.clone())
    }

    fn records(&self) -> Result<Vec<DatabaseRecord>, Box<dyn std::error::Error>> {
        let alias = self.alias()?;
        tokio::task::block_in_place(|| crate::network::databases::list(&alias))
    }

    fn record(&self, id: &str) -> Result<DatabaseRecord, Box<dyn std::error::Error>> {
        self.records()?
            .into_iter()
            .find(|record| record.id == id)
            .ok_or_else(|| format!("Database {id} has no record in this network.").into())
    }

    fn publish(
        &self,
        id: &str,
        record: Option<&DatabaseRecord>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let alias = self.alias()?;
        tokio::task::block_in_place(|| crate::network::databases::publish(&alias, id, record))
    }

    fn backup_storage(
        &self,
        name: &str,
    ) -> Result<Option<tako_core::StorageBinding>, Box<dyn std::error::Error>> {
        let secrets = project_context::ProjectContext {
            project_dir: self.project_dir.clone(),
            config_path: self.config_path.clone(),
        }
        .load_secrets_for_environment(Some(&self.env))?;
        crate::commands::storage::decrypt_database_backup_storage(
            &self.env,
            &self.config,
            name,
            &secrets,
            Some(&self.project_dir),
        )
    }
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

fn confirm(question: &str, description: &str, yes: bool) -> Result<(), Box<dyn std::error::Error>> {
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
    cmd: SqliteCommands,
    config_path: Option<&Path>,
) -> Result<(), Box<dyn std::error::Error>> {
    match cmd {
        SqliteCommands::Create {
            name,
            env,
            server,
            binding,
            backup,
            access,
        } => {
            let ctx = Context::load(config_path, env.as_deref()).await?;
            create(
                &ctx,
                &name,
                server.as_deref(),
                binding.as_deref(),
                backup,
                access.into(),
            )
            .await
        }
        SqliteCommands::Link {
            name,
            env,
            binding,
            backup,
        } => {
            let ctx = Context::load(config_path, env.as_deref()).await?;
            link(&ctx, &name, binding.as_deref(), backup)
        }
        SqliteCommands::List { env } => {
            let ctx = Context::load(config_path, env.as_deref()).await?;
            list(&ctx)
        }
        SqliteCommands::Status { name, env } => {
            let ctx = Context::load(config_path, env.as_deref()).await?;
            status(&ctx, name.as_deref()).await
        }
        SqliteCommands::Shell { name, sql, env } => {
            let ctx = Context::load(config_path, env.as_deref()).await?;
            ctx.require_admin()?;
            let (database, home) = ctx.database(&name)?;
            shell::run(&home, &database.id, &name, sql).await
        }
        SqliteCommands::Backup { name, env } => {
            let ctx = Context::load(config_path, env.as_deref()).await?;
            backup_now(&ctx, &name).await
        }
        SqliteCommands::Backups { name, env } => {
            let ctx = Context::load(config_path, env.as_deref()).await?;
            backups(&ctx, &name).await
        }
        SqliteCommands::Restore {
            name,
            env,
            latest: _,
            snapshot,
            server,
            yes,
        } => {
            let ctx = Context::load(config_path, env.as_deref()).await?;
            restore(&ctx, &name, snapshot, server.as_deref(), yes).await
        }
        SqliteCommands::Import {
            name,
            file,
            env,
            yes,
        } => {
            let ctx = Context::load(config_path, env.as_deref()).await?;
            ctx.require_admin()?;
            let (database, home) = ctx.database(&name)?;
            confirm(
                &format!("Replace {name} with {}?", file.display()),
                "Apps' requests to the database pause while it is replaced. A snapshot of the current contents is taken first when backups are configured.",
                yes,
            )?;
            if output::is_dry_run() {
                return dry_run(
                    &format!("Import {} into {name}", file.display()),
                    serde_json::json!({"database": name, "file": file}),
                );
            }
            transfer::import(&home, &database.id, &name, &file).await
        }
        SqliteCommands::Export { name, file, env } => {
            let ctx = Context::load(config_path, env.as_deref()).await?;
            ctx.require_admin()?;
            let (database, home) = ctx.database(&name)?;
            if output::is_dry_run() {
                return dry_run(
                    &format!("Export {name} to {}", file.display()),
                    serde_json::json!({"database": name, "file": file}),
                );
            }
            transfer::export(&home, &database.id, &name, &file).await
        }
        SqliteCommands::Delete { name, env, yes } => {
            let ctx = Context::load(config_path, env.as_deref()).await?;
            delete(&ctx, &name, yes).await
        }
    }
}

async fn create(
    ctx: &Context,
    name: &str,
    server: Option<&str>,
    binding: Option<&str>,
    backup: Option<String>,
    access: tako_core::DatabaseAccess,
) -> Result<(), Box<dyn std::error::Error>> {
    tako_core::validate_database_name(name)?;
    let binding = binding.unwrap_or(name);
    tako_core::validate_database_name(binding)?;
    let backup = backup.or_else(|| ctx.env_backup_storage());
    if ctx.config.databases.contains_key(name) {
        return Err(format!("Database '{name}' is already declared in tako.toml.").into());
    }
    ctx.require_admin()?;
    let selector = match server {
        Some(server) => server.to_owned(),
        None => {
            let targets = crate::commands::helpers::resolve_servers_for_env(
                &ctx.config,
                &ctx.servers,
                &ctx.env,
            )?;
            match targets.as_slice() {
                [only] => ctx
                    .servers
                    .get(only)
                    .and_then(|entry| entry.native.as_ref())
                    .map(|native| native.server_name.to_string())
                    .ok_or("The environment's server is not a native network server")?,
                _ => {
                    return Err(format!(
                        "{} deploys to several servers. Pass --server to choose the database's server.",
                        ctx.env
                    )
                    .into());
                }
            }
        }
    };
    let home = resolve_home(&ctx.servers, &ctx.network, &selector, &ctx.env)?;
    if ctx.records()?.iter().any(|record| record.name == name) {
        return Err(format!(
            "The network already has a database named '{name}'. Use `tako sqlite link {name}` to add it to this project."
        )
        .into());
    }
    let resource = DatabaseResourceConfig {
        id: tako_database::DatabaseId::generate().to_string(),
        server: home.name.clone(),
        backup,
        access,
    };
    // Validate the edit before creating anything.
    let mut preview = ctx.config.clone();
    preview.databases.insert(name.to_owned(), resource.clone());
    if let Some(env) = preview.envs.get_mut(&ctx.env) {
        env.databases.insert(binding.to_owned(), name.to_owned());
    }
    preview.validate()?;
    let backup_storage = match &resource.backup {
        Some(_) => {
            let mut with_backup = ctx.config.clone();
            with_backup
                .databases
                .insert(name.to_owned(), resource.clone());
            let secrets = project_context::ProjectContext {
                project_dir: ctx.project_dir.clone(),
                config_path: ctx.config_path.clone(),
            }
            .load_secrets_for_environment(Some(&ctx.env))?;
            crate::commands::storage::decrypt_database_backup_storage(
                &ctx.env,
                &with_backup,
                name,
                &secrets,
                Some(&ctx.project_dir),
            )?
        }
        None => None,
    };
    if output::is_dry_run() {
        return dry_run(
            &format!("Create database {name} on {}", home.name),
            serde_json::json!({"database": name, "server": home.name}),
        );
    }

    let mut key = [0u8; 32];
    getrandom::fill(&mut key)?;
    let record = DatabaseRecord {
        id: resource.id.clone(),
        name: name.to_owned(),
        server: home.device_id.clone(),
        access,
        backup_key_base64: BASE64.encode(key),
        created_at_unix_secs: unix_now(),
    };
    let _t = output::timed(&format!("Create database {name}"));
    output::with_spinner_async(
        &format!("Creating {name} on {}", home.name),
        &format!("Created {name} on {}", home.name),
        async {
            send::<DatabaseInfo>(
                &home,
                Command::CreateDatabase {
                    id: record.id.clone(),
                    name: name.to_owned(),
                    backup_key_base64: record.backup_key_base64.clone(),
                    backup: backup_storage,
                },
            )
            .await
            .map_err(|e| e.to_string())
        },
    )
    .await?;
    // The record holds the snapshot key; without it the database cannot be
    // recovered elsewhere, so a failure undoes the creation.
    if let Err(error) = ctx.publish(&record.id, Some(&record)) {
        let _ = send::<serde_json::Value>(
            &home,
            Command::DeleteDatabase {
                id: record.id.clone(),
            },
        )
        .await;
        return Err(format!("Could not record the database in the network: {error}").into());
    }
    TakoToml::upsert_database_in_file(
        &ctx.config_path,
        name,
        &resource,
        Some((&ctx.env, binding)),
    )?;
    if output::is_json() {
        return output::json_result(serde_json::json!({
            "ok": true,
            "command": "sqlite",
            "database": {"name": name, "id": record.id, "server": home.name, "binding": binding, "env": ctx.env},
        }));
    }
    output::hint(&format!(
        "Bound as tako.databases.{binding} in {}. Deploy to use it.",
        ctx.env
    ));
    warn_without_backup(name, &resource);
    Ok(())
}

/// Hosted databases are snapshotted only to a configured backup storage.
fn warn_without_backup(name: &str, resource: &DatabaseResourceConfig) {
    if resource.backup.is_none() {
        output::warning(&format!(
            "{name} has no backup storage, so it has no snapshots. Set `backup = \"<storage>\"` in [databases.{name}] to a private S3 storage and deploy."
        ));
    }
}

fn link(
    ctx: &Context,
    name: &str,
    binding: Option<&str>,
    backup: Option<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    let binding = binding.unwrap_or(name);
    tako_core::validate_database_name(binding)?;
    let backup = backup.or_else(|| ctx.env_backup_storage());
    let records = ctx.records()?;
    let record = records
        .iter()
        .find(|record| record.name == name)
        .ok_or_else(|| format!("The network has no database named '{name}'."))?;
    let server = ctx
        .servers
        .servers
        .values()
        .filter_map(|entry| entry.native.as_ref())
        .find(|native| native.server_id.to_string() == record.server)
        .map(|native| native.server_name.to_string())
        .unwrap_or_else(|| record.server.clone());
    let resource = DatabaseResourceConfig {
        id: record.id.clone(),
        server,
        backup,
        access: record.access,
    };
    if output::is_dry_run() {
        return dry_run(
            &format!("Add database {name} to tako.toml"),
            serde_json::json!({"database": name}),
        );
    }
    TakoToml::upsert_database_in_file(
        &ctx.config_path,
        name,
        &resource,
        Some((&ctx.env, binding)),
    )?;
    if output::is_json() {
        return output::json_result(serde_json::json!({
            "ok": true, "command": "sqlite",
            "database": {"name": name, "id": record.id, "binding": binding, "env": ctx.env},
        }));
    }
    output::success(&format!(
        "Linked {name} as tako.databases.{binding} in {}",
        ctx.env
    ));
    warn_without_backup(name, &resource);
    Ok(())
}

fn list(ctx: &Context) -> Result<(), Box<dyn std::error::Error>> {
    let records = ctx.records()?;
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
        let databases: Vec<_> = records
            .iter()
            .map(|record| {
                serde_json::json!({
                    "name": record.name,
                    "id": record.id,
                    "server": server_name(&record.server),
                    "server_id": record.server,
                    "access": record.access,
                    "created_at_unix_secs": record.created_at_unix_secs,
                    "in_project": ctx.config.databases.values().any(|d| d.id == record.id),
                })
            })
            .collect();
        return output::json_result(
            serde_json::json!({"ok": true, "command": "sqlite", "databases": databases}),
        );
    }
    if records.is_empty() {
        output::info("No databases yet. Create one with `tako sqlite create <name>`.");
        return Ok(());
    }
    output::section("Databases");
    for record in &records {
        let access = match record.access {
            tako_core::DatabaseAccess::Local => "local",
            tako_core::DatabaseAccess::Network => "network",
        };
        let project = if ctx.config.databases.values().any(|d| d.id == record.id) {
            ""
        } else {
            " · not in this project"
        };
        output::bullet(&format!(
            "{} on {} · {access} access{project}",
            output::strong(&record.name),
            server_name(&record.server)
        ));
    }
    Ok(())
}

async fn status(ctx: &Context, name: Option<&str>) -> Result<(), Box<dyn std::error::Error>> {
    let names: Vec<String> = match name {
        Some(name) => vec![name.to_owned()],
        None => {
            let mut names: Vec<_> = ctx.config.databases.keys().cloned().collect();
            names.sort();
            names
        }
    };
    if names.is_empty() {
        output::info("This project declares no databases.");
        return Ok(());
    }
    let mut infos = Vec::new();
    for name in &names {
        let (database, home) = ctx.database(name)?;
        let value: serde_json::Value = send(
            &home,
            Command::DatabaseStatus {
                id: Some(database.id.clone()),
            },
        )
        .await?;
        let info: Vec<DatabaseInfo> = serde_json::from_value(value["databases"].clone())?;
        if let Some(info) = info.into_iter().next() {
            infos.push((name.clone(), home.name.clone(), info));
        }
    }
    if output::is_json() {
        let databases: Vec<_> = infos
            .iter()
            .map(|(name, server, info)| serde_json::json!({"name": name, "server": server, "status": info}))
            .collect();
        return output::json_result(
            serde_json::json!({"ok": true, "command": "sqlite", "databases": databases}),
        );
    }
    for (name, server, info) in &infos {
        output::section(name);
        output::info(&format!(
            "{} on {server}",
            output::format_size(info.size_bytes)
        ));
        let users: Vec<_> = info
            .bindings
            .iter()
            .chain(info.remote_grants.iter())
            .cloned()
            .collect();
        output::info(&if users.is_empty() {
            "Not used by any app yet".to_string()
        } else {
            format!("Used by {}", users.join(", "))
        });
        match &info.backup {
            None => output::info("Backups not configured"),
            Some(backup) => {
                let last = backup
                    .last_snapshot
                    .as_ref()
                    .map(|s| format!("last snapshot {}", format_time(s.created_at_unix_secs)))
                    .unwrap_or_else(|| "no snapshots yet".into());
                output::info(&format!("Backups to {} · {last}", backup.bucket));
                if let Some(error) = &backup.last_error {
                    output::warning(&format!("Last snapshot failed: {error}"));
                }
            }
        }
    }
    Ok(())
}

async fn backup_now(ctx: &Context, name: &str) -> Result<(), Box<dyn std::error::Error>> {
    ctx.require_admin()?;
    let (database, home) = ctx.database(name)?;
    if output::is_dry_run() {
        return dry_run(
            &format!("Snapshot {name}"),
            serde_json::json!({"database": name}),
        );
    }
    let _t = output::timed(&format!("Snapshot {name}"));
    let value: serde_json::Value = output::with_spinner_async(
        &format!("Snapshotting {name}"),
        &format!("Uploaded a snapshot of {name}"),
        async {
            send(
                &home,
                Command::SnapshotDatabase {
                    id: database.id.clone(),
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
            serde_json::json!({"ok": true, "command": "sqlite", "result": value}),
        );
    }
    if let Ok(snapshot) = serde_json::from_value::<DatabaseSnapshot>(value["snapshot"].clone()) {
        output::hint(&format!(
            "Snapshot {} · {}",
            snapshot.id,
            output::format_size(snapshot.database_bytes)
        ));
    }
    Ok(())
}

async fn backups(ctx: &Context, name: &str) -> Result<(), Box<dyn std::error::Error>> {
    ctx.require_admin()?;
    let (database, home) = ctx.database(name)?;
    let value: serde_json::Value = send(
        &home,
        Command::ListDatabaseSnapshots {
            id: database.id.clone(),
        },
    )
    .await?;
    let snapshots: Vec<DatabaseSnapshot> = serde_json::from_value(value["snapshots"].clone())?;
    if output::is_json() {
        return output::json_result(
            serde_json::json!({"ok": true, "command": "sqlite", "database": name, "snapshots": snapshots}),
        );
    }
    if snapshots.is_empty() {
        output::info(&format!("{name} has no snapshots yet."));
        return Ok(());
    }
    output::section(&format!("Snapshots of {name}"));
    for snapshot in &snapshots {
        let reason = match snapshot.reason {
            SnapshotReason::Scheduled => "scheduled",
            SnapshotReason::Manual => "manual",
            SnapshotReason::Release => "before release",
            SnapshotReason::Restore => "before restore",
        };
        output::bullet(&format!(
            "{} · {} · {} · {reason}",
            snapshot.id,
            format_time(snapshot.created_at_unix_secs),
            output::format_size(snapshot.database_bytes)
        ));
    }
    Ok(())
}

async fn restore(
    ctx: &Context,
    name: &str,
    snapshot: Option<String>,
    server: Option<&str>,
    yes: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    ctx.require_admin()?;
    let (database, home) = ctx.database(name)?;
    let target = match server {
        Some(server) => resolve_home(&ctx.servers, &ctx.network, server, &ctx.env)?,
        None => home.clone(),
    };
    let moving = target.device_id != home.device_id;
    let which = snapshot.as_deref().unwrap_or("the newest snapshot");
    if moving {
        output::warning(&format!(
            "{name} moves to {}. Deploy every app that binds it afterwards.",
            target.name
        ));
    }
    confirm(
        &format!("Restore {name} from {which} on {}?", target.name),
        "Apps' requests to the database pause while it is replaced. Writes since the snapshot are lost; a snapshot of the current contents is taken first when possible.",
        yes,
    )?;
    if output::is_dry_run() {
        return dry_run(
            &format!("Restore {name}"),
            serde_json::json!({"database": name}),
        );
    }
    let record = ctx.record(&database.id)?;
    if moving {
        let backup = ctx.backup_storage(name)?.ok_or(
            "Recovering onto another server needs [databases.<name>].backup in tako.toml.",
        )?;
        send::<DatabaseInfo>(
            &target,
            Command::CreateDatabase {
                id: database.id.clone(),
                name: name.to_owned(),
                backup_key_base64: record.backup_key_base64.clone(),
                backup: Some(backup),
            },
        )
        .await?;
    }
    let _t = output::timed(&format!("Restore {name}"));
    let value: serde_json::Value = output::with_spinner_async(
        &format!("Restoring {name}"),
        &format!("Restored {name}"),
        async {
            send(
                &target,
                Command::RestoreDatabase {
                    id: database.id.clone(),
                    snapshot: snapshot.clone(),
                    backup_key_base64: Some(record.backup_key_base64.clone()),
                },
            )
            .await
            .map_err(|e| e.to_string())
        },
    )
    .await?;
    if moving {
        let moved = DatabaseRecord {
            server: target.device_id.clone(),
            ..record
        };
        ctx.publish(&moved.id, Some(&moved))?;
        let resource = DatabaseResourceConfig {
            server: target.name.clone(),
            ..database
        };
        TakoToml::upsert_database_in_file(&ctx.config_path, name, &resource, None)?;
    }
    for warning in value["warnings"].as_array().into_iter().flatten() {
        if let Some(warning) = warning.as_str() {
            output::warning(warning);
        }
    }
    if output::is_json() {
        return output::json_result(
            serde_json::json!({"ok": true, "command": "sqlite", "result": value}),
        );
    }
    Ok(())
}

async fn delete(ctx: &Context, name: &str, yes: bool) -> Result<(), Box<dyn std::error::Error>> {
    ctx.require_admin()?;
    let (database, home) = ctx.database(name)?;
    output::warning(&format!(
        "Deleting {name} removes its file from {}.",
        home.name
    ));
    confirm(
        &format!("Delete database {name}?"),
        "Its snapshots stay in backup storage. Apps must not bind it anymore.",
        yes,
    )?;
    if output::is_dry_run() {
        return dry_run(
            &format!("Delete database {name}"),
            serde_json::json!({"database": name}),
        );
    }
    output::with_spinner_async(
        &format!("Deleting {name}"),
        &format!("Deleted {name}"),
        async {
            send::<serde_json::Value>(
                &home,
                Command::DeleteDatabase {
                    id: database.id.clone(),
                },
            )
            .await
            .map_err(|e| e.to_string())
        },
    )
    .await?;
    ctx.publish(&database.id, None)?;
    TakoToml::remove_database_from_file(&ctx.config_path, name)?;
    if output::is_json() {
        return output::json_success("sqlite");
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
    crate::commands::backups::format_backup_time(unix as i64)
}
