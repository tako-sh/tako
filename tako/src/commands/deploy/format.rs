use std::path::Path;

use crate::config::ServerEntry;
use crate::output;

use super::task_tree::ArtifactBuildGroup;

/// A label + value pair for the deploy summary. The label is rendered in accent
/// color and the value in the default (normal) color.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SummaryLine {
    pub(super) label: String,
    pub(super) value: String,
}

pub(super) fn format_build_plan_target_label(group: &ArtifactBuildGroup) -> String {
    group
        .display_target_label
        .as_deref()
        .unwrap_or("shared target")
        .to_string()
}

pub(super) fn format_preflight_complete_message(server_names: &[String]) -> String {
    if server_names.len() == 1 {
        format!("Checked {}", server_names[0])
    } else {
        format!("Checked {} servers", server_names.len())
    }
}

pub(super) fn format_deploy_summary_lines(
    primary_label: &str,
    primary_value: &str,
    routes: &[String],
) -> Vec<SummaryLine> {
    let mut lines = vec![SummaryLine {
        label: primary_label.to_string(),
        value: primary_value.to_string(),
    }];
    if let Some((first_route, remaining_routes)) = routes.split_first() {
        lines.push(SummaryLine {
            label: "Routes".to_string(),
            value: format_route_url(first_route),
        });
        for route in remaining_routes {
            lines.push(SummaryLine {
                label: String::new(),
                value: format_route_url(route),
            });
        }
    }
    lines
}

pub(super) fn print_deploy_summary(primary_label: &str, primary_value: &str, routes: &[String]) {
    let lines = format_deploy_summary_lines(primary_label, primary_value, routes);
    let max_label_width = lines.iter().map(|l| l.label.len()).max().unwrap_or(0);
    for line in lines {
        let padded_label = format!("{:<width$}", line.label, width = max_label_width);
        let formatted = format!("{} {}", padded_label, line.value);
        if output::is_pretty() {
            output::line(&formatted);
        } else {
            tracing::info!("{}", formatted);
        }
    }
}

pub(super) fn format_route_url(route: &str) -> String {
    format!("https://{route}")
}

pub(super) fn format_build_stages_summary_for_output(
    stage_summary: &[String],
    target_label: Option<&str>,
) -> Option<String> {
    if stage_summary.is_empty() {
        return None;
    }
    Some(format_build_stages_summary(stage_summary, target_label))
}

pub(super) fn format_build_stages_summary(
    stage_summary: &[String],
    target_label: Option<&str>,
) -> String {
    match target_label {
        Some(label) => format!("Build stages for {}: {}", label, stage_summary.join(" -> ")),
        None => format!("Build stages: {}", stage_summary.join(" -> ")),
    }
}

pub(super) fn format_runtime_probe_message(target_label: Option<&str>) -> String {
    match target_label {
        Some(label) => format!("Resolving runtime version for {}", label),
        None => "Resolving runtime version".to_string(),
    }
}

pub(super) fn format_runtime_probe_success(target_label: Option<&str>) -> String {
    match target_label {
        Some(label) => format!("Runtime version resolved for {}", label),
        None => "Runtime version resolved".to_string(),
    }
}

pub(super) fn format_build_artifact_message(target_label: Option<&str>) -> String {
    match target_label {
        Some(label) => format!("Building artifact for {}", label),
        None => "Building artifact".to_string(),
    }
}

pub(super) fn format_build_artifact_success(target_label: Option<&str>) -> String {
    match target_label {
        Some(label) => format!("Artifact built for {}", label),
        None => "Artifact built".to_string(),
    }
}

pub(super) fn format_build_completed_message(target_label: Option<&str>) -> String {
    match target_label {
        Some(label) => format!("Built for {}", label),
        None => "Built".to_string(),
    }
}

pub(super) fn format_prepare_artifact_message(target_label: Option<&str>) -> String {
    match target_label {
        Some(label) => format!("Preparing artifact for {}", label),
        None => "Preparing artifact".to_string(),
    }
}

