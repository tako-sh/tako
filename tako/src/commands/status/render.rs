use super::remote::{display_server_version, sort_global_apps};
use super::time::format_deployed_at;
use super::{GlobalServerStatusResult, ServerStatusResult};
use crate::config::ServersToml;
use crate::output;
use tako_core::{AppState, InstanceState};

#[derive(Clone, Copy)]
pub(super) enum StatusColor {
    Success,
    Warning,
    Error,
}

fn colorize(text: &str, color: Option<StatusColor>) -> String {
    match color {
        Some(StatusColor::Success) => output::theme_success(text),
        Some(StatusColor::Warning) => output::theme_warning(text),
        Some(StatusColor::Error) => output::theme_error(text),
        None => text.to_string(),
    }
}

pub(super) fn render_global_status(
    servers: &ServersToml,
    server_names: &[String],
    server_results: &mut std::collections::HashMap<String, GlobalServerStatusResult>,
) {
    for server_name in server_names {
        let Some(global) = server_results.remove(server_name.as_str()) else {
            continue;
        };
        let entry = servers.get(server_name.as_str());
        let description = entry
            .and_then(|e| e.description.as_deref())
            .filter(|d| !d.trim().is_empty())
            .map(str::to_string);

        for line in format_server_status_lines(server_name, description.as_deref(), &global) {
            eprintln!("{line}");
        }
        eprintln!();
    }
}

fn format_server_status_lines(
    server_name: &str,
    description: Option<&str>,
    global: &GlobalServerStatusResult,
) -> Vec<String> {
    let mut lines = vec![format!("Server {}", output::strong(server_name))];

    if let Some(ref err) = global.error {
        lines.push(format!(
            "{}Error  {}",
            output::INDENT,
            colorize(err, Some(StatusColor::Error))
        ));
        return lines;
    }

    lines.push(format_server_summary(global));

    if !global.routes.is_empty() {
        lines.push(String::new());
        lines.push(format!("{}Routes", output::INDENT));
        lines.extend(format_route_lines(&global.routes));
    }

    if !global.apps.is_empty() && global.service_status == "active" {
        lines.push(String::new());
        lines.push(format!("{}Apps", output::INDENT));
        lines.extend(format_app_lines(&global.apps));
    }

    if let Some(desc) = description {
        lines.push(String::new());
        lines.push(format!("{}Description  {desc}", output::INDENT));
    }

    lines
}

fn format_server_summary(global: &GlobalServerStatusResult) -> String {
    let (status_label, status_color) = service_status_display(&global.service_status);
    let mut segments = vec![colorize(&status_label, Some(status_color))];

    if let Some(ref ver) = global.server_version {
        segments.push(display_server_version(ver));
    }

    match (&global.server_uptime, &global.process_uptime) {
        (Some(host_uptime), Some(process_uptime)) if global.service_status != "upgrading" => {
            segments.push(format!("host up {host_uptime}"));
            segments.push(format!("server up {process_uptime}"));
        }
        (Some(uptime), _) => segments.push(format!("up {uptime}")),
        (None, Some(uptime)) if global.service_status != "upgrading" => {
            segments.push(format!("up {uptime}"));
        }
        (None, _) => {}
    }

    format!("{}{}", output::INDENT, segments.join("  "))
}

fn format_route_lines(routes: &[(String, String)]) -> Vec<String> {
    let label_width = routes.iter().map(|(app, _)| app.len()).max().unwrap_or(0);
    let mut lines = Vec::new();
    let mut last_app = "";

    for (app, pattern) in routes {
        let label = if app == last_app {
            ""
        } else {
            last_app = app;
            app
        };
        lines.push(format!(
            "{indent}{label:<label_width$}  {pattern}",
            indent = output::INDENT.repeat(2),
        ));
    }

    lines
}

