use std::path::{Path, PathBuf};

use clap::Subcommand;
use tako_core::{
    BackupDownloadUrlResponse, BackupInfo, BackupListResponse, BackupStatusResponse, Command,
};

use crate::app::require_app_name_from_config_path;
use crate::commands::project_context;
use crate::config::{ServersToml, TakoToml};
use crate::output;

mod client;
mod display;
mod download;
mod target;

use client::{send_typed_to_server, send_typed_to_servers};
use display::{format_status_line, output_backup_line};
use download::download_url_to_file;
use target::{resolve_backup_target, resolve_single_server};

#[derive(Subcommand)]
pub enum BackupCommands {
    /// Create a backup immediately
    Now {
        /// Environment to back up (defaults to production)
        #[arg(long)]
        env: Option<String>,
        /// Specific server to back up
        #[arg(long)]
        server: Option<String>,
    },

    /// List backups
    #[command(visible_alias = "ls")]
    List {
        /// Environment to query (defaults to production)
        #[arg(long)]
        env: Option<String>,
        /// Specific server to query
        #[arg(long)]
        server: Option<String>,
    },

    /// Show backup status
    Status {
        /// Environment to query (defaults to production)
        #[arg(long)]
        env: Option<String>,
    },

    /// Download a backup archive
    Download {
        /// Backup id from `tako backups list`
        backup_id: String,
        /// Environment to query (defaults to production)
        #[arg(long)]
        env: Option<String>,
        /// Specific server to download from
        #[arg(long)]
        server: Option<String>,
        /// Output archive path
        #[arg(long)]
        output: Option<PathBuf>,
    },

    /// Restore app data from a backup archive
    Restore {
        /// Backup id from `tako backups list`
        backup_id: String,
        /// Environment to restore (defaults to production)
        #[arg(long)]
        env: Option<String>,
        /// Specific server to restore
        #[arg(long)]
        server: Option<String>,
        /// Skip confirmation prompt
        #[arg(short = 'y', long = "yes")]
        yes: bool,
    },
}

pub fn run(
    cmd: BackupCommands,
    config_path: Option<&Path>,
) -> Result<(), Box<dyn std::error::Error>> {
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(run_async(cmd, config_path))
}

async fn run_async(
    cmd: BackupCommands,
    config_path: Option<&Path>,
) -> Result<(), Box<dyn std::error::Error>> {
    let context = project_context::resolve_existing(config_path)?;
    let app_name = require_app_name_from_config_path(&context.config_path)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e.to_string()))?;
    let tako_config = TakoToml::load_from_file(&context.config_path)?;
    let servers = ServersToml::load()?;

    match cmd {
        BackupCommands::Now { env, server } => {
            let target = resolve_backup_target(
                &app_name,
                env.as_deref(),
                server.as_deref(),
                &tako_config,
                &servers,
            )?;
            backup_now(&target, &servers).await
        }
        BackupCommands::List { env, server } => {
            let target = resolve_backup_target(
                &app_name,
                env.as_deref(),
                server.as_deref(),
                &tako_config,
                &servers,
            )?;
            list_backups(&target, &servers).await
        }
        BackupCommands::Status { env } => {
            let target =
                resolve_backup_target(&app_name, env.as_deref(), None, &tako_config, &servers)?;
            backup_status(&target, &servers).await
        }
        BackupCommands::Download {
            backup_id,
            env,
            server,
            output,
        } => {
            let target = resolve_backup_target(
                &app_name,
                env.as_deref(),
                server.as_deref(),
                &tako_config,
                &servers,
            )?;
            download_backup(&target, &servers, &backup_id, output).await
        }
        BackupCommands::Restore {
            backup_id,
            env,
            server,
            yes,
        } => {
            let target = resolve_backup_target(
                &app_name,
                env.as_deref(),
                server.as_deref(),
                &tako_config,
                &servers,
            )?;
            restore_backup(&target, &servers, &backup_id, yes).await
        }
    }
}

async fn backup_now(
    target: &target::BackupTarget,
    servers: &ServersToml,
) -> Result<(), Box<dyn std::error::Error>> {
    output::section("Backups");
    output::info(&format!(
        "{} ({})",
        output::strong(&target.app_name),
        output::strong(&target.env)
    ));
    if output::is_dry_run() {
        output::dry_run_skip("Create backup");
        return Ok(());
    }

    let results = send_typed_to_servers::<BackupInfo>(
        &target.server_names,
        servers,
        Command::BackupNow {
            app: target.remote_app_name.clone(),
        },
        "backup_now",
        &format!(
            "Creating backups on {} server(s)",
            target.server_names.len()
        ),
    )
    .await?;

    let mut failures = Vec::new();
    for (server_name, result) in results {
        match result {
            Ok(info) => output::bullet(&format!(
                "{}: {} ({})",
                server_name,
                output::strong(&info.id),
                output::format_size(info.size_bytes)
            )),
            Err(error) => {
                output::error(&format!("{server_name}: {error}"));
                failures.push(server_name);
            }
        }
    }

    if failures.is_empty() {
        output::success("Backups");
        Ok(())
    } else {
        Err(format!("Failed to back up {} server(s)", failures.len()).into())
    }
}

