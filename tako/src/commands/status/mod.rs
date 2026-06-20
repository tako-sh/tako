mod remote;
mod render;
mod time;

use crate::commands::server;
use crate::config::ServersToml;
use crate::output;
use crate::ui::{TaskIcon, TaskItemState, TaskState, TaskTreeSession, TreeNode};
use serde::Serialize;
use std::collections::HashMap;
use std::time::Instant;
use tako_core::AppStatus;
use tracing::Instrument;

use remote::query_global_server_status;
use render::render_global_status;

#[cfg(test)]
use remote::{
    display_server_version, expand_status_by_running_builds, format_remote_app_label,
    normalize_server_version, parse_list_apps_response, parse_remote_app_name,
    parse_server_env_from_tako_toml, sort_global_apps,
};
#[cfg(test)]
use render::{app_state_summary, service_status_display};
#[cfg(test)]
use tako_core::{AppState, InstanceState, Response};
#[cfg(test)]
use time::{
    format_deployed_at, format_duration_human, format_unix_timestamp_local,
    format_unix_timestamp_with_offset, parse_uptime_since,
};

/// Server status result from querying a remote server
#[derive(Debug, Clone, Serialize)]
struct ServerStatusResult {
    service_status: String,
    server_version: Option<String>,
    app_status: Option<AppStatus>,
    deployed_at_unix_secs: Option<i64>,
    error: Option<String>,
}

/// Global app status discovered on a specific server.
#[derive(Debug, Clone, Serialize)]
struct GlobalAppStatusResult {
    app_name: String,
    env_name: String,
    status: ServerStatusResult,
}

/// Global server status result with all apps discovered on a server.
#[derive(Debug, Serialize)]
struct GlobalServerStatusResult {
    service_status: String,
    server_version: Option<String>,
    server_uptime: Option<String>,
    process_uptime: Option<String>,
    routes: Vec<(String, String)>,
    apps: Vec<GlobalAppStatusResult>,
    error: Option<String>,
}

#[cfg(test)]
static LOCAL_OFFSET: std::sync::OnceLock<::time::UtcOffset> = std::sync::OnceLock::new();

pub async fn run(json: bool) -> Result<(), Box<dyn std::error::Error>> {
    let mut servers = ServersToml::load()?;

    if !json
        && servers.is_empty()
        && server::prompt_to_add_server(
            "No servers configured yet. Add one now to see deployment status.",
        )
        .await?
        .is_some()
    {
        servers = ServersToml::load()?;
    }

    run_global_status(&servers, json).await
}

async fn run_global_status(
    servers: &ServersToml,
    json: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    if servers.is_empty() {
        if json {
            output::json_result(serde_json::json!({
                "ok": true,
                "command": "status",
                "servers": [],
            }))?;
        } else {
            output::warning("No servers configured.");
            output::hint(&format!(
                "Run {} to add one.",
                output::strong("tako servers add")
            ));
        }
        return Ok(());
    }

    let server_names = sorted_server_names(servers);

    let mut results = collect_global_status_results(servers, &server_names, !json).await?;
    if json {
        render_global_status_json(servers, &server_names, &results)?;
    } else {
        render_global_status(servers, &server_names, &mut results);
    }

    Ok(())
}

fn render_global_status_json(
    servers: &ServersToml,
    server_names: &[String],
    server_results: &HashMap<String, GlobalServerStatusResult>,
) -> Result<(), Box<dyn std::error::Error>> {
    let servers = server_names
        .iter()
        .filter_map(|name| {
            let result = server_results.get(name)?;
            let entry = servers.get(name.as_str());
            Some(serde_json::json!({
                "name": name,
                "host": entry.map(|entry| entry.host.as_str()),
                "port": entry.map(|entry| entry.port),
                "description": entry.and_then(|entry| entry.description.as_deref()),
                "service_status": &result.service_status,
                "server_version": &result.server_version,
                "server_uptime": &result.server_uptime,
                "process_uptime": &result.process_uptime,
                "routes": result.routes.iter().map(|(app, pattern)| {
                    serde_json::json!({
                        "app": app,
                        "pattern": pattern,
                    })
                }).collect::<Vec<_>>(),
                "apps": &result.apps,
                "error": &result.error,
            }))
        })
        .collect::<Vec<_>>();

    output::json_result(serde_json::json!({
        "ok": true,
        "command": "status",
        "servers": servers,
    }))
}

