use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command as ProcessCommand;
use std::sync::OnceLock;

use clap::Subcommand;
use time::{OffsetDateTime, UtcOffset};

use crate::app::require_app_name_from_config_path;
use crate::commands::project_context;
use crate::config::{ServerEntry, ServersToml, TakoToml};
use crate::management_http::ManagementClient;
use crate::output;
use tako_core::{Command, ListReleasesResponse, ReleaseInfo, Response};
use tracing::Instrument;

static LOCAL_OFFSET: OnceLock<UtcOffset> = OnceLock::new();

#[derive(Subcommand)]
pub enum ReleaseCommands {
    /// List previously deployed releases/builds for the current app
    #[command(visible_alias = "ls")]
    List {
        /// Environment to query (defaults to production)
        #[arg(long)]
        env: Option<String>,
    },

    /// Roll back the current app/environment to a previous release/build id
    Rollback {
        /// Target release/build id
        release: String,

        /// Environment to roll back (defaults to production)
        #[arg(long)]
        env: Option<String>,

        /// Skip confirmation prompt
        #[arg(short = 'y', long = "yes")]
        yes: bool,
    },
}

pub fn run(
    cmd: ReleaseCommands,
    config_path: Option<&Path>,
) -> Result<(), Box<dyn std::error::Error>> {
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(run_async(cmd, config_path))
}

async fn run_async(
    cmd: ReleaseCommands,
    config_path: Option<&Path>,
) -> Result<(), Box<dyn std::error::Error>> {
    let context = project_context::resolve_existing(config_path)?;
    let app_name = require_app_name_from_config_path(&context.config_path)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e.to_string()))?;
    let tako_config = TakoToml::load_from_file(&context.config_path)?;
    let servers = ServersToml::load()?;

    match cmd {
        ReleaseCommands::List { env } => {
            let env = resolve_env_name(env.as_deref(), &tako_config)?;
            let server_names = resolve_server_names_for_env(&tako_config, &servers, &env)?;
            list_releases(&app_name, &env, &server_names, &servers).await
        }
        ReleaseCommands::Rollback { release, env, yes } => {
            let env = resolve_env_name(env.as_deref(), &tako_config)?;
            let server_names = resolve_server_names_for_env(&tako_config, &servers, &env)?;
            rollback_release(&app_name, &release, &env, yes, &server_names, &servers).await
        }
    }
}

fn resolve_env_name(requested_env: Option<&str>, tako_config: &TakoToml) -> Result<String, String> {
    let env = super::helpers::resolve_env(requested_env);
    if !tako_config.envs.contains_key(env.as_str()) {
        let mut available: Vec<String> = tako_config.envs.keys().cloned().collect();
        available.sort();
        let available = if available.is_empty() {
            "(none)".to_string()
        } else {
            available.join(", ")
        };
        return Err(format!(
            "Environment '{}' not found in tako.toml. Available: {}",
            env, available
        ));
    }
    Ok(env)
}

fn resolve_server_names_for_env(
    tako_config: &TakoToml,
    servers: &ServersToml,
    env: &str,
) -> Result<Vec<String>, String> {
    let mut resolved = super::helpers::resolve_servers_for_env(tako_config, servers, env)?;
    resolved.sort();
    resolved.dedup();
    super::helpers::validate_server_names(&resolved, servers)?;
    Ok(resolved)
}