async fn list_backups(
    target: &target::BackupTarget,
    servers: &ServersToml,
) -> Result<(), Box<dyn std::error::Error>> {
    output::section("Backups");
    output::info(&format!(
        "{} ({})",
        output::strong(&target.app_name),
        output::strong(&target.env)
    ));

    let results = send_typed_to_servers::<BackupListResponse>(
        &target.server_names,
        servers,
        Command::ListBackups {
            app: target.remote_app_name.clone(),
        },
        "list_backups",
        &format!(
            "Loading backups from {} server(s)",
            target.server_names.len()
        ),
    )
    .await?;

    let mut any_success = false;
    for (server_name, result) in results {
        match result {
            Ok(response) => {
                any_success = true;
                output::info(&output::strong(&server_name));
                if response.backups.is_empty() {
                    output::muted("No backups found.");
                } else {
                    for backup in response.backups {
                        output_backup_line(&backup);
                    }
                }
            }
            Err(error) => output::warning(&format!(
                "{}: failed to load backups ({})",
                output::strong(&server_name),
                error
            )),
        }
    }

    if any_success {
        Ok(())
    } else {
        Err("Failed to query backups from all target servers".into())
    }
}

async fn backup_status(
    target: &target::BackupTarget,
    servers: &ServersToml,
) -> Result<(), Box<dyn std::error::Error>> {
    output::section("Backups");
    output::info(&format!(
        "{} ({})",
        output::strong(&target.app_name),
        output::strong(&target.env)
    ));

    let results = send_typed_to_servers::<BackupStatusResponse>(
        &target.server_names,
        servers,
        Command::BackupStatus {
            app: target.remote_app_name.clone(),
        },
        "backup_status",
        &format!(
            "Checking backups on {} server(s)",
            target.server_names.len()
        ),
    )
    .await?;

    let mut any_success = false;
    for (server_name, result) in results {
        match result {
            Ok(status) => {
                any_success = true;
                output::bullet(&format_status_line(&server_name, &status));
            }
            Err(error) => output::warning(&format!(
                "{}: failed to load backup status ({})",
                output::strong(&server_name),
                error
            )),
        }
    }

    if any_success {
        Ok(())
    } else {
        Err("Failed to query backup status from all target servers".into())
    }
}

async fn download_backup(
    target: &target::BackupTarget,
    servers: &ServersToml,
    backup_id: &str,
    output_path: Option<PathBuf>,
) -> Result<(), Box<dyn std::error::Error>> {
    let server_name = resolve_single_server(
        &target.server_names,
        "Pass --server to choose a backup source.",
    )?;
    let server = servers
        .get(&server_name)
        .ok_or_else(|| format!("Server '{}' not found in config.toml", server_name))?;
    let output_path = output_path.unwrap_or_else(|| PathBuf::from(format!("{backup_id}.tar.zst")));

    output::section("Backups");
    output::info(&format!(
        "{} ({}) from {}",
        output::strong(&target.app_name),
        output::strong(&target.env),
        output::strong(&server_name)
    ));
    if output::is_dry_run() {
        output::dry_run_skip(&format!("Download backup to {}", output_path.display()));
        return Ok(());
    }

    let download = send_typed_to_server::<BackupDownloadUrlResponse>(
        server,
        Command::BackupDownloadUrl {
            app: target.remote_app_name.clone(),
            backup_id: backup_id.to_string(),
        },
        "backup_download_url",
    )
    .await?;

    let bytes = output::with_spinner_async(
        "Downloading backup",
        "Backup downloaded",
        download_url_to_file(&download.url, &output_path),
    )
    .await
    .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;

    output::section("Summary");
    output::info(&format!(
        "Saved {} ({})",
        output::strong(&output_path.display().to_string()),
        output::format_size(bytes)
    ));
    Ok(())
}

async fn restore_backup(
    target: &target::BackupTarget,
    servers: &ServersToml,
    backup_id: &str,
    assume_yes: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let server_name = resolve_single_server(
        &target.server_names,
        "Pass --server to choose where to restore.",
    )?;
    let server = servers
        .get(&server_name)
        .ok_or_else(|| format!("Server '{}' not found in config.toml", server_name))?;

    if output::is_interactive() && !assume_yes {
        let confirmed = output::confirm_with_description(
            &format!(
                "Restore {} in {} on {} from {}?",
                output::strong(&target.app_name),
                output::strong(&target.env),
                output::strong(&server_name),
                output::strong(backup_id)
            ),
            Some("This stops the app and replaces its data directory."),
            false,
        )?;
        if !confirmed {
            return Err(output::operation_cancelled_error().into());
        }
    }

    output::section("Backups");
    output::info(&format!(
        "{} ({}) on {}",
        output::strong(&target.app_name),
        output::strong(&target.env),
        output::strong(&server_name)
    ));
    if output::is_dry_run() {
        output::dry_run_skip("Restore backup");
        return Ok(());
    }

    let info = output::with_spinner_async(
        "Restoring backup",
        "Backup restored",
        send_typed_to_server::<BackupInfo>(
            server,
            Command::RestoreBackup {
                app: target.remote_app_name.clone(),
                backup_id: backup_id.to_string(),
            },
            "restore_backup",
        ),
    )
    .await
    .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;

    output::section("Summary");
    output::info(&format!(
        "Restored {} from {} ({})",
        output::strong(&target.app_name),
        output::strong(&info.id),
        output::format_size(info.size_bytes)
    ));
    Ok(())
}