fn sorted_server_names(servers: &ServersToml) -> Vec<String> {
    let mut server_names: Vec<String> = servers.names().into_iter().map(str::to_string).collect();
    server_names.sort();
    server_names
}

async fn collect_global_status_results(
    servers: &ServersToml,
    server_names: &[String],
    show_task_tree: bool,
) -> Result<HashMap<String, GlobalServerStatusResult>, Box<dyn std::error::Error>> {
    let mut join_set = tokio::task::JoinSet::new();

    for server_name in server_names {
        let Some(entry) = servers.get(server_name.as_str()) else {
            continue;
        };

        let name = server_name.clone();
        let host = entry.host.clone();
        let port = entry.port;
        let span = output::scope(&name);
        join_set.spawn(
            async move {
                let status = query_global_server_status(&name, &host, port).await;
                (name, status)
            }
            .instrument(span),
        );
    }

    let total = join_set.len();
    let mut done = 0usize;
    let task_started_at = Instant::now();
    let mut status_tasks = server_names
        .iter()
        .map(|name| {
            status_task(
                name,
                TaskState::Running {
                    started_at: task_started_at,
                },
            )
        })
        .collect::<Vec<_>>();
    let task_tree = (show_task_tree && output::is_pretty() && output::is_interactive())
        .then(|| TaskTreeSession::new(build_status_task_tree(&status_tasks)));

    let mut server_results: HashMap<String, GlobalServerStatusResult> = HashMap::new();

    while let Some(join_result) = join_set.join_next().await {
        let (server_name, status) = match join_result {
            Ok(pair) => pair,
            Err(err) => {
                done += 1;
                refresh_status_task_tree(&task_tree, &mut status_tasks, None, true);
                output::error(&format!("Status task panicked: {err}"));
                continue;
            }
        };

        done += 1;
        let failed = status.error.is_some();
        refresh_status_task_tree(&task_tree, &mut status_tasks, Some(&server_name), failed);

        server_results.insert(server_name, status);
    }

    if let Some(session) = task_tree {
        let final_state = if done == total {
            TaskState::Succeeded { elapsed: None }
        } else {
            TaskState::Failed { elapsed: None }
        };
        session.set_tree(build_status_task_tree_with_state(
            &status_tasks,
            final_state,
        ));
        session.finalize();
    }

    Ok(server_results)
}

fn status_task(name: &str, state: TaskState) -> TaskItemState {
    TaskItemState {
        id: status_task_id(name),
        label: name.to_string(),
        state,
        icon: TaskIcon::Box,
        detail: None,
        progress: None,
        children: Vec::new(),
    }
}

fn status_task_id(name: &str) -> String {
    format!("status:{name}")
}

fn build_status_task_tree(tasks: &[TaskItemState]) -> Vec<TreeNode> {
    build_status_task_tree_with_state(
        tasks,
        TaskState::Running {
            started_at: Instant::now(),
        },
    )
}

fn build_status_task_tree_with_state(tasks: &[TaskItemState], state: TaskState) -> Vec<TreeNode> {
    vec![TreeNode::Task(TaskItemState {
        id: "status".into(),
        label: "Retrieving status".into(),
        state,
        icon: TaskIcon::State,
        detail: None,
        progress: None,
        children: tasks.to_vec(),
    })]
}

fn refresh_status_task_tree(
    task_tree: &Option<TaskTreeSession>,
    status_tasks: &mut [TaskItemState],
    server_name: Option<&str>,
    failed: bool,
) {
    let task = if let Some(server_name) = server_name {
        let task_id = status_task_id(server_name);
        status_tasks.iter_mut().find(|task| task.id == task_id)
    } else {
        status_tasks
            .iter_mut()
            .find(|task| matches!(task.state, TaskState::Running { .. }))
    };

    if let Some(task) = task {
        let elapsed = match task.state {
            TaskState::Running { started_at } => Some(started_at.elapsed()),
            _ => None,
        };
        task.state = if failed {
            TaskState::Failed { elapsed }
        } else {
            TaskState::Succeeded { elapsed }
        };
    }

    if let Some(session) = task_tree {
        session.set_tree(build_status_task_tree(status_tasks));
    }
}