async fn list_releases(
    app_name: &str,
    env: &str,
    server_names: &[String],
    servers: &ServersToml,
) -> Result<(), Box<dyn std::error::Error>> {
    let remote_app_name = tako_core::deployment_app_id(app_name, env);
    output::section("Releases");
    output::info(&format!(
        "{} ({})",
        output::strong(app_name),
        output::strong(env)
    ));

    let mut tasks = Vec::new();
    for server_name in server_names {
        let Some(server) = servers.get(server_name) else {
            continue;
        };
        let server_name = server_name.clone();
        let server = server.clone();
        let remote_app_name = remote_app_name.clone();
        let span = output::scope(&server_name);
        tasks.push(tokio::spawn(
            async move {
                let result = fetch_releases_for_server(&server, &remote_app_name).await;
                (server_name, result)
            }
            .instrument(span),
        ));
    }

    let mut merged: BTreeMap<String, ReleaseInfo> = BTreeMap::new();
    let mut any_success = false;
    let task_results = output::with_spinner_async_simple(
        &format!("Loading releases from {} server(s)", server_names.len()),
        async {
            let mut results = Vec::new();
            for task in tasks {
                results.push(task.await);
            }
            results
        },
    )
    .await;
    for task in task_results {
        let (server_name, result) = task?;
        match result {
            Ok(releases) => {
                any_success = true;
                for release in releases {
                    let entry = merged
                        .entry(release.version.clone())
                        .or_insert_with(|| release.clone());
                    entry.current = entry.current || release.current;
                    entry.deployed_at_unix_secs =
                        max_unix_secs(entry.deployed_at_unix_secs, release.deployed_at_unix_secs);
                    if entry.commit_message.is_none() && release.commit_message.is_some() {
                        entry.commit_message = release.commit_message.clone();
                    }
                    entry.git_dirty = merge_git_dirty(entry.git_dirty, release.git_dirty);
                }
            }
            Err(error) => output::warning(&format!(
                "{}: failed to load releases ({})",
                output::strong(&server_name),
                error
            )),
        }
    }

    if !any_success {
        return Err("Failed to query releases from all target servers".into());
    }

    let mut releases: Vec<ReleaseInfo> = merged.into_values().collect();
    releases.sort_by(|a, b| {
        b.deployed_at_unix_secs
            .cmp(&a.deployed_at_unix_secs)
            .then_with(|| b.version.cmp(&a.version))
    });

    if releases.is_empty() {
        output::muted("No releases found.");
        return Ok(());
    }

    for release in releases {
        output_release_lines(&release);
    }

    Ok(())
}

async fn rollback_release(
    app_name: &str,
    release: &str,
    env: &str,
    assume_yes: bool,
    server_names: &[String],
    servers: &ServersToml,
) -> Result<(), Box<dyn std::error::Error>> {
    let remote_app_name = tako_core::deployment_app_id(app_name, env);
    if should_confirm_production_rollback(env, assume_yes, output::is_interactive()) {
        let confirmed = output::confirm(
            &format!(
                "Rollback {} in {} to {}?",
                output::strong(app_name),
                output::strong(env),
                output::strong(release)
            ),
            false,
        )?;
        if !confirmed {
            return Err(output::operation_cancelled_error().into());
        }
    }

    output::section("Rollback");
    output::info(&format!(
        "{} ({}) -> {}",
        output::strong(app_name),
        output::strong(env),
        output::strong(release)
    ));

    let mut tasks = Vec::new();
    for server_name in server_names {
        let Some(server) = servers.get(server_name) else {
            continue;
        };
        let server_name = server_name.clone();
        let server = server.clone();
        let remote_app_name = remote_app_name.clone();
        let release = release.to_string();
        let span = output::scope(&server_name);
        tasks.push(tokio::spawn(
            async move {
                let result = rollback_server_release(&server, &remote_app_name, &release).await;
                (server_name, server, result)
            }
            .instrument(span),
        ));
    }

    let mut success_count = 0usize;
    let mut errors = Vec::new();
    let rollback_results = output::with_spinner_async_simple(
        &format!("Rolling back on {} server(s)", server_names.len()),
        async {
            let mut results = Vec::new();
            for task in tasks {
                results.push(task.await);
            }
            results
        },
    )
    .await;
    for task in rollback_results {
        let (server_name, _server, result) = task?;
        match result {
            Ok(()) => {
                output::bullet(&format!("{} rolled back", output::strong(&server_name)));
                success_count += 1;
            }
            Err(error) => {
                output::error(&format!(
                    "{} rollback failed: {}",
                    output::strong(&server_name),
                    error
                ));
                errors.push(format!("{}: {}", server_name, error));
            }
        }
    }

    if errors.is_empty() {
        output::success("Rollback");
        output::section("Summary");
        output::info(&format!(
            "Rolled back {} to {} on {} server(s)",
            output::strong(app_name),
            output::strong(release),
            success_count
        ));
        Ok(())
    } else {
        output::error("Rollback");
        output::section("Summary");
        output::warning(&format!(
            "Rollback partially failed: {}/{} servers succeeded",
            output::strong(&success_count.to_string()),
            server_names.len()
        ));
        for error in errors {
            output::error(&error);
        }
        Err("Rollback failed on one or more servers".into())
    }
}