pub(super) fn format_prepare_artifact_success(target_label: Option<&str>) -> String {
    match target_label {
        Some(label) => format!("Artifact prepared for {}", label),
        None => "Artifact prepared".to_string(),
    }
}

pub(super) fn format_artifact_cache_hit_message_for_output(target_label: Option<&str>) -> String {
    match target_label {
        Some(label) => format!("Artifact cache hit for {}", label),
        None => "Artifact cache hit".to_string(),
    }
}

pub(super) fn format_artifact_cache_invalid_message(
    target_label: Option<&str>,
    error: &str,
) -> String {
    match target_label {
        Some(label) => format!(
            "Artifact cache entry for {} is invalid ({}); rebuilding.",
            label, error
        ),
        None => format!("Artifact cache entry is invalid ({}); rebuilding.", error),
    }
}

pub(super) fn format_artifact_ready_message(
    target_label: Option<&str>,
    artifact_path: &str,
    artifact_size: &str,
) -> String {
    match target_label {
        Some(label) => format!(
            "Artifact ready for {}: {} ({})",
            label, artifact_path, artifact_size
        ),
        None => format!("Artifact ready: {} ({})", artifact_path, artifact_size),
    }
}

pub(super) fn format_artifact_ready_message_for_output(target_label: Option<&str>) -> String {
    match target_label {
        Some(label) => format!("Artifact ready for {}", label),
        None => "Artifact ready".to_string(),
    }
}

pub(super) fn format_deploy_main_message(
    main: &str,
    target_label: &str,
    use_unified_target_process: bool,
) -> String {
    if use_unified_target_process {
        return format!("Deploy main: {}", main);
    }
    format!("Deploy main: {} (artifact target: {})", main, target_label)
}

pub(super) fn format_parallel_deploy_step(server_count: usize) -> String {
    format!("Deploying to {} server(s) in parallel", server_count)
}

pub(super) fn format_server_deploy_target(name: &str, entry: &ServerEntry) -> String {
    format!("{name} (tako@{}:{})", entry.host, entry.port)
}

pub(super) fn format_server_deploy_success(name: &str, entry: &ServerEntry) -> String {
    format_server_deploy_target(name, entry)
}

pub(super) fn format_server_deploy_failure(name: &str, entry: &ServerEntry, error: &str) -> String {
    format!("{}: {}", format_server_deploy_target(name, entry), error)
}

pub(super) fn format_deploy_step_failure(step: &str, error: &str) -> String {
    format!("{step} failed: {error}")
}

pub(super) fn format_server_mapping_option(name: &str, entry: &ServerEntry) -> String {
    match entry.description.as_deref().map(str::trim) {
        Some(description) if !description.is_empty() => {
            format!("{name} ({description})  tako@{}:{}", entry.host, entry.port)
        }
        _ => format!("{name}  tako@{}:{}", entry.host, entry.port),
    }
}

pub(super) fn format_environment_not_found_error(env: &str, available: &[String]) -> String {
    let available_text = if available.is_empty() {
        "(none)".to_string()
    } else {
        available.join(", ")
    };
    format!(
        "Environment '{}' not found. Available: {}",
        env, available_text
    )
}

pub(super) fn format_no_servers_for_env_error(env: &str) -> String {
    format!(
        "No servers configured for environment '{}'. Add `servers = [\"<name>\"]` under [envs.{}] in tako.toml.",
        env, env
    )
}

pub(super) fn format_no_global_servers_error() -> String {
    "No servers have been added. Run 'tako servers add <host>' first, then add the server under [envs.production].servers in tako.toml.".to_string()
}

pub(super) fn format_server_not_found_error(server_name: &str) -> String {
    format!(
        "Server '{}' not found in config.toml [[servers]]. Run 'tako servers add --name {} <host>'.",
        server_name, server_name
    )
}

pub(super) fn format_partial_failure_error(failed_servers: usize) -> String {
    format!("{} server(s) failed", failed_servers)
}

pub(super) fn format_runtime_summary(runtime_name: &str, version: Option<&str>) -> String {
    match version.map(str::trim) {
        Some(version) if !version.is_empty() => {
            format!("Runtime: {} ({})", runtime_name, version)
        }
        _ => format!("Runtime: {}", runtime_name),
    }
}

