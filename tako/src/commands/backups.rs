use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use clap::Subcommand;
use serde::de::DeserializeOwned;
use tako_core::{
    BackupDownloadUrlResponse, BackupInfo, BackupListResponse, BackupStatusResponse, Command,
    Response,
};
use time::{OffsetDateTime, UtcOffset};
use tokio::io::AsyncWriteExt;
use tracing::Instrument;

use crate::app::require_app_name_from_config_path;
use crate::commands::project_context;
use crate::config::{ServerEntry, ServersToml, TakoToml};
use crate::management_http::ManagementClient;
use crate::output;

static LOCAL_OFFSET: OnceLock<UtcOffset> = OnceLock::new();

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

#[derive(Debug, Clone)]
struct BackupTarget {
    app_name: String,
    env: String,
    remote_app_name: String,
    server_names: Vec<String>,
}

fn resolve_backup_target(
    app_name: &str,
    requested_env: Option<&str>,
    requested_server: Option<&str>,
    tako_config: &TakoToml,
    servers: &ServersToml,
) -> Result<BackupTarget, Box<dyn std::error::Error>> {
    let env = super::helpers::resolve_env(requested_env);
    if !tako_config.envs.contains_key(&env) {
        return Err(format!("Environment '{}' not found in tako.toml.", env).into());
    }

    let mut server_names = match requested_server {
        Some(server_name) => {
            if !servers.contains(server_name) {
                return Err(format!("Server '{}' not found in config.toml", server_name).into());
            }
            let mapped = tako_config.get_servers_for_env(&env);
            if !mapped.is_empty() && !mapped.contains(&server_name) {
                return Err(format!(
                    "Server '{}' is not configured for environment '{}'.",
                    server_name, env
                )
                .into());
            }
            vec![server_name.to_string()]
        }
        None => super::helpers::resolve_servers_for_env(tako_config, servers, &env)
            .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?,
    };
    server_names.sort();
    server_names.dedup();
    super::helpers::validate_server_names(&server_names, servers)
        .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;

    Ok(BackupTarget {
        app_name: app_name.to_string(),
        env: env.clone(),
        remote_app_name: tako_core::deployment_app_id(app_name, &env),
        server_names,
    })
}

