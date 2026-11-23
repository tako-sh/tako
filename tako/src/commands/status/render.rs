use super::remote::{display_server_version, sort_global_apps};
use super::time::{format_deployed_at, format_duration_human};
use super::{GlobalServerStatusResult, ServerStatusResult};
use crate::config::ServerInventory;
use crate::output;
use tako_core::compose::ComposeServiceKind;
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
    servers: &ServerInventory,
    server_names: &[String],
    server_results: &mut std::collections::HashMap<String, GlobalServerStatusResult>,
    project: Option<&crate::commands::project_resources::ProjectResources>,
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

        let databases = super::server_databases(servers, server_name, project);
        for line in
            format_server_status_lines(server_name, description.as_deref(), &global, &databases)
        {
            eprintln!("{line}");
        }
        eprintln!();
    }
}

fn format_server_status_lines(
    server_name: &str,
    description: Option<&str>,
    global: &GlobalServerStatusResult,
    databases: &[(String, Vec<String>)],
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

    let resources = global
        .resources
        .as_ref()
        .map(format_resource_lines)
        .unwrap_or_default();
    if !resources.is_empty() {
        lines.push(String::new());
        lines.push(format!("{}Resources", output::INDENT));
        lines.extend(resources);
    }

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

    if !databases.is_empty() {
        lines.push(String::new());
        lines.push(format!("{}Databases", output::INDENT));
        lines.extend(format_database_lines(databases));
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

    let process_uptime = global
        .process_uptime
        .as_ref()
        .filter(|_| global.service_status != "upgrading");
    match (&global.server_uptime, process_uptime) {
        (Some(host_uptime), Some(process_uptime)) => {
            segments.push(format!("host up {host_uptime}"));
            segments.push(format!("server up {process_uptime}"));
        }
        (Some(host_uptime), None) => segments.push(format!("host up {host_uptime}")),
        (None, Some(process_uptime)) => segments.push(format!("up {process_uptime}")),
        (None, None) => {}
    }

    format!("{}{}", output::INDENT, segments.join("  "))
}

/// CPU, memory, swap, and data disk rows, colored as they run out.
fn format_resource_lines(host: &tako_core::HostStats) -> Vec<String> {
    let mut rows: Vec<(&str, String)> = Vec::new();

    if let Some(cpu) = &host.cpu {
        let mut text = format!(
            "{} {}",
            cpu.cores,
            if cpu.cores == 1 { "core" } else { "cores" }
        );
        if let Some([one, five, fifteen]) = cpu.load_average {
            let busy = (one >= f64::from(cpu.cores)).then_some(StatusColor::Warning);
            text.push_str("  ");
            text.push_str(&colorize(
                &format!("load {one:.2} {five:.2} {fifteen:.2}"),
                busy,
            ));
        }
        rows.push(("CPU", text));
    }

    if let Some(memory) = &host.memory
        && memory.total_bytes > 0
    {
        let used = memory.total_bytes.saturating_sub(memory.available_bytes);
        rows.push(("Memory", format_usage(used, memory.total_bytes)));
        if memory.swap_total_bytes > 0 {
            let used = memory
                .swap_total_bytes
                .saturating_sub(memory.swap_free_bytes);
            rows.push(("Swap", format_usage(used, memory.swap_total_bytes)));
        }
    }

    if let Some(disk) = &host.disk
        && disk.total_bytes > 0
    {
        // Like `df`, measure use against what is usable, not root's reserve.
        let percent = usage_percent(
            disk.used_bytes,
            disk.used_bytes.saturating_add(disk.available_bytes),
        );
        rows.push((
            "Disk",
            format!(
                "{} free of {} ({})",
                output::format_size(disk.available_bytes),
                output::format_size(disk.total_bytes),
                colorize(&format!("{percent}% used"), usage_color(percent)),
            ),
        ));
    }

    let width = rows.iter().map(|(label, _)| label.len()).max().unwrap_or(0);
    rows.into_iter()
        .map(|(label, text)| {
            format!(
                "{indent}{label:<width$}  {text}",
                indent = output::INDENT.repeat(2)
            )
        })
        .collect()
}

fn format_usage(used: u64, total: u64) -> String {
    let percent = usage_percent(used, total);
    format!(
        "{} used of {} ({})",
        output::format_size(used),
        output::format_size(total),
        colorize(&format!("{percent}%"), usage_color(percent)),
    )
}

/// Whole percent, rounded up as `df` does.
fn usage_percent(used: u64, total: u64) -> u64 {
    if total == 0 {
        return 0;
    }
    let percent = (u128::from(used) * 100).div_ceil(u128::from(total));
    percent.min(100) as u64
}

fn usage_color(percent: u64) -> Option<StatusColor> {
    match percent {
        90.. => Some(StatusColor::Error),
        80.. => Some(StatusColor::Warning),
        _ => None,
    }
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
            for child in &status.applications {
                let label = match (child.kind, child.sdk) {
                    (ComposeServiceKind::Http, true) => "HTTP app, SDK",
                    (ComposeServiceKind::Http, false) => "HTTP app",
                    (ComposeServiceKind::Worker, _) => "SDK worker",
                };
                let count = match child.kind {
                    ComposeServiceKind::Http => format!("{} instance(s)", child.instances.len()),
                    ComposeServiceKind::Worker => format!(
                        "{} container(s)",
                        status
                            .services
                            .iter()
                            .filter(|service| service.service == child.service)
                            .count()
                    ),
                };
                lines.push(format!(
                    "{}{} ({label})  {}  {count}",
                    output::INDENT.repeat(3),
                    child.service,
                    child.state,
                ));
            }
            for service in status.services.iter().filter(|service| {
                !status
                    .applications
                    .iter()
                    .any(|app| app.service == service.service)
            }) {
                lines.push(format!(
                    "{}{}  {}  {}",
                    output::INDENT.repeat(3),
                    format!("{} (Docker service)", service.service),
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

/// One line per database homed on a server, naming the app environments of
/// this project that bind it.
fn format_database_lines(databases: &[(String, Vec<String>)]) -> Vec<String> {
    let width = databases
        .iter()
        .map(|(name, _)| name.len())
        .max()
        .unwrap_or(0);
    databases
        .iter()
        .map(|(name, bound_by)| {
            let detail = if bound_by.is_empty() {
                output::theme_muted("not used by this project").to_string()
            } else {
                let users: Vec<_> = bound_by
                    .iter()
                    .map(|scope| crate::commands::project_resources::database_user_label(scope))
                    .collect();
                format!("used by {}", users.join(", "))
            };
            format!(
                "{indent}{name:<width$}  {detail}",
                indent = output::INDENT.repeat(2)
            )
        })
        .collect()
}

/// The project's app environments and what each holds, after the servers.
pub(super) fn render_project(project: &crate::commands::project_resources::ProjectResources) {
    for line in format_project_lines(project) {
        eprintln!("{line}");
    }
    eprintln!();
}

fn format_project_lines(
    project: &crate::commands::project_resources::ProjectResources,
) -> Vec<String> {
    use crate::commands::project_resources::Access;
    let mut lines = vec![format!("App {}", output::strong(&project.app))];
    let width = project
        .envs
        .iter()
        .map(|env| env.name.len())
        .max()
        .unwrap_or(0);
    for env in &project.envs {
        let mut header = vec![format!("{:<width$}", env.name)];
        match (&env.network, &env.access) {
            (_, Access::Local) => header.push("local only".to_string()),
            (Some(network), Access::Full) => header.push(format!("network {network}")),
            (Some(network), Access::Deploy) => {
                header.push(format!("network {network} (deploy client)"))
            }
            (Some(network), _) => header.push(format!("network {network} (not a member)")),
            (None, _) => {}
        }
        if !env.servers.is_empty() {
            header.push(format!("servers {}", env.servers.join(", ")));
        }
        lines.push(format!("{}{}", output::INDENT, header.join("  ")));

        let indent = output::INDENT.repeat(2);
        let mut held = Vec::new();
        if let Some(count) = env.secrets {
            held.push(format!(
                "{count} secret{}",
                if count == 1 { "" } else { "s" }
            ));
        }
        if let Some(credentials) = &env.credentials
            && !credentials.is_empty()
        {
            held.push(format!("credentials {}", credentials.join(", ")));
        }
        for storage in &env.storages {
            let missing = if storage.credentials == Some(false) {
                " (no credentials)"
            } else {
                ""
            };
            held.push(format!(
                "storage {} → {}{missing}",
                storage.binding, storage.resource
            ));
        }
        if env.backup_key == Some(true) {
            held.push("backup key".to_string());
        }
        if !held.is_empty() {
            lines.push(format!("{indent}{}", held.join("  ")));
        }
        if !env.databases.is_empty() {
            let databases: Vec<String> = env
                .databases
                .iter()
                .map(|db| match &db.server {
                    Some(server) => format!("{} → {} on {server}", db.usage.label(), db.database),
                    None => format!("{} → {} (no network record)", db.usage.label(), db.database),
                })
                .collect();
            lines.push(format!("{indent}databases {}", databases.join(", ")));
        }
        if !env.kv.is_empty() {
            let stores: Vec<String> = env
                .kv
                .iter()
                .map(|db| match &db.server {
                    Some(server) => format!("{} → {} on {server}", db.usage.label(), db.database),
                    None => format!("{} → {} (no network record)", db.usage.label(), db.database),
                })
                .collect();
            lines.push(format!("{indent}kv {}", stores.join(", ")));
        }
        if let Some(note) = &env.note {
            lines.push(format!("{indent}{}", output::theme_muted(note)));
        }
    }
    lines
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
            resources: None,
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
                        kind: Default::default(),
                        applications: Vec::new(),
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

        let lines = format_server_status_lines("prod-a", None, &global, &[]);

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
            resources: None,
            routes: Vec::new(),
            apps: vec![super::super::GlobalAppStatusResult {
                app_name: "web".to_string(),
                env_name: "production".to_string(),
                status: ServerStatusResult {
                    service_status: "active".to_string(),
                    server_version: None,
                    app_status: Some(AppStatus {
                        kind: Default::default(),
                        applications: Vec::new(),
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

        let lines = format_server_status_lines("prod-a", None, &global, &[]);
        assert_eq!(
            &lines[lines.len() - 2..],
            [
                "    web  healthy 0/0  production  abc123",
                "      Restarts  2",
            ]
        );

        global.apps[0].status.app_status.as_mut().unwrap().recovery = None;
        let lines = format_server_status_lines("prod-a", None, &global, &[]);
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
            resources: None,
            routes: Vec::new(),
            apps: Vec::new(),
            error: Some("Remote management failed".to_string()),
        };

        let lines = format_server_status_lines("prod-a", None, &global, &[]);

        assert_eq!(
            lines,
            vec!["Server prod-a", "  Error  Remote management failed"]
        );
    }

    fn project_with(
        envs: Vec<crate::commands::project_resources::EnvResources>,
    ) -> crate::commands::project_resources::ProjectResources {
        crate::commands::project_resources::ProjectResources {
            app: "shop".into(),
            config: crate::config::TakoToml::default(),
            secrets: crate::config::SecretsStore::default(),
            envs,
            databases: Default::default(),
        }
    }

    fn env(
        name: &str,
        access: crate::commands::project_resources::Access,
    ) -> crate::commands::project_resources::EnvResources {
        use crate::commands::project_resources::*;
        let readable = matches!(access, Access::Full);
        EnvResources {
            name: name.into(),
            network: Some("main".into()),
            network_id: Some("0123456789abcdef0123456789abcdef".into()),
            servers: vec!["edge-a".into()],
            secrets: readable.then_some(3),
            credentials: readable.then(|| vec!["postgres_url".into()]),
            storages: vec![StorageBinding {
                binding: "uploads".into(),
                resource: "media".into(),
                credentials: readable.then_some(false),
            }],
            backup_key: readable.then_some(true),
            databases: vec![
                ProjectDatabase {
                    usage: DatabaseUsage::Binding {
                        binding: "main".into(),
                    },
                    database: "orders".into(),
                    server: Some("edge-a".into()),
                },
                ProjectDatabase {
                    usage: DatabaseUsage::ChannelReplay,
                    database: "events".into(),
                    server: Some("edge-b".into()),
                },
            ],
            kv: vec![],
            note: (!readable).then(|| DEPLOY_CLIENT_NOTE.into()),
            access,
        }
    }

    #[test]
    fn project_lines_show_each_environments_resources_without_values() {
        use crate::commands::project_resources::Access;
        let project = project_with(vec![
            env("production", Access::Full),
            env("staging", Access::Deploy),
        ]);
        let lines: Vec<String> = format_project_lines(&project)
            .into_iter()
            .map(|line| console::strip_ansi_codes(&line).to_string())
            .collect();
        assert_eq!(
            lines,
            vec![
                "App shop",
                "  production  network main  servers edge-a",
                "    3 secrets  credentials postgres_url  storage uploads → media (no credentials)  backup key",
                "    databases main → orders on edge-a, channel replay → events on edge-b",
                "  staging     network main (deploy client)  servers edge-a",
                "    storage uploads → media",
                "    databases main → orders on edge-a, channel replay → events on edge-b",
                "    Secrets, credentials and backup keys need a full client.",
            ]
        );
    }

    fn plain(lines: Vec<String>) -> Vec<String> {
        lines
            .into_iter()
            .map(|line| console::strip_ansi_codes(&line).to_string())
            .collect()
    }

    fn host_stats() -> tako_core::HostStats {
        const GB: u64 = 1 << 30;
        tako_core::HostStats {
            uptime_secs: Some(12 * 86_400 + 3 * 3600),
            cpu: Some(tako_core::CpuStats {
                cores: 4,
                load_average: Some([0.52, 0.58, 0.6]),
            }),
            memory: Some(tako_core::MemoryStats {
                total_bytes: 8 * GB,
                available_bytes: 6 * GB,
                swap_total_bytes: 0,
                swap_free_bytes: 0,
            }),
            disk: Some(tako_core::DiskStats {
                path: "/var/lib/tako".into(),
                total_bytes: 40 * GB,
                used_bytes: 26 * GB,
                available_bytes: 12 * GB,
            }),
        }
    }

    fn active_server(host: Option<tako_core::HostStats>) -> GlobalServerStatusResult {
        GlobalServerStatusResult {
            service_status: "active".into(),
            server_version: Some("0.1.0".into()),
            server_uptime: host
                .as_ref()
                .and_then(|host| host.uptime_secs)
                .map(format_duration_human),
            process_uptime: Some("4d 6h".into()),
            resources: host,
            routes: vec![],
            apps: vec![],
            error: None,
        }
    }

    #[test]
    fn server_lines_show_host_resources_under_the_summary() {
        let lines = plain(format_server_status_lines(
            "prod-a",
            None,
            &active_server(Some(host_stats())),
            &[],
        ));
        assert_eq!(
            lines,
            vec![
                "Server prod-a",
                "  active  v0.1.0  host up 12d 3h  server up 4d 6h",
                "",
                "  Resources",
                "    CPU     4 cores  load 0.52 0.58 0.60",
                "    Memory  2.00 GB used of 8.00 GB (25%)",
                "    Disk    12.00 GB free of 40.00 GB (69% used)",
            ]
        );
    }

    #[test]
    fn resource_lines_add_swap_and_leave_out_what_the_server_could_not_read() {
        let mut host = host_stats();
        if let Some(memory) = host.memory.as_mut() {
            memory.swap_total_bytes = 2 << 30;
            memory.swap_free_bytes = 1 << 30;
        }
        host.disk = None;
        host.cpu.as_mut().unwrap().load_average = None;
        assert_eq!(
            plain(format_resource_lines(&host)),
            vec![
                "    CPU     4 cores",
                "    Memory  2.00 GB used of 8.00 GB (25%)",
                "    Swap    1.00 GB used of 2.00 GB (50%)",
            ]
        );

        let only_uptime = tako_core::HostStats {
            uptime_secs: Some(60),
            ..Default::default()
        };
        assert!(format_resource_lines(&only_uptime).is_empty());
        let lines =
            format_server_status_lines("prod-a", None, &active_server(Some(only_uptime)), &[]);
        assert_eq!(lines.len(), 2, "{lines:?}");
    }

    #[test]
    fn server_summary_labels_each_uptime() {
        let mut server = active_server(None);
        assert_eq!(
            plain(vec![format_server_summary(&server)]),
            ["  active  v0.1.0  up 4d 6h"]
        );

        server.server_uptime = Some("12d 3h".into());
        server.service_status = "upgrading".into();
        assert!(plain(vec![format_server_summary(&server)])[0].ends_with("v0.1.0  host up 12d 3h"));
    }

    #[test]
    fn usage_percent_rounds_up_and_colors_near_full() {
        assert_eq!(usage_percent(0, 100), 0);
        assert_eq!(usage_percent(1, 1000), 1);
        assert_eq!(usage_percent(799, 1000), 80);
        assert_eq!(usage_percent(5, 0), 0);
        assert_eq!(usage_percent(u64::MAX, u64::MAX), 100);
        assert!(usage_color(79).is_none());
        assert!(matches!(usage_color(80), Some(StatusColor::Warning)));
        assert!(matches!(usage_color(90), Some(StatusColor::Error)));
    }

    #[test]
    fn server_lines_list_the_databases_homed_there() {
        let global = GlobalServerStatusResult {
            service_status: "active".into(),
            server_version: None,
            server_uptime: None,
            process_uptime: None,
            resources: None,
            routes: vec![],
            apps: vec![],
            error: None,
        };
        let databases = vec![
            ("audit".to_string(), vec![]),
            (
                "orders".to_string(),
                vec!["shop/production#channels".to_string()],
            ),
        ];
        let lines: Vec<String> = format_server_status_lines("edge-a", None, &global, &databases)
            .into_iter()
            .map(|line| console::strip_ansi_codes(&line).to_string())
            .collect();
        assert_eq!(
            &lines[2..],
            &[
                String::new(),
                "  Databases".to_string(),
                "    audit   not used by this project".to_string(),
                "    orders  used by shop/production (channel replay)".to_string(),
            ]
        );
    }
}
