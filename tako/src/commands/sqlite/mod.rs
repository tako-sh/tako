//! `tako sqlite`: Tako-hosted SQLite databases.

use std::path::{Path, PathBuf};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use clap::{Subcommand, ValueEnum};
use serde::de::DeserializeOwned;
use tako_core::{Command, DatabaseInfo, DatabaseSnapshot, Response, SnapshotReason};

use crate::commands::project_context;
use crate::config::{ServerInventory, TakoToml};
use crate::management_http::ManagementClient;
use crate::network::databases::DatabaseRecord;
use crate::output;

mod shell;
pub(crate) mod target;
mod transfer;

use target::{Home, env_network, resolve_home};

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
        /// Database name, unique within the network
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
        /// Private S3 storage resource that receives snapshots, set as [databases.<name>].backup (defaults to the environment's backup.storage)
        #[arg(long)]
        backup: Option<String>,
        /// Which apps may use the database
        #[arg(long, value_enum, default_value = "local")]
        access: AccessArg,
    },

    /// Bind a database of the network in this project
    Link {
        /// Database name
        name: String,
        /// Environment to bind it to (defaults to production)
        #[arg(long)]
        env: Option<String>,
        /// Binding name the app uses (defaults to the name)
        #[arg(long)]
        binding: Option<String>,
        /// Private S3 storage resource that receives snapshots, set as [databases.<name>].backup (defaults to the environment's backup.storage)
        #[arg(long)]
        backup: Option<String>,
    },

    /// Choose which apps may use a database
    Access {
        /// Database name
        name: String,
        /// `local`: apps on the database's server; `network`: apps on any server of the network
        #[arg(value_enum)]
        access: AccessArg,
        /// Environment whose network to use (defaults to production)
        #[arg(long)]
        env: Option<String>,
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
        /// Database name (every database the environment binds when omitted)
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

    /// Apply the environment's pending SQL migrations to its databases
    Migrate {
        /// Environment whose bindings to migrate (defaults to production)
        #[arg(long)]
        env: Option<String>,
    },

    /// Replace a database with an empty one and apply its migrations
    Reset {
        /// Database name
        name: String,
        /// Environment whose network to use (defaults to production); `development` resets the local file
        #[arg(long)]
        env: Option<String>,
        /// Reset a database a production environment binds
        #[arg(long)]
        force: bool,
        /// Skip confirmation prompt
        #[arg(short = 'y', long = "yes")]
        yes: bool,
    },

    /// Delete a database and its snapshots
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

    /// A database of the network and its home server.
    fn database(&self, name: &str) -> Result<(DatabaseRecord, Home), Box<dyn std::error::Error>> {
        let record = self.record(name)?;
        let home = resolve_home(&self.servers, &self.network, &record.server, &self.env)
            .map_err(|error| format!("The server of database '{name}': {error}"))?;
        Ok((record, home))
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

    fn record(&self, name: &str) -> Result<DatabaseRecord, Box<dyn std::error::Error>> {
        tako_core::validate_database_name(name)?;
        self.records()?
            .into_iter()
            .find(|record| record.name == name)
            .ok_or_else(|| {
                format!(
                    "The network has no database named '{name}'. Run `tako sqlite list` to see its databases."
                )
                .into()
            })
    }

    fn publish(
        &self,
        record: &DatabaseRecord,
        backup_key_base64: Option<&str>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let alias = self.alias()?;
        tokio::task::block_in_place(|| {
            crate::network::databases::publish(&alias, record, backup_key_base64)
        })
    }

    fn backup_key(&self, name: &str) -> Result<String, Box<dyn std::error::Error>> {
        let alias = self.alias()?;
        let key =
            tokio::task::block_in_place(|| crate::network::databases::backup_key(&alias, name))?;
        Ok(key.to_string())
    }

    /// Whether any environment of this project binds the database.
    fn binds(&self, name: &str) -> bool {
        self.config
            .envs
            .iter()
            .filter(|(env, _)| env.as_str() != "development")
            .any(|(_, env)| env.databases.values().any(|database| database == name))
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
        SqliteCommands::Access { name, access, env } => {
            let ctx = Context::load(config_path, env.as_deref()).await?;
            set_access(&ctx, &name, access.into())
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
            let (_, home) = ctx.database(&name)?;
            shell::run(&home, &name, sql).await
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
            let (_, home) = ctx.database(&name)?;
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
            transfer::import(&home, &name, &file).await
        }
        SqliteCommands::Export { name, file, env } => {
            let ctx = Context::load(config_path, env.as_deref()).await?;
            ctx.require_admin()?;
            let (_, home) = ctx.database(&name)?;
            if output::is_dry_run() {
                return dry_run(
                    &format!("Export {name} to {}", file.display()),
                    serde_json::json!({"database": name, "file": file}),
                );
            }
            transfer::export(&home, &name, &file).await
        }
        SqliteCommands::Delete { name, env, yes } => {
            let ctx = Context::load(config_path, env.as_deref()).await?;
            delete(&ctx, &name, yes).await
        }
        SqliteCommands::Migrate { env } => {
            if crate::commands::helpers::resolve_env(env.as_deref()) == "development" {
                return migrate_local(config_path);
            }
            let ctx = Context::load(config_path, env.as_deref()).await?;
            migrate(&ctx).await
        }
        SqliteCommands::Reset {
            name,
            env,
            force,
            yes,
        } => {
            if crate::commands::helpers::resolve_env(env.as_deref()) == "development" {
                return reset_local(config_path, &name, yes);
            }
            let ctx = Context::load(config_path, env.as_deref()).await?;
            reset(&ctx, &name, force, yes).await
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
    // `--backup` is written to tako.toml only when it is not the default.
    let backup = backup.filter(|backup| Some(backup) != ctx.env_backup_storage().as_ref());
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
    // Names identify databases in the network.
    if ctx.records()?.iter().any(|record| record.name == name) {
        return Err(format!(
            "The network already has a database named '{name}'. Use `tako sqlite link {name}` to bind it in this project."
        )
        .into());
    }
    // Validate the edit before creating anything.
    let mut preview = ctx.config.clone();
    if let Some(backup) = &backup {
        preview.databases.insert(
            name.to_owned(),
            crate::config::DatabaseResourceConfig {
                backup: Some(backup.clone()),
            },
        );
    }
    if let Some(env) = preview.envs.get_mut(&ctx.env) {
        env.databases.insert(binding.to_owned(), name.to_owned());
    }
    preview.validate()?;
    let secrets = project_context::ProjectContext {
        project_dir: ctx.project_dir.clone(),
        config_path: ctx.config_path.clone(),
    }
    .load_secrets_for_environment(Some(&ctx.env))?;
    let backup_storage = crate::commands::storage::decrypt_database_backup_storage(
        &ctx.env,
        &preview,
        name,
        &secrets,
        Some(&ctx.project_dir),
    )?;
    if output::is_dry_run() {
        return dry_run(
            &format!("Create database {name} on {}", home.name),
            serde_json::json!({"database": name, "server": home.name}),
        );
    }

    let mut key = [0u8; 32];
    getrandom::fill(&mut key)?;
    let backup_key = BASE64.encode(key);
    let mut generation = [0u8; 16];
    getrandom::fill(&mut generation)?;
    let record = DatabaseRecord {
        name: name.to_owned(),
        server: home.device_id.clone(),
        access,
        generation: hex::encode(generation),
        created_at_unix_secs: unix_now(),
    };
    let has_backup = backup_storage.is_some();
    let _t = output::timed(&format!("Create database {name}"));
    output::with_spinner_async(
        &format!("Creating {name} on {}", home.name),
        &format!("Created {name} on {}", home.name),
        async {
            send::<DatabaseInfo>(
                &home,
                Command::CreateDatabase {
                    name: name.to_owned(),
                    generation: record.generation.clone(),
                    backup_key_base64: backup_key.clone(),
                    backup: backup_storage,
                },
            )
            .await
            .map_err(|e| e.to_string())
        },
    )
    .await?;
    // The snapshot key is needed to recover the database elsewhere, so a
    // failure to record it undoes the creation.
    if let Err(error) = ctx.publish(&record, Some(&backup_key)) {
        let _ = send::<serde_json::Value>(
            &home,
            Command::DeleteDatabase {
                name: name.to_owned(),
            },
        )
        .await;
        return Err(format!("Could not record the database in the network: {error}").into());
    }
    TakoToml::upsert_database_in_file(
        &ctx.config_path,
        name,
        backup.as_deref(),
        Some((&ctx.env, binding)),
    )?;
    if output::is_json() {
        return output::json_result(serde_json::json!({
            "ok": true,
            "command": "sqlite",
            "database": {"name": name, "server": home.name, "access": access, "binding": binding, "env": ctx.env},
        }));
    }
    output::hint(&format!(
        "Bound as tako.databases.{binding} in {}. Deploy to use it.",
        ctx.env
    ));
    warn_without_backup(name, &ctx.env, has_backup);
    Ok(())
}

/// Hosted databases are snapshotted only to a configured backup storage.
fn warn_without_backup(name: &str, env: &str, has_backup: bool) {
    if !has_backup {
        output::warning(&format!(
            "{name} has no backup storage, so it has no snapshots. Set [envs.{env}].backup.storage, or `backup = \"<storage>\"` in [databases.{name}], to a private S3 storage and deploy."
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
    let backup = backup.filter(|backup| Some(backup) != ctx.env_backup_storage().as_ref());
    ctx.record(name)?;
    if output::is_dry_run() {
        return dry_run(
            &format!("Bind database {name} in tako.toml"),
            serde_json::json!({"database": name}),
        );
    }
    TakoToml::upsert_database_in_file(
        &ctx.config_path,
        name,
        backup.as_deref(),
        Some((&ctx.env, binding)),
    )?;
    if output::is_json() {
        return output::json_result(serde_json::json!({
            "ok": true, "command": "sqlite",
            "database": {"name": name, "binding": binding, "env": ctx.env},
        }));
    }
    output::success(&format!(
        "Linked {name} as tako.databases.{binding} in {}",
        ctx.env
    ));
    let has_backup = crate::commands::storage::database_backup_setting(&ctx.config, &ctx.env, name)
        .is_some()
        || backup.is_some();
    warn_without_backup(name, &ctx.env, has_backup);
    Ok(())
}

fn set_access(
    ctx: &Context,
    name: &str,
    access: tako_core::DatabaseAccess,
) -> Result<(), Box<dyn std::error::Error>> {
    ctx.require_admin()?;
    let record = ctx.record(name)?;
    let label = match access {
        tako_core::DatabaseAccess::Local => "local",
        tako_core::DatabaseAccess::Network => "network",
    };
    if record.access == access {
        if output::is_json() {
            return output::json_result(serde_json::json!({
                "ok": true, "command": "sqlite", "database": {"name": name, "access": access},
            }));
        }
        output::success(&format!("{name} already has {label} access"));
        return Ok(());
    }
    if output::is_dry_run() {
        return dry_run(
            &format!("Set {name} to {label} access"),
            serde_json::json!({"database": name, "access": access}),
        );
    }
    ctx.publish(&DatabaseRecord { access, ..record }, None)?;
    if output::is_json() {
        return output::json_result(serde_json::json!({
            "ok": true, "command": "sqlite", "database": {"name": name, "access": access},
        }));
    }
    output::success(&format!("{name} has {label} access"));
    output::hint("Deploy the apps that bind it to apply the change.");
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
                    "server": server_name(&record.server),
                    "server_id": record.server,
                    "access": record.access,
                    "created_at_unix_secs": record.created_at_unix_secs,
                    "in_project": ctx.binds(&record.name),
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
        let project = if ctx.binds(&record.name) {
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
            let names: std::collections::BTreeSet<String> = ctx
                .config
                .envs
                .get(&ctx.env)
                .map(|env| env.databases.values().cloned().collect())
                .unwrap_or_default();
            names.into_iter().collect()
        }
    };
    if names.is_empty() {
        output::info(&format!("{} binds no databases.", ctx.env));
        return Ok(());
    }
    let mut infos = Vec::new();
    for name in &names {
        let (_, home) = ctx.database(name)?;
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
    let (_, home) = ctx.database(name)?;
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
    let (_, home) = ctx.database(name)?;
    let value: serde_json::Value = send(
        &home,
        Command::ListDatabaseSnapshots {
            name: name.to_owned(),
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
            SnapshotReason::Migration => "before migrations",
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
    let (record, home) = ctx.database(name)?;
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
    let backup_key = ctx.backup_key(name)?;
    if moving {
        let backup = ctx.backup_storage(name)?.ok_or(
            "Recovering onto another server needs a backup storage: set [envs.<env>].backup.storage or [databases.<name>].backup in tako.toml.",
        )?;
        send::<DatabaseInfo>(
            &target,
            Command::CreateDatabase {
                name: name.to_owned(),
                generation: record.generation.clone(),
                backup_key_base64: backup_key.clone(),
                backup: Some(backup),
            },
        )
        .await?;
    }
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
                    backup_key_base64: Some(backup_key.clone()),
                },
            )
            .await
            .map_err(|e| e.to_string())
        },
    )
    .await;
    let value = match restored {
        Ok(value) => value,
        Err(error) => {
            // Remove the empty copy, so the recovery can be retried.
            if moving
                && let Err(cleanup) = send::<serde_json::Value>(
                    &target,
                    Command::DeleteDatabase {
                        name: name.to_owned(),
                    },
                )
                .await
            {
                output::warning(&format!(
                    "Could not remove the new copy of {name} on {}: {cleanup}",
                    target.name
                ));
            }
            return Err(error.into());
        }
    };
    if moving {
        let moved = DatabaseRecord {
            server: target.device_id.clone(),
            ..record
        };
        ctx.publish(&moved, None)?;
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
    let (_, home) = ctx.database(name)?;
    output::warning(&format!(
        "Deleting {name} removes its file from {}.",
        home.name
    ));
    // Grants can outlive the deployments that used them, so the server
    // revokes them; name them so nothing still in use is deleted unseen.
    let status: serde_json::Value = send(
        &home,
        Command::DatabaseStatus {
            name: Some(name.to_owned()),
        },
    )
    .await?;
    let granted: Vec<&str> = status["databases"][0]["remote_grants"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(serde_json::Value::as_str)
        .collect();
    if !granted.is_empty() {
        output::warning(&format!(
            "{} may still use {name} from other servers; their requests will fail.",
            granted.join(", ")
        ));
    }
    confirm(
        &format!("Delete database {name} and its snapshots?"),
        "The database and every snapshot of it are deleted permanently. Apps must not bind it anymore.",
        yes,
    )?;
    if output::is_dry_run() {
        return dry_run(
            &format!("Delete database {name}"),
            serde_json::json!({"database": name}),
        );
    }
    let deleted: serde_json::Value = output::with_spinner_async(
        &format!("Deleting {name}"),
        &format!("Deleted {name}"),
        async {
            send::<serde_json::Value>(
                &home,
                Command::DeleteDatabase {
                    name: name.to_owned(),
                },
            )
            .await
            .map_err(|e| e.to_string())
        },
    )
    .await?;
    print_warnings(&deleted);
    let alias = ctx.alias()?;
    tokio::task::block_in_place(|| crate::network::databases::remove(&alias, name))?;
    TakoToml::remove_database_from_file(&ctx.config_path, name)?;
    if output::is_json() {
        return output::json_success("sqlite");
    }
    Ok(())
}

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
    migrations: Vec<tako_database::Migration>,
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

async fn migrate(ctx: &Context) -> Result<(), Box<dyn std::error::Error>> {
    let bindings: Vec<(String, String)> = ctx
        .config
        .envs
        .get(&ctx.env)
        .map(|env| {
            env.databases
                .iter()
                .map(|(binding, database)| (binding.clone(), database.clone()))
                .collect()
        })
        .unwrap_or_default();
    let owner = migration_owner(ctx)?;
    let admin = crate::commands::helpers::can_configure_bindings(&ctx.config, &ctx.env)?;
    let app = (!admin).then(|| tako_core::deployment_app_id(&owner, &ctx.env));
    let mut results = Vec::new();
    for (binding, database) in bindings {
        let migrations = crate::database_migrations::load(&ctx.project_dir, &ctx.config, &binding)?;
        if migrations.is_empty() {
            continue;
        }
        let (_, home) = ctx.database(&database)?;
        let report = apply_migrations(
            &home,
            &database,
            &owner,
            migrations,
            output::is_dry_run(),
            app.clone(),
        )
        .await
        .map_err(|e| format!("Migrate {binding} ({database}): {e}"))?;
        for warning in &report.warnings {
            output::warning(&format!("{binding}: {warning}"));
        }
        results.push((binding, database, report));
    }
    if output::is_json() {
        let databases: Vec<_> = results
            .iter()
            .map(|(binding, database, report)| serde_json::json!({"binding": binding, "database": database, "report": report}))
            .collect();
        return output::json_result(serde_json::json!({
            "ok": true, "command": "sqlite", "dry_run": output::is_dry_run(), "migrations": databases,
        }));
    }
    if results.is_empty() {
        output::info(&format!("{} has no migrations to apply.", ctx.env));
    }
    for (binding, _, report) in &results {
        let (verb, names) = if output::is_dry_run() {
            ("Would apply", &report.pending)
        } else {
            ("Applied", &report.applied)
        };
        if names.is_empty() {
            output::success(&format!("{binding} is up to date"));
        } else {
            output::success(&format!("{verb} {} to {binding}", names.join(", ")));
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

fn migrate_local(config_path: Option<&Path>) -> Result<(), Box<dyn std::error::Error>> {
    let (project, config) = load_project(config_path)?;
    if output::is_dry_run() {
        return dry_run(
            "Apply development migrations",
            serde_json::json!({"env": "development"}),
        );
    }
    let migrated = crate::local_databases::migrate_development(&project.project_dir, &config)?;
    for warning in &migrated.warnings {
        output::warning(warning);
    }
    if output::is_json() {
        return output::json_result(serde_json::json!({
            "ok": true, "command": "sqlite", "applied": migrated.applied,
        }));
    }
    if migrated.applied.is_empty() {
        output::success("Development databases are up to date");
    }
    for line in &migrated.applied {
        output::success(line);
    }
    Ok(())
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
        tako_database::reset_database_file(&path)?;
    }
    let migrated = crate::local_databases::migrate_development(&project.project_dir, &config)?;
    for warning in &migrated.warnings {
        output::warning(warning);
    }
    if output::is_json() {
        return output::json_result(serde_json::json!({
            "ok": true, "command": "sqlite", "reset": name, "applied": migrated.applied,
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
    let (_, home) = ctx.database(name)?;
    let production: Vec<&str> = ctx
        .config
        .envs
        .iter()
        .filter(|(env, config)| {
            env.as_str() == "production" && config.databases.values().any(|d| d == name)
        })
        .map(|(env, _)| env.as_str())
        .collect();
    if !production.is_empty() && !force {
        return Err(format!(
            "{name} is bound by the production environment. Pass --force to reset it anyway."
        )
        .into());
    }
    confirm(
        &format!("Reset {name} on {}?", home.name),
        "Every table in it is deleted; then this environment's migrations run again. A snapshot of the current contents is taken first when backups are configured.",
        yes,
    )?;
    if output::is_dry_run() {
        return dry_run(
            &format!("Reset {name}"),
            serde_json::json!({"database": name}),
        );
    }
    let reset: serde_json::Value = output::with_spinner_async(
        &format!("Resetting {name}"),
        &format!("Reset {name}"),
        async {
            send::<serde_json::Value>(
                &home,
                Command::ResetDatabase {
                    name: name.to_owned(),
                },
            )
            .await
            .map_err(|e| e.to_string())
        },
    )
    .await?;
    print_warnings(&reset);
    // Bring the schema back for this environment's bindings to it.
    let owner = migration_owner(ctx)?;
    let mut applied = Vec::new();
    let bindings: Vec<String> = ctx
        .config
        .envs
        .get(&ctx.env)
        .map(|env| {
            env.databases
                .iter()
                .filter(|(_, database)| database.as_str() == name)
                .map(|(binding, _)| binding.clone())
                .collect()
        })
        .unwrap_or_default();
    for binding in bindings {
        let migrations = crate::database_migrations::load(&ctx.project_dir, &ctx.config, &binding)?;
        if migrations.is_empty() {
            continue;
        }
        let report = apply_migrations(&home, name, &owner, migrations, false, None)
            .await
            .map_err(|e| format!("{name} was reset, but its migrations failed: {e}"))?;
        for warning in &report.warnings {
            output::warning(&format!("{binding}: {warning}"));
        }
        applied.extend(report.applied);
    }
    if output::is_json() {
        return output::json_result(serde_json::json!({
            "ok": true, "command": "sqlite", "reset": name, "applied": applied,
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
    crate::commands::backups::format_backup_time(unix as i64)
}
