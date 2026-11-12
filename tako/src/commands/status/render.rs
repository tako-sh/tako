use super::remote::{display_server_version, sort_global_apps};
use super::time::{format_deployed_at, format_duration_human};
use super::{GlobalServerStatusResult, ServerStatusResult};
use crate::config::ServersToml;
use crate::output;
use tako_core::{AppState, InstanceState, RecoveryStatus};

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
        if let Some(status) = &app.status.app_status {
            for service in &status.services {
                lines.push(format!(
                    "{}{}  {}  {}",
                    output::INDENT.repeat(3),
                    service.service,
                    service.state,
                    service.health
                ));
            }
            for (service, replicas) in &status.replica_overrides {
                lines.push(format!(
                    "{}{}  desired {} (override)",
                    output::INDENT.repeat(3),
                    service,
                    replicas
                ));
            }
            if let Some(error) = &status.services_error {
                lines.push(format!("{}Services  {}", output::INDENT.repeat(3), error));
            }
            if let Some(recovery) = &status.recovery {
                lines.push(format!(
                    "{}{}",
                    output::INDENT.repeat(3),
                    format_recovery(recovery, ::time::OffsetDateTime::now_utc().unix_timestamp())
                ));
            }
        }
    }

    lines
}

/// One line on why instances were restarted and when the next start is due.
fn format_recovery(recovery: &RecoveryStatus, now_unix_secs: i64) -> String {
    let mut segments = vec![format!("Restarts  {}", recovery.restart_count)];
    if let Some(exit) = &recovery.last_exit {
        let mut last = format!("last {}", exit.reason);
        match (exit.exit_code, exit.signal) {
            (_, Some(signal)) => last.push_str(&format!(" (signal {signal})")),
            (Some(code), None) => last.push_str(&format!(" (exit code {code})")),
            (None, None) => {}
        }
        if let Some(at) = format_deployed_at(exit.at_unix_secs) {
            last.push_str(&format!(" {at}"));
        }
        segments.push(last);
    }
    if let Some(next) = recovery.next_restart_at_unix_secs {
        let wait = next.saturating_sub(now_unix_secs);
        segments.push(if wait <= 0 {
            "next restart due".to_string()
        } else if wait < 60 {
            output::theme_warning(format!("next restart in {wait}s"))
        } else {
            output::theme_warning(format!(
                "next restart in {}",
                format_duration_human(wait as u64)
            ))
        });
    }
    segments.join("  ")
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
    use tako_core::{AppStatus, InstanceExitReason, InstanceStatus};

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
                        replica_overrides: Default::default(),
                        services: Vec::new(),
                        services_error: None,
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
                        recovery: None,
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
    fn format_recovery_explains_restarts_the_last_exit_and_the_next_attempt() {
        let exit = |reason, exit_code, signal| tako_core::InstanceExit {
            reason,
            exit_code,
            signal,
            at_unix_secs: 1_772_798_400,
        };
        let at = format_deployed_at(1_772_798_400).unwrap();
        let now = 1_772_798_410;

        let backing_off = RecoveryStatus {
            restart_count: 3,
            last_exit: Some(exit(InstanceExitReason::FailedToStart, Some(1), None)),
            next_restart_at_unix_secs: Some(now + 8),
        };
        assert_eq!(
            format_recovery(&backing_off, now),
            format!("Restarts  3  last failed to start (exit code 1) {at}  next restart in 8s")
        );
        let later = RecoveryStatus {
            next_restart_at_unix_secs: Some(now + 120),
            ..backing_off.clone()
        };
        assert!(format_recovery(&later, now).ends_with("next restart in 2m"));
        let due = RecoveryStatus {
            next_restart_at_unix_secs: Some(now),
            ..backing_off
        };
        assert!(format_recovery(&due, now).ends_with("next restart due"));

        let killed = RecoveryStatus {
            restart_count: 1,
            last_exit: Some(exit(InstanceExitReason::Crashed, None, Some(9))),
            next_restart_at_unix_secs: None,
        };
        assert_eq!(
            format_recovery(&killed, now),
            format!("Restarts  1  last crashed (signal 9) {at}")
        );
        let hung = RecoveryStatus {
            restart_count: 1,
            last_exit: Some(exit(InstanceExitReason::Unhealthy, None, None)),
            next_restart_at_unix_secs: None,
        };
        assert_eq!(
            format_recovery(&hung, now),
            format!("Restarts  1  last unhealthy {at}")
        );
    }

    #[test]
    fn format_server_status_lines_shows_recovery_under_the_app() {
        let mut global = GlobalServerStatusResult {
            service_status: "active".to_string(),
            server_version: None,
            server_uptime: None,
            process_uptime: None,
            routes: Vec::new(),
            apps: vec![super::super::GlobalAppStatusResult {
                app_name: "web".to_string(),
                env_name: "production".to_string(),
                status: ServerStatusResult {
                    service_status: "active".to_string(),
                    server_version: None,
                    app_status: Some(AppStatus {
                        replica_overrides: Default::default(),
                        services: Vec::new(),
                        services_error: None,
                        name: "web".to_string(),
                        version: "abc123".to_string(),
                        instances: vec![],
                        builds: vec![],
                        state: AppState::Running,
                        last_error: Some("Instance marked dead".to_string()),
                        recovery: Some(RecoveryStatus {
                            restart_count: 2,
                            last_exit: None,
                            next_restart_at_unix_secs: None,
                        }),
                    }),
                    deployed_at_unix_secs: None,
                    error: None,
                },
            }],
            error: None,
        };

        let lines = format_server_status_lines("prod-a", None, &global);
        assert_eq!(
            &lines[lines.len() - 2..],
            [
                "    web  healthy 0/0  production  abc123",
                "      Restarts  2",
            ]
        );

        global.apps[0].status.app_status.as_mut().unwrap().recovery = None;
        let lines = format_server_status_lines("prod-a", None, &global);
        assert_eq!(
            lines.last().unwrap(),
            "    web  healthy 0/0  production  abc123"
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