fn format_app_lines(apps: &[super::GlobalAppStatusResult]) -> Vec<String> {
    let mut apps = apps.to_vec();
    sort_global_apps(&mut apps);

    let label_width = apps.iter().map(|app| app.app_name.len()).max().unwrap_or(0);
    let mut lines = Vec::new();

    for app in &apps {
        let (state, color) = app_state_summary(Some(&app.status));
        let mut segments = vec![colorize(&state, Some(color))];

        if app.env_name != "unknown" {
            segments.push(app.env_name.clone());
        }

        if let Some(ref app_status) = app.status.app_status
            && !app_status.version.is_empty()
        {
            segments.push(app_status.version.clone());
        }

        if let Some(unix_secs) = app.status.deployed_at_unix_secs
            && let Some(formatted) = format_deployed_at(unix_secs)
        {
            segments.push(format!("deployed {formatted}"));
        }

        lines.push(format!(
            "{indent}{app_name:<label_width$}  {details}",
            indent = output::INDENT.repeat(2),
            app_name = app.app_name.as_str(),
            details = segments.join("  "),
        ));
    }

    lines
}

pub(super) fn service_status_display(status: &str) -> (String, StatusColor) {
    match status {
        "active" => ("active".into(), StatusColor::Success),
        "upgrading" => ("upgrading".into(), StatusColor::Warning),
        "inactive" | "failed" => ("offline".into(), StatusColor::Error),
        "unknown" => ("offline".into(), StatusColor::Error),
        other => (other.to_string(), StatusColor::Warning),
    }
}

pub(super) fn app_state_summary(status: Option<&ServerStatusResult>) -> (String, StatusColor) {
    let Some(result) = status else {
        return ("unknown".into(), StatusColor::Warning);
    };

    if let Some(app_status) = &result.app_status {
        let healthy = app_status
            .instances
            .iter()
            .filter(|i| i.state == InstanceState::Healthy || i.state == InstanceState::Ready)
            .count();
        let total = app_status.instances.len();

        return match app_status.state {
            AppState::Running => (format!("healthy {healthy}/{total}"), StatusColor::Success),
            AppState::Idle => ("idle".into(), StatusColor::Warning),
            AppState::Deploying => ("deploying".into(), StatusColor::Warning),
            AppState::Stopped => ("stopped".into(), StatusColor::Warning),
            AppState::Error => ("error".into(), StatusColor::Error),
        };
    }

    if result.service_status == "active" {
        ("not deployed".into(), StatusColor::Warning)
    } else {
        ("unavailable".into(), StatusColor::Error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tako_core::{AppStatus, InstanceStatus};

    #[test]
    fn format_server_status_lines_uses_compact_sections() {
        let global = GlobalServerStatusResult {
            service_status: "active".to_string(),
            server_version: Some("0.1.0".to_string()),
            server_uptime: None,
            process_uptime: Some("4d 6h".to_string()),
            routes: vec![
                (
                    "web (production)".to_string(),
                    "web.example.com".to_string(),
                ),
                (
                    "web (production)".to_string(),
                    "*.web.example.com".to_string(),
                ),
            ],
            apps: vec![super::super::GlobalAppStatusResult {
                app_name: "web".to_string(),
                env_name: "production".to_string(),
                status: ServerStatusResult {
                    service_status: "active".to_string(),
                    server_version: Some("0.1.0".to_string()),
                    app_status: Some(AppStatus {
                        name: "web".to_string(),
                        version: "abc123".to_string(),
                        instances: vec![InstanceStatus {
                            id: "i1".to_string(),
                            state: InstanceState::Healthy,
                            pid: Some(42),
                            uptime_secs: 60,
                            requests_total: 10,
                        }],
                        builds: vec![],
                        state: AppState::Running,
                        last_error: None,
                    }),
                    deployed_at_unix_secs: None,
                    error: None,
                },
            }],
            error: None,
        };

        let lines = format_server_status_lines("prod-a", None, &global);

        assert_eq!(
            lines,
            vec![
                "Server prod-a",
                "  active  v0.1.0  up 4d 6h",
                "",
                "  Routes",
                "    web (production)  web.example.com",
                "                      *.web.example.com",
                "",
                "  Apps",
                "    web  healthy 1/1  production  abc123",
            ]
        );
    }

    #[test]
    fn format_server_status_lines_shows_errors_directly() {
        let global = GlobalServerStatusResult {
            service_status: "unknown".to_string(),
            server_version: None,
            server_uptime: None,
            process_uptime: None,
            routes: Vec::new(),
            apps: Vec::new(),
            error: Some("Remote management failed".to_string()),
        };

        let lines = format_server_status_lines("prod-a", None, &global);

        assert_eq!(
            lines,
            vec!["Server prod-a", "  Error  Remote management failed"]
        );
    }
}