pub(super) fn format_entry_point_summary(entry_point: &Path) -> String {
    format!("Entry point: {}", entry_point.display())
}

pub(super) fn format_servers_summary(server_names: &[String]) -> String {
    format!("Servers: {}", server_names.join(", "))
}

pub(super) fn format_server_target_metadata_error(
    missing: &[String],
    invalid: &[String],
) -> String {
    let mut details = Vec::new();
    if !missing.is_empty() {
        details.push(format!("missing targets for: {}", missing.join(", ")));
    }
    if !invalid.is_empty() {
        details.push(format!("invalid targets for: {}", invalid.join(", ")));
    }

    format!(
        "Deploy target metadata check failed: {}. Remove and add each affected server again (`tako servers rm <name>` then `tako servers add --name <name> <host>`). Deploy does not probe server targets.",
        details.join("; ")
    )
}

pub(super) fn format_server_targets_summary(
    server_targets: &[(String, crate::config::ServerTarget)],
    use_unified_target_process: bool,
) -> Option<String> {
    if use_unified_target_process {
        return None;
    }
    let mut labels = server_targets
        .iter()
        .map(|(_, target)| target.label())
        .collect::<Vec<_>>();
    labels.sort();
    labels.dedup();
    Some(format!("Server targets: {}", labels.join(", ")))
}

pub(super) fn should_use_unified_js_target_process(runtime_tool: &str) -> bool {
    matches!(runtime_tool, "bun" | "node")
}

pub(super) fn shorten_commit(commit: &str) -> &str {
    &commit[..commit.len().min(12)]
}

pub(super) fn should_use_per_server_spinners(server_count: usize, interactive: bool) -> bool {
    interactive && server_count == 1
}

pub(super) fn should_use_local_build_spinners(interactive: bool) -> bool {
    interactive
}

pub(super) fn format_bun_lockfile_preflight_error(detail: &str) -> String {
    let normalized = detail.trim();
    if normalized.contains("lockfile had changes, but lockfile is frozen") {
        return "Bun lockfile check failed: package manifests and the Bun lockfile are out of sync. Run `bun install`, commit bun.lock/bun.lockb, then re-run `tako deploy`.".to_string();
    }
    if normalized.is_empty() {
        return "Bun lockfile check failed with no output.".to_string();
    }
    format!("Bun lockfile check failed: {}", normalized)
}

pub(super) fn format_stage_label(stage_number: usize, stage_name: Option<&str>) -> String {
    match stage_name.map(str::trim).filter(|value| !value.is_empty()) {
        Some(name) => format!("Stage '{name}'"),
        None => format!("Stage {stage_number}"),
    }
}

/// Alias for the shared size formatter.
pub(super) fn format_size(bytes: u64) -> String {
    output::format_size(bytes)
}

pub(super) fn format_path_relative_to(project_dir: &Path, path: &Path) -> String {
    match path.strip_prefix(project_dir) {
        Ok(relative) if !relative.as_os_str().is_empty() => relative.display().to_string(),
        Ok(_) => ".".to_string(),
        Err(_) => path.display().to_string(),
    }
}

#[cfg(test)]
pub(super) fn format_prepare_deploy_section(env: &str) -> String {
    format!("Preparing deployment for {}", output::strong(env))
}

#[cfg(test)]
pub(super) fn format_deploy_overview_lines(
    app_name: &str,
    _env: &str,
    target_count: usize,
    primary_target_and_server: Option<(&str, &ServerEntry)>,
) -> Vec<String> {
    let mut lines = vec![format!("App       : {app_name}")];
    match primary_target_and_server {
        Some((target_name, server)) => {
            lines.push(format!("Target    : {target_name}"));
            lines.push(format!("Host      : tako@{}:{}", server.host, server.port));
        }
        None => {
            let label = if target_count == 1 {
                "1 server".to_string()
            } else {
                format!("{target_count} servers")
            };
            lines.push(format!("Target    : {label}"));
        }
    }
    lines
}

#[cfg(test)]
mod tests;