#[cfg(test)]
fn local_offset() -> ::time::UtcOffset {
    *LOCAL_OFFSET
        .get_or_init(|| ::time::UtcOffset::current_local_offset().unwrap_or(::time::UtcOffset::UTC))
}

#[cfg(test)]
fn format_unix_timestamp_with_date_command(unix_secs: i64) -> Option<String> {
    use std::process::Command;
    let unix = unix_secs.to_string();

    let mut bsd = Command::new("date");
    bsd.args(["-r", &unix, "+%c"]);
    if let Some(value) = run_local_date_command(bsd) {
        return Some(value);
    }

    let mut gnu = Command::new("date");
    gnu.args(["-d", &format!("@{}", unix_secs), "+%c"]);
    run_local_date_command(gnu)
}

#[cfg(test)]
fn run_local_date_command(mut cmd: std::process::Command) -> Option<String> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shell::shell_single_quote;
    use ::time::UtcOffset;
    use tako_core::{BuildStatus, InstanceStatus};

    #[test]
    fn sort_global_apps_orders_by_app_then_env() {
        let status = ServerStatusResult {
            service_status: "active".to_string(),
            server_version: None,
            app_status: None,
            deployed_at_unix_secs: None,
            error: None,
        };
        let mut apps = vec![
            GlobalAppStatusResult {
                app_name: "web".to_string(),
                env_name: "staging".to_string(),
                status: status.clone(),
            },
            GlobalAppStatusResult {
                app_name: "api".to_string(),
                env_name: "production".to_string(),
                status: status.clone(),
            },
            GlobalAppStatusResult {
                app_name: "web".to_string(),
                env_name: "production".to_string(),
                status,
            },
        ];

        sort_global_apps(&mut apps);

        let ordered: Vec<(String, String)> = apps
            .into_iter()
            .map(|entry| (entry.app_name, entry.env_name))
            .collect();
        assert_eq!(
            ordered,
            vec![
                ("api".to_string(), "production".to_string()),
                ("web".to_string(), "production".to_string()),
                ("web".to_string(), "staging".to_string()),
            ]
        );
    }

    #[test]
    fn expand_status_by_running_builds_returns_one_entry_per_build() {
        let status = ServerStatusResult {
            service_status: "active".to_string(),
            server_version: Some("0.1.0".to_string()),
            app_status: Some(AppStatus {
                name: "demo".to_string(),
                version: "v2".to_string(),
                instances: vec![],
                builds: vec![
                    BuildStatus {
                        version: "v1".to_string(),
                        state: AppState::Running,
                        instances: vec![InstanceStatus {
                            id: "abc1".to_string(),
                            state: InstanceState::Healthy,
                            pid: Some(111),
                            uptime_secs: 10,
                            requests_total: 0,
                        }],
                    },
                    BuildStatus {
                        version: "v2".to_string(),
                        state: AppState::Running,
                        instances: vec![InstanceStatus {
                            id: "abc2".to_string(),
                            state: InstanceState::Healthy,
                            pid: Some(222),
                            uptime_secs: 12,
                            requests_total: 0,
                        }],
                    },
                ],
                state: AppState::Deploying,
                last_error: None,
            }),
            deployed_at_unix_secs: None,
            error: None,
        };

        let expanded = expand_status_by_running_builds(status);
        let versions: Vec<&str> = expanded
            .iter()
            .filter_map(|entry| entry.app_status.as_ref().map(|app| app.version.as_str()))
            .collect();
        assert_eq!(expanded.len(), 2);
        assert!(versions.contains(&"v1"));
        assert!(versions.contains(&"v2"));
    }

    #[test]
    fn normalize_server_version_strips_binary_prefix() {
        assert_eq!(
            normalize_server_version("tako-server 0.1.0".to_string()),
            "0.1.0"
        );
        assert_eq!(normalize_server_version("0.2.0".to_string()), "0.2.0");
    }

    #[test]
    fn format_unix_timestamp_with_offset_formats_in_requested_timezone() {
        let utc = format_unix_timestamp_with_offset(0, UtcOffset::UTC).unwrap();
        assert_eq!(utc, "1970-01-01 00:00:00");

        let plus_two = UtcOffset::from_hms(2, 0, 0).unwrap();
        let plus_two_formatted = format_unix_timestamp_with_offset(0, plus_two).unwrap();
        assert_eq!(plus_two_formatted, "1970-01-01 02:00:00");
    }

    #[test]
    fn format_unix_timestamp_local_uses_locale_or_fallback() {
        let rendered = format_unix_timestamp_local(0).expect("formatted timestamp");
        assert!(!rendered.is_empty());
    }

    #[test]
    fn display_server_version_prefixes_v_when_missing() {
        assert_eq!(display_server_version("0.1.0"), "v0.1.0");
        assert_eq!(display_server_version("v0.2.0"), "v0.2.0");
    }

    #[test]
    fn shell_single_quote_escapes_single_quotes() {
        assert_eq!(shell_single_quote("a'b"), "'a'\\''b'");
    }

    #[test]
    fn status_task_tree_uses_group_with_boxed_server_rows() {
        let started_at = Instant::now();
        let mut tasks = vec![
            status_task("prod-a", TaskState::Running { started_at }),
            status_task("prod-b", TaskState::Running { started_at }),
        ];

        refresh_status_task_tree(&None, &mut tasks, Some("prod-a"), false);
        let lines = crate::ui::render_plain_lines(&build_status_task_tree(&tasks));

        assert_eq!(lines[0], "Retrieving status…");
        assert!(lines[1].starts_with("  ■ prod-a"));
        assert!(lines[2].starts_with("  ◧ prod-b…"));
    }

    #[test]
    fn parse_list_apps_response_extracts_unique_sorted_names() {
        let response = Response::Ok {
            data: serde_json::json!({
                "apps": [
                    { "name": "web" },
                    { "name": "api" },
                    { "name": "api" }
                ]
            }),
        };

        let names = parse_list_apps_response(response).unwrap();
        assert_eq!(names, vec!["api".to_string(), "web".to_string()]);
    }

    #[test]
    fn parse_remote_app_name_extracts_env_from_deployment_id() {
        assert_eq!(
            parse_remote_app_name("web/staging"),
            ("web".to_string(), Some("staging".to_string()))
        );
        assert_eq!(format_remote_app_label("web/staging"), "web (staging)");
    }

    #[test]
    fn parse_server_env_from_tako_toml_prefers_matching_server_name() {
        let content = r#"
[envs.production]
route = "app.example.com"
servers = ["eu"]

[envs.staging]
route = "staging.example.com"
servers = ["us"]
"#;
        let env = parse_server_env_from_tako_toml(content, "us");
        assert_eq!(env.as_deref(), Some("staging"));
    }

    #[test]
    fn parse_server_env_from_tako_toml_falls_back_to_single_mapping() {
        let content = r#"
[envs.production]
route = "app.example.com"
servers = ["only"]
"#;
        let env = parse_server_env_from_tako_toml(content, "missing");
        assert_eq!(env.as_deref(), Some("production"));
    }

    #[test]
    fn parse_server_env_from_tako_toml_returns_none_for_ambiguous_mappings() {
        let content = r#"
[envs.production]
route = "app.example.com"
servers = ["first"]

[envs.staging]
route = "staging.example.com"
servers = ["second"]
"#;
        assert!(parse_server_env_from_tako_toml(content, "missing").is_none());
    }

    #[tokio::test]
    async fn collect_global_status_task_maps_join_error_to_unknown_status() {
        let handle: tokio::task::JoinHandle<GlobalServerStatusResult> = tokio::spawn(async move {
            panic!("boom");
        });

        let result = match handle.await {
            Ok(status) => status,
            Err(err) => GlobalServerStatusResult {
                service_status: "unknown".to_string(),
                server_version: None,
                server_uptime: None,
                process_uptime: None,
                routes: Vec::new(),
                apps: Vec::new(),
                error: Some(format!("Status task failed: {}", err)),
            },
        };

        assert_eq!(result.service_status, "unknown");
        assert!(result.server_version.is_none());
        assert!(result.apps.is_empty());
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("Status task failed")
        );
    }

    #[test]
    fn format_duration_human_formats_days_hours_minutes() {
        assert_eq!(format_duration_human(0), "0m");
        assert_eq!(format_duration_human(59), "0m");
        assert_eq!(format_duration_human(60), "1m");
        assert_eq!(format_duration_human(3600), "1h 0m");
        assert_eq!(format_duration_human(3660), "1h 1m");
        assert_eq!(format_duration_human(86400), "1d 0h");
        assert_eq!(format_duration_human(90000), "1d 1h");
        assert_eq!(format_duration_human(7 * 86400 + 11 * 3600), "7d 11h");
    }

    #[test]
    fn parse_uptime_since_parses_standard_format() {
        let dt = parse_uptime_since("2026-02-27 14:30:00").unwrap();
        assert_eq!(dt.year(), 2026);
        assert_eq!(dt.month() as u8, 2);
        assert_eq!(dt.day(), 27);
        assert_eq!(dt.hour(), 14);
        assert_eq!(dt.minute(), 30);
    }

    #[test]
    fn parse_uptime_since_returns_none_for_garbage() {
        assert!(parse_uptime_since("not a date").is_none());
        assert!(parse_uptime_since("").is_none());
    }

    #[test]
    fn service_status_display_maps_states_correctly() {
        let (label, _) = service_status_display("active");
        assert_eq!(label, "active");

        let (label, _) = service_status_display("inactive");
        assert_eq!(label, "offline");

        let (label, _) = service_status_display("unknown");
        assert_eq!(label, "offline");

        let (label, _) = service_status_display("upgrading");
        assert_eq!(label, "upgrading");
    }

    #[test]
    fn app_state_summary_shows_healthy_count() {
        let status = ServerStatusResult {
            service_status: "active".to_string(),
            server_version: Some("0.1.0".to_string()),
            app_status: Some(AppStatus {
                name: "demo".to_string(),
                version: "v123".to_string(),
                instances: vec![
                    InstanceStatus {
                        id: "abc1".to_string(),
                        state: InstanceState::Healthy,
                        pid: Some(111),
                        uptime_secs: 10,
                        requests_total: 0,
                    },
                    InstanceStatus {
                        id: "abc2".to_string(),
                        state: InstanceState::Healthy,
                        pid: Some(112),
                        uptime_secs: 10,
                        requests_total: 0,
                    },
                    InstanceStatus {
                        id: "abc3".to_string(),
                        state: InstanceState::Starting,
                        pid: Some(113),
                        uptime_secs: 1,
                        requests_total: 0,
                    },
                ],
                builds: vec![],
                state: AppState::Running,
                last_error: None,
            }),
            deployed_at_unix_secs: None,
            error: None,
        };

        let (summary, _) = app_state_summary(Some(&status));
        assert_eq!(summary, "healthy 2/3");
    }

    #[test]
    fn app_state_summary_shows_deploying() {
        let status = ServerStatusResult {
            service_status: "active".to_string(),
            server_version: Some("0.1.0".to_string()),
            app_status: Some(AppStatus {
                name: "demo".to_string(),
                version: "v123".to_string(),
                instances: vec![],
                builds: vec![],
                state: AppState::Deploying,
                last_error: None,
            }),
            deployed_at_unix_secs: None,
            error: None,
        };

        let (summary, _) = app_state_summary(Some(&status));
        assert_eq!(summary, "deploying");
    }

    #[test]
    fn format_deployed_at_formats_epoch_in_local_time() {
        let ts = 1772798400;
        let formatted = format_deployed_at(ts).unwrap();
        assert!(formatted.starts_with("Mar 6, 2026"));
    }
}