fn should_confirm_production_rollback(env: &str, assume_yes: bool, interactive: bool) -> bool {
    env == "production" && !assume_yes && interactive
}

async fn fetch_releases_for_server(
    server: &ServerEntry,
    app_name: &str,
) -> Result<Vec<ReleaseInfo>, String> {
    let _t = output::timed(&format!("Fetch releases for {app_name}"));
    let mut client = ManagementClient::new(&server.host)
        .await
        .map_err(|e| e.to_string())?;
    let response = client
        .send(&Command::ListReleases {
            app: app_name.to_string(),
        })
        .await
        .map_err(|e| e.to_string())?;
    let result = parse_release_list_response(response);
    if let Ok(ref releases) = result {
        tracing::debug!("Returned {} release(s)", releases.len());
    }
    result
}

async fn rollback_server_release(
    server: &ServerEntry,
    app_name: &str,
    release: &str,
) -> Result<(), String> {
    let _t = output::timed(&format!("Rollback {app_name} to {release}"));
    let mut client = ManagementClient::new(&server.host)
        .await
        .map_err(|e| e.to_string())?;
    let response = client
        .send(&Command::Rollback {
            app: app_name.to_string(),
            version: release.to_string(),
        })
        .await
        .map_err(|e| e.to_string())?;
    let result = parse_ok_response(response);
    if result.is_ok() {
        tracing::debug!("Rollback succeeded");
    }
    result
}

fn parse_release_list_response(response: Response) -> Result<Vec<ReleaseInfo>, String> {
    match response {
        Response::Ok { data } => {
            let parsed: ListReleasesResponse = serde_json::from_value(data)
                .map_err(|e| format!("invalid list_releases response: {}", e))?;
            Ok(parsed.releases)
        }
        Response::Error { message } => Err(message),
    }
}

fn parse_ok_response(response: Response) -> Result<(), String> {
    match response {
        Response::Ok { .. } => Ok(()),
        Response::Error { message } => Err(message),
    }
}

fn merge_git_dirty(existing: Option<bool>, incoming: Option<bool>) -> Option<bool> {
    match (existing, incoming) {
        (Some(true), _) | (_, Some(true)) => Some(true),
        (Some(false), Some(false)) => Some(false),
        (Some(value), None) | (None, Some(value)) => Some(value),
        (None, None) => None,
    }
}

fn max_unix_secs(a: Option<i64>, b: Option<i64>) -> Option<i64> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}

fn output_release_lines(release: &ReleaseInfo) {
    let head = format_release_head(release);
    let deployed = format_release_deployed(release);
    let commit = format_release_commit_line(release);

    output::info(&format!(
        "{} {}",
        output::strong(&head),
        output::theme_muted(&deployed)
    ));
    output::muted(&commit);
}

fn format_release_head(release: &ReleaseInfo) -> String {
    let mut head = release.version.clone();
    if release.current {
        head.push_str(" [current]");
    }
    head
}

fn format_release_deployed(release: &ReleaseInfo) -> String {
    match release.deployed_at_unix_secs {
        Some(unix) => {
            let mut line = format!(
                "deployed: {}",
                format_unix_timestamp_local(unix).unwrap_or_else(|| "-".to_string())
            );
            if let Some(relative) = format_relative_within_24h(unix) {
                line.push_str(&format!(" {{{}}}", relative));
            }
            line
        }
        None => "deployed: -".to_string(),
    }
}

fn format_release_commit_line(release: &ReleaseInfo) -> String {
    let commit_message = release
        .commit_message
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or("(no commit message)");
    let cleanliness = match release.git_dirty {
        Some(true) => "dirty",
        Some(false) => "clean",
        None => "unknown",
    };
    format!("{} [{}]", commit_message, cleanliness)
}

fn local_offset() -> UtcOffset {
    *LOCAL_OFFSET.get_or_init(|| UtcOffset::current_local_offset().unwrap_or(UtcOffset::UTC))
}

fn format_unix_timestamp_local(unix_secs: i64) -> Option<String> {
    format_unix_timestamp_with_date_command(unix_secs)
        .or_else(|| format_unix_timestamp_with_offset(unix_secs, local_offset()))
}