async fn backup_now(
    target: &BackupTarget,
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
    target: &BackupTarget,
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
    target: &BackupTarget,
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
    target: &BackupTarget,
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
    target: &BackupTarget,
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

async fn send_typed_to_servers<T>(
    server_names: &[String],
    servers: &ServersToml,
    command: Command,
    response_name: &'static str,
    progress: &str,
) -> Result<Vec<(String, Result<T, String>)>, Box<dyn std::error::Error>>
where
    T: DeserializeOwned + Send + 'static,
{
    let mut tasks = Vec::new();
    for server_name in server_names {
        let Some(server) = servers.get(server_name) else {
            continue;
        };
        let server_name = server_name.clone();
        let server = server.clone();
        let command = command.clone();
        let span = output::scope(&server_name);
        tasks.push(tokio::spawn(
            async move {
                let result = send_typed_to_server::<T>(&server, command, response_name).await;
                (server_name, result)
            }
            .instrument(span),
        ));
    }

    let results = if output::is_interactive() && tasks.len() > 1 {
        output::with_spinner_async_simple(progress, async {
            let mut results = Vec::new();
            for task in tasks {
                results.push(task.await);
            }
            results
        })
        .await
    } else {
        let mut results = Vec::new();
        for task in tasks {
            results.push(task.await);
        }
        results
    };

    let mut out = Vec::new();
    for result in results {
        match result {
            Ok(value) => out.push(value),
            Err(error) => out.push(("<task>".to_string(), Err(error.to_string()))),
        }
    }
    Ok(out)
}

async fn send_typed_to_server<T>(
    server: &ServerEntry,
    command: Command,
    response_name: &'static str,
) -> Result<T, String>
where
    T: DeserializeOwned,
{
    let mut client = ManagementClient::new(&server.host)
        .await
        .map_err(|e| e.to_string())?;
    parse_typed_response(
        client.send(&command).await.map_err(|e| e.to_string())?,
        response_name,
    )
}

fn parse_typed_response<T>(response: Response, response_name: &str) -> Result<T, String>
where
    T: DeserializeOwned,
{
    match response {
        Response::Ok { data } => serde_json::from_value(data)
            .map_err(|e| format!("invalid {response_name} response: {e}")),
        Response::Error { message } => Err(message),
    }
}

fn resolve_single_server(server_names: &[String], message: &str) -> Result<String, String> {
    match server_names {
        [server] => Ok(server.clone()),
        [] => Err("No target servers found.".to_string()),
        _ => Err(message.to_string()),
    }
}

async fn download_url_to_file(url: &str, path: &Path) -> Result<u64, String> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| format!("create output directory {}: {e}", parent.display()))?;
    }

    let mut response = reqwest::Client::new()
        .get(url)
        .send()
        .await
        .map_err(|e| format!("download backup: {e}"))?;
    if !response.status().is_success() {
        return Err(format!("download backup returned {}", response.status()));
    }

    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .await
        .map_err(|e| format!("create output file {}: {e}", path.display()))?;
    let mut written = 0_u64;
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| format!("read backup download: {e}"))?
    {
        file.write_all(&chunk)
            .await
            .map_err(|e| format!("write output file {}: {e}", path.display()))?;
        written = written.saturating_add(chunk.len() as u64);
    }
    file.shutdown()
        .await
        .map_err(|e| format!("flush output file {}: {e}", path.display()))?;
    Ok(written)
}

fn output_backup_line(backup: &BackupInfo) {
    output::bullet(&format!(
        "{} {} {}",
        output::strong(&backup.id),
        format_backup_time(backup.created_at_unix_secs),
        output::format_size(backup.size_bytes)
    ));
}

fn format_status_line(server_name: &str, status: &BackupStatusResponse) -> String {
    if !status.enabled {
        return format!("{server_name}: disabled");
    }
    let last = status
        .last_backup
        .as_ref()
        .map(|backup| {
            format!(
                "last {} at {}",
                backup.id,
                format_backup_time(backup.created_at_unix_secs)
            )
        })
        .unwrap_or_else(|| "no backups yet".to_string());
    let next = status
        .next_backup_at_unix_secs
        .map(format_backup_time)
        .unwrap_or_else(|| "-".to_string());
    let retention = status
        .retention_days
        .map(|days| format!("{days}d retention"))
        .unwrap_or_else(|| "retention unknown".to_string());
    format!("{server_name}: enabled, {last}, next {next}, {retention}")
}

fn local_offset() -> UtcOffset {
    *LOCAL_OFFSET.get_or_init(|| UtcOffset::current_local_offset().unwrap_or(UtcOffset::UTC))
}

fn format_backup_time(unix_secs: i64) -> String {
    OffsetDateTime::from_unix_timestamp(unix_secs)
        .map(|dt| {
            let dt = dt.to_offset(local_offset());
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
    fn resolve_single_server_accepts_only_target() {
        assert_eq!(
            resolve_single_server(&["prod".to_string()], "choose").unwrap(),
            "prod"
        );
    }

    #[test]
    fn resolve_single_server_rejects_multiple_targets() {
        let err = resolve_single_server(&["a".to_string(), "b".to_string()], "choose one")
            .expect_err("should reject");
        assert_eq!(err, "choose one");
    }

    #[test]
    fn status_line_marks_disabled_backup() {
        let status = BackupStatusResponse {
            app: "demo/production".to_string(),
            enabled: false,
            retention_days: None,
            last_backup: None,
            next_backup_at_unix_secs: None,
        };
        assert_eq!(format_status_line("prod", &status), "prod: disabled");
    }
}