fn format_unix_timestamp_with_offset(unix_secs: i64, offset: UtcOffset) -> Option<String> {
    let dt = OffsetDateTime::from_unix_timestamp(unix_secs)
        .ok()?
        .to_offset(offset);
    Some(format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        dt.year(),
        dt.month() as u8,
        dt.day(),
        dt.hour(),
        dt.minute(),
        dt.second()
    ))
}

fn format_unix_timestamp_with_date_command(unix_secs: i64) -> Option<String> {
    let unix = unix_secs.to_string();

    // macOS/BSD date
    let mut bsd = ProcessCommand::new("date");
    bsd.args(["-r", &unix, "+%c"]);
    if let Some(value) = run_local_date_command(bsd) {
        return Some(value);
    }

    // GNU date
    let mut gnu = ProcessCommand::new("date");
    gnu.args(["-d", &format!("@{}", unix_secs), "+%c"]);
    run_local_date_command(gnu)
}

fn run_local_date_command(mut cmd: ProcessCommand) -> Option<String> {
    let output = cmd.output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    let trimmed = text.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn format_relative_within_24h(unix_secs: i64) -> Option<String> {
    let now = OffsetDateTime::now_utc().unix_timestamp();
    let delta = now.checked_sub(unix_secs)?;
    if !(0..=86_400).contains(&delta) {
        return None;
    }
    if delta < 60 {
        Some("just now".to_string())
    } else if delta < 3_600 {
        Some(format!("{}m ago", delta / 60))
    } else {
        Some(format!("{}h ago", delta / 3_600))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_git_dirty_prefers_dirty_when_any_source_is_dirty() {
        assert_eq!(merge_git_dirty(Some(false), Some(true)), Some(true));
        assert_eq!(merge_git_dirty(Some(true), Some(false)), Some(true));
        assert_eq!(merge_git_dirty(Some(false), Some(false)), Some(false));
        assert_eq!(merge_git_dirty(None, Some(false)), Some(false));
        assert_eq!(merge_git_dirty(None, None), None);
    }

    #[test]
    fn max_unix_secs_returns_max_when_both_values_exist() {
        assert_eq!(max_unix_secs(Some(10), Some(20)), Some(20));
        assert_eq!(max_unix_secs(Some(20), Some(10)), Some(20));
        assert_eq!(max_unix_secs(Some(20), None), Some(20));
        assert_eq!(max_unix_secs(None, Some(10)), Some(10));
        assert_eq!(max_unix_secs(None, None), None);
    }

    #[test]
    fn production_rollback_confirmation_only_when_interactive_and_not_yes() {
        assert!(should_confirm_production_rollback(
            "production",
            false,
            true
        ));
        assert!(!should_confirm_production_rollback(
            "production",
            true,
            true
        ));
        assert!(!should_confirm_production_rollback("staging", false, true));
        assert!(!should_confirm_production_rollback(
            "production",
            false,
            false
        ));
    }

    #[test]
    fn relative_format_uses_hours_or_minutes_within_day() {
        let now = OffsetDateTime::now_utc().unix_timestamp();
        let hour = format_relative_within_24h(now - 3_600).unwrap();
        assert!(hour.ends_with("h ago"));

        let minute = format_relative_within_24h(now - 120).unwrap();
        assert!(minute.ends_with("m ago"));

        assert!(format_relative_within_24h(now - 86_500).is_none());
    }

    #[test]
    fn release_head_includes_current_marker_when_applicable() {
        let release = ReleaseInfo {
            version: "abc1234".to_string(),
            current: true,
            deployed_at_unix_secs: None,
            commit_message: None,
            git_dirty: None,
        };
        assert_eq!(format_release_head(&release), "abc1234 [current]");
    }

    #[test]
    fn release_deployed_line_uses_placeholder_without_timestamp() {
        let release = ReleaseInfo {
            version: "abc1234".to_string(),
            current: false,
            deployed_at_unix_secs: None,
            commit_message: None,
            git_dirty: None,
        };
        assert_eq!(format_release_deployed(&release), "deployed: -");
    }

    #[test]
    fn release_commit_line_includes_cleanliness_marker() {
        let release = ReleaseInfo {
            version: "abc1234".to_string(),
            current: false,
            deployed_at_unix_secs: None,
            commit_message: Some("feat: add rollback".to_string()),
            git_dirty: Some(true),
        };
        assert_eq!(
            format_release_commit_line(&release),
            "feat: add rollback [dirty]"
        );
    }
}
